//! Live Schema Evolution & Safe `#![forbid(unsafe_code)]` Non-POD `GuestApp::register_dynamic_component`
//! Integration Tests (`ABI 0.2.0`):
//!
//! 1. Compiles and loads a genuine `#![forbid(unsafe_code)]` guest `.cdylib` registering a non-POD
//!    `ModComponent` (`String`, `Vec<u32>`, `bool`, `FfiOption<u32>`), spawning mod-scoped and
//!    persistent entities, inserting onto host entities, mutating via `Chunk::dynamic_component_mut`,
//!    and verifying 100% destructor execution with zero leaks (`live_values == 0`).
//! 2. Compiles two independent guest `.cdylib` versions (`UnitStateV1` and `UnitStateV2`) and tests
//!    live `reload_mod_from`:
//!    `UnitStateV1 { hp: i32, tag: String, scores: Vec<u32>, legacy: u64 }` ->
//!    `UnitStateV2 { shield: f32, scores: Vec<u32>, hp: i64, tag: String }`,
//!    preserving active + `Disabled` + persistent entities, migrating `ModScoped`, widening
//!    `hp: i32 -> i64`, reallocating `String` and `Vec<u32>` on the new guest's allocator without
//!    crossover, populating `shield` from `Default`, and dropping `legacy` and old heap buffers on
//!    the old guest's allocator prior to `dlclose`.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host::world_ext::ModScoped;
use bevy_abi_mod_host_sys::drop_trampoline::drop_slot_live_values;
use bevy_abi_mod_sys::StrView;
use bevy_app::{App, FixedUpdate, PostUpdate};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

struct TempCdylibCrate {
    dir: PathBuf,
}

impl TempCdylibCrate {
    fn build(tag: &str, crate_name: &str, lib_rs: &str) -> (Self, PathBuf) {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bam_schema_evo_crate_{}_{}_{}",
            tag,
            std::process::id(),
            id
        ));
        let src_dir = dir.join("src");
        fs::create_dir_all(&src_dir).expect("create temp guest crate src dir");

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let guest_path = workspace_root.join("crates").join("bevy_abi_mod_guest");
        let sys_path = workspace_root.join("crates").join("bevy_abi_mod_sys");
        let lock_src = workspace_root.join("Cargo.lock");
        if lock_src.is_file() {
            let _ = fs::copy(&lock_src, dir.join("Cargo.lock"));
        }

        let cargo_toml = format!(
            r#"[workspace]

[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = {{ path = "{guest}", features = ["derive"] }}
bevy_abi_mod_sys = {{ path = "{sys}" }}
"#,
            guest = guest_path.display(),
            sys = sys_path.display(),
        );
        fs::write(dir.join("Cargo.toml"), cargo_toml).expect("write temp Cargo.toml");
        fs::write(src_dir.join("lib.rs"), lib_rs).expect("write temp src/lib.rs");

        let shared_target_dir =
            std::env::temp_dir().join(format!("bam_schema_evo_target_{}", std::process::id()));
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--offline")
            .current_dir(&dir)
            .env("CARGO_TARGET_DIR", &shared_target_dir)
            .output()
            .expect("invoke cargo build for temp cdylib fixture");
        assert!(
            output.status.success(),
            "failed to build temp cdylib '{crate_name}':\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let filename = format!(
            "{}{}{}",
            std::env::consts::DLL_PREFIX,
            crate_name,
            std::env::consts::DLL_SUFFIX
        );
        let built_path = shared_target_dir.join("debug").join(&filename);
        assert!(
            built_path.is_file(),
            "expected compiled cdylib at {}",
            built_path.display()
        );
        let dylib_path = dir.join(filename);
        fs::copy(&built_path, &dylib_path)
            .expect("copy compiled cdylib into unique temp crate dir");

        (Self { dir }, dylib_path)
    }
}

impl Drop for TempCdylibCrate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn register_dynamic_component_non_pod_lifecycle_and_zero_leak_drops() {
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
    static TICK_SNAPSHOTS: Mutex<Vec<(u64, bool, i32, String, Vec<u32>, u32)>> =
        Mutex::new(Vec::new());

    unsafe extern "C" fn record_drop() {
        DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn record_tick(
        entity_bits: u64,
        active: u8,
        hp: i32,
        tag: StrView,
        scores_ptr: *const u32,
        scores_len: usize,
        opt_some: u8,
        opt_val: u32,
    ) {
        // SAFETY: Guest passes valid UTF-8 `StrView` and `scores_ptr[0..scores_len]` for this call.
        let tag_str = unsafe { tag.try_as_str() }.unwrap().to_owned();
        let scores = if scores_len == 0 {
            Vec::new()
        } else {
            // SAFETY: Non-empty slice provided by live guest `Vec<u32>`.
            unsafe { core::slice::from_raw_parts(scores_ptr, scores_len) }.to_vec()
        };
        TICK_SNAPSHOTS.lock().unwrap().push((
            entity_bits,
            active != 0,
            hp,
            tag_str,
            scores,
            if opt_some != 0 { opt_val } else { u32::MAX },
        ));
    }

    DROP_COUNT.store(0, Ordering::SeqCst);
    TICK_SNAPSHOTS.lock().unwrap().clear();

    let lib_rs = r#"
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

static DROP_CB: AtomicUsize = AtomicUsize::new(0);
static TICK_CB: AtomicUsize = AtomicUsize::new(0);

fn set_callbacks(drop_ptr: *const c_void, tick_ptr: *const c_void) {
    DROP_CB.store(drop_ptr as usize, Ordering::SeqCst);
    TICK_CB.store(tick_ptr as usize, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DropProbe;

impl Drop for DropProbe {
    fn drop(&mut self) {
        let addr = DROP_CB.load(Ordering::SeqCst);
        if addr != 0 {
            // SAFETY: Host registered static `extern "C" fn()` under `bam.v0.dyn_test.record_drop`.
            let cb: unsafe extern "C" fn() = unsafe { core::mem::transmute(addr) };
            unsafe { cb() };
        }
    }
}

fn emit_tick(
    entity_bits: u64,
    active: bool,
    hp: i32,
    tag: &str,
    scores: &[u32],
    opt: bevy_abi_mod_guest::FfiOption<u32>,
) {
    let addr = TICK_CB.load(Ordering::SeqCst);
    if addr != 0 {
        let cb: unsafe extern "C" fn(
            u64,
            u8,
            i32,
            bevy_abi_mod_guest::StrView,
            *const u32,
            usize,
            u8,
            u32,
        ) = unsafe { core::mem::transmute(addr) };
        let (opt_some, opt_val) = match opt.into_option() {
            Some(v) => (1u8, v),
            None => (0u8, 0u32),
        };
        unsafe {
            cb(
                entity_bits,
                u8::from(active),
                hp,
                bevy_abi_mod_guest::StrView::from_str(tag),
                scores.as_ptr(),
                scores.len(),
                opt_some,
                opt_val,
            );
        }
    }
}

mod logic {
    #![forbid(unsafe_code)]
    use bevy_abi_mod_guest::prelude::*;

    #[derive(Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(name = "dyn_test::Profile")]
    pub struct Profile {
        pub active: bool,
        pub hp: i32,
        pub tag: String,
        pub scores: Vec<u32>,
        pub opt: FfiOption<u32>,
        #[mod_field(skip)]
        pub _drop_probe: crate::DropProbe,
    }

    impl Default for Profile {
        fn default() -> Self {
            Self {
                active: false,
                hp: 0,
                tag: String::new(),
                scores: Vec::new(),
                opt: FfiOption::none(),
                _drop_probe: crate::DropProbe,
            }
        }
    }

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let drop_ptr = match app.resolve_custom_symbol(c"bam.v0.dyn_test.record_drop") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let tick_ptr = match app.resolve_custom_symbol(c"bam.v0.dyn_test.record_tick") {
            Ok(p) => p,
            Err(e) => return e,
        };
        crate::set_callbacks(drop_ptr, tick_ptr);

        if let Err(err) = app.register_dynamic_component::<Profile>() {
            return err;
        }

        let mut spawned_once = false;
        let rc = app
            .system(c"dyn_profile_tick")
            .query(|q| q.write_dynamic::<Profile>())
            .schedule(ScheduleId::UPDATE)
            .build(move |ctx| {
                let (iter, cmds) = ctx.iter_and_commands();
                if !spawned_once {
                    spawned_once = true;
                    let _ = cmds.spawn_dynamic_component(Profile {
                        active: true,
                        hp: 100,
                        tag: String::from("scoped_a"),
                        scores: vec![1, 2, 3],
                        opt: FfiOption::some(42),
                        _drop_probe: crate::DropProbe,
                    });
                    let _ = cmds.spawn_dynamic_component(Profile {
                        active: false,
                        hp: 200,
                        tag: String::from("scoped_b"),
                        scores: vec![4, 5],
                        opt: FfiOption::none(),
                        _drop_probe: crate::DropProbe,
                    });
                    let _ = cmds.spawn_dynamic_component_persistent(Profile {
                        active: true,
                        hp: 300,
                        tag: String::from("persistent"),
                        scores: vec![7, 8, 9],
                        opt: FfiOption::some(99),
                        _drop_probe: crate::DropProbe,
                    });
                }
                while let Some(mut chunk) = iter.next_chunk_view() {
                    let entities = chunk.entities().to_vec();
                    let profiles = chunk
                        .dynamic_component_mut::<Profile>(0)
                        .expect("same-mod Profile slice");
                    for (ent, prof) in entities.into_iter().zip(profiles.iter_mut()) {
                        prof.hp += 10;
                        prof.active = !prof.active;
                        prof.tag.push_str("_tick");
                        prof.scores.push(99);
                        if prof.opt.is_none() {
                            prof.opt = FfiOption::some(777);
                        }
                        crate::emit_tick(
                            ent.to_bits(),
                            prof.active,
                            prof.hp,
                            &prof.tag,
                            &prof.scores,
                            prof.opt,
                        );
                    }
                }
            });
        if rc.is_err() {
            return rc;
        }
        FfiResult::OK
    }
}

bevy_abi_mod_guest::export_mod!(name: c"dyn_profile_mod", version: c"0.1.0", logic::build);
"#;

    let (_crate_guard, dylib_path) =
        TempCdylibCrate::build("dyn_profile", "dyn_profile_mod", lib_rs);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);

    // SAFETY: Static `extern "C"` test functions remain valid for the process lifetime.
    unsafe {
        let mut reg = app.world_mut().resource_mut::<ModRegistry>();
        reg.register_custom_symbol("bam.v0.dyn_test.record_drop", record_drop as *const c_void);
        reg.register_custom_symbol("bam.v0.dyn_test.record_tick", record_tick as *const c_void);
    }

    let mod_id = app
        .load_mod_with(
            &dylib_path,
            LoadOptions::new().with_shadow_copy(true).strict(),
        )
        .expect("load #![forbid(unsafe_code)] dyn_profile_mod");

    // Tick 1: spawns 3 entities via commands; Tick 2: mutates all 3!
    app.update();
    assert!(TICK_SNAPSHOTS.lock().unwrap().is_empty());
    app.update();

    let snaps = TICK_SNAPSHOTS.lock().unwrap().clone();
    assert_eq!(snaps.len(), 3);
    assert!(snaps.iter().any(|(_, active, hp, tag, scores, opt)| {
        !*active && *hp == 110 && tag == "scoped_a_tick" && scores == &[1, 2, 3, 99] && *opt == 42
    }));
    assert!(snaps.iter().any(|(_, active, hp, tag, scores, opt)| {
        *active && *hp == 210 && tag == "scoped_b_tick" && scores == &[4, 5, 99] && *opt == 777
    }));
    assert!(snaps.iter().any(|(_, active, hp, tag, scores, opt)| {
        !*active && *hp == 310 && tag == "persistent_tick" && scores == &[7, 8, 9, 99] && *opt == 99
    }));

    let profile_entry = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("dyn_test::Profile")
        .expect("Profile registered in ModRegistry")
        .clone();
    let drop_slot = profile_entry
        .drop_slot
        .expect("non-POD Profile must allocate drop_slot");
    assert_eq!(drop_slot_live_values(drop_slot), Some(3));
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

    // Find the persistent entity vs scoped entities.
    let persistent_ent = snaps
        .iter()
        .find(|(_, _, _, tag, _, _)| tag == "persistent_tick")
        .map(|(bits, ..)| Entity::from_bits(*bits))
        .unwrap();
    let scoped_ents: Vec<Entity> = snaps
        .iter()
        .filter(|(_, _, _, tag, _, _)| tag != "persistent_tick")
        .map(|(bits, ..)| Entity::from_bits(*bits))
        .collect();
    assert_eq!(scoped_ents.len(), 2);

    // Despawn 1 scoped entity explicitly while mod is active -> 1 drop immediately!
    app.world_mut().despawn(scoped_ents[0]);
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
    assert_eq!(drop_slot_live_values(drop_slot), Some(2));

    // Despawn the persistent entity while mod is active -> 2 drops!
    app.world_mut().despawn(persistent_ent);
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 2);
    assert_eq!(drop_slot_live_values(drop_slot), Some(1));

    // Unload mod -> bulk-despawns remaining ModScoped entity -> 3/3 = 100% drops, live_values == 0!
    app.unload_mod(mod_id).expect("unload dyn_profile_mod");
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 3);
    assert!(!app.world().entities().contains(scoped_ents[1]));
    assert!(
        drop_slot_live_values(drop_slot) == Some(0) || drop_slot_live_values(drop_slot).is_none(),
        "recycled or drained drop slot must have zero live values"
    );
}

#[test]
fn live_reload_mod_schema_evolution_v1_to_v2() {
    static V1_DROPS: AtomicUsize = AtomicUsize::new(0);
    static V2_DROPS: AtomicUsize = AtomicUsize::new(0);
    static V1_SPAWNED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
    static V2_OBSERVED: Mutex<Vec<(u64, f32, i64, String, Vec<u32>)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record_v1_drop() {
        V1_DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn record_v2_drop() {
        V2_DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn record_v1_spawn(label: StrView, entity_bits: u64) {
        // SAFETY: Valid UTF-8 `StrView` from guest.
        let s = unsafe { label.try_as_str() }.unwrap().to_owned();
        V1_SPAWNED.lock().unwrap().push((s, entity_bits));
    }
    unsafe extern "C" fn record_v2_entity(
        entity_bits: u64,
        shield: f32,
        hp: i64,
        tag: StrView,
        scores_ptr: *const u32,
        scores_len: usize,
    ) {
        // SAFETY: Valid UTF-8 `StrView` and slice from guest `UnitStateV2`.
        let tag_str = unsafe { tag.try_as_str() }.unwrap().to_owned();
        let scores = if scores_len == 0 {
            Vec::new()
        } else {
            // SAFETY: Non-empty slice from live `Vec<u32>`.
            unsafe { core::slice::from_raw_parts(scores_ptr, scores_len) }.to_vec()
        };
        V2_OBSERVED
            .lock()
            .unwrap()
            .push((entity_bits, shield, hp, tag_str, scores));
    }

    V1_DROPS.store(0, Ordering::SeqCst);
    V2_DROPS.store(0, Ordering::SeqCst);
    V1_SPAWNED.lock().unwrap().clear();
    V2_OBSERVED.lock().unwrap().clear();

    let v1_src = r#"
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

static DROP_CB: AtomicUsize = AtomicUsize::new(0);
static SPAWN_CB: AtomicUsize = AtomicUsize::new(0);

fn set_callbacks(drop_ptr: *const c_void, spawn_ptr: *const c_void) {
    DROP_CB.store(drop_ptr as usize, Ordering::SeqCst);
    SPAWN_CB.store(spawn_ptr as usize, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DropProbe;

impl Drop for DropProbe {
    fn drop(&mut self) {
        let addr = DROP_CB.load(Ordering::SeqCst);
        if addr != 0 {
            let cb: unsafe extern "C" fn() = unsafe { core::mem::transmute(addr) };
            unsafe { cb() };
        }
    }
}

fn notify_spawn(label: &str, entity_bits: u64) {
    let addr = SPAWN_CB.load(Ordering::SeqCst);
    if addr != 0 {
        let cb: unsafe extern "C" fn(bevy_abi_mod_guest::StrView, u64) =
            unsafe { core::mem::transmute(addr) };
        unsafe { cb(bevy_abi_mod_guest::StrView::from_str(label), entity_bits) };
    }
}

mod logic {
    #![forbid(unsafe_code)]
    use bevy_abi_mod_guest::prelude::*;

    #[derive(Debug, Clone, PartialEq, Default, ModComponent)]
    #[mod_component(name = "evo::UnitState")]
    pub struct UnitState {
        pub hp: i32,
        pub tag: String,
        pub scores: Vec<u32>,
        pub legacy: u64,
        #[mod_field(skip)]
        pub _drop_probe: crate::DropProbe,
    }

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let drop_ptr = match app.resolve_custom_symbol(c"bam.v0.evo.v1_drop") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let spawn_ptr = match app.resolve_custom_symbol(c"bam.v0.evo.v1_spawn") {
            Ok(p) => p,
            Err(e) => return e,
        };
        crate::set_callbacks(drop_ptr, spawn_ptr);

        if let Err(err) = app.register_dynamic_component::<UnitState>() {
            return err;
        }

        let mut spawned = false;
        app.system(c"evo_v1_setup")
            .schedule(ScheduleId::UPDATE)
            .build(move |ctx| {
                if spawned {
                    return;
                }
                spawned = true;
                let cmds = ctx.commands();
                let e1 = cmds
                    .spawn_dynamic_component(UnitState {
                        hp: -42,
                        tag: String::from("alpha_v1"),
                        scores: vec![1, 2, 3],
                        legacy: 0xDEAD_BEEF,
                        _drop_probe: crate::DropProbe,
                    })
                    .unwrap();
                crate::notify_spawn("active_1", e1.to_bits());

                let e2 = cmds
                    .spawn_dynamic_component(UnitState {
                        hp: 250,
                        tag: String::from("beta_v1"),
                        scores: vec![10, 20],
                        legacy: 999,
                        _drop_probe: crate::DropProbe,
                    })
                    .unwrap();
                crate::notify_spawn("active_2", e2.to_bits());

                let e3 = cmds
                    .spawn_dynamic_component(UnitState {
                        hp: 77,
                        tag: String::from("disabled_v1"),
                        scores: vec![7],
                        legacy: 42,
                        _drop_probe: crate::DropProbe,
                    })
                    .unwrap();
                crate::notify_spawn("disabled_3", e3.to_bits());

                let e4 = cmds
                    .spawn_dynamic_component_persistent(UnitState {
                        hp: 500,
                        tag: String::from("persist_v1"),
                        scores: vec![5, 6, 7, 8],
                        legacy: 1234,
                        _drop_probe: crate::DropProbe,
                    })
                    .unwrap();
                crate::notify_spawn("persist_4", e4.to_bits());
            })
    }
}

bevy_abi_mod_guest::export_mod!(name: c"unit_evo_mod", version: c"0.1.0", logic::build);
"#;

    let v2_src = r#"
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

static DROP_CB: AtomicUsize = AtomicUsize::new(0);
static OBS_CB: AtomicUsize = AtomicUsize::new(0);

fn set_callbacks(drop_ptr: *const c_void, obs_ptr: *const c_void) {
    DROP_CB.store(drop_ptr as usize, Ordering::SeqCst);
    OBS_CB.store(obs_ptr as usize, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DropProbe;

impl Drop for DropProbe {
    fn drop(&mut self) {
        let addr = DROP_CB.load(Ordering::SeqCst);
        if addr != 0 {
            let cb: unsafe extern "C" fn() = unsafe { core::mem::transmute(addr) };
            unsafe { cb() };
        }
    }
}

fn emit_v2(entity_bits: u64, shield: f32, hp: i64, tag: &str, scores: &[u32]) {
    let addr = OBS_CB.load(Ordering::SeqCst);
    if addr != 0 {
        let cb: unsafe extern "C" fn(u64, f32, i64, bevy_abi_mod_guest::StrView, *const u32, usize) =
            unsafe { core::mem::transmute(addr) };
        unsafe {
            cb(
                entity_bits,
                shield,
                hp,
                bevy_abi_mod_guest::StrView::from_str(tag),
                scores.as_ptr(),
                scores.len(),
            );
        }
    }
}

mod logic {
    #![forbid(unsafe_code)]
    use bevy_abi_mod_guest::prelude::*;

    #[derive(Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(name = "evo::UnitState")]
    pub struct UnitState {
        pub shield: f32,
        pub scores: Vec<u32>,
        pub hp: i64,
        pub tag: String,
        #[mod_field(skip)]
        pub _drop_probe: crate::DropProbe,
    }

    impl Default for UnitState {
        fn default() -> Self {
            Self {
                shield: 25.5,
                scores: Vec::new(),
                hp: 0,
                tag: String::new(),
                _drop_probe: crate::DropProbe,
            }
        }
    }

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let drop_ptr = match app.resolve_custom_symbol(c"bam.v0.evo.v2_drop") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let obs_ptr = match app.resolve_custom_symbol(c"bam.v0.evo.v2_obs") {
            Ok(p) => p,
            Err(e) => return e,
        };
        crate::set_callbacks(drop_ptr, obs_ptr);

        if let Err(err) = app.register_dynamic_component::<UnitState>() {
            return err;
        }

        app.system(c"evo_v2_observe")
            .query(|q| q.write_dynamic::<UnitState>())
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    let entities = chunk.entities().to_vec();
                    let units = chunk
                        .dynamic_component_mut::<UnitState>(0)
                        .expect("same-mod UnitStateV2 slice");
                    for (ent, unit) in entities.into_iter().zip(units.iter_mut()) {
                        crate::emit_v2(
                            ent.to_bits(),
                            unit.shield,
                            unit.hp,
                            &unit.tag,
                            &unit.scores,
                        );
                    }
                }
            })
    }
}

bevy_abi_mod_guest::export_mod!(name: c"unit_evo_mod", version: c"0.2.0", logic::build);
"#;

    let (_v1_guard, v1_dylib) = TempCdylibCrate::build("unit_evo_v1", "unit_evo_mod", v1_src);
    let (_v2_guard, v2_dylib) = TempCdylibCrate::build("unit_evo_v2", "unit_evo_mod", v2_src);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);

    // SAFETY: Static `extern "C"` test functions remain valid for the process lifetime.
    unsafe {
        let mut reg = app.world_mut().resource_mut::<ModRegistry>();
        reg.register_custom_symbol("bam.v0.evo.v1_drop", record_v1_drop as *const c_void);
        reg.register_custom_symbol("bam.v0.evo.v2_drop", record_v2_drop as *const c_void);
        reg.register_custom_symbol("bam.v0.evo.v1_spawn", record_v1_spawn as *const c_void);
        reg.register_custom_symbol("bam.v0.evo.v2_obs", record_v2_entity as *const c_void);
    }

    let opts = LoadOptions::new().with_shadow_copy(true).strict();
    let id_v1 = app
        .load_mod_with(&v1_dylib, opts.clone())
        .expect("load unit_evo_mod v1");
    app.update();

    let spawned = V1_SPAWNED.lock().unwrap().clone();
    assert_eq!(spawned.len(), 4);
    let find_ent = |label: &str| -> Entity {
        spawned
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, bits)| Entity::from_bits(*bits))
            .unwrap()
    };
    let e_active_1 = find_ent("active_1");
    let e_active_2 = find_ent("active_2");
    let e_disabled_3 = find_ent("disabled_3");
    let e_persist_4 = find_ent("persist_4");

    // Disable `e_disabled_3` before reload to verify `Disabled` entity migration too!
    app.world_mut().entity_mut(e_disabled_3).insert(Disabled);
    assert_eq!(V1_DROPS.load(Ordering::SeqCst), 0);

    // Reload from V1 -> V2 with live archetype schema evolution!
    let id_v2 = app
        .reload_mod_from(id_v1, &v2_dylib, opts)
        .expect("reload_mod_from v1 -> v2 schema evolution succeeds");
    assert_ne!(id_v1, id_v2);

    // Every V1 instance (all 4 entities, including Disabled & persistent) MUST have dropped on V1's
    // allocator BEFORE V1 unmapped!
    assert_eq!(V1_DROPS.load(Ordering::SeqCst), 4);
    assert_eq!(V2_DROPS.load(Ordering::SeqCst), 0);

    // All 4 entities remain alive and `ModScoped` was migrated from `id_v1` to `id_v2`!
    for ent in [e_active_1, e_active_2, e_disabled_3, e_persist_4] {
        assert!(app.world().entities().contains(ent));
    }
    assert_eq!(
        app.world().get::<ModScoped>(e_active_1),
        Some(&ModScoped(id_v2))
    );
    assert_eq!(
        app.world().get::<ModScoped>(e_active_2),
        Some(&ModScoped(id_v2))
    );
    assert_eq!(
        app.world().get::<ModScoped>(e_disabled_3),
        Some(&ModScoped(id_v2))
    );
    assert!(app.world().get::<ModScoped>(e_persist_4).is_none());

    // Run V2's Update system and verify evolved UnitStateV2 fields on active + persistent entities!
    app.update();
    let v2_obs = V2_OBSERVED.lock().unwrap().clone();
    assert_eq!(
        v2_obs.len(),
        3,
        "only the 3 non-Disabled entities run in normal Update"
    );
    assert!(v2_obs.iter().any(|(bits, shield, hp, tag, scores)| {
        *bits == e_active_1.to_bits()
            && (*shield - 25.5).abs() < f32::EPSILON
            && *hp == -42i64
            && tag == "alpha_v1"
            && scores == &[1, 2, 3]
    }));
    assert!(v2_obs.iter().any(|(bits, shield, hp, tag, scores)| {
        *bits == e_active_2.to_bits()
            && (*shield - 25.5).abs() < f32::EPSILON
            && *hp == 250i64
            && tag == "beta_v1"
            && scores == &[10, 20]
    }));
    assert!(v2_obs.iter().any(|(bits, shield, hp, tag, scores)| {
        *bits == e_persist_4.to_bits()
            && (*shield - 25.5).abs() < f32::EPSILON
            && *hp == 500i64
            && tag == "persist_v1"
            && scores == &[5, 6, 7, 8]
    }));

    // Also verify `e_disabled_3` migrated its fields cleanly via V2's witness `export_fields`!
    let v2_entry = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("evo::UnitState")
        .expect("evo::UnitState registered by V2")
        .clone();
    assert!(
        app.world()
            .entity(e_disabled_3)
            .contains_id(v2_entry.component_id)
    );
    let v2_export_ptr = app
        .world()
        .resource::<ModRegistry>()
        .resolve_symbol("bam.v0.evo::UnitState.vtable")
        .expect("V2 vtable symbol");
    let _ = v2_export_ptr;
    // Enable `e_disabled_3` and run one more tick so V2's own typed `dynamic_component_mut` reads it!
    V2_OBSERVED.lock().unwrap().clear();
    app.world_mut()
        .entity_mut(e_disabled_3)
        .remove::<Disabled>();
    app.update();
    let v2_obs2 = V2_OBSERVED.lock().unwrap().clone();
    assert_eq!(v2_obs2.len(), 4);
    assert!(v2_obs2.iter().any(|(bits, shield, hp, tag, scores)| {
        *bits == e_disabled_3.to_bits()
            && (*shield - 25.5).abs() < f32::EPSILON
            && *hp == 77i64
            && tag == "disabled_v1"
            && scores == &[7]
    }));

    // Re-disable `e_disabled_3` to confirm `unload_mod` still cleans up Disabled + ModScoped!
    app.world_mut().entity_mut(e_disabled_3).insert(Disabled);

    // Despawn persistent entity before unload -> 1 V2 drop, then unload_mod -> 3 more V2 drops!
    app.world_mut().despawn(e_persist_4);
    assert_eq!(V2_DROPS.load(Ordering::SeqCst), 1);

    let drop_slot_v2 = v2_entry.drop_slot.expect("V2 drop slot");
    app.unload_mod(id_v2).expect("unload V2 mod");
    assert_eq!(V2_DROPS.load(Ordering::SeqCst), 4);
    assert!(!app.world().entities().contains(e_active_1));
    assert!(!app.world().entities().contains(e_active_2));
    assert!(!app.world().entities().contains(e_disabled_3));
    assert!(
        drop_slot_live_values(drop_slot_v2) == Some(0)
            || drop_slot_live_values(drop_slot_v2).is_none()
    );
}
