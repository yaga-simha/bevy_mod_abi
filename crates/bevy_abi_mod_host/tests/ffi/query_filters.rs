//! Host FFI integration tests for Task #10 Query Filters:
//! - `DefaultQueryFilters` (`Disabled` and custom dense/sparse disabling components) & `AccessMode::ALLOW`
//! - `AccessMode::HAS` (`Has<T>`) dense + sparse archetypal boolean synthesis & `&mut T` parallelism
//! - `Or<(...)>` filter groups (`BamTermDesc::or_group`) including `Changed<T>` / `Added<T>` / sparse
//! - `AccessMode::REMOVED_READ` (`RemovedComponents<T>`) multi-batch cursor streaming across frames

use crate::support::{Position, Velocity};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HostWireBridge, LoadContext, ModRegistry,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_iter_read_removed, host_par_for_each_chunk, host_register_system, host_sys_query,
};
use bevy_abi_mod_sys::{
    BamIter, BamQueryDesc, BamSystemCtx, BamSystemDesc, FfiBool, FfiEntity, FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::schedule::Schedule;
use bevy_ecs::system::System as BevySystem;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "qf_test::TagA")]
struct TagA {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "qf_test::TagB")]
struct TagB {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "qf_test::TagC")]
struct TagC {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "qf_test::TagD")]
struct TagD {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "qf_test::CustomDisabled")]
struct CustomDisabled;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "qf_test::SparseDisabled")]
struct SparseDisabled;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "qf_test::SparseMarker")]
struct SparseMarker {
    weight: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "qf_test::SparseTagB")]
struct SparseTagB {
    code: u32,
}

struct DisabledBridge;

impl HostWireBridge<Disabled> for DisabledBridge {
    type Wire = FfiBool;
    const CANONICAL_NAME: &'static str = "bevy_ecs::entity_disabling::Disabled";
    const ALIAS: Option<&'static str> = Some("Disabled");
    const SUPPORTS_SET: bool = false;

    fn to_wire(_value: &Disabled) -> Self::Wire {
        FfiBool::TRUE
    }
}

fn register_disabled_bridge(app: &mut App) -> BamComponent {
    let cid = app.world_mut().register_component::<Disabled>();
    let mut reg = app.world_mut().resource_mut::<ModRegistry>();
    reg.register_bridged_component::<Disabled, DisabledBridge>(cid, 0, None)
        .expect("register Disabled bridge")
}

#[derive(Default)]
struct CollectedEntities {
    entities: Mutex<Vec<Entity>>,
}

unsafe extern "C" fn collect_entities_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let state = unsafe { &*(user_data as *const CollectedEntities) };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut len = 0usize;
    let mut guard = state.entities.lock().unwrap();
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        let ents = unsafe { host_iter_entities(iter) };
        if ents.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..len {
            let ffi_ent = unsafe { *ents.add(i) };
            guard.push(Entity::from_bits(ffi_ent.to_bits()));
        }
    }
    FfiResult::OK
}

// ============================================================================
// 1. DefaultQueryFilters (`Disabled` & custom disablers) + `AccessMode::ALLOW`
// ============================================================================

#[test]
fn default_query_filters_exclude_disabled_and_allow_includes_them() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let disabled_h = register_disabled_bridge(&mut app);
    let custom_disabled_h = app
        .register_mod_component::<CustomDisabled>(ComponentOptions::new())
        .unwrap();
    let sparse_disabled_h = app
        .register_mod_component::<SparseDisabled>(ComponentOptions::new())
        .unwrap();

    app.world_mut()
        .register_disabling_component::<CustomDisabled>();
    app.world_mut()
        .register_disabling_component::<SparseDisabled>();

    let e_enabled = app.world_mut().spawn(Position::default()).id();
    let e_disabled = app.world_mut().spawn((Position::default(), Disabled)).id();
    let e_custom = app
        .world_mut()
        .spawn((Position::default(), CustomDisabled))
        .id();
    let e_sparse = app
        .world_mut()
        .spawn((Position::default(), SparseDisabled))
        .id();

    let reg = app.world().resource::<ModRegistry>().clone();

    let default_state = Arc::new(CollectedEntities::default());
    let allow_disabled_state = Arc::new(CollectedEntities::default());
    let allow_custom_state = Arc::new(CollectedEntities::default());
    let allow_sparse_state = Arc::new(CollectedEntities::default());

    let sys_default = GuestSystem::new(
        "default_filter_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::read(pos_h)])],
        collect_entities_cb,
        Arc::as_ptr(&default_state) as *mut c_void,
        reg.clone(),
    );
    let sys_allow_disabled = GuestSystem::new(
        "allow_disabled_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::allow(disabled_h),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&allow_disabled_state) as *mut c_void,
        reg.clone(),
    );
    let sys_allow_custom = GuestSystem::new(
        "allow_custom_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::allow(custom_disabled_h),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&allow_custom_state) as *mut c_void,
        reg.clone(),
    );
    let sys_allow_sparse = GuestSystem::new(
        "allow_sparse_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::allow(sparse_disabled_h),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&allow_sparse_state) as *mut c_void,
        reg,
    );

    app.add_systems(
        Update,
        (
            sys_default,
            sys_allow_disabled,
            sys_allow_custom,
            sys_allow_sparse,
        ),
    );
    app.update();

    let got_default = default_state.entities.lock().unwrap().clone();
    assert_eq!(got_default, vec![e_enabled]);

    let mut got_allow_disabled = allow_disabled_state.entities.lock().unwrap().clone();
    got_allow_disabled.sort();
    let mut expected_allow_disabled = vec![e_enabled, e_disabled];
    expected_allow_disabled.sort();
    assert_eq!(got_allow_disabled, expected_allow_disabled);

    let mut got_allow_custom = allow_custom_state.entities.lock().unwrap().clone();
    got_allow_custom.sort();
    let mut expected_allow_custom = vec![e_enabled, e_custom];
    expected_allow_custom.sort();
    assert_eq!(got_allow_custom, expected_allow_custom);

    let mut got_allow_sparse = allow_sparse_state.entities.lock().unwrap().clone();
    got_allow_sparse.sort();
    let mut expected_allow_sparse = vec![e_enabled, e_sparse];
    expected_allow_sparse.sort();
    assert_eq!(got_allow_sparse, expected_allow_sparse);
}

// ============================================================================
// 2. `AccessMode::HAS` (`Has<T>`) & Parallel Scheduler Compatibility
// ============================================================================

#[derive(Default)]
struct HasScanState {
    results: Mutex<Vec<(Entity, bool, bool)>>,
    mut_col_was_null: AtomicBool,
}

unsafe extern "C" fn scan_has_cb(sys_ctx: *mut BamSystemCtx, user_data: *mut c_void) -> FfiResult {
    let state = unsafe { &*(user_data as *const HasScanState) };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut len = 0usize;
    let mut guard = state.results.lock().unwrap();
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        let ents = unsafe { host_iter_entities(iter) };
        let has_vel_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const FfiBool;
        let has_sparse_ptr = unsafe { host_iter_column_readonly(iter, 2) } as *const FfiBool;
        if ents.is_null() || has_vel_ptr.is_null() || has_sparse_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // Attempting mutable column access on `HAS` must return null.
        let mut_ptr = unsafe { host_iter_column_mutable(iter, 1) };
        state
            .mut_col_was_null
            .store(mut_ptr.is_null(), Ordering::SeqCst);

        for i in 0..len {
            let e = Entity::from_bits(unsafe { (*ents.add(i)).to_bits() });
            let has_v = unsafe { (*has_vel_ptr.add(i)).get() };
            let has_s = unsafe { (*has_sparse_ptr.add(i)).get() };
            guard.push((e, has_v, has_s));
        }
    }
    FfiResult::OK
}

#[test]
fn has_synthesizes_dense_and_sparse_presence_buffers() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let sparse_h = app
        .register_mod_component::<SparseMarker>(ComponentOptions::new())
        .unwrap();

    let e0 = app.world_mut().spawn(Position::default()).id();
    let e1 = app
        .world_mut()
        .spawn((Position::default(), Velocity::default()))
        .id();
    let e2 = app
        .world_mut()
        .spawn((Position::default(), SparseMarker { weight: 1.0 }))
        .id();
    let e3 = app
        .world_mut()
        .spawn((
            Position::default(),
            Velocity::default(),
            SparseMarker { weight: 2.0 },
        ))
        .id();

    let state = Arc::new(HasScanState::default());
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "has_scan_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::has(vel_h),
            BamTermDesc::has(sparse_h),
        ])],
        scan_has_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    assert!(state.mut_col_was_null.load(Ordering::SeqCst));
    let mut results = state.results.lock().unwrap().clone();
    results.sort_by_key(|(e, _, _)| *e);
    let mut expected = vec![
        (e0, false, false),
        (e1, true, false),
        (e2, false, true),
        (e3, true, true),
    ];
    expected.sort_by_key(|(e, _, _)| *e);
    assert_eq!(results, expected);
}

unsafe extern "C" fn par_has_task(sub_iter: *mut BamIter, user_data: *mut c_void) -> FfiResult {
    let true_count = unsafe { &*(user_data as *const AtomicUsize) };
    let mut len = 0usize;
    while unsafe { host_iter_next_chunk(sub_iter, &mut len) }.get() {
        let has_vel = unsafe { host_iter_column_readonly(sub_iter, 1) } as *const FfiBool;
        if has_vel.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..len {
            if unsafe { (*has_vel.add(i)).get() } {
                true_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn par_has_sys_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    unsafe { host_par_for_each_chunk(iter, Some(par_has_task), user_data) }
}

#[test]
fn has_works_in_parallel_chunks_and_does_not_conflict_with_mut_t() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();

    for i in 0..600 {
        if i % 3 == 0 {
            app.world_mut()
                .spawn((Position::default(), Velocity::default()));
        } else {
            app.world_mut().spawn(Position::default());
        }
    }

    let reg = app.world().resource::<ModRegistry>().clone();

    unsafe extern "C" fn noop_sys(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    // 1. Validate intra-system compatibility: Query 0 has `WRITE Velocity`, Query 1 has `HAS Velocity`.
    {
        let mut load_ctx = LoadContext::new(reg.clone());
        let q0_terms = [BamTermDesc::write(vel_h)];
        let q1_terms = [BamTermDesc::read(pos_h), BamTermDesc::has(vel_h)];
        let queries = [
            BamQueryDesc::new(q0_terms.as_ptr(), q0_terms.len()),
            BamQueryDesc::new(q1_terms.as_ptr(), q1_terms.len()),
        ];
        let desc = BamSystemDesc::new(
            c"write_and_has_sys".as_ptr(),
            queries.as_ptr(),
            queries.len(),
            noop_sys,
            core::ptr::null_mut(),
        );
        assert_eq!(
            unsafe { host_register_system(load_ctx.as_ffi_ptr(), &desc) },
            FfiResult::OK
        );
        load_ctx.finish_init();
    }

    // 2. Verify two separate GuestSystems (`&mut Velocity` vs `Has<Velocity>`) have compatible
    // `SystemAccess` in Bevy's scheduler and execute cleanly in the same schedule!
    unsafe extern "C" fn write_vel_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        let ran = unsafe { &*(ud as *const AtomicBool) };
        ran.store(true, Ordering::SeqCst);
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut len = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
            let ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Velocity;
            if ptr.is_null() {
                return FfiResult::NULL_POINTER;
            }
        }
        FfiResult::OK
    }

    let mut writer_sys = GuestSystem::new(
        "writer_vel",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::write(vel_h)])],
        write_vel_cb,
        core::ptr::null_mut(),
        reg.clone(),
    );
    let mut has_sys = GuestSystem::new(
        "reader_has_vel",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::has(vel_h),
        ])],
        par_has_sys_cb,
        core::ptr::null_mut(),
        reg.clone(),
    );
    let writer_access = writer_sys.initialize(app.world_mut());
    let has_access = has_sys.initialize(app.world_mut());
    assert!(
        writer_access.is_compatible(&has_access),
        "Has<Velocity> must be scheduler-compatible with &mut Velocity"
    );

    let writer_ran = Arc::new(AtomicBool::new(false));
    let true_count = Arc::new(AtomicUsize::new(0));
    let writer_sys = GuestSystem::new(
        "writer_vel",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::write(vel_h)])],
        write_vel_cb,
        Arc::as_ptr(&writer_ran) as *mut c_void,
        reg.clone(),
    );
    let has_sys = GuestSystem::new(
        "par_has_vel",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::has(vel_h),
        ])],
        par_has_sys_cb,
        Arc::as_ptr(&true_count) as *mut c_void,
        reg,
    );

    let mut schedule = Schedule::default();
    schedule.add_systems((writer_sys, has_sys));
    schedule.run(app.world_mut());

    assert!(writer_ran.load(Ordering::SeqCst));
    assert_eq!(true_count.load(Ordering::SeqCst), 200);
}

// ============================================================================
// 3. `Or<(...)>` Filter Groups (`BamTermDesc::or_group`)
// ============================================================================

#[test]
fn or_groups_archetypal_and_multi_group_conjunction() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let a_h = app
        .register_mod_component::<TagA>(ComponentOptions::new())
        .unwrap();
    let b_h = app
        .register_mod_component::<TagB>(ComponentOptions::new())
        .unwrap();
    let c_h = app
        .register_mod_component::<TagC>(ComponentOptions::new())
        .unwrap();
    let d_h = app
        .register_mod_component::<TagD>(ComponentOptions::new())
        .unwrap();

    let e_none = app.world_mut().spawn(Position::default()).id();
    let e_a = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 1 }))
        .id();
    let e_b = app
        .world_mut()
        .spawn((Position::default(), TagB { id: 2 }))
        .id();
    let e_ab = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 3 }, TagB { id: 4 }))
        .id();
    let e_ac = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 5 }, TagC { id: 6 }))
        .id();
    let e_bd = app
        .world_mut()
        .spawn((Position::default(), TagB { id: 7 }, TagD { id: 8 }))
        .id();

    let reg = app.world().resource::<ModRegistry>().clone();

    // Or<(With<A>, With<B>)>
    let or_ab_state = Arc::new(CollectedEntities::default());
    let sys_or_ab = GuestSystem::new(
        "or_ab_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::filter_only(a_h).with_or_group(1),
            BamTermDesc::filter_only(b_h).with_or_group(1),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&or_ab_state) as *mut c_void,
        reg.clone(),
    );

    // Or<(Without<A>, With<B>)>
    let or_without_a_or_b_state = Arc::new(CollectedEntities::default());
    let sys_without_a_or_b = GuestSystem::new(
        "or_without_a_or_b_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::without(a_h).with_or_group(1),
            BamTermDesc::filter_only(b_h).with_or_group(1),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&or_without_a_or_b_state) as *mut c_void,
        reg.clone(),
    );

    // Conjunction of two distinct OR groups: (With<A> || With<B>) && (With<C> || With<D>)
    let multi_or_state = Arc::new(CollectedEntities::default());
    let sys_multi_or = GuestSystem::new(
        "multi_or_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::filter_only(a_h).with_or_group(1),
            BamTermDesc::filter_only(b_h).with_or_group(1),
            BamTermDesc::filter_only(c_h).with_or_group(2),
            BamTermDesc::filter_only(d_h).with_or_group(2),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&multi_or_state) as *mut c_void,
        reg,
    );

    app.add_systems(Update, (sys_or_ab, sys_without_a_or_b, sys_multi_or));
    app.update();

    let mut got_ab = or_ab_state.entities.lock().unwrap().clone();
    got_ab.sort();
    let mut exp_ab = vec![e_a, e_b, e_ab, e_ac, e_bd];
    exp_ab.sort();
    assert_eq!(got_ab, exp_ab);

    let mut got_wo_a_b = or_without_a_or_b_state.entities.lock().unwrap().clone();
    got_wo_a_b.sort();
    let mut exp_wo_a_b = vec![e_none, e_b, e_ab, e_bd];
    exp_wo_a_b.sort();
    assert_eq!(got_wo_a_b, exp_wo_a_b);

    let mut got_multi = multi_or_state.entities.lock().unwrap().clone();
    got_multi.sort();
    let mut exp_multi = vec![e_ac, e_bd];
    exp_multi.sort();
    assert_eq!(got_multi, exp_multi);
}

#[test]
fn or_groups_changed_added_dense_and_sparse_across_frames() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let a_h = app
        .register_mod_component::<TagA>(ComponentOptions::new())
        .unwrap();
    let b_h = app
        .register_mod_component::<TagB>(ComponentOptions::new())
        .unwrap();
    let sparse_b_h = app
        .register_mod_component::<SparseTagB>(ComponentOptions::new())
        .unwrap();

    // Dense entities:
    let e_both_unchanged = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 1 }, TagB { id: 1 }))
        .id();
    let e_both_mut_a = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 2 }, TagB { id: 2 }))
        .id();
    let e_only_a_later_add_b = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 3 }))
        .id();

    // Sparse entity:
    let e_sparse = app
        .world_mut()
        .spawn((Position::default(), TagA { id: 4 }, SparseTagB { code: 9 }))
        .id();

    let reg = app.world().resource::<ModRegistry>().clone();

    let dense_or_state = Arc::new(CollectedEntities::default());
    let mixed_or_state = Arc::new(CollectedEntities::default());
    let with_a_or_changed_b_state = Arc::new(CollectedEntities::default());

    let sys_dense_or = GuestSystem::new(
        "dense_changed_added_or",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::changed(a_h).with_or_group(1),
            BamTermDesc::added(b_h).with_or_group(1),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&dense_or_state) as *mut c_void,
        reg.clone(),
    );

    let sys_mixed_or = GuestSystem::new(
        "mixed_sparse_or",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::changed(a_h).with_or_group(1),
            BamTermDesc::added(sparse_b_h).with_or_group(1),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&mixed_or_state) as *mut c_void,
        reg.clone(),
    );

    let sys_with_a_or_changed_b = GuestSystem::new(
        "with_a_or_changed_b",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::read(pos_h),
            BamTermDesc::filter_only(a_h).with_or_group(1),
            BamTermDesc::changed(b_h).with_or_group(1),
        ])],
        collect_entities_cb,
        Arc::as_ptr(&with_a_or_changed_b_state) as *mut c_void,
        reg,
    );

    app.add_systems(
        Update,
        (sys_dense_or, sys_mixed_or, sys_with_a_or_changed_b),
    );

    // Frame 1: all entities just spawned -> all Added/Changed match!
    app.update();
    {
        let mut got = dense_or_state
            .entities
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        got.sort();
        let mut exp = vec![
            e_both_unchanged,
            e_both_mut_a,
            e_only_a_later_add_b,
            e_sparse,
        ];
        exp.sort();
        assert_eq!(got, exp);

        let mut got_mixed = mixed_or_state
            .entities
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        got_mixed.sort();
        assert_eq!(got_mixed, exp);
        with_a_or_changed_b_state.entities.lock().unwrap().clear();
    }

    // Frame 2: no mutations -> dense/mixed Changed/Added Or groups yield 0,
    // while Or<(With<TagA>, Changed<TagB>)> STILL yields all 4 entities because TagA is present!
    app.update();
    {
        assert!(dense_or_state.entities.lock().unwrap().is_empty());
        assert!(mixed_or_state.entities.lock().unwrap().is_empty());
        let mut got_wa = with_a_or_changed_b_state
            .entities
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        got_wa.sort();
        let mut exp = vec![
            e_both_unchanged,
            e_both_mut_a,
            e_only_a_later_add_b,
            e_sparse,
        ];
        exp.sort();
        assert_eq!(got_wa, exp);
    }

    // Frame 3: mutate TagA on `e_both_mut_a`, insert TagB on `e_only_a_later_add_b`,
    // mutate SparseTagB (should NOT trigger Added<SparseTagB>) vs insert SparseTagB on `e_both_mut_a`
    app.world_mut()
        .entity_mut(e_both_mut_a)
        .get_mut::<TagA>()
        .unwrap()
        .id = 200;
    app.world_mut()
        .entity_mut(e_only_a_later_add_b)
        .insert(TagB { id: 300 });
    app.world_mut()
        .entity_mut(e_sparse)
        .get_mut::<SparseTagB>()
        .unwrap()
        .code = 99;
    app.world_mut()
        .entity_mut(e_both_unchanged)
        .insert(SparseTagB { code: 55 });

    app.update();
    {
        let mut got = dense_or_state
            .entities
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        got.sort();
        let mut exp = vec![e_both_mut_a, e_only_a_later_add_b];
        exp.sort();
        assert_eq!(got, exp);

        let mut got_mixed = mixed_or_state
            .entities
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        got_mixed.sort();
        // `e_both_mut_a` via Changed<TagA>, `e_both_unchanged` via Added<SparseTagB>.
        let mut exp_mixed = vec![e_both_mut_a, e_both_unchanged];
        exp_mixed.sort();
        assert_eq!(got_mixed, exp_mixed);
    }
}

#[test]
fn or_group_rejects_invalid_data_access_modes_and_enforces_conflicts() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let a_h = app
        .register_mod_component::<TagA>(ComponentOptions::new())
        .unwrap();
    let res_h = app
        .register_mod_resource::<crate::support::RustFilterTelemetry>()
        .unwrap();

    let reg = app.world().resource::<ModRegistry>().clone();

    unsafe extern "C" fn noop_sys(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    let mut load_ctx = LoadContext::new(reg);
    for bad_term in [
        BamTermDesc::write(pos_h).with_or_group(1),
        BamTermDesc::read(pos_h).with_or_group(1),
        BamTermDesc::optional_read(pos_h).with_or_group(1),
        BamTermDesc::optional_write(pos_h).with_or_group(1),
        BamTermDesc::allow(pos_h).with_or_group(1),
        BamTermDesc::has(pos_h).with_or_group(1),
        BamTermDesc::removed_read(pos_h).with_or_group(1),
        BamTermDesc::resource_read(res_h).with_or_group(1),
    ] {
        let terms = [bad_term, BamTermDesc::filter_only(a_h).with_or_group(1)];
        let q = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let desc = BamSystemDesc::new(
            c"bad_or_group_sys".as_ptr(),
            &q,
            1,
            noop_sys,
            core::ptr::null_mut(),
        );
        let res = unsafe { host_register_system(load_ctx.as_ffi_ptr(), &desc) };
        assert_eq!(
            res,
            FfiResult::INVALID_ARGUMENT,
            "mode {:?} must be rejected inside or_group",
            bad_term.mode
        );
    }

    // Verify `Or<(Changed<Position>, With<TagA>)>` in Query 0 conflicts with `WRITE Position` in Query 1!
    let q0_terms = [
        BamTermDesc::changed(pos_h).with_or_group(1),
        BamTermDesc::filter_only(a_h).with_or_group(1),
    ];
    let q1_terms = [BamTermDesc::write(pos_h)];
    let queries = [
        BamQueryDesc::new(q0_terms.as_ptr(), q0_terms.len()),
        BamQueryDesc::new(q1_terms.as_ptr(), q1_terms.len()),
    ];
    let conflict_desc = BamSystemDesc::new(
        c"or_changed_conflict_sys".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        noop_sys,
        core::ptr::null_mut(),
    );
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &conflict_desc) },
        FfiResult::ACCESS_CONFLICT
    );
    load_ctx.finish_init();
}

// ============================================================================
// 4. `AccessMode::REMOVED_READ` (`RemovedComponents<T>`) Batch Cursor Reads
// ============================================================================

struct RemovedReaderState {
    batch_size: usize,
    batches: Mutex<Vec<Vec<Entity>>>,
    par_conflict: AtomicBool,
    boundary_ok: AtomicBool,
}

unsafe extern "C" fn read_removed_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let state = unsafe { &*(user_data as *const RemovedReaderState) };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }

    // Boundary checks on `host_iter_read_removed`:
    let mut count = 999usize;
    let zero_rc = unsafe { host_iter_read_removed(iter, 0, core::ptr::null_mut(), 0, &mut count) };
    let null_out_read =
        unsafe { host_iter_read_removed(iter, 0, core::ptr::null_mut(), 0, core::ptr::null_mut()) };
    let mut one = [FfiEntity::NULL; 1];
    let oob_rc = unsafe { host_iter_read_removed(iter, 99, one.as_mut_ptr(), 1, &mut count) };
    let perm_rc = unsafe { host_iter_read_removed(iter, 1, one.as_mut_ptr(), 1, &mut count) };
    state.boundary_ok.store(
        zero_rc == FfiResult::OK
            && count == 0
            && null_out_read == FfiResult::NULL_POINTER
            && oob_rc == FfiResult::OUT_OF_RANGE
            && perm_rc == FfiResult::PERMISSION_DENIED,
        Ordering::SeqCst,
    );

    let mut frame_entities = Vec::new();
    let mut buf = vec![FfiEntity::NULL; state.batch_size];
    loop {
        let mut read = 0usize;
        let rc = unsafe {
            host_iter_read_removed(iter, 0, buf.as_mut_ptr(), state.batch_size, &mut read)
        };
        if rc.is_err() {
            return rc;
        }
        if read == 0 {
            break;
        }
        for &ffi_ent in &buf[..read] {
            frame_entities.push(Entity::from_bits(ffi_ent.to_bits()));
        }
        if read < state.batch_size {
            break;
        }
    }
    state.batches.lock().unwrap().push(frame_entities);
    FfiResult::OK
}

unsafe extern "C" fn par_removed_task(sub_iter: *mut BamIter, user_data: *mut c_void) -> FfiResult {
    let state = unsafe { &*(user_data as *const RemovedReaderState) };
    let mut out = [FfiEntity::NULL; 4];
    let mut read = 0usize;
    let rc = unsafe { host_iter_read_removed(sub_iter, 0, out.as_mut_ptr(), 4, &mut read) };
    if rc == FfiResult::ACCESS_CONFLICT {
        state.par_conflict.store(true, Ordering::SeqCst);
    }
    FfiResult::OK
}

unsafe extern "C" fn par_removed_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    unsafe { host_par_for_each_chunk(iter, Some(par_removed_task), user_data) }
}

#[test]
fn removed_read_streams_multi_batch_cursor_across_frames_without_dups() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();

    let entities: Vec<Entity> = (0..10)
        .map(|i| {
            app.world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity::default(),
                ))
                .id()
        })
        .collect();

    let state = Arc::new(RemovedReaderState {
        batch_size: 3,
        batches: Mutex::new(Vec::new()),
        par_conflict: AtomicBool::new(false),
        boundary_ok: AtomicBool::new(false),
    });

    let reg = app.world().resource::<ModRegistry>().clone();

    unsafe extern "C" fn noop_sys(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    // `REMOVED_READ Position` combined with `WRITE Position` in the same system must NOT conflict!
    {
        let mut load_ctx = LoadContext::new(reg.clone());
        let terms = [BamTermDesc::removed_read(pos_h), BamTermDesc::write(pos_h)];
        let q = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let desc = BamSystemDesc::new(
            c"removed_and_write_sys".as_ptr(),
            &q,
            1,
            noop_sys,
            core::ptr::null_mut(),
        );
        assert_eq!(
            unsafe { host_register_system(load_ctx.as_ffi_ptr(), &desc) },
            FfiResult::OK
        );
        load_ctx.finish_init();
    }

    let sys = GuestSystem::new(
        "removed_reader_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::removed_read(pos_h),
            BamTermDesc::read(vel_h),
        ])],
        read_removed_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg.clone(),
    );

    // Also test standalone `REMOVED_READ` (no entity terms) on a second cursor!
    let standalone_state = Arc::new(RemovedReaderState {
        batch_size: 2,
        batches: Mutex::new(Vec::new()),
        par_conflict: AtomicBool::new(false),
        boundary_ok: AtomicBool::new(false),
    });
    unsafe extern "C" fn standalone_removed_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let state = unsafe { &*(user_data as *const RemovedReaderState) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut frame_entities = Vec::new();
        let mut buf = vec![FfiEntity::NULL; state.batch_size];
        loop {
            let mut read = 0usize;
            let rc = unsafe {
                host_iter_read_removed(iter, 0, buf.as_mut_ptr(), state.batch_size, &mut read)
            };
            if rc.is_err() {
                return rc;
            }
            for &ffi_ent in &buf[..read] {
                frame_entities.push(Entity::from_bits(ffi_ent.to_bits()));
            }
            if read < state.batch_size {
                break;
            }
        }
        state.batches.lock().unwrap().push(frame_entities);
        FfiResult::OK
    }
    let standalone_sys = GuestSystem::new(
        "standalone_removed_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::removed_read(
            pos_h,
        )])],
        standalone_removed_cb,
        Arc::as_ptr(&standalone_state) as *mut c_void,
        reg.clone(),
    );

    let par_sys = GuestSystem::new(
        "par_removed_sys",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::removed_read(pos_h),
            BamTermDesc::read(vel_h),
        ])],
        par_removed_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );

    app.add_systems(Update, (sys, standalone_sys, par_sys));

    // Frame 1: no removals yet.
    app.update();
    assert!(state.boundary_ok.load(Ordering::SeqCst));
    assert!(state.par_conflict.load(Ordering::SeqCst));
    assert!(state.batches.lock().unwrap()[0].is_empty());
    assert!(standalone_state.batches.lock().unwrap()[0].is_empty());

    // Remove `Position` from 4 entities and despawn 3 entities (7 total removals, exceeding batch_size=3!).
    for &e in &entities[0..4] {
        app.world_mut().entity_mut(e).remove::<Position>();
    }
    for &e in &entities[4..7] {
        app.world_mut().despawn(e);
    }

    // Frame 2: reads all 7 across 3 batches (3 + 3 + 1), with no missed or duplicate entities.
    app.update();
    let mut got_f2 = state.batches.lock().unwrap()[1].clone();
    let mut got_standalone_f2 = standalone_state.batches.lock().unwrap()[1].clone();
    let mut expected_f2 = entities[0..7].to_vec();
    got_f2.sort();
    got_standalone_f2.sort();
    expected_f2.sort();
    assert_eq!(got_f2, expected_f2);
    assert_eq!(got_standalone_f2, expected_f2);

    // Frame 3: no new removals -> cursor must yield 0!
    app.update();
    assert!(state.batches.lock().unwrap()[2].is_empty());
    assert!(standalone_state.batches.lock().unwrap()[2].is_empty());

    // Frame 4: remove 2 more entities -> cursor yields ONLY those 2!
    for &e in &entities[7..9] {
        app.world_mut().entity_mut(e).remove::<Position>();
    }
    app.update();
    let mut got_f4 = state.batches.lock().unwrap()[3].clone();
    let mut got_standalone_f4 = standalone_state.batches.lock().unwrap()[3].clone();
    let mut expected_f4 = entities[7..9].to_vec();
    got_f4.sort();
    got_standalone_f4.sort();
    expected_f4.sort();
    assert_eq!(got_f4, expected_f4);
    assert_eq!(got_standalone_f4, expected_f4);
}
