//! Integration tests for ABI `0.3.0` decoupled schedule sets (`host_configure_schedule_set`,
//! `host_mount_system_in_schedule` with Bevy `0.20` `.chain()` / `.chain_weak()` / `.before_weak()`
//! / `.after_weak()`), fast schedule-only hot-rewiring (`rewire_mod_schedule_sets_in_world`), and
//! the Bevy `0.20` `FallbackErrorHandler` / `host_report_guest_fault` error bridge.

use bevy_abi_mod_host_sys::schedule::{
    ModScheduleSetRegistry, ScheduleSetConfig, ScheduleSetMode,
    apply_mod_schedule_sets_to_schedule, configure_guest_system_for_schedule,
    install_mod_schedule_set_pass, mount_mod_systems_in_world, rewire_mod_schedule_sets_in_world,
};
use bevy_abi_mod_host_sys::system::dispatch_mod_fault_to_fallback_handler;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, ComponentOptions, GuestSystem, LoadContext, ModErrorPolicy, ModHost, ModId,
    ModNamedSystemSet, ModQuarantine, ModRegistry, ModScheduleMap, ModSystemError,
    ModSystemFaulted, SystemInitError, host_cmd_spawn_empty, host_configure_schedule_set,
    host_iter_next_chunk, host_mount_system_in_schedule, host_register_system_handle,
    host_report_guest_fault, host_sys_commands, host_sys_host_ctx, host_sys_query,
    host_sys_res_mut_ptr,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BAM_SCHEDULE_SET_FLAG_AFTER_WEAK, BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK,
    BAM_SCHEDULE_SET_FLAG_PARALLEL, BAM_SCHEDULE_SET_FLAG_SEQUENCE,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BamCommands, BamComponent, BamIter, BamScheduleSetDesc,
    BamSystem, BamSystemCtx, BamSystemDesc, BamSystemSet, BamTermDesc, FfiEntity, FfiResult,
    ScheduleId, StrView,
};
use bevy_app::Update;
use bevy_ecs::component::Component;
use bevy_ecs::error::{BevyError, ErrorContext, FallbackErrorHandler, Severity};
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{
    FlattenedDependencies, IntoScheduleConfigs, LogLevel, MultiThreadedExecutor, NodeId, Schedule,
    ScheduleBuildError, ScheduleBuildPass, ScheduleBuildSettings, ScheduleGraph, ScheduleLabel,
    Schedules, SystemKey, SystemSetKey,
};
use bevy_ecs::system::{Res, ResMut, System as _};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CustomPhysicsSchedule;

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "v03_sched::TraceLog")]
struct TraceLog {
    steps: [u32; 16],
    len: u32,
}

impl TraceLog {
    fn as_slice(&self) -> &[u32] {
        &self.steps[..(self.len as usize).min(self.steps.len())]
    }

    fn clear(&mut self) {
        self.steps = [0; 16];
        self.len = 0;
    }
}

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "v03_sched::SiblingA")]
struct SiblingA {
    hits: u32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "v03_sched::SiblingB")]
struct SiblingB {
    hits: u32,
}

#[repr(C)]
#[derive(Component, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "v03_sched::TrackedTag")]
struct TrackedTag {
    value: u32,
}

#[derive(Resource, Default)]
struct NativeResA;

#[derive(Resource, Default)]
struct NativeResB;

#[derive(Resource, Default)]
struct NativeTrace(Vec<u32>);

fn encode_step_ud(handle: BamComponent, step: u32) -> *mut c_void {
    (((handle.0 as usize) << 32) | (step as usize)) as *mut c_void
}

unsafe extern "C" fn push_trace_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let step = user_data as usize as u32;
    let handle = BamComponent::from_raw((user_data as usize >> 32) as u32);
    // SAFETY: `sys_ctx` is an active `HostSystemCtx` with declared `RESOURCE_WRITE` on `TraceLog`.
    let ptr = unsafe { host_sys_res_mut_ptr(sys_ctx, handle, null_mut()) };
    if ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `ptr` points to the exclusively borrowed, 4-byte-aligned `TraceLog` resource.
    let trace = unsafe { &mut *ptr.cast::<c_void>().cast::<TraceLog>() };
    let idx = trace.len as usize;
    if idx < trace.steps.len() {
        trace.steps[idx] = step;
        trace.len += 1;
    }
    FfiResult::OK
}

unsafe extern "C" fn bump_u32_res_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let handle = BamComponent::from_raw(user_data as usize as u32);
    // SAFETY: `sys_ctx` is an active `HostSystemCtx` with declared `RESOURCE_WRITE` on a `u32` resource.
    let ptr = unsafe { host_sys_res_mut_ptr(sys_ctx, handle, null_mut()) };
    if ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Both `SiblingA` and `SiblingB` are `#[repr(C)]` 4-byte-aligned single-`u32` structs.
    unsafe {
        *ptr.cast::<c_void>().cast::<u32>() += 1;
    }
    FfiResult::OK
}

unsafe extern "C" fn count_changed_entities_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let counter = user_data.cast::<AtomicUsize>();
    let mut iter: *mut BamIter = null_mut();
    // SAFETY: `sys_ctx` is active and declares query 0.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut total = 0usize;
    let mut chunk_len = 0usize;
    // SAFETY: `iter` is active and `chunk_len` is valid stack storage.
    while unsafe { host_iter_next_chunk(iter, &raw mut chunk_len) }.get() {
        total += chunk_len;
    }
    if !counter.is_null() {
        // SAFETY: Test keeps the `AtomicUsize` alive across all schedule runs.
        unsafe { (*counter).store(total, Ordering::SeqCst) };
    }
    FfiResult::OK
}

unsafe extern "C" fn noop_cb(_sys_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
    FfiResult::OK
}

fn setup_world_with_resources() -> (World, ModRegistry, BamComponent, BamComponent, BamComponent) {
    let mut world = World::new();
    let trace_id = world.register_component::<TraceLog>();
    let sib_a_id = world.register_component::<SiblingA>();
    let sib_b_id = world.register_component::<SiblingB>();

    let mut reg = ModRegistry::new();
    let trace_h = reg.register_resource::<TraceLog>(trace_id).unwrap();
    let sib_a_h = reg.register_resource::<SiblingA>(sib_a_id).unwrap();
    let sib_b_h = reg.register_resource::<SiblingB>(sib_b_id).unwrap();

    world.insert_resource(reg.clone());
    world.insert_resource(TraceLog::default());
    world.insert_resource(SiblingA::default());
    world.insert_resource(SiblingB::default());
    (world, reg, trace_h, sib_a_h, sib_b_h)
}

#[test]
fn configure_schedule_set_handles_scoping_validation_and_cycle_detection() {
    let mut world = World::new();
    let mut sched_map = ModScheduleMap::new();
    let custom_sched = ScheduleId::custom(1024);
    assert!(sched_map.register(custom_sched, CustomPhysicsSchedule));
    world.insert_resource(sched_map);
    world.insert_resource(ModHost::new());

    let reg = ModRegistry::new();

    // Null world_ptr must be rejected cleanly with `INVALID_STATE`.
    {
        let mut standalone = LoadContext::new(reg.clone());
        let desc = BamScheduleSetDesc::new(c"Solo".as_ptr());
        let mut out = BamSystemSet::INVALID;
        // SAFETY: Valid active `LoadContext`, descriptor, and out pointer.
        let rc =
            unsafe { host_configure_schedule_set(standalone.as_ffi_ptr(), &desc, &raw mut out) };
        assert_eq!(rc, FfiResult::INVALID_STATE);
        assert_eq!(out, BamSystemSet::INVALID);
        standalone.finish_init();
    }

    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        Some(ModId(5)),
        Some(Arc::from("combat")),
    );
    let lp = load_ctx.as_ffi_ptr();

    // SAFETY: All descriptors, handle arrays, and out-pointers live on the stack for the duration
    // of each FFI call.
    unsafe {
        // 1. Allocate monotonic handles 0, 1, 2.
        let mut set_a = BamSystemSet::INVALID;
        let desc_a =
            BamScheduleSetDesc::new(c"Input".as_ptr()).with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
        assert_eq!(
            host_configure_schedule_set(lp, &desc_a, &raw mut set_a),
            FfiResult::OK
        );
        assert_eq!(set_a, BamSystemSet::from_raw(0));

        // Duplicate name in same (mod, generation, schedule) rejected.
        let mut dup_out = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(lp, &desc_a, &raw mut dup_out),
            FfiResult::INVALID_ARGUMENT
        );

        // Mutually exclusive mode flags rejected by descriptor validation.
        let bad_modes = BamScheduleSetDesc::new(c"BadMode".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE | BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK);
        assert_eq!(
            host_configure_schedule_set(lp, &bad_modes, &raw mut dup_out),
            FfiResult::INVALID_ARGUMENT
        );

        // Unregistered custom schedule rejected with `NOT_FOUND`.
        let mut bad_sched = BamScheduleSetDesc::new(c"MissingSched".as_ptr());
        bad_sched.schedule = ScheduleId::custom(2048);
        assert_eq!(
            host_configure_schedule_set(lp, &bad_sched, &raw mut dup_out),
            FfiResult::NOT_FOUND
        );

        // `set_b` ordered after `set_a` (`set_a -> set_b`).
        let after_a = [set_a];
        let mut desc_b = BamScheduleSetDesc::new(c"Physics".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK | BAM_SCHEDULE_SET_FLAG_AFTER_WEAK);
        desc_b.after_sets = after_a.as_ptr();
        desc_b.after_count = 1;
        let mut set_b = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(lp, &desc_b, &raw mut set_b),
            FfiResult::OK
        );
        assert_eq!(set_b, BamSystemSet::from_raw(1));

        // Invalid / unknown handle in `before_sets` rejected.
        let invalid_arr = [BamSystemSet::INVALID];
        let mut bad_before = BamScheduleSetDesc::new(c"InvalidRef".as_ptr());
        bad_before.before_sets = invalid_arr.as_ptr();
        bad_before.before_count = 1;
        assert_eq!(
            host_configure_schedule_set(lp, &bad_before, &raw mut dup_out),
            FfiResult::INVALID_ARGUMENT
        );

        let missing_arr = [BamSystemSet::from_raw(77)];
        let mut missing_before = BamScheduleSetDesc::new(c"MissingRef".as_ptr());
        missing_before.before_sets = missing_arr.as_ptr();
        missing_before.before_count = 1;
        assert_eq!(
            host_configure_schedule_set(lp, &missing_before, &raw mut dup_out),
            FfiResult::NOT_FOUND
        );

        // Transitive cycle: `set_a -> set_b`, now add `set_c` with `after_sets: [set_b]` and
        // `before_sets: [set_a]` (`set_b -> set_c -> set_a` -> cycle!).
        let before_a = [set_a];
        let after_b = [set_b];
        let mut cyclic_desc = BamScheduleSetDesc::new(c"Cyclic".as_ptr());
        cyclic_desc.before_sets = before_a.as_ptr();
        cyclic_desc.before_count = 1;
        cyclic_desc.after_sets = after_b.as_ptr();
        cyclic_desc.after_count = 1;
        assert_eq!(
            host_configure_schedule_set(lp, &cyclic_desc, &raw mut dup_out),
            FfiResult::CYCLE_DETECTED
        );

        // Registered custom schedule succeeds and rejects cross-schedule edge to `Update` set.
        let mut custom_desc = BamScheduleSetDesc::new(c"CustomSet".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_PARALLEL);
        custom_desc.schedule = custom_sched;
        custom_desc.after_sets = after_a.as_ptr();
        custom_desc.after_count = 1;
        assert_eq!(
            host_configure_schedule_set(lp, &custom_desc, &raw mut dup_out),
            FfiResult::INVALID_ARGUMENT
        );

        custom_desc.after_sets = null();
        custom_desc.after_count = 0;
        let mut set_custom = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(lp, &custom_desc, &raw mut set_custom),
            FfiResult::OK
        );
        assert_eq!(set_custom, BamSystemSet::from_raw(2));
    }

    load_ctx.finish_init();

    let set_reg = world.resource::<ModScheduleSetRegistry>();
    assert_eq!(set_reg.len(), 3);
    let e0 = set_reg.get(BamSystemSet::from_raw(0)).unwrap();
    assert_eq!(
        e0.named_set,
        ModNamedSystemSet::with_generation("combat", "Input", 0)
    );
    assert_eq!(e0.mode, ScheduleSetMode::Sequence);
    assert_eq!(e0.schedule, ScheduleId::UPDATE);

    let e1 = set_reg.get(BamSystemSet::from_raw(1)).unwrap();
    assert_eq!(
        e1.named_set,
        ModNamedSystemSet::with_generation("combat", "Physics", 0)
    );
    assert_eq!(e1.mode, ScheduleSetMode::SequenceWeak);
    assert!(e1.after_weak);
    assert!(!e1.before_weak);
    assert_eq!(e1.after_sets, vec![BamSystemSet::from_raw(0)]);

    let e2 = set_reg.get(BamSystemSet::from_raw(2)).unwrap();
    assert_eq!(e2.mode, ScheduleSetMode::Parallel);
    assert_eq!(e2.schedule, custom_sched);

    // `set_edges` cycle detection leaves existing edges intact, and `unregister_mod` cleans up.
    let mut reg_mut = world.resource_mut::<ModScheduleSetRegistry>();
    assert_eq!(
        reg_mut.set_edges(
            BamSystemSet::from_raw(0),
            vec![],
            vec![BamSystemSet::from_raw(1)],
            false,
            false,
        ),
        Err(FfiResult::CYCLE_DETECTED)
    );
    assert_eq!(
        reg_mut.set_edges(
            BamSystemSet::from_raw(0),
            vec![BamSystemSet::from_raw(0)],
            vec![],
            false,
            false,
        ),
        Err(FfiResult::CYCLE_DETECTED)
    );
    reg_mut.unregister_mod(ModId(5));
    assert!(reg_mut.is_empty());
}

#[test]
fn mount_system_in_schedule_validates_and_supports_remount_across_schedules() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        Some(ModId(9)),
        Some(Arc::from("mover")),
    );
    let lp = load_ctx.as_ffi_ptr();

    let mut sys_h = BamSystem::INVALID;
    let mut upd_set = BamSystemSet::INVALID;
    let mut fixed_set = BamSystemSet::INVALID;

    // SAFETY: All descriptors and out-pointers are valid stack values.
    unsafe {
        let sys_desc = BamSystemDesc::new(c"step_a".as_ptr(), null(), 0, noop_cb, null_mut());
        assert_eq!(
            host_register_system_handle(lp, &sys_desc, &raw mut sys_h),
            FfiResult::OK
        );
        assert_eq!(sys_h, BamSystem::from_raw(1));

        let upd_desc =
            BamScheduleSetDesc::new(c"UpdSeq".as_ptr()).with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
        assert_eq!(
            host_configure_schedule_set(lp, &upd_desc, &raw mut upd_set),
            FfiResult::OK
        );

        let mut fixed_desc = BamScheduleSetDesc::new(c"FixSeq".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK);
        fixed_desc.schedule = ScheduleId::FIXED_UPDATE;
        assert_eq!(
            host_configure_schedule_set(lp, &fixed_desc, &raw mut fixed_set),
            FfiResult::OK
        );

        // `BamSystem(0)` and unknown set / mismatched schedule rejected cleanly.
        assert_eq!(
            host_mount_system_in_schedule(lp, BamSystem::from_raw(0), ScheduleId::UPDATE, upd_set),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(
                lp,
                sys_h,
                ScheduleId::UPDATE,
                BamSystemSet::from_raw(88)
            ),
            FfiResult::NOT_FOUND
        );
        assert_eq!(
            host_mount_system_in_schedule(lp, sys_h, ScheduleId::UPDATE, fixed_set),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(
                lp,
                sys_h,
                ScheduleId::custom(4096),
                BamSystemSet::INVALID
            ),
            FfiResult::NOT_FOUND
        );

        // Mount into `Update` + `upd_set`.
        assert_eq!(
            host_mount_system_in_schedule(lp, sys_h, ScheduleId::UPDATE, upd_set),
            FfiResult::OK
        );

        // Cross-schedule mount failure must NOT disturb existing `upd_set` mount!
        assert_eq!(
            host_mount_system_in_schedule(lp, sys_h, ScheduleId::FIXED_UPDATE, upd_set),
            FfiResult::INVALID_ARGUMENT
        );
    }

    assert_eq!(
        load_ctx.registered_systems[0].schedule(),
        ScheduleId::UPDATE
    );
    assert_eq!(
        load_ctx.registered_systems[0].in_sets(),
        &[String::from("mover::UpdSeq")]
    );

    // Now remount into `FIXED_UPDATE` + `fixed_set`: detaches cleanly from `upd_set`.
    // SAFETY: Active `LoadContext` and valid registered handles.
    unsafe {
        assert_eq!(
            host_mount_system_in_schedule(lp, sys_h, ScheduleId::FIXED_UPDATE, fixed_set),
            FfiResult::OK
        );
    }
    assert_eq!(
        load_ctx.registered_systems[0].schedule(),
        ScheduleId::FIXED_UPDATE
    );
    assert_eq!(
        load_ctx.registered_systems[0].in_sets(),
        &[String::from("mover::FixSeq")]
    );
    load_ctx.finish_init();

    let set_reg = world.resource::<ModScheduleSetRegistry>();
    assert!(set_reg.get(upd_set).unwrap().mounted_systems.is_empty());
    assert_eq!(
        set_reg.get(fixed_set).unwrap().mounted_systems,
        vec![ModNamedSystemSet::with_generation("mover", "step_a", 0)]
    );
}

#[test]
fn sequence_vs_sequence_weak_edges_and_concurrent_unrelated_siblings() {
    // Part 1: Verify `SEQUENCE` (`.chain()`) vs `SEQUENCE_WEAK` (`.chain_weak()`) on non-deferred
    // systems via BOTH `apply_mod_schedule_sets_to_schedule` (native Bevy `.chain()`/`.chain_weak()`)
    // AND `ModScheduleSetRewirePass` (`rewire_mod_schedule_sets_in_world`):
    // - `SEQUENCE` keeps strict edges even between non-conflicting systems.
    // - `SEQUENCE_WEAK` drops edges between non-conflicting systems while keeping transitive
    //   conflict edges (`write_1 -> read_disjoint -> write_2` materializes `write_1 -> write_2`
    //   and leaves `read_disjoint` unconstrained!).
    fn read_a(_: Res<NativeResA>) {}
    fn read_b(_: Res<NativeResB>) {}
    fn write_trace_1(mut t: ResMut<NativeTrace>) {
        t.0.push(1);
    }
    fn write_trace_2(mut t: ResMut<NativeTrace>) {
        t.0.push(2);
    }

    for use_rewire_pass in [false, true] {
        let mut world = World::new();
        world.insert_resource(NativeResA);
        world.insert_resource(NativeResB);
        world.insert_resource(NativeTrace::default());

        let mut set_reg = ModScheduleSetRegistry::new();
        let seq_h = set_reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("native"),
                set_name: Arc::from("StrictSeq"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Sequence,
                before_sets: vec![],
                after_sets: vec![],
                before_weak: false,
                after_weak: false,
            })
            .unwrap();
        let s1 = ModNamedSystemSet::new("native", "r1");
        let s2 = ModNamedSystemSet::new("native", "r2");
        set_reg
            .mount_system(seq_h, ScheduleId::UPDATE, s1.clone())
            .unwrap();
        set_reg
            .mount_system(seq_h, ScheduleId::UPDATE, s2.clone())
            .unwrap();

        let weak_h = set_reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("native"),
                set_name: Arc::from("WeakSeq"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::SequenceWeak,
                before_sets: vec![],
                after_sets: vec![],
                before_weak: false,
                after_weak: false,
            })
            .unwrap();
        let w1 = ModNamedSystemSet::new("native", "w1");
        let w_mid = ModNamedSystemSet::new("native", "w_mid");
        let w2 = ModNamedSystemSet::new("native", "w2");
        set_reg
            .mount_system(weak_h, ScheduleId::UPDATE, w1.clone())
            .unwrap();
        set_reg
            .mount_system(weak_h, ScheduleId::UPDATE, w_mid.clone())
            .unwrap();
        set_reg
            .mount_system(weak_h, ScheduleId::UPDATE, w2.clone())
            .unwrap();

        let weak_nc_h = set_reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("native"),
                set_name: Arc::from("WeakNoConflict"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::SequenceWeak,
                before_sets: vec![],
                after_sets: vec![],
                before_weak: false,
                after_weak: false,
            })
            .unwrap();
        let nc1 = ModNamedSystemSet::new("native", "nc1");
        let nc2 = ModNamedSystemSet::new("native", "nc2");
        set_reg
            .mount_system(weak_nc_h, ScheduleId::UPDATE, nc1.clone())
            .unwrap();
        set_reg
            .mount_system(weak_nc_h, ScheduleId::UPDATE, nc2.clone())
            .unwrap();

        let strict_container = set_reg.get(seq_h).unwrap().named_set.clone();
        let weak_container = set_reg.get(weak_h).unwrap().named_set.clone();
        let weak_nc_container = set_reg.get(weak_nc_h).unwrap().named_set.clone();

        world.insert_resource(set_reg.clone());

        let mut schedule = Schedule::new(Update);
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            ..Default::default()
        });
        schedule.add_systems((
            read_a.in_set(s1).in_set(strict_container.clone()),
            read_b.in_set(s2).in_set(strict_container),
            write_trace_1.in_set(w1).in_set(weak_container.clone()),
            read_a.in_set(w_mid).in_set(weak_container.clone()),
            write_trace_2.in_set(w2).in_set(weak_container),
            read_a.in_set(nc1).in_set(weak_nc_container.clone()),
            read_b.in_set(nc2).in_set(weak_nc_container),
        ));

        if use_rewire_pass {
            let mut schedules = Schedules::new();
            schedules.insert(schedule);
            world.insert_resource(schedules);
            rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::UPDATE)
                .expect("rewire pass build succeeds under LogLevel::Error");
            world.run_schedule(Update);
        } else {
            apply_mod_schedule_sets_to_schedule(&set_reg, ScheduleId::UPDATE, &mut schedule);
            schedule
                .initialize(&mut world)
                .expect("native chain/chain_weak build succeeds under LogLevel::Error");
            schedule.run(&mut world);
        }

        // Transitive conflict (`write_trace_1 -> w_mid -> write_trace_2`) executed strictly `[1, 2]`
        // with zero ambiguities under `LogLevel::Error`!
        assert_eq!(world.resource::<NativeTrace>().0, vec![1, 2]);
    }

    // Part 1b: Directly inspect flattened edge counts via a probe `ScheduleBuildPass` to prove:
    // - `SEQUENCE` keeps 1 strict edge between non-conflicting `[read_a, read_b]`
    // - `SEQUENCE_WEAK` drops edges between non-conflicting `[read_a, read_b]` (0 edges)
    // - `SEQUENCE_WEAK` over `[write_1, read_a, write_2]` keeps ONLY the 1 transitive conflict edge
    //   `write_1 -> write_2` (leaving `read_a` unconstrained!)
    type TestSystemKeyIndexSet = <<bevy_ecs::schedule::graph::DagGroups<
        SystemSetKey,
        SystemKey,
    > as core::ops::Deref>::Target as core::ops::Index<
        &'static SystemSetKey,
    >>::Output;

    #[derive(Debug)]
    struct EdgeCountProbePass(Arc<AtomicUsize>);

    impl ScheduleBuildPass for EdgeCountProbePass {
        type EdgeOptions = ();

        fn add_dependency(&mut self, _: NodeId, _: NodeId, _: Option<&Self::EdgeOptions>) {}

        fn collapse_set(
            &mut self,
            _: SystemSetKey,
            _: &TestSystemKeyIndexSet,
            _: &bevy_ecs::schedule::graph::DiGraph<NodeId>,
        ) -> impl Iterator<Item = (NodeId, NodeId)> {
            core::iter::empty()
        }

        fn build(
            &mut self,
            _: &mut World,
            _: &mut ScheduleGraph,
            dependency_flattened: FlattenedDependencies<'_>,
        ) -> Result<(), ScheduleBuildError> {
            self.0
                .store(dependency_flattened.graph().edge_count(), Ordering::SeqCst);
            Ok(())
        }
    }

    for (mode, expected_edges) in [
        (ScheduleSetMode::Sequence, 1usize),
        (ScheduleSetMode::SequenceWeak, 0usize),
        (ScheduleSetMode::Parallel, 0usize),
    ] {
        let mut world = World::new();
        world.insert_resource(NativeResA);
        world.insert_resource(NativeResB);
        let mut set_reg = ModScheduleSetRegistry::new();
        let h = set_reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("probe"),
                set_name: Arc::from("Pair"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode,
                before_sets: vec![],
                after_sets: vec![],
                before_weak: false,
                after_weak: false,
            })
            .unwrap();
        let s1 = ModNamedSystemSet::new("probe", "a");
        let s2 = ModNamedSystemSet::new("probe", "b");
        set_reg
            .mount_system(h, ScheduleId::UPDATE, s1.clone())
            .unwrap();
        set_reg
            .mount_system(h, ScheduleId::UPDATE, s2.clone())
            .unwrap();
        let container = set_reg.get(h).unwrap().named_set.clone();
        world.insert_resource(set_reg);

        let observed_edges = Arc::new(AtomicUsize::new(usize::MAX));
        let mut schedule = Schedule::new(Update);
        install_mod_schedule_set_pass(&mut schedule, ScheduleId::UPDATE);
        schedule.add_build_pass(EdgeCountProbePass(Arc::clone(&observed_edges)));
        schedule.add_systems((
            read_a.in_set(s1).in_set(container.clone()),
            read_b.in_set(s2).in_set(container),
        ));
        schedule.initialize(&mut world).unwrap();
        assert_eq!(
            observed_edges.load(Ordering::SeqCst),
            expected_edges,
            "mode {mode:?} non-conflicting pair flattened edge count mismatch"
        );
    }

    // Part 2: Guest systems in `SEQUENCE`, `SEQUENCE_WEAK`, and an unrelated `PARALLEL` sibling set
    // running on Bevy's `MultiThreadedExecutor` with `LogLevel::Error` across multiple
    // `ScheduleBuildSettings` configurations (`auto_insert_apply_deferred` true & false)!
    for auto_sync in [true, false] {
        let (mut world, reg, trace_h, sib_a_h, sib_b_h) = setup_world_with_resources();
        let mut load_ctx = LoadContext::with_manifest_name(
            reg,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(1)),
            Some(Arc::from("sim")),
        );
        let lp = load_ctx.as_ffi_ptr();

        let trace_term = [BamTermDesc::resource_write(trace_h)];
        let trace_q = [bevy_abi_mod_sys::BamQueryDesc::new(
            trace_term.as_ptr(),
            trace_term.len(),
        )];
        let sib_a_term = [BamTermDesc::resource_write(sib_a_h)];
        let sib_a_q = [bevy_abi_mod_sys::BamQueryDesc::new(
            sib_a_term.as_ptr(),
            sib_a_term.len(),
        )];
        let sib_b_term = [BamTermDesc::resource_write(sib_b_h)];
        let sib_b_q = [bevy_abi_mod_sys::BamQueryDesc::new(
            sib_b_term.as_ptr(),
            sib_b_term.len(),
        )];

        // SAFETY: All query descriptors, set descriptors, and out-handles live on the stack.
        unsafe {
            let mut set_seq = BamSystemSet::INVALID;
            let mut set_weak = BamSystemSet::INVALID;
            let mut set_sibling = BamSystemSet::INVALID;

            let seq_desc = BamScheduleSetDesc::new(c"StrictPhase".as_ptr())
                .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
            assert_eq!(
                host_configure_schedule_set(lp, &seq_desc, &raw mut set_seq),
                FfiResult::OK
            );

            let after_seq = [set_seq];
            let mut weak_desc = BamScheduleSetDesc::new(c"WeakPhase".as_ptr())
                .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK | BAM_SCHEDULE_SET_FLAG_AFTER_WEAK);
            weak_desc.after_sets = after_seq.as_ptr();
            weak_desc.after_count = 1;
            assert_eq!(
                host_configure_schedule_set(lp, &weak_desc, &raw mut set_weak),
                FfiResult::OK
            );

            let sib_desc = BamScheduleSetDesc::new(c"ParallelSiblings".as_ptr())
                .with_flags(BAM_SCHEDULE_SET_FLAG_PARALLEL);
            assert_eq!(
                host_configure_schedule_set(lp, &sib_desc, &raw mut set_sibling),
                FfiResult::OK
            );

            let mut s_handles = [BamSystem::INVALID; 6];
            let defs: [(
                &core::ffi::CStr,
                *const bevy_abi_mod_sys::BamQueryDesc,
                bevy_abi_mod_sys::BamSystemFn,
                *mut c_void,
                BamSystemSet,
            ); 6] = [
                (
                    c"seq_a",
                    trace_q.as_ptr(),
                    push_trace_cb,
                    encode_step_ud(trace_h, 1),
                    set_seq,
                ),
                (
                    c"seq_b",
                    trace_q.as_ptr(),
                    push_trace_cb,
                    encode_step_ud(trace_h, 2),
                    set_seq,
                ),
                (
                    c"weak_a",
                    trace_q.as_ptr(),
                    push_trace_cb,
                    encode_step_ud(trace_h, 3),
                    set_weak,
                ),
                (
                    c"weak_b",
                    trace_q.as_ptr(),
                    push_trace_cb,
                    encode_step_ud(trace_h, 4),
                    set_weak,
                ),
                (
                    c"sib_a",
                    sib_a_q.as_ptr(),
                    bump_u32_res_cb,
                    sib_a_h.0 as usize as *mut c_void,
                    set_sibling,
                ),
                (
                    c"sib_b",
                    sib_b_q.as_ptr(),
                    bump_u32_res_cb,
                    sib_b_h.0 as usize as *mut c_void,
                    set_sibling,
                ),
            ];
            for (idx, &(name, q_ptr, cb, ud, target_set)) in defs.iter().enumerate() {
                let d = BamSystemDesc::new(name.as_ptr(), q_ptr, 1, cb, ud);
                assert_eq!(
                    host_register_system_handle(lp, &d, &raw mut s_handles[idx]),
                    FfiResult::OK
                );
                assert_eq!(
                    host_mount_system_in_schedule(
                        lp,
                        s_handles[idx],
                        ScheduleId::UPDATE,
                        target_set
                    ),
                    FfiResult::OK
                );
            }
        }

        let systems = core::mem::take(&mut load_ctx.registered_systems);
        load_ctx.finish_init();

        let mut schedules = Schedules::new();
        let mut sched = Schedule::new(Update);
        sched.set_executor(MultiThreadedExecutor::new());
        sched.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            hierarchy_detection: LogLevel::Error,
            auto_insert_apply_deferred: auto_sync,
            ..Default::default()
        });
        schedules.insert(sched);
        world.insert_resource(schedules);

        mount_mod_systems_in_world(&mut world, "sim", 0, systems)
            .unwrap_or_else(|e| panic!("auto_sync={auto_sync} build failed: {e:?}"));

        // Verify sibling systems are NOT statically ordered against `StrictPhase`/`WeakPhase`
        // or against each other in the schedule graph.
        {
            let sched_ref = world.resource::<Schedules>().get(Update).unwrap();
            assert_eq!(
                sched_ref.graph().dependency().graph().edge_count(),
                0,
                "dynamic edges live in ModScheduleSetRewirePass, not stale static edges"
            );
        }

        world.run_schedule(Update);
        assert_eq!(
            world.resource::<TraceLog>().as_slice(),
            &[1, 2, 3, 4],
            "auto_sync={auto_sync} must order StrictPhase (1->2) before WeakPhase (3->4)"
        );
        assert_eq!(world.resource::<SiblingA>().hits, 1);
        assert_eq!(world.resource::<SiblingB>().hits, 1);
    }
}

#[test]
fn before_sets_and_after_sets_strict_and_weak_order_deterministically_across_permutations() {
    // Exhaustively test all 24 insertion permutations of the 4-stage pipeline across BOTH native
    // `apply_mod_schedule_sets_to_schedule` (`.before()` / `.after()` / `.before_weak()` /
    // `.after_weak()`) and `ModScheduleSetRewirePass` under strict `LogLevel::Error` settings.
    let perms: [[usize; 4]; 24] = [
        [0, 1, 2, 3],
        [0, 1, 3, 2],
        [0, 2, 1, 3],
        [0, 2, 3, 1],
        [0, 3, 1, 2],
        [0, 3, 2, 1],
        [1, 0, 2, 3],
        [1, 0, 3, 2],
        [1, 2, 0, 3],
        [1, 2, 3, 0],
        [1, 3, 0, 2],
        [1, 3, 2, 0],
        [2, 0, 1, 3],
        [2, 0, 3, 1],
        [2, 1, 0, 3],
        [2, 1, 3, 0],
        [2, 3, 0, 1],
        [2, 3, 1, 0],
        [3, 0, 1, 2],
        [3, 0, 2, 1],
        [3, 1, 0, 2],
        [3, 1, 2, 0],
        [3, 2, 0, 1],
        [3, 2, 1, 0],
    ];

    for (perm_idx, perm) in perms.iter().enumerate() {
        let (mut world, reg, trace_h, _, _) = setup_world_with_resources();
        let mut load_ctx = LoadContext::with_manifest_name(
            reg,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(2)),
            Some(Arc::from("pipeline")),
        );
        let lp = load_ctx.as_ffi_ptr();

        let trace_term = [BamTermDesc::resource_write(trace_h)];
        let trace_q = [bevy_abi_mod_sys::BamQueryDesc::new(
            trace_term.as_ptr(),
            trace_term.len(),
        )];

        // Build a 4-stage pipeline mixing strict `after_sets`, weak `BEFORE_WEAK`, weak `AFTER_WEAK`:
        // Stage1 -> Stage2 -> Stage3 -> Stage4!
        // SAFETY: Stack descriptors and out-handles remain valid across every FFI call.
        unsafe {
            let mut s1 = BamSystemSet::INVALID;
            let mut s2 = BamSystemSet::INVALID;
            let mut s4 = BamSystemSet::INVALID;
            let mut s3 = BamSystemSet::INVALID;

            // Stage 1
            let d1 = BamScheduleSetDesc::new(c"Stage1".as_ptr());
            assert_eq!(
                host_configure_schedule_set(lp, &d1, &raw mut s1),
                FfiResult::OK
            );

            // Stage 2: strict `after_sets: [Stage1]`
            let after_s1 = [s1];
            let mut d2 = BamScheduleSetDesc::new(c"Stage2".as_ptr());
            d2.after_sets = after_s1.as_ptr();
            d2.after_count = 1;
            assert_eq!(
                host_configure_schedule_set(lp, &d2, &raw mut s2),
                FfiResult::OK
            );

            // Stage 4: weak `after_sets: [Stage2]`
            let after_s2 = [s2];
            let mut d4 = BamScheduleSetDesc::new(c"Stage4".as_ptr())
                .with_flags(BAM_SCHEDULE_SET_FLAG_AFTER_WEAK);
            d4.after_sets = after_s2.as_ptr();
            d4.after_count = 1;
            assert_eq!(
                host_configure_schedule_set(lp, &d4, &raw mut s4),
                FfiResult::OK
            );

            // Stage 3: `after_sets: [Stage2]` (weak) AND `before_sets: [Stage4]` (weak)
            let before_s4 = [s4];
            let mut d3 = BamScheduleSetDesc::new(c"Stage3".as_ptr())
                .with_flags(BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK | BAM_SCHEDULE_SET_FLAG_AFTER_WEAK);
            d3.after_sets = after_s2.as_ptr();
            d3.after_count = 1;
            d3.before_sets = before_s4.as_ptr();
            d3.before_count = 1;
            assert_eq!(
                host_configure_schedule_set(lp, &d3, &raw mut s3),
                FfiResult::OK
            );

            let stages = [
                (c"sys_1", 1u32, s1),
                (c"sys_2", 2u32, s2),
                (c"sys_3", 3u32, s3),
                (c"sys_4", 4u32, s4),
            ];
            for &idx in perm {
                let (name, step, target_set) = stages[idx];
                let mut h = BamSystem::INVALID;
                let sd = BamSystemDesc::new(
                    name.as_ptr(),
                    trace_q.as_ptr(),
                    1,
                    push_trace_cb,
                    encode_step_ud(trace_h, step),
                );
                assert_eq!(
                    host_register_system_handle(lp, &sd, &raw mut h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_mount_system_in_schedule(lp, h, ScheduleId::UPDATE, target_set),
                    FfiResult::OK
                );
            }
        }

        let systems = core::mem::take(&mut load_ctx.registered_systems);
        load_ctx.finish_init();

        let auto_sync = (perm_idx / 2) % 2 == 0;
        if perm_idx % 2 == 0 {
            let set_reg = world.resource::<ModScheduleSetRegistry>().clone();
            let mut schedule = Schedule::new(Update);
            schedule.set_build_settings(ScheduleBuildSettings {
                ambiguity_detection: LogLevel::Error,
                hierarchy_detection: LogLevel::Error,
                auto_insert_apply_deferred: auto_sync,
                ..Default::default()
            });
            for sys in systems {
                schedule.add_systems(configure_guest_system_for_schedule(
                    &world, "pipeline", 0, sys,
                ));
            }
            apply_mod_schedule_sets_to_schedule(&set_reg, ScheduleId::UPDATE, &mut schedule);
            schedule
                .initialize(&mut world)
                .unwrap_or_else(|e| panic!("perm {perm:?} native schedule init failed: {e:?}"));
            schedule.run(&mut world);
        } else {
            let mut schedules = Schedules::new();
            let mut schedule = Schedule::new(Update);
            schedule.set_build_settings(ScheduleBuildSettings {
                ambiguity_detection: LogLevel::Error,
                hierarchy_detection: LogLevel::Error,
                auto_insert_apply_deferred: auto_sync,
                ..Default::default()
            });
            schedules.insert(schedule);
            world.insert_resource(schedules);
            mount_mod_systems_in_world(&mut world, "pipeline", 0, systems)
                .unwrap_or_else(|e| panic!("perm {perm:?} rewire pass init failed: {e:?}"));
            world.run_schedule(Update);
        }

        assert_eq!(
            world.resource::<TraceLog>().as_slice(),
            &[1, 2, 3, 4],
            "perm {perm:?} produced non-deterministic set order"
        );
    }
}

#[test]
fn schedule_only_hot_rewire_under_400us_with_zero_guest_system_reinit() {
    let (mut world, mut reg, trace_h, _, _) = setup_world_with_resources();
    let tag_id = world.register_component::<TrackedTag>();
    let tag_h = reg
        .register_component::<TrackedTag>(tag_id, ComponentOptions::default())
        .unwrap();
    world.insert_resource(reg.clone());
    world.spawn(TrackedTag { value: 42 });

    let changed_matches = Arc::new(AtomicUsize::new(999));

    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        Some(ModId(3)),
        Some(Arc::from("hot")),
    );
    let lp = load_ctx.as_ffi_ptr();

    let trace_term = [BamTermDesc::resource_write(trace_h)];
    let trace_q = [bevy_abi_mod_sys::BamQueryDesc::new(
        trace_term.as_ptr(),
        trace_term.len(),
    )];
    let changed_terms = [
        BamTermDesc::new(tag_h, AccessMode::READ),
        BamTermDesc::new(tag_h, AccessMode::CHANGED),
    ];
    let changed_q = [bevy_abi_mod_sys::BamQueryDesc::new(
        changed_terms.as_ptr(),
        changed_terms.len(),
    )];

    let mut set_alpha = BamSystemSet::INVALID;
    let mut set_beta = BamSystemSet::INVALID;

    // SAFETY: All stack descriptors and output handles are valid for every FFI call.
    unsafe {
        let d_alpha =
            BamScheduleSetDesc::new(c"Alpha".as_ptr()).with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
        assert_eq!(
            host_configure_schedule_set(lp, &d_alpha, &raw mut set_alpha),
            FfiResult::OK
        );

        let after_alpha = [set_alpha];
        let mut d_beta =
            BamScheduleSetDesc::new(c"Beta".as_ptr()).with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
        d_beta.after_sets = after_alpha.as_ptr();
        d_beta.after_count = 1;
        assert_eq!(
            host_configure_schedule_set(lp, &d_beta, &raw mut set_beta),
            FfiResult::OK
        );

        for (name, step, target) in [
            (c"s1", 1u32, set_alpha),
            (c"s2", 2u32, set_alpha),
            (c"s3", 3u32, set_beta),
            (c"s4", 4u32, set_beta),
        ] {
            let mut h = BamSystem::INVALID;
            let sd = BamSystemDesc::new(
                name.as_ptr(),
                trace_q.as_ptr(),
                1,
                push_trace_cb,
                encode_step_ud(trace_h, step),
            );
            assert_eq!(
                host_register_system_handle(lp, &sd, &raw mut h),
                FfiResult::OK
            );
            assert_eq!(
                host_mount_system_in_schedule(lp, h, ScheduleId::UPDATE, target),
                FfiResult::OK
            );
        }

        // Also mount a `Changed<TrackedTag>` observer system in `set_alpha`: if hot-rewiring
        // ever re-initialized `GuestSystem`, `last_run` would reset and `Changed` would falsely
        // re-fire on the unmodified entity!
        let mut changed_h = BamSystem::INVALID;
        let cd = BamSystemDesc::new(
            c"changed_probe".as_ptr(),
            changed_q.as_ptr(),
            1,
            count_changed_entities_cb,
            Arc::as_ptr(&changed_matches) as *mut c_void,
        );
        assert_eq!(
            host_register_system_handle(lp, &cd, &raw mut changed_h),
            FfiResult::OK
        );
        assert_eq!(
            host_mount_system_in_schedule(lp, changed_h, ScheduleId::UPDATE, set_alpha),
            FfiResult::OK
        );
    }

    let systems = core::mem::take(&mut load_ctx.registered_systems);
    load_ctx.finish_init();

    let mut schedules = Schedules::new();
    let mut sched = Schedule::new(Update);
    sched.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedules.insert(sched);
    world.insert_resource(schedules);

    mount_mod_systems_in_world(&mut world, "hot", 0, systems).expect("initial mount succeeds");
    world.run_schedule(Update);
    assert_eq!(world.resource::<TraceLog>().as_slice(), &[1, 2, 3, 4]);
    // First run sees the newly spawned `TrackedTag` entity once.
    assert_eq!(changed_matches.load(Ordering::SeqCst), 1);

    // Snapshot every `(SystemKey, last_run)` BEFORE hot-rewiring!
    let snapshot_before: Vec<(
        bevy_ecs::schedule::SystemKey,
        bevy_ecs::change_detection::Tick,
    )> = {
        let sched_ref = world.resource::<Schedules>().get(Update).unwrap();
        let mut rows: Vec<_> = sched_ref
            .systems()
            .unwrap()
            .filter(|(_, s)| s.system_type() == core::any::TypeId::of::<GuestSystem>())
            .map(|(k, s)| (k, s.get_last_run()))
            .collect();
        rows.sort_by_key(|&(k, _)| k);
        rows
    };
    assert_eq!(snapshot_before.len(), 5);

    // Hot-rewire topology in `ModScheduleSetRegistry`:
    // 1. Flip `Beta` BEFORE `Alpha` (`Beta -> Alpha`)!
    // 2. Switch `Alpha` to `SEQUENCE_WEAK` and reverse its members (`s2 -> s1 -> changed_probe`)!
    // 3. Reverse `Beta`'s members (`s4 -> s3`)!
    {
        let mut set_reg = world.resource_mut::<ModScheduleSetRegistry>();
        set_reg
            .set_edges(set_beta, vec![set_alpha], vec![], false, false)
            .unwrap();
        set_reg
            .set_edges(set_alpha, vec![], vec![], false, false)
            .unwrap();
        set_reg
            .set_mode(set_alpha, ScheduleSetMode::SequenceWeak)
            .unwrap();
        set_reg
            .reorder_mounted_systems(
                set_alpha,
                vec![
                    ModNamedSystemSet::new("hot", "s2"),
                    ModNamedSystemSet::new("hot", "s1"),
                    ModNamedSystemSet::new("hot", "changed_probe"),
                ],
            )
            .unwrap();
        set_reg
            .reorder_mounted_systems(
                set_beta,
                vec![
                    ModNamedSystemSet::new("hot", "s4"),
                    ModNamedSystemSet::new("hot", "s3"),
                ],
            )
            .unwrap();
    }

    // Warm one rewire to prime allocator/caches, then measure steady-state schedule-only rewire
    // across 20 runs and assert median latency < 0.4 ms (400 us)!
    rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::UPDATE)
        .expect("hot rewire build succeeds");

    // Verify ZERO `GuestSystem` kernels were re-registered or re-initialized!
    let snapshot_after: Vec<(
        bevy_ecs::schedule::SystemKey,
        bevy_ecs::change_detection::Tick,
    )> = {
        let sched_ref = world.resource::<Schedules>().get(Update).unwrap();
        let mut rows: Vec<_> = sched_ref
            .systems()
            .unwrap()
            .filter(|(_, s)| s.system_type() == core::any::TypeId::of::<GuestSystem>())
            .map(|(k, s)| (k, s.get_last_run()))
            .collect();
        rows.sort_by_key(|&(k, _)| k);
        rows
    };
    assert_eq!(
        snapshot_before, snapshot_after,
        "hot-rewire must preserve identical SystemKeys and last_run ticks (zero GuestSystem re-init)"
    );

    // Execute post-rewire schedule: order must now be `[4, 3, 2, 1]` and `Changed<TrackedTag>`
    // must match 0 entities (confirming `GuestSystem` change-detection state was never reset!).
    world.resource_mut::<TraceLog>().clear();
    world.run_schedule(Update);
    assert_eq!(world.resource::<TraceLog>().as_slice(), &[4, 3, 2, 1]);
    assert_eq!(
        changed_matches.load(Ordering::SeqCst),
        0,
        "GuestSystem::initialize must NOT have re-run during schedule-only rewire"
    );

    // Measure 20 consecutive hot-rewires flipping `Beta <-> Alpha` and verify median < 0.4 ms!
    let mut samples = Vec::with_capacity(20);
    for i in 0..20usize {
        {
            let mut set_reg = world.resource_mut::<ModScheduleSetRegistry>();
            if i % 2 == 0 {
                set_reg
                    .set_edges(set_beta, vec![], vec![set_alpha], false, false)
                    .unwrap();
            } else {
                set_reg
                    .set_edges(set_beta, vec![set_alpha], vec![], false, false)
                    .unwrap();
            }
        }
        let start = Instant::now();
        rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::UPDATE)
            .expect("repeated hot rewire succeeds");
        samples.push(start.elapsed());
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    eprintln!(
        "schedule-only hot-rewire median={median:?}, min={:?}, max={:?} (debug={})",
        samples[0],
        samples[samples.len() - 1],
        cfg!(debug_assertions)
    );
    // Strict `< 0.4 ms` (`400 us`) budget in release (`opt-level = 3`), with a generous debug
    // envelope (`< 4 ms`) because `bevy_ecs`'s `ScheduleGraph::build_schedule` runs at
    // `opt-level = 0` under debug `cargo test`.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(4)
    } else {
        Duration::from_micros(400)
    };
    assert!(
        median < budget,
        "schedule-only hot-rewire median {median:?} exceeded {budget:?} target (samples={samples:?})"
    );
}

// ============================================================================
// Bevy 0.20 FallbackErrorHandler Bridge & host_report_guest_fault Tests
// ============================================================================

#[derive(Debug, Clone)]
struct CapturedFallback {
    ctx_name: String,
    ctx_kind: String,
    severity: Severity,
    display: String,
    mod_err: Option<ModSystemError>,
}

static FALLBACK_LOCK: Mutex<()> = Mutex::new(());
static CAPTURED_FALLBACKS: Mutex<Vec<CapturedFallback>> = Mutex::new(Vec::new());

fn record_fallback_error(err: BevyError, ctx: ErrorContext) {
    let mod_err = err.downcast_ref::<ModSystemError>().cloned();
    CAPTURED_FALLBACKS.lock().unwrap().push(CapturedFallback {
        ctx_name: ctx.name().to_string(),
        ctx_kind: ctx.kind().to_string(),
        severity: err.severity(),
        display: err.to_string(),
        mod_err,
    });
}

struct FaultScenario {
    calls: AtomicUsize,
    report_msg: Option<&'static str>,
    return_code: FfiResult,
    spawn_before_fault: bool,
}

unsafe extern "C" fn fault_scenario_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Test keeps `FaultScenario` alive across schedule invocations.
    let scenario = unsafe { &*user_data.cast::<FaultScenario>() };
    scenario.calls.fetch_add(1, Ordering::SeqCst);

    if scenario.spawn_before_fault {
        let mut cmds: *mut BamCommands = null_mut();
        // SAFETY: Active `HostSystemCtx` and stack out-pointer.
        let rc = unsafe { host_sys_commands(sys_ctx, &raw mut cmds) };
        if rc.is_err() {
            return rc;
        }
        let mut ent = FfiEntity::NULL;
        // SAFETY: Active `HostCommands` and stack out-pointer.
        let sc = unsafe { host_cmd_spawn_empty(cmds, &raw mut ent) };
        if sc.is_err() {
            return sc;
        }
    }

    if let Some(msg) = scenario.report_msg {
        // SAFETY: Active `HostSystemCtx` returns valid `BamHostCtx`.
        let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
        // SAFETY: Valid `host_ctx` and UTF-8 `StrView`.
        let _ = unsafe { host_report_guest_fault(host_ctx, StrView::from_str(msg)) };
    }

    scenario.return_code
}

#[test]
fn fallback_error_handler_bridge_handles_panics_errors_and_report_guest_fault() {
    let _guard = FALLBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    CAPTURED_FALLBACKS.lock().unwrap().clear();

    // 1. `ReportToBevy` + `host_report_guest_fault` + `FfiResult::PANIC_CAUGHT`:
    //    - Sets atomic quarantine tombstone (`ModQuarantine`)
    //    - Discards uncommitted commands
    //    - Wires structured `ModSystemError` + `.with_context(...)` + `Severity::Error` into
    //      Bevy 0.20's `FallbackErrorHandler` without aborting!
    {
        let mut world = World::new();
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());
        world.init_resource::<Messages<ModSystemFaulted>>();
        world.insert_resource(FallbackErrorHandler(record_fallback_error));

        let baseline_entities = world.entities().len();
        let scenario = FaultScenario {
            calls: AtomicUsize::new(0),
            report_msg: Some("panic: division by zero in combat tick"),
            return_code: FfiResult::PANIC_CAUGHT,
            spawn_before_fault: true,
        };
        let reg = world.resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "combat_panic_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            reg,
        )
        .with_mod_id(ModId(11))
        .with_mod_namespace("combat_mod")
        .with_mod_generation(3)
        .with_error_policy(ModErrorPolicy::ReportToBevy);

        world.resource_mut::<ModQuarantine>().register_system(
            ModId(11),
            sys.system_name_arc(),
            sys.enabled_flag(),
        );

        let mut schedule = Schedule::new(Update);
        schedule.add_systems(sys);

        schedule.run(&mut world);
        // Second run must skip quarantined system!
        schedule.run(&mut world);

        assert_eq!(scenario.calls.load(Ordering::SeqCst), 1);
        assert_eq!(world.entities().len(), baseline_entities);
        assert!(
            world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(11), "combat_panic_sys")
        );

        // Verify `FallbackErrorHandler` received exact structured context.
        let captured = CAPTURED_FALLBACKS.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].ctx_kind, "system");
        assert!(
            captured[0].ctx_name == "combat_panic_sys"
                || captured[0].ctx_name.contains("debug feature"),
            "unexpected ErrorContext::name: {}",
            captured[0].ctx_name
        );
        assert_eq!(captured[0].severity, Severity::Error);
        assert!(captured[0].display.contains("namespace=combat_mod"));
        assert!(captured[0].display.contains("generation=3"));
        assert!(captured[0].display.contains("combat_panic_sys"));
        assert!(
            captured[0]
                .display
                .contains("panic: division by zero in combat tick")
        );
        let mod_err = captured[0].mod_err.as_ref().expect("ModSystemError");
        assert_eq!(mod_err.mod_id, Some(ModId(11)));
        assert_eq!(mod_err.mod_namespace.as_deref(), Some("combat_mod"));
        assert_eq!(mod_err.generation, 3);
        assert_eq!(mod_err.system_name, "combat_panic_sys");
        assert_eq!(mod_err.status, FfiResult::PANIC_CAUGHT);
        assert!(mod_err.panicked);
        assert_eq!(
            mod_err.last_error.as_deref(),
            Some("panic: division by zero in combat tick")
        );
        drop(captured);

        // Verify `Messages<ModSystemFaulted>` mirrors the exact structured fault fields.
        let msgs = world.resource::<Messages<ModSystemFaulted>>();
        let mut cursor = msgs.get_cursor();
        let events: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].mod_namespace.as_deref(), Some("combat_mod"));
        assert_eq!(events[0].generation, 3);
        assert_eq!(events[0].status, FfiResult::PANIC_CAUGHT);
        assert!(events[0].panicked);
        assert_eq!(
            events[0].last_error.as_deref(),
            Some("panic: division by zero in combat tick")
        );
    }

    // 2. `host_report_guest_fault` called while returning `FfiResult::OK`:
    //    Still surfaces in `last_error`, `ModSystemFaulted`, `ModQuarantine`, and `FallbackErrorHandler`!
    CAPTURED_FALLBACKS.lock().unwrap().clear();
    {
        let mut world = World::new();
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());
        world.init_resource::<Messages<ModSystemFaulted>>();
        world.insert_resource(FallbackErrorHandler(record_fallback_error));

        let scenario = FaultScenario {
            calls: AtomicUsize::new(0),
            report_msg: Some("invariant broken: negative health"),
            return_code: FfiResult::OK,
            spawn_before_fault: true,
        };
        let baseline_entities = world.entities().len();
        let reg = world.resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "report_only_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            reg,
        )
        .with_mod_id(ModId(12))
        .with_mod_namespace("ai_mod")
        .with_mod_generation(2)
        .with_error_policy(ModErrorPolicy::ReportToBevy);
        world.resource_mut::<ModQuarantine>().register_system(
            ModId(12),
            sys.system_name_arc(),
            sys.enabled_flag(),
        );

        let mut schedule = Schedule::new(Update);
        schedule.add_systems(sys);
        schedule.run(&mut world);

        assert_eq!(world.entities().len(), baseline_entities);
        assert!(
            world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(12), "report_only_sys")
        );
        let captured = CAPTURED_FALLBACKS.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let mod_err = captured[0].mod_err.as_ref().expect("ModSystemError");
        assert_eq!(mod_err.status, FfiResult::INTERNAL_ERROR);
        assert!(!mod_err.panicked);
        assert_eq!(mod_err.mod_namespace.as_deref(), Some("ai_mod"));
        assert_eq!(mod_err.generation, 2);
        assert_eq!(
            mod_err.last_error.as_deref(),
            Some("invariant broken: negative health")
        );
        drop(captured);
    }

    // 3. `ReportToBevy` under Bevy 0.20's DEFAULT `FallbackErrorHandler` (`match_severity`)
    //    must NOT panic or abort the process while quarantining the faulting guest system!
    {
        let mut world = World::new();
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());
        world.init_resource::<Messages<ModSystemFaulted>>();
        // Deliberately do NOT override `FallbackErrorHandler`: Bevy's default `match_severity` runs!

        let scenario = FaultScenario {
            calls: AtomicUsize::new(0),
            report_msg: Some("guest panic must not abort host"),
            return_code: FfiResult::PANIC_CAUGHT,
            spawn_before_fault: false,
        };
        let reg = world.resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "non_aborting_panic_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            reg,
        )
        .with_mod_id(ModId(13))
        .with_mod_namespace("sandbox_mod")
        .with_error_policy(ModErrorPolicy::ReportToBevy);
        world.resource_mut::<ModQuarantine>().register_system(
            ModId(13),
            sys.system_name_arc(),
            sys.enabled_flag(),
        );

        let mut schedule = Schedule::new(Update);
        schedule.add_systems(sys);
        schedule.run(&mut world);
        schedule.run(&mut world);

        assert_eq!(scenario.calls.load(Ordering::SeqCst), 1);
        assert!(
            world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(13), "non_aborting_panic_sys")
        );
    }

    // 4. `DelegateToBevyFallback` preserves non-auto-quarantining semantics with enriched context.
    CAPTURED_FALLBACKS.lock().unwrap().clear();
    {
        let mut world = World::new();
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());
        world.init_resource::<Messages<ModSystemFaulted>>();
        world.insert_resource(FallbackErrorHandler(record_fallback_error));

        let scenario = FaultScenario {
            calls: AtomicUsize::new(0),
            report_msg: Some("transient network timeout"),
            return_code: FfiResult::INVALID_STATE,
            spawn_before_fault: true,
        };
        let baseline_entities = world.entities().len();
        let reg = world.resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "delegate_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            reg,
        )
        .with_mod_id(ModId(14))
        .with_mod_namespace("net_mod")
        .with_mod_generation(5)
        .with_error_policy(ModErrorPolicy::DelegateToBevyFallback);
        world.resource_mut::<ModQuarantine>().register_system(
            ModId(14),
            sys.system_name_arc(),
            sys.enabled_flag(),
        );

        let mut schedule = Schedule::new(Update);
        schedule.add_systems(sys);
        schedule.run(&mut world);
        schedule.run(&mut world);

        assert_eq!(scenario.calls.load(Ordering::SeqCst), 2);
        assert_eq!(world.entities().len(), baseline_entities);
        assert!(
            !world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(14), "delegate_sys")
        );
        let captured = CAPTURED_FALLBACKS.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert!(
            captured[0]
                .display
                .contains("guest mod Some(ModId(14)) system \"delegate_sys\" faulted with")
        );
        assert_eq!(
            captured[0].mod_err.as_ref().unwrap().last_error.as_deref(),
            Some("transient network timeout")
        );
        drop(captured);
    }

    // 5. `Quarantine` and `Log` stay isolated even when the host uses `FallbackErrorHandler(panic)`,
    //    while `SystemInitError` under `ReportToBevy` bridges cleanly to `FallbackErrorHandler`.
    CAPTURED_FALLBACKS.lock().unwrap().clear();
    {
        let mut world = World::new();
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());
        world.init_resource::<Messages<ModSystemFaulted>>();
        world.insert_resource(FallbackErrorHandler(bevy_ecs::error::panic));

        let scenario = FaultScenario {
            calls: AtomicUsize::new(0),
            report_msg: Some("isolated fault"),
            return_code: FfiResult::INTERNAL_ERROR,
            spawn_before_fault: false,
        };
        let reg = world.resource::<ModRegistry>().clone();
        let sys_q = GuestSystem::new(
            "quarantined_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            reg.clone(),
        )
        .with_mod_id(ModId(15))
        .with_mod_namespace("iso_mod")
        .with_error_policy(ModErrorPolicy::Quarantine);
        world.resource_mut::<ModQuarantine>().register_system(
            ModId(15),
            sys_q.system_name_arc(),
            sys_q.enabled_flag(),
        );

        let mut schedule = Schedule::new(Update);
        schedule.add_systems(sys_q);
        schedule.run(&mut world);
        assert!(
            world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(15), "quarantined_sys")
        );

        // Now switch handler to capture and verify `SystemInitError` bridges to `FallbackErrorHandler`.
        world.insert_resource(FallbackErrorHandler(record_fallback_error));
        let bad_init_sys = GuestSystem::new(
            "bad_init_sys",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::read(
                BamComponent::from_raw(9999),
            )])],
            noop_cb,
            null_mut(),
            reg,
        )
        .with_mod_id(ModId(16))
        .with_mod_namespace("broken_mod")
        .with_mod_generation(4)
        .with_error_policy(ModErrorPolicy::ReportToBevy);
        world.resource_mut::<ModQuarantine>().register_system(
            ModId(16),
            bad_init_sys.system_name_arc(),
            bad_init_sys.enabled_flag(),
        );

        let mut init_sched = Schedule::new(Update);
        init_sched.add_systems(bad_init_sys);
        init_sched.initialize(&mut world).unwrap();

        assert!(
            world
                .resource::<ModQuarantine>()
                .is_quarantined(ModId(16), "bad_init_sys")
        );
        let captured = CAPTURED_FALLBACKS.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let mod_err = captured[0].mod_err.as_ref().expect("ModSystemError");
        assert_eq!(mod_err.mod_namespace.as_deref(), Some("broken_mod"));
        assert_eq!(mod_err.generation, 4);
        assert_eq!(mod_err.status, FfiResult::INVALID_COMPONENT);
        assert!(
            mod_err
                .last_error
                .as_deref()
                .is_some_and(|s| s.contains("BamComponent(9999)"))
        );
        drop(captured);

        let msgs = world.resource::<Messages<ModSystemFaulted>>();
        let mut cursor = msgs.get_cursor();
        let init_events: Vec<&ModSystemFaulted> = cursor
            .read(msgs)
            .filter(|e| e.system_name == "bad_init_sys")
            .collect();
        assert_eq!(init_events.len(), 1);
        assert_eq!(
            init_events[0].init_error,
            Some(SystemInitError::UnknownComponent(String::from(
                "BamComponent(9999)"
            )))
        );

        // 6. Direct `dispatch_mod_fault_to_fallback_handler` + `GuestSystem::initialize_count()`
        //    + `GuestSystem::last_error()` verification.
        CAPTURED_FALLBACKS.lock().unwrap().clear();
        let mut direct_sys = GuestSystem::new(
            "direct_probe_sys",
            vec![],
            fault_scenario_cb,
            (&raw const scenario).cast_mut().cast::<c_void>(),
            ModRegistry::new(),
        )
        .with_mod_id(ModId(17))
        .with_mod_namespace("direct_mod")
        .with_mod_generation(7)
        .with_error_policy(ModErrorPolicy::Quarantine);
        assert_eq!(direct_sys.initialize_count(), 0);
        let _ = direct_sys.initialize(&mut world);
        assert_eq!(direct_sys.initialize_count(), 1);
        assert_eq!(direct_sys.mod_generation(), 7);
        assert_eq!(direct_sys.mod_namespace(), Some("direct_mod"));
        direct_sys.run((), &mut world).unwrap();
        assert_eq!(direct_sys.last_error(), Some("isolated fault"));

        dispatch_mod_fault_to_fallback_handler(
            &world,
            ModSystemError {
                mod_id: Some(ModId(17)),
                mod_namespace: Some(String::from("direct_mod")),
                generation: 7,
                system_name: String::from("direct_probe_sys"),
                status: FfiResult::INTERNAL_ERROR,
                panicked: false,
                last_error: Some(String::from("direct fallback dispatch")),
            },
            direct_sys.get_last_run(),
        );
        let captured = CAPTURED_FALLBACKS.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let direct_err = captured[0].mod_err.as_ref().expect("ModSystemError");
        assert_eq!(direct_err.mod_namespace.as_deref(), Some("direct_mod"));
        assert_eq!(direct_err.generation, 7);
        assert_eq!(
            direct_err.last_error.as_deref(),
            Some("direct fallback dispatch")
        );
    }
}
