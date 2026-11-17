#![allow(
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use bevy_abi_mod_guest::prelude::*;
use bevy_abi_mod_guest::raw::{Host, read_descriptor};
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

// ============================================================================
// 1. Component VTable Mirrors Matching `#[derive(ModComponent)]` (`ABI 0.2.0`)
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PositionVTable {
    pub header: VTableHeader,
    pub get_x: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_x: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
    pub get_y: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_y: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
    pub get_z: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_z: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VelocityVTable {
    pub header: VTableHeader,
    pub get_x: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_x: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
    pub get_y: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_y: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
    pub get_z: Option<unsafe extern "C" fn(*const c_void) -> f32>,
    pub set_z: Option<unsafe extern "C" fn(*mut c_void, f32) -> FfiResult>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CombatHealthVTable {
    pub header: VTableHeader,
    pub get_current: Option<unsafe extern "C" fn(*const c_void) -> i32>,
    pub set_current: Option<unsafe extern "C" fn(*mut c_void, i32) -> FfiResult>,
    pub get_max: Option<unsafe extern "C" fn(*const c_void) -> i32>,
    pub set_max: Option<unsafe extern "C" fn(*mut c_void, i32) -> FfiResult>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MetadataTagVTable {
    pub header: VTableHeader,
    pub get_label: Option<unsafe extern "C" fn(*const c_void) -> StrView>,
    pub set_label: Option<unsafe extern "C" fn(*mut c_void, StrView) -> FfiResult>,
}

// ============================================================================
// 2. Dynamic Component & POD Resource Definitions
// ============================================================================

/// Dynamic runtime component registered by `asymmetric_rust_mod` (`asymmetric_rust_mod::RustModCustomData`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

impl GuestComponent for RustModCustomData {
    const NAME: &'static CStr = c"RustModCustomData";
}

/// POD resource layout mirrored in the Rust guest mod.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct Phase4RustRes {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub spawned_default_entity: u64,
}

#[repr(C)]
#[derive(Clone, Copy, AbiPod)]
struct RawPos {
    x: f32,
    y: f32,
    z: f32,
}

// ============================================================================
// 3. Test Harness Controls
// ============================================================================

static PANIC_TRIGGER: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_CALLED: AtomicBool = AtomicBool::new(false);
static PHASE4_RUST_RUN_ENABLED: AtomicBool = AtomicBool::new(true);

/// Triggers a simulated panic during the next execution of `rust_panicking`.
///
/// # Safety
/// Safe to invoke from test harness threads; operates atomically with `SeqCst` ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asymmetric_rust_mod_trigger_panic() {
    PANIC_TRIGGER.store(true, Ordering::SeqCst);
}

/// Resets the simulated panic trigger so subsequent system executions run normally.
///
/// # Safety
/// Safe to invoke from test harness threads; operates atomically with `SeqCst` ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asymmetric_rust_mod_reset_panic() {
    PANIC_TRIGGER.store(false, Ordering::SeqCst);
}

/// Queries whether the mod has received a shutdown signal via `bevy_abi_mod_shutdown`.
///
/// # Safety
/// Caller must ensure the dynamic library remains loaded during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asymmetric_rust_mod_was_shutdown() -> bool {
    SHUTDOWN_CALLED.load(Ordering::SeqCst)
}

/// Toggles the Phase 4 conditional system `run_if` predicate in `asymmetric_rust_mod`.
///
/// # Safety
/// Caller must ensure the dynamic library remains loaded during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asymmetric_rust_mod_set_phase4_enabled(enabled: FfiBool) {
    PHASE4_RUST_RUN_ENABLED.store(enabled.get(), Ordering::SeqCst);
}

// ============================================================================
// 4. Per-Instance Resolution Helpers (Zero Process-Global API/VTable State)
// ============================================================================

/// Resolves the first matching VTable symbol and its corresponding `BamComponent` handle.
///
/// # Safety
/// `V` must be a `#[repr(C)]` VTable struct prefixed at byte offset `0` by `VTableHeader`.
unsafe fn resolve_vtable_and_handle<'a, V: Copy + 'static>(
    host: &Host,
    vtable_candidates: &[&CStr],
) -> Option<(&'a V, BamComponent)> {
    for &sym in vtable_candidates {
        let raw = host.get_proc_address(sym);
        if raw.is_null() || !(raw as usize).is_multiple_of(core::mem::align_of::<V>()) {
            continue;
        }
        // SAFETY: `raw` is non-null, aligned to `V`, and points to a `VTableHeader`-prefixed symbol.
        let Ok(hdr) = (unsafe { read_descriptor::<VTableHeader>(raw) }) else {
            continue;
        };
        if hdr.validate(core::mem::size_of::<V>(), hdr.uuid).is_err() {
            continue;
        }
        let Ok(comp) = bevy_abi_mod_guest::raw::resolve_component(host, None, Some(hdr.uuid))
        else {
            continue;
        };
        // SAFETY: `hdr.validate` confirmed `struct_size >= size_of::<V>()` and ABI compatibility;
        // static host VTables outlive the mod session.
        let vt_ref: &'a V = unsafe { &*raw.cast::<V>() };
        return Some((vt_ref, comp));
    }
    None
}

fn resolve_any_named(app: &GuestApp<'_>, candidates: &[&CStr]) -> Option<BamComponent> {
    for &name in candidates {
        if let Ok(comp) = app.resolve_component_named(Some(name), None) {
            return Some(comp);
        }
    }
    None
}

// ============================================================================
// 5. Per-World `setup` (`L-02`, `L-08`, `G-02`)
// ============================================================================

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    app.on_shutdown(|| {
        SHUTDOWN_CALLED.store(true, Ordering::SeqCst);
        PANIC_TRIGGER.store(false, Ordering::SeqCst);
        PHASE4_RUST_RUN_ENABLED.store(true, Ordering::SeqCst);
    });

    // Register `asymmetric_rust_mod::RustModCustomData` dynamically (`C-10`, `G-07`).
    let custom_comp = match app.register_component::<RustModCustomData>() {
        Ok(h) => h,
        Err(err) => return err,
    };

    let host = *app.host();

    // SAFETY: Each `*VTable` struct mirrors the `#[repr(C)]` layout emitted by `#[derive(ModComponent)]`.
    let pos_resolved = unsafe {
        resolve_vtable_and_handle::<PositionVTable>(
            &host,
            &[
                c"bam.v0.testbed::Position.vtable",
                c"bam.v0.e2e::asymmetric_mods::Position.vtable",
                c"bam.v0.Position.vtable",
            ],
        )
    };
    // SAFETY: `VelocityVTable` mirrors `#[derive(ModComponent)]` VTable layout.
    let vel_resolved = unsafe {
        resolve_vtable_and_handle::<VelocityVTable>(
            &host,
            &[
                c"bam.v0.testbed::Velocity.vtable",
                c"bam.v0.e2e::asymmetric_mods::Velocity.vtable",
                c"bam.v0.Velocity.vtable",
            ],
        )
    };
    // SAFETY: `CombatHealthVTable` mirrors `#[derive(ModComponent)]` VTable layout.
    let health_resolved = unsafe {
        resolve_vtable_and_handle::<CombatHealthVTable>(
            &host,
            &[
                c"bam.v0.testbed::CombatHealth.vtable",
                c"bam.v0.testbed::Health.vtable",
                c"bam.v0.e2e::asymmetric_mods::CombatHealth.vtable",
                c"bam.v0.CombatHealth.vtable",
            ],
        )
    };
    // SAFETY: `MetadataTagVTable` mirrors `#[derive(ModComponent)]` VTable layout.
    let tag_resolved = unsafe {
        resolve_vtable_and_handle::<MetadataTagVTable>(
            &host,
            &[
                c"bam.v0.testbed::MetadataTag.vtable",
                c"bam.v0.testbed::NameTag.vtable",
                c"bam.v0.MetadataTag.vtable",
                c"bam.v0.NameTag.vtable",
            ],
        )
    };

    // 1. Kinematics System (`Position` WRITE + `Velocity` READ)
    if let (Some((&pos_vt, pos_comp)), Some((&vel_vt, vel_comp))) = (pos_resolved, vel_resolved) {
        let status = app
            .system(c"rust_kinematics")
            .query(|q| q.write_handle(pos_comp).read_handle(vel_comp))
            .build(move |ctx| {
                let (
                    Some(get_px),
                    Some(set_px),
                    Some(get_py),
                    Some(set_py),
                    Some(get_pz),
                    Some(set_pz),
                    Some(get_vx),
                    Some(get_vy),
                    Some(get_vz),
                ) = (
                    pos_vt.get_x,
                    pos_vt.set_x,
                    pos_vt.get_y,
                    pos_vt.set_y,
                    pos_vt.get_z,
                    pos_vt.set_z,
                    vel_vt.get_x,
                    vel_vt.get_y,
                    vel_vt.get_z,
                )
                else {
                    return FfiResult::NULL_POINTER;
                };

                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let count = chunk.len();
                    let vel_ptrs: Vec<*const c_void> = {
                        let vel_slice = chunk.readonly_slice(1);
                        let mut v = Vec::with_capacity(count);
                        for row in 0..count {
                            let Some(p) = vel_slice.get_ptr(row, vel_vt.header.size) else {
                                return FfiResult::NULL_POINTER;
                            };
                            v.push(p.cast::<c_void>());
                        }
                        v
                    };

                    let mut pos_slice = chunk.mutable_slice(0);
                    for (row, &v_ptr) in vel_ptrs.iter().enumerate() {
                        let Some(p_raw) = pos_slice.get_mut_ptr(row, pos_vt.header.size) else {
                            return FfiResult::NULL_POINTER;
                        };
                        let p_ptr = p_raw.cast::<c_void>();
                        // SAFETY: `p_ptr` and `v_ptr` point to valid initialized `Position` and `Velocity` elements.
                        unsafe {
                            let rx = set_px(p_ptr, get_px(p_ptr) + get_vx(v_ptr));
                            let ry = set_py(p_ptr, get_py(p_ptr) + get_vy(v_ptr));
                            let rz = set_pz(p_ptr, get_pz(p_ptr) + get_vz(v_ptr));
                            if rx.is_err() || ry.is_err() || rz.is_err() {
                                return FfiResult::INTERNAL_ERROR;
                            }
                        }
                    }
                }
                FfiResult::OK
            });
        if status.is_err() {
            return status;
        }
    }

    // 2. Combat Damage System (`CombatHealth` WRITE + deferred despawn at 0 HP)
    if let Some((&health_vt, health_comp)) = health_resolved {
        let status = app
            .system(c"rust_combat")
            .query(|q| q.write_handle(health_comp))
            .build(move |ctx| {
                let (Some(get_cur), Some(set_cur)) = (health_vt.get_current, health_vt.set_current)
                else {
                    return FfiResult::NULL_POINTER;
                };
                let (iter, cmds) = ctx.iter_and_commands();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    let entities: Vec<FfiEntity> = chunk.entities().to_vec();
                    let mut health_slice = chunk.mutable_slice(0);
                    for (row, &entity) in entities.iter().enumerate() {
                        let Some(h_raw) = health_slice.get_mut_ptr(row, health_vt.header.size)
                        else {
                            return FfiResult::NULL_POINTER;
                        };
                        let h_ptr = h_raw.cast::<c_void>();
                        // SAFETY: `h_ptr` points to a valid `CombatHealth` element in column 0.
                        let new_hp = unsafe {
                            let cur = get_cur(h_ptr);
                            let next = (cur - 15).max(0);
                            let rc = set_cur(h_ptr, next);
                            if rc.is_err() {
                                return rc;
                            }
                            next
                        };
                        if new_hp == 0 {
                            let _ = cmds.despawn(entity);
                        }
                    }
                }
                FfiResult::OK
            });
        if status.is_err() {
            return status;
        }
    }

    // 3. Metadata System (`MetadataTag` / `NameTag` WRITE via untracked slice + selective `mark_changed`)
    if let Some((&tag_vt, tag_comp)) = tag_resolved {
        let status = app
            .system(c"rust_metadata")
            .query(|q| q.write_handle(tag_comp))
            .build(move |ctx| {
                let (Some(get_label), Some(set_label)) = (tag_vt.get_label, tag_vt.set_label)
                else {
                    return FfiResult::NULL_POINTER;
                };
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let count = chunk.len();
                    let mut changed_rows = Vec::new();
                    {
                        let mut tag_slice = chunk.mutable_slice_untracked(0);
                        for row in 0..count {
                            let Some(t_raw) = tag_slice.get_mut_ptr(row, tag_vt.header.size) else {
                                return FfiResult::NULL_POINTER;
                            };
                            let t_ptr = t_raw.cast::<c_void>();
                            // SAFETY: `t_ptr` points to a valid `MetadataTag`/`NameTag` element.
                            let view = unsafe { get_label(t_ptr) };
                            // SAFETY: `view` borrows the UTF-8 buffer of the host `String` prior to mutation.
                            if let Ok(label) = unsafe { view.as_str() }
                                && !label.starts_with("Modded_")
                            {
                                let mut updated = String::with_capacity(label.len() + 8);
                                updated.push_str("Modded_");
                                updated.push_str(label);
                                // SAFETY: `t_ptr` is valid and `StrView::from_str` borrows `updated` for the call.
                                let rc = unsafe { set_label(t_ptr, StrView::from_str(&updated)) };
                                if rc.is_err() {
                                    return rc;
                                }
                                changed_rows.push(row);
                            }
                        }
                    }
                    for row in changed_rows {
                        let rc = chunk.mark_changed(0, row);
                        if rc.is_err() {
                            return rc;
                        }
                    }
                }
                FfiResult::OK
            });
        if status.is_err() {
            return status;
        }
    }

    // 4. Command Spawner System (handle-based `spawn_empty`, `insert`, and `despawn`)
    if let Some((_, pos_comp)) = pos_resolved {
        let status = app.system(c"rust_command_spawner").build(move |ctx| {
            let cmds = ctx.commands();
            let new_entity = match cmds.spawn_empty() {
                Ok(e) => e,
                Err(err) => return err,
            };
            let p = RawPos {
                x: 100.0,
                y: 200.0,
                z: 300.0,
            };
            let rc = cmds.insert(new_entity, pos_comp, &p);
            if rc.is_err() {
                return rc;
            }
            let custom = RustModCustomData {
                power: 42.0,
                shield: 100,
            };
            let rc = cmds.insert(new_entity, custom_comp, &custom);
            if rc.is_err() {
                return rc;
            }
            cmds.despawn(new_entity)
        });
        if status.is_err() {
            return status;
        }
    }

    // 5. Dynamic Runtime Component Mutation System (`asymmetric_rust_mod::RustModCustomData`)
    {
        let status = app
            .system(c"rust_custom_system")
            .query(|q| q.write_handle(custom_comp))
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(items) = chunk.component_mut::<RustModCustomData>(0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for item in items.iter_mut() {
                        item.power += 10.0;
                        item.shield += 1;
                    }
                }
                FfiResult::OK
            });
        if status.is_err() {
            return status;
        }
    }

    // 6. Phase 4 Conditional System (`Optional` + `Changed` + `Added` + `Resource` + `run_if`)
    // SAFETY: `PositionVTable` matches the `x, y, z: f32` VTable layout of `Phase4Transform`.
    let p4_tf_resolved = unsafe {
        resolve_vtable_and_handle::<PositionVTable>(
            &host,
            &[
                c"bam.v0.testbed::Phase4Transform.vtable",
                c"bam.v0.e2e::asymmetric_mods::Phase4Transform.vtable",
                c"bam.v0.ffi::sparse_and_resources::p4_resources_and_optional::Phase4Transform.vtable",
            ],
        )
    };
    let p4_sensor = resolve_any_named(
        app,
        &[
            c"testbed::Phase4Sensor",
            c"e2e::asymmetric_mods::Phase4Sensor",
            c"ffi::sparse_and_resources::p4_resources_and_optional::Phase4Sensor",
        ],
    );
    let p4_fresh = resolve_any_named(
        app,
        &[
            c"testbed::Phase4FreshTag",
            c"e2e::asymmetric_mods::Phase4FreshTag",
            c"ffi::sparse_and_resources::p4_resources_and_optional::Phase4FreshTag",
        ],
    );
    let p4_removable = resolve_any_named(
        app,
        &[
            c"testbed::Phase4Removable",
            c"e2e::asymmetric_mods::Phase4Removable",
            c"ffi::sparse_and_resources::p4_resources_and_optional::Phase4Removable",
        ],
    );
    let p4_res = resolve_any_named(
        app,
        &[
            c"testbed::Phase4RustRes",
            c"e2e::asymmetric_mods::Phase4RustRes",
            c"ffi::sparse_and_resources::p4_resources_and_optional::Phase4RustRes",
        ],
    );
    let nametag_comp = resolve_any_named(
        app,
        &[
            c"testbed::NameTag",
            c"e2e::asymmetric_mods::NameTag",
            c"ffi::sparse_and_resources::p4_resources_and_optional::NameTag",
            c"NameTag",
        ],
    );

    if let (
        Some((&p4_tf_vt, p4_tf_comp)),
        Some((&vel_vt, vel_comp)),
        Some(sensor_comp),
        Some(fresh_comp),
        Some(res_comp),
    ) = (p4_tf_resolved, vel_resolved, p4_sensor, p4_fresh, p4_res)
    {
        let status = app
            .system(c"rust_phase4_ecs_system")
            .query(|q| {
                q.write_handle(p4_tf_comp)
                    .optional_read_handle(vel_comp)
                    .changed_handle(sensor_comp)
                    .added_handle(fresh_comp)
            })
            .resource_write_handle(res_comp)
            .run_if_fn(|| PHASE4_RUST_RUN_ENABLED.load(Ordering::SeqCst))
            .build(move |ctx| {
                let (Some(get_px), Some(set_px), Some(get_vx)) =
                    (p4_tf_vt.get_x, p4_tf_vt.set_x, vel_vt.get_x)
                else {
                    return FfiResult::NULL_POINTER;
                };

                let (scale, need_spawn_default) = {
                    let Some(res) = ctx.resource::<Phase4RustRes>(0) else {
                        return FfiResult::INVALID_COMPONENT;
                    };
                    (res.scale, res.spawned_default_entity == 0)
                };

                let mut spawned_id = 0u64;
                if need_spawn_default
                    && let Some(nt_comp) = nametag_comp
                    && let Ok(spawned) = ctx.commands().spawn_empty()
                    && ctx.commands().insert_default(spawned, nt_comp).is_ok()
                {
                    spawned_id = spawned.0;
                }

                let mut matched_delta = 0u32;
                let mut opt_vel_delta = 0u32;

                {
                    let (iter, cmds) = ctx.iter_and_commands();
                    while let Some(mut chunk) = iter.next_chunk_view() {
                        let count = chunk.len();
                        let entities_copy: Vec<FfiEntity> = chunk.entities().to_vec();
                        let vel_ptrs: Option<Vec<*const c_void>> = {
                            let vel_slice = chunk.readonly_slice(1);
                            if vel_slice.is_empty() {
                                None
                            } else {
                                let mut v = Vec::with_capacity(count);
                                for row in 0..count {
                                    let Some(p) = vel_slice.get_ptr(row, vel_vt.header.size) else {
                                        return FfiResult::NULL_POINTER;
                                    };
                                    v.push(p.cast::<c_void>());
                                }
                                Some(v)
                            }
                        };

                        let mut pos_slice = chunk.mutable_slice(0);
                        for row in 0..count {
                            let Some(p_raw) = pos_slice.get_mut_ptr(row, p4_tf_vt.header.size)
                            else {
                                return FfiResult::NULL_POINTER;
                            };
                            let p_ptr = p_raw.cast::<c_void>();
                            // SAFETY: `p_ptr` points to a valid `Phase4Transform` element.
                            let px = unsafe { get_px(p_ptr) };
                            if let Some(ref v_vec) = vel_ptrs {
                                // SAFETY: `v_vec[row]` points to a valid `Velocity` element.
                                let vx = unsafe { get_vx(v_vec[row]) };
                                // SAFETY: `p_ptr` is valid and aligned.
                                let rc = unsafe { set_px(p_ptr, px + vx * scale) };
                                if rc.is_err() {
                                    return rc;
                                }
                                opt_vel_delta += 1;
                            } else {
                                // SAFETY: `p_ptr` is valid and aligned.
                                let rc = unsafe { set_px(p_ptr, px + scale) };
                                if rc.is_err() {
                                    return rc;
                                }
                            }
                            matched_delta += 1;

                            if let Some(rem_comp) = p4_removable {
                                let _ = cmds.remove(entities_copy[row], rem_comp);
                            }
                        }
                    }
                }

                if spawned_id != 0 || matched_delta > 0 || opt_vel_delta > 0 {
                    let Some(res_mut) = ctx.resource_mut::<Phase4RustRes>(0) else {
                        return FfiResult::INVALID_COMPONENT;
                    };
                    if spawned_id != 0 {
                        res_mut.spawned_default_entity = spawned_id;
                    }
                    res_mut.matched_rows += matched_delta;
                    res_mut.optional_vel_hits += opt_vel_delta;
                }

                FfiResult::OK
            });
        if status.is_err() {
            return status;
        }
    }

    // 7. Fault Isolation Panicking System (`panic = "unwind"` -> `FfiResult::PANIC_CAUGHT`)
    app.system(c"rust_panicking").build(|_ctx| {
        if PANIC_TRIGGER.swap(false, Ordering::SeqCst) {
            panic!("Intentional cross-runtime panic in asymmetric_rust_mod");
        }
        FfiResult::OK
    })
}

export_mod!(name: c"asymmetric_rust_mod", version: c"0.2.0", setup);
