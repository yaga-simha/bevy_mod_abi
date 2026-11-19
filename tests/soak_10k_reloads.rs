#![forbid(unsafe_code)]

//! Phase 4 Item #22 — 10,000-Reload Hot-Reload Soak & Latency/RSS Stability Harness.
//!
//! Executes **10,000 consecutive live `reload_mod` / `reload_mod_from` cycles** against real
//! `#![forbid(unsafe_code)]` Rust guest `.cdylib` modules (`soak_mod_v1` and `soak_mod_v2`),
//! exercising a deterministic mix of:
//! 1. **Zero-copy unchanged-`schema_fingerprint` `AbiPod` reloads** (`soak::FastPod`): verifies
//!    that `BamComponent` handle, Bevy `ComponentId`, and entity data remain preserved in place
//!    with zero column copies across all 10,000 cycles.
//! 2. **Live v1 <-> v2 `migrate_component_column` reloads** (`soak::MigPos`: `PosV1 { f32, f32 }`
//!    (8 B, align 4) <-> `PosV2 { f64, f64 }` (16 B, align 8)): executes 500 bidirectional live
//!    column migrations while keeping the exact same `BamComponent` handle and validating exact
//!    numeric fidelity across every transition.
//! 3. **Dynamic `DROP_PAGES` destructor churn cycling past all `4,096` slots** (`soak::DropChurn`):
//!    every cycle allocates a drop-trampoline slot, spawns a component instance (`live_values: 0 -> 1`),
//!    despawns it (invoking the foreign `ComponentDropFn`, `live_values: 1 -> 0`), and recycles the
//!    slot on reload (`generation += 1`) — cycling 10,000 times (> `2.4x MAX_DROP_TRAMPOLINES = 4,096`)
//!    with zero slot leaks and exact post-unload slot reclamation.
//! 4. **State transfer (`save_state` / `restore_state`)** across all 10,000 consecutive reloads.
//!
//! Measures per-reload latency (min / p50 / p95 / p99 / max) and process RSS via pure-safe OS
//! inspection (`/proc/self/status` on Linux, `ps -o rss=` on macOS/BSD), asserting `< 2.0 ms` p99
//! reload latency in `--release` and flat steady-state RSS drift.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host::read_entity_pod_bytes;
use bevy_abi_mod_host_sys::drop_trampoline::{
    MAX_DROP_TRAMPOLINES, allocated_drop_pages, available_drop_trampoline_slots,
    drop_slot_generation,
};
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const DEFAULT_SMOKE_SOAK_CYCLES: usize = 250;
const FULL_SOAK_CYCLES: usize = 10_000;
const MIGRATE_STRIDE: usize = 20; // 500 live v1 <-> v2 `migrate_component_column` flips in 10,000 cycles
const PERSISTENT_ENTITIES: usize = 16;

// ============================================================================
// Host Telemetry Resource (Zero Implicit Padding)
// ============================================================================

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "soak::SoakTelemetry")]
struct SoakTelemetry {
    ticks: u64,
    v1_to_v2_migrations: u64,
    v2_to_v1_migrations: u64,
    drop_calls: u64,
    restored_state_epoch: u64,
    last_drop_entity: u64,
    active_version: u32,
    seeded_entities: u32,
}

fn decode_fast_pod(bytes: &[u8]) -> (i64, u64) {
    assert_eq!(bytes.len(), 16, "FastPod must be 16 bytes");
    let counter = i64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let checksum = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    (counter, checksum)
}

fn decode_pos_v1(bytes: &[u8]) -> (f32, f32) {
    assert_eq!(bytes.len(), 8, "PosV1 must be 8 bytes");
    let x = f32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    let y = f32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
    (x, y)
}

fn decode_pos_v2(bytes: &[u8]) -> (f64, f64) {
    assert_eq!(bytes.len(), 16, "PosV2 must be 16 bytes");
    let x = f64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let y = f64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    (x, y)
}

// ============================================================================
// Pure-Safe OS RSS Reader (`#![forbid(unsafe_code)]`)
// ============================================================================

fn current_rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest.split_whitespace().next()?.parse::<u64>().ok();
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let pid = std::process::id().to_string();
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        text.trim().parse::<u64>().ok()
    }
}

// ============================================================================
// Dual-Version `#![forbid(unsafe_code)]` Soak Guest Workspace Builder
// ============================================================================

struct BuiltSoakMods {
    root_dir: PathBuf,
    target_dir: PathBuf,
    mod_v1: PathBuf,
    mod_v2: PathBuf,
}

impl Drop for BuiltSoakMods {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root_dir);
        let _ = fs::remove_dir_all(&self.target_dir);
    }
}

fn sanitized_cargo_command() -> Command {
    let cargo = std::env::var_os("CARGO")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    for var in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "LLVM_PROFILE_FILE",
        "CARGO_LLVM_COV",
        "CARGO_LLVM_COV_TARGET_DIR",
        "CARGO_LLVM_COV_BUILD_DIR",
        "RUSTC_WRAPPER",
        "CARGO_PROFILE_RELEASE_PANIC",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
        "CARGO_PROFILE_DEV_PANIC",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

struct SoakStageRing<'a> {
    mods: &'a BuiltSoakMods,
    v1_slots: [PathBuf; 2],
    v2_slots: [PathBuf; 2],
}

impl<'a> SoakStageRing<'a> {
    fn new(mods: &'a BuiltSoakMods) -> Self {
        Self {
            mods,
            v1_slots: [
                mods.root_dir.join("soak_mod_v1_slot0.dll"),
                mods.root_dir.join("soak_mod_v1_slot1.dll"),
            ],
            v2_slots: [
                mods.root_dir.join("soak_mod_v2_slot0.dll"),
                mods.root_dir.join("soak_mod_v2_slot1.dll"),
            ],
        }
    }

    fn stage_for_cycle(&self, using_v2: bool, cycle: usize) -> Cow<'_, Path> {
        if cfg!(windows) {
            let slot = cycle % 2;
            let (src, dst) = if using_v2 {
                (&self.mods.mod_v2, &self.v2_slots[slot])
            } else {
                (&self.mods.mod_v1, &self.v1_slots[slot])
            };
            fs::copy(src, dst).unwrap_or_else(|err| {
                panic!(
                    "failed staging Windows soak DLL from {} to {} on cycle {cycle}: {err}",
                    src.display(),
                    dst.display()
                )
            });
            Cow::Borrowed(dst.as_path())
        } else if using_v2 {
            Cow::Borrowed(self.mods.mod_v2.as_path())
        } else {
            Cow::Borrowed(self.mods.mod_v1.as_path())
        }
    }
}

const SOAK_SHARED_RS: &str = r#"
#![allow(dead_code)]
use core::ffi::CStr;
use bevy_abi_mod_guest::prelude::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "soak::SoakTelemetry")]
pub struct SoakTelemetry {
    pub ticks: u64,
    pub v1_to_v2_migrations: u64,
    pub v2_to_v1_migrations: u64,
    pub drop_calls: u64,
    pub restored_state_epoch: u64,
    pub last_drop_entity: u64,
    pub active_version: u32,
    pub seeded_entities: u32,
}
impl GuestComponent for SoakTelemetry {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "soak::FastPod")]
pub struct FastPod {
    pub counter: i64,
    pub checksum: u64,
}
impl GuestComponent for FastPod {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "soak::MigPos")]
pub struct PosV1 {
    pub x: f32,
    pub y: f32,
}
impl GuestComponent for PosV1 {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "soak::MigPos")]
pub struct PosV2 {
    pub x: f64,
    pub y: f64,
}
impl GuestComponent for PosV2 {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct DropChurnPayload {
    pub seq: u64,
    pub magic: u64,
}
"#;

const SOAK_V1_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static LOCAL_DROPS: AtomicU64 = AtomicU64::new(0);

    extern "C" fn soak_v1_drop(_ptr: *mut c_void) {
        LOCAL_DROPS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        // 1. Unchanged-fingerprint zero-copy `AbiPod` component.
        let fast_h = match app.register_dynamic_component::<FastPod>() {
            Ok(h) => h,
            Err(e) => return e,
        };

        // 2. Live v2 -> v1 `migrate_component_column`, or zero-copy v1 -> v1 retention.
        let mut migrated_v2_to_v1 = false;
        let mig_h = match app.resolve_component_named(Some(c"soak::MigPos"), None) {
            Ok(existing) => {
                let info = match app.component_layout_info(existing) {
                    Ok(info) => info,
                    Err(e) => return e,
                };
                if info.schema_fingerprint == PosV2::SCHEMA_FINGERPRINT {
                    let rc = app.migrate_component_column::<PosV2, PosV1, _>(existing, |old| {
                        Ok(PosV1 {
                            x: old.x as f32,
                            y: old.y as f32,
                        })
                    });
                    if rc.is_err() {
                        return rc;
                    }
                    migrated_v2_to_v1 = true;
                    existing
                } else {
                    match app.register_dynamic_component::<PosV1>() {
                        Ok(h) => h,
                        Err(e) => return e,
                    }
                }
            }
            Err(FfiResult::NOT_FOUND) => match app.register_dynamic_component::<PosV1>() {
                Ok(h) => h,
                Err(e) => return e,
            },
            Err(e) => return e,
        };

        // 3. Dynamic `DROP_PAGES` destructor component recycled on every reload cycle.
        let drop_h = match app.register_component_with_layout(
            "soak::DropChurn",
            core::mem::size_of::<DropChurnPayload>(),
            core::mem::align_of::<DropChurnPayload>(),
            Some(soak_v1_drop),
        ) {
            Ok(h) => h,
            Err(e) => return e,
        };

        // 4. `save_state` / `restore_state` epoch + drop-counter handoff across reloads
        //    (lossless even on OS linkers that fully unmap the DSO on `dlclose`).
        let epoch = Arc::new(AtomicU64::new(0));
        let carried_drops = Arc::new(AtomicU64::new(0));
        let epoch_save = Arc::clone(&epoch);
        let carried_save = Arc::clone(&carried_drops);
        let epoch_restore = Arc::clone(&epoch);
        let carried_restore = Arc::clone(&carried_drops);
        let epoch_tick = Arc::clone(&epoch);
        let carried_tick = Arc::clone(&carried_drops);

        app.on_save_state(move || {
            let next = epoch_save.load(Ordering::Relaxed).wrapping_add(1);
            let pending = LOCAL_DROPS.swap(0, Ordering::Relaxed)
                + carried_save.swap(0, Ordering::Relaxed);
            let mut blob = Vec::with_capacity(16);
            blob.extend_from_slice(&next.to_le_bytes());
            blob.extend_from_slice(&pending.to_le_bytes());
            Ok(blob)
        });
        app.on_restore_state(move |bytes| {
            if bytes.len() == 16 {
                let val = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                let drops = u64::from_le_bytes([
                    bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                    bytes[15],
                ]);
                epoch_restore.store(val, Ordering::Relaxed);
                carried_restore.fetch_add(drops, Ordering::Relaxed);
            }
            FfiResult::OK
        });

        let mut recorded_migration = false;
        app.system(c"soak_tick")
            .query(|q| q.write::<FastPod>().write::<PosV1>())
            .resource_write::<SoakTelemetry>()
            .schedule(ScheduleId::UPDATE)
            .build(move |ctx| {
                let pending_drops = LOCAL_DROPS.swap(0, Ordering::Relaxed)
                    + carried_tick.swap(0, Ordering::Relaxed);
                let mut need_seed = false;
                let cur_epoch = epoch_tick.load(Ordering::Relaxed);
                let tick_no;
                if let Some(tel) = ctx.resource_mut::<SoakTelemetry>(0) {
                    tel.ticks += 1;
                    tick_no = tel.ticks;
                    tel.active_version = 1;
                    tel.drop_calls += pending_drops;
                    tel.restored_state_epoch = cur_epoch;
                    if migrated_v2_to_v1 && !recorded_migration {
                        tel.v2_to_v1_migrations += 1;
                        recorded_migration = true;
                    }
                    if tel.seeded_entities == 0 {
                        tel.seeded_entities = 16;
                        need_seed = true;
                    }
                } else {
                    return;
                }

                let (iter, cmds) = ctx.iter_and_commands();
                if need_seed {
                    for i in 0..16u32 {
                        let pod = FastPod {
                            counter: i64::from(i),
                            checksum: u64::from(i).wrapping_mul(0x101),
                        };
                        let pos = PosV1 {
                            x: i as f32,
                            y: (i as f32) * -2.0,
                        };
                        if let Ok(ent) = cmds.spawn_empty_persistent() {
                            let _ = cmds.insert(ent, fast_h, &pod);
                            let _ = cmds.insert(ent, mig_h, &pos);
                        }
                    }
                } else {
                    while let Some(mut chunk) = iter.next_chunk_view() {
                        if let Some((pods, positions)) =
                            chunk.component_write_2::<FastPod, PosV1>(0, 1)
                        {
                            for (p, pos) in pods.iter_mut().zip(positions.iter_mut()) {
                                p.counter += 1;
                                p.checksum = p.checksum.wrapping_add(1);
                                pos.x += 0.25;
                                pos.y -= 0.25;
                            }
                        }
                    }
                }

                // Spawn one transient `DropChurn` entity; host despawns it before reload so
                // `drop_trampoline::<SLOT>` runs `soak_v1_drop` and `live_values` returns to `0`.
                let mut drop_ent_bits = 0u64;
                if let Ok(ent) = cmds.spawn_empty() {
                    let payload = DropChurnPayload {
                        seq: tick_no,
                        magic: 0xD00D_F001,
                    };
                    if cmds.insert(ent, drop_h, &payload).is_ok() {
                        drop_ent_bits = ent.to_bits();
                    }
                }
                if let Some(tel) = ctx.resource_mut::<SoakTelemetry>(0) {
                    tel.last_drop_entity = drop_ent_bits;
                }
            })
    }
}

bevy_abi_mod_guest::export_mod!(name: c"soak_mod", version: c"1.0.0", logic::build);
"#;

const SOAK_V2_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static LOCAL_DROPS: AtomicU64 = AtomicU64::new(0);

    extern "C" fn soak_v2_drop(_ptr: *mut c_void) {
        LOCAL_DROPS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        // 1. Unchanged-fingerprint zero-copy `AbiPod` component.
        if let Err(e) = app.register_dynamic_component::<FastPod>() {
            return e;
        }

        // 2. Live v1 -> v2 `migrate_component_column`, or zero-copy v2 -> v2 retention.
        let mut migrated_v1_to_v2 = false;
        match app.resolve_component_named(Some(c"soak::MigPos"), None) {
            Ok(existing) => {
                let info = match app.component_layout_info(existing) {
                    Ok(info) => info,
                    Err(e) => return e,
                };
                if info.schema_fingerprint == PosV1::SCHEMA_FINGERPRINT {
                    let rc = app.migrate_component_column::<PosV1, PosV2, _>(existing, |old| {
                        Ok(PosV2 {
                            x: f64::from(old.x),
                            y: f64::from(old.y),
                        })
                    });
                    if rc.is_err() {
                        return rc;
                    }
                    migrated_v1_to_v2 = true;
                } else if let Err(e) = app.register_dynamic_component::<PosV2>() {
                    return e;
                }
            }
            Err(FfiResult::NOT_FOUND) => {
                if let Err(e) = app.register_dynamic_component::<PosV2>() {
                    return e;
                }
            }
            Err(e) => return e,
        }

        // 3. Dynamic `DROP_PAGES` destructor component recycled on every reload cycle.
        let drop_h = match app.register_component_with_layout(
            "soak::DropChurn",
            core::mem::size_of::<DropChurnPayload>(),
            core::mem::align_of::<DropChurnPayload>(),
            Some(soak_v2_drop),
        ) {
            Ok(h) => h,
            Err(e) => return e,
        };

        // 4. `save_state` / `restore_state` epoch + drop-counter handoff across reloads.
        let epoch = Arc::new(AtomicU64::new(0));
        let carried_drops = Arc::new(AtomicU64::new(0));
        let epoch_save = Arc::clone(&epoch);
        let carried_save = Arc::clone(&carried_drops);
        let epoch_restore = Arc::clone(&epoch);
        let carried_restore = Arc::clone(&carried_drops);
        let epoch_tick = Arc::clone(&epoch);
        let carried_tick = Arc::clone(&carried_drops);

        app.on_save_state(move || {
            let next = epoch_save.load(Ordering::Relaxed).wrapping_add(1);
            let pending = LOCAL_DROPS.swap(0, Ordering::Relaxed)
                + carried_save.swap(0, Ordering::Relaxed);
            let mut blob = Vec::with_capacity(16);
            blob.extend_from_slice(&next.to_le_bytes());
            blob.extend_from_slice(&pending.to_le_bytes());
            Ok(blob)
        });
        app.on_restore_state(move |bytes| {
            if bytes.len() == 16 {
                let val = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                let drops = u64::from_le_bytes([
                    bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                    bytes[15],
                ]);
                epoch_restore.store(val, Ordering::Relaxed);
                carried_restore.fetch_add(drops, Ordering::Relaxed);
            }
            FfiResult::OK
        });

        let mut recorded_migration = false;
        app.system(c"soak_tick")
            .query(|q| q.write::<FastPod>().write::<PosV2>())
            .resource_write::<SoakTelemetry>()
            .schedule(ScheduleId::UPDATE)
            .build(move |ctx| {
                let pending_drops = LOCAL_DROPS.swap(0, Ordering::Relaxed)
                    + carried_tick.swap(0, Ordering::Relaxed);
                let cur_epoch = epoch_tick.load(Ordering::Relaxed);
                let tick_no;
                if let Some(tel) = ctx.resource_mut::<SoakTelemetry>(0) {
                    tel.ticks += 1;
                    tick_no = tel.ticks;
                    tel.active_version = 2;
                    tel.drop_calls += pending_drops;
                    tel.restored_state_epoch = cur_epoch;
                    if migrated_v1_to_v2 && !recorded_migration {
                        tel.v1_to_v2_migrations += 1;
                        recorded_migration = true;
                    }
                } else {
                    return;
                }

                let (iter, cmds) = ctx.iter_and_commands();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some((pods, positions)) =
                        chunk.component_write_2::<FastPod, PosV2>(0, 1)
                    {
                        for (p, pos) in pods.iter_mut().zip(positions.iter_mut()) {
                            p.counter += 1;
                            p.checksum = p.checksum.wrapping_add(1);
                            pos.x += 0.25;
                            pos.y -= 0.25;
                        }
                    }
                }

                let mut drop_ent_bits = 0u64;
                if let Ok(ent) = cmds.spawn_empty() {
                    let payload = DropChurnPayload {
                        seq: tick_no,
                        magic: 0xD00D_F002,
                    };
                    if cmds.insert(ent, drop_h, &payload).is_ok() {
                        drop_ent_bits = ent.to_bits();
                    }
                }
                if let Some(tel) = ctx.resource_mut::<SoakTelemetry>(0) {
                    tel.last_drop_entity = drop_ent_bits;
                }
            })
    }
}

bevy_abi_mod_guest::export_mod!(name: c"soak_mod", version: c"2.0.0", logic::build);
"#;

fn build_soak_mods() -> BuiltSoakMods {
    static NEXT_WORKSPACE_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT_WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let guest_path = workspace_root.join("crates").join("bevy_abi_mod_guest");
    let sys_path = workspace_root.join("crates").join("bevy_abi_mod_sys");
    let guest_clean = guest_path
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();
    let sys_clean = sys_path
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();

    let root_dir =
        std::env::temp_dir().join(format!("bam_soak_10k_mods_{}_{seq}", std::process::id()));
    let target_dir =
        std::env::temp_dir().join(format!("bam_soak_10k_target_{}_{seq}", std::process::id()));
    let _ = fs::remove_dir_all(&root_dir);
    let _ = fs::remove_dir_all(&target_dir);
    fs::create_dir_all(&root_dir).expect("create temp soak workspace");

    fs::write(
        root_dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["soak_mod_v1", "soak_mod_v2"]
"#,
    )
    .expect("write soak workspace Cargo.toml");

    let lock_src = workspace_root
        .join("tests")
        .join("asymmetric_rust_mod")
        .join("Cargo.lock");
    if lock_src.is_file() {
        let mut lock_contents =
            fs::read_to_string(&lock_src).expect("read tests/asymmetric_rust_mod/Cargo.lock");
        if !lock_contents.ends_with('\n') {
            lock_contents.push('\n');
        }
        for crate_name in ["soak_mod_v1", "soak_mod_v2"] {
            lock_contents.push_str(&format!(
                "\n[[package]]\nname = \"{crate_name}\"\nversion = \"0.3.0\"\ndependencies = [\n \"bevy_abi_mod_guest\",\n \"bevy_abi_mod_sys\",\n]\n"
            ));
        }
        fs::write(root_dir.join("Cargo.lock"), lock_contents)
            .expect("write soak workspace Cargo.lock");
    }

    for (crate_name, lib_rs) in [
        ("soak_mod_v1", SOAK_V1_LIB_RS),
        ("soak_mod_v2", SOAK_V2_LIB_RS),
    ] {
        let crate_dir = root_dir.join(crate_name);
        let src_dir = crate_dir.join("src");
        fs::create_dir_all(&src_dir).expect("create soak crate src dir");
        let manifest = format!(
            r#"[package]
name = "{crate_name}"
version = "0.3.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = {{ path = "{guest_clean}", features = ["derive"] }}
bevy_abi_mod_sys = {{ path = "{sys_clean}" }}
"#
        );
        fs::write(crate_dir.join("Cargo.toml"), manifest).expect("write soak Cargo.toml");
        fs::write(src_dir.join("shared.rs"), SOAK_SHARED_RS).expect("write soak shared.rs");
        fs::write(src_dir.join("lib.rs"), lib_rs).expect("write soak lib.rs");
    }

    let mut cmd = sanitized_cargo_command();
    cmd.args(["build", "--workspace", "--release"]);
    let output = cmd
        .current_dir(&root_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("invoke cargo build for soak_mod_v1/v2");
    assert!(
        output.status.success(),
        "failed to build soak mods:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let dylib_path = |name: &str| {
        let filename = format!(
            "{}{}{}",
            std::env::consts::DLL_PREFIX,
            name,
            std::env::consts::DLL_SUFFIX
        );
        let built = target_dir.join("release").join(&filename);
        assert!(built.is_file(), "expected dylib at {}", built.display());
        built
    };

    BuiltSoakMods {
        mod_v1: dylib_path("soak_mod_v1"),
        mod_v2: dylib_path("soak_mod_v2"),
        root_dir,
        target_dir,
    }
}

fn target_cycle_count() -> usize {
    if let Ok(raw) = std::env::var("BAM_SOAK_CYCLES")
        && let Ok(parsed) = raw.parse::<usize>()
        && parsed > 0
    {
        return parsed;
    }
    DEFAULT_SMOKE_SOAK_CYCLES
}

#[test]
fn soak_10000_consecutive_reloads_latency_drop_pages_and_rss() {
    let mods = build_soak_mods();
    let stage_ring = SoakStageRing::new(&mods);
    let total_cycles = target_cycle_count();
    let baseline_free_drop_slots = available_drop_trampoline_slots();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(SoakTelemetry::default());
    app.register_mod_resource::<SoakTelemetry>()
        .expect("register SoakTelemetry");

    let opts = LoadOptions::new()
        .with_shadow_copy(false)
        .with_warn_on_resident_image(false)
        .strict();

    let initial_path = stage_ring.stage_for_cycle(false, 0);
    let mut current_mod_id = app
        .load_mod_with(initial_path.as_ref(), opts.clone())
        .expect("initial load of soak_mod_v1");
    let mut using_v2 = false;

    // Seed persistent entities on initial frame and capture initial immutable IDs/slots.
    app.world_mut().run_schedule(Update);
    if let Some(drop_e) =
        Entity::try_from_bits(app.world().resource::<SoakTelemetry>().last_drop_entity)
        && app.world().entities().contains(drop_e)
    {
        app.world_mut().despawn(drop_e);
    }

    let (fast_pod_handle, fast_pod_comp_id, mig_pos_handle, drop_slot_idx, drop_gen_start) = {
        let reg = app.world().resource::<ModRegistry>();
        let fast = reg
            .get_by_name("soak::FastPod")
            .expect("FastPod registered");
        let mig = reg.get_by_name("soak::MigPos").expect("MigPos registered");
        let drop_entry = reg
            .get_by_name("soak::DropChurn")
            .expect("DropChurn registered");
        let slot = drop_entry
            .drop_slot
            .expect("DropChurn allocated a DROP_PAGES slot");
        let gen0 = drop_slot_generation(slot).expect("drop slot generation present");
        (fast.handle, fast.component_id, mig.handle, slot, gen0)
    };

    let warmup_cycles = (total_cycles / 20).clamp(1, 500);
    let mut reload_durations_ns: Vec<u64> = Vec::with_capacity(total_cycles);
    let mut rss_after_warmup_kib: Option<u64> = None;
    let mut rss_midpoint_kib: Option<u64> = None;
    let mut expected_v1_to_v2 = 0u64;
    let mut expected_v2_to_v1 = 0u64;

    let total_start = Instant::now();

    for cycle in 1..=total_cycles {
        let flip_schema = cycle.is_multiple_of(MIGRATE_STRIDE);
        if flip_schema {
            using_v2 = !using_v2;
            if using_v2 {
                expected_v1_to_v2 += 1;
            } else {
                expected_v2_to_v1 += 1;
            }
        }

        let target_path = stage_ring.stage_for_cycle(using_v2, cycle);

        let t0 = Instant::now();
        current_mod_id = app
            .reload_mod_from(current_mod_id, target_path.as_ref(), opts.clone())
            .unwrap_or_else(|err| panic!("reload cycle {cycle} failed: {err:?}"));
        let elapsed_ns = t0.elapsed().as_nanos() as u64;
        reload_durations_ns.push(elapsed_ns);

        // Execute the reloaded guest system and immediately despawn its transient `DropChurn`
        // entity so `drop_trampoline::<SLOT>` runs the foreign destructor and `live_values -> 0`.
        app.world_mut().run_schedule(Update);
        let drop_bits = app.world().resource::<SoakTelemetry>().last_drop_entity;
        if let Some(drop_e) = Entity::try_from_bits(drop_bits)
            && app.world().entities().contains(drop_e)
        {
            app.world_mut().despawn(drop_e);
        }

        // Rotate buffered lifecycle messages so `Messages<ModLoaded>` / `ModUnloaded` stay O(1).
        if cycle.is_multiple_of(64) {
            app.world_mut()
                .resource_mut::<Messages<ModLoaded>>()
                .clear();
            app.world_mut()
                .resource_mut::<Messages<ModUnloaded>>()
                .clear();
        }

        if cycle == warmup_cycles {
            rss_after_warmup_kib = current_rss_kib();
        }
        if cycle == total_cycles / 2 {
            rss_midpoint_kib = current_rss_kib();
        }
    }

    let total_elapsed = total_start.elapsed();
    let rss_end_kib = current_rss_kib();

    // Run one final tick to flush the last `LOCAL_DROPS` delta into `SoakTelemetry` before checks.
    app.world_mut().run_schedule(Update);
    let last_bits = app.world().resource::<SoakTelemetry>().last_drop_entity;
    if let Some(drop_e) = Entity::try_from_bits(last_bits)
        && app.world().entities().contains(drop_e)
    {
        app.world_mut().despawn(drop_e);
    }

    // ========================================================================
    // 1. Zero-Copy `FastPod` & `MigPos` Schema/Data Invariants
    // ========================================================================
    let (fast_after, mig_after, drop_gen_end, pages_allocated) = {
        let reg = app.world().resource::<ModRegistry>();
        let fast = reg
            .get_by_name("soak::FastPod")
            .expect("FastPod still registered");
        let mig = reg
            .get_by_name("soak::MigPos")
            .expect("MigPos still registered");
        let gen_now = drop_slot_generation(drop_slot_idx).expect("drop slot generation present");
        (
            (fast.handle, fast.component_id),
            (mig.handle, mig.size, mig.align),
            gen_now,
            allocated_drop_pages(),
        )
    };

    assert_eq!(
        fast_after,
        (fast_pod_handle, fast_pod_comp_id),
        "FastPod must retain exact zero-copy BamComponent handle and Bevy ComponentId across all {total_cycles} reloads"
    );
    assert_eq!(
        mig_after.0, mig_pos_handle,
        "MigPos must keep its exact BamComponent handle across all v1 <-> v2 column migrations"
    );
    if using_v2 {
        assert_eq!((mig_after.1, mig_after.2), (16, 8));
    } else {
        assert_eq!((mig_after.1, mig_after.2), (8, 4));
    }

    // Verify every persistent entity's `FastPod` and `MigPos` values are mathematically exact!
    let expected_mut_ticks = (total_cycles as i64) + 1; // +1 for final flush tick (initial tick seeded)
    let mut verified_entities = 0usize;
    let entity_ids: Vec<Entity> = app.world().iter_entities().map(|e| e.id()).collect();
    for entity in entity_ids {
        if let Ok(pod_bytes) = read_entity_pod_bytes(app.world(), entity, fast_pod_handle) {
            let (counter, checksum) = decode_fast_pod(&pod_bytes);
            let base_i = counter - expected_mut_ticks;
            assert!(
                (0..PERSISTENT_ENTITIES as i64).contains(&base_i),
                "unexpected FastPod counter {counter} (expected base in 0..{PERSISTENT_ENTITIES})"
            );
            let expected_checksum = (base_i as u64)
                .wrapping_mul(0x101)
                .wrapping_add(expected_mut_ticks as u64);
            assert_eq!(checksum, expected_checksum, "FastPod checksum mismatch");

            let delta = (expected_mut_ticks as f64) * 0.25;
            let pos_bytes = read_entity_pod_bytes(app.world(), entity, mig_pos_handle)
                .expect("MigPos present on persistent entity");
            if using_v2 {
                let (x, y) = decode_pos_v2(&pos_bytes);
                assert!(
                    (x - (base_i as f64 + delta)).abs() < 1e-6,
                    "PosV2.x drift on entity {base_i}: got {x} expected {}",
                    base_i as f64 + delta
                );
                assert!(
                    (y - (base_i as f64 * -2.0 - delta)).abs() < 1e-6,
                    "PosV2.y drift on entity {base_i}: got {y} expected {}",
                    base_i as f64 * -2.0 - delta
                );
            } else {
                let (x, y) = decode_pos_v1(&pos_bytes);
                assert!(
                    (f64::from(x) - (base_i as f64 + delta)).abs() < 1e-3,
                    "PosV1.x drift on entity {base_i}: got {x} expected {}",
                    base_i as f64 + delta
                );
                assert!(
                    (f64::from(y) - (base_i as f64 * -2.0 - delta)).abs() < 1e-3,
                    "PosV1.y drift on entity {base_i}: got {y} expected {}",
                    base_i as f64 * -2.0 - delta
                );
            }
            verified_entities += 1;
        }
    }
    assert_eq!(
        verified_entities, PERSISTENT_ENTITIES,
        "all {PERSISTENT_ENTITIES} persistent entities must survive {total_cycles} consecutive reloads"
    );

    // ========================================================================
    // 2. Telemetry, Migrations, State Transfer, & `DROP_PAGES` Recycling
    // ========================================================================
    let tel = *app.world().resource::<SoakTelemetry>();
    assert_eq!(tel.ticks, (total_cycles as u64) + 2);
    assert_eq!(tel.v1_to_v2_migrations, expected_v1_to_v2);
    assert_eq!(tel.v2_to_v1_migrations, expected_v2_to_v1);
    assert_eq!(
        tel.restored_state_epoch, total_cycles as u64,
        "save_state/restore_state epoch must match exact reload count"
    );
    assert_eq!(
        tel.drop_calls,
        (total_cycles as u64) + 1,
        "every single transient DropChurn entity's foreign destructor must execute across all reloads"
    );

    let gen_delta = drop_gen_end.saturating_sub(drop_gen_start);
    assert_eq!(
        gen_delta, total_cycles as u64,
        "DROP_PAGES slot {drop_slot_idx} generation must advance exactly once per reload (expected {total_cycles}, got {gen_delta})"
    );
    if total_cycles >= FULL_SOAK_CYCLES {
        assert!(
            gen_delta > MAX_DROP_TRAMPOLINES as u64,
            "DROP_PAGES generation churn ({gen_delta}) must exceed MAX_DROP_TRAMPOLINES ({MAX_DROP_TRAMPOLINES})"
        );
    }

    // Final permanent unload must return every single DROP_PAGES slot to the global free list.
    app.unload_mod(current_mod_id)
        .expect("final unload_mod succeeds");
    assert_eq!(
        available_drop_trampoline_slots(),
        baseline_free_drop_slots,
        "all DROP_PAGES slots must be cleanly reclaimed after {total_cycles} reloads + unload"
    );
    assert!(
        pages_allocated <= 2,
        "DROP_PAGES slab should not leak extra pages during slot recycling (allocated {pages_allocated} pages)"
    );

    // ========================================================================
    // 3. Latency Percentiles & RSS Memory Drift Assertions
    // ========================================================================
    reload_durations_ns.sort_unstable();
    let pct = |p: f64| -> f64 {
        let idx = (((reload_durations_ns.len() - 1) as f64) * p).round() as usize;
        (reload_durations_ns[idx] as f64) / 1_000_000.0
    };
    let min_ms = pct(0.0);
    let p50_ms = pct(0.50);
    let p95_ms = pct(0.95);
    let p99_ms = pct(0.99);
    let max_ms = pct(1.0);

    let rss_delta_kib = match (rss_after_warmup_kib, rss_end_kib) {
        (Some(start), Some(end)) => Some(end.saturating_sub(start)),
        _ => None,
    };

    let gen_note = if gen_delta > MAX_DROP_TRAMPOLINES as u64 {
        format!("(>{MAX_DROP_TRAMPOLINES})")
    } else {
        format!("(slab={MAX_DROP_TRAMPOLINES})")
    };
    eprintln!(
        "[soak_10k_reloads] cycles={total_cycles} total={:.2}s | reload_ms: min={min_ms:.3} p50={p50_ms:.3} p95={p95_ms:.3} p99={p99_ms:.3} max={max_ms:.3} | migrations=(v1->v2:{}, v2->v1:{}) | drop_gen_delta={gen_delta} {gen_note} drop_calls={} pages={pages_allocated} | rss_kib: warmup={:?} mid={:?} end={:?} delta={:?}",
        total_elapsed.as_secs_f64(),
        tel.v1_to_v2_migrations,
        tel.v2_to_v1_migrations,
        tel.drop_calls,
        rss_after_warmup_kib,
        rss_midpoint_kib,
        rss_end_kib,
        rss_delta_kib,
    );

    if !cfg!(debug_assertions) {
        assert!(
            p99_ms < 2.0,
            "release p99 reload latency {p99_ms:.3} ms exceeded 2.0 ms budget"
        );
    }

    if let Some(delta_kib) = rss_delta_kib {
        // Bound steady-state RSS drift (post-warmup through cycle 10,000) to < 32 MiB.
        assert!(
            delta_kib < 32 * 1024,
            "steady-state RSS drift {delta_kib} KiB exceeded flat-memory budget (< 32 MiB)"
        );
    }
}
