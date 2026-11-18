//! Weak ordering (`after_weak`/`before_weak`/`chain_weak`), ambiguity suppression,
//! `ModErrorPolicy::DelegateToBevyFallback`, and multi-seed `ScheduleBuildSettings::shuffle_seed`
//! determinism tests (`ABI 0.2.0`).

use bevy_abi_mod_host::schedule_sets::{configure_guest_system_in_world, configure_weak_set_chain};
use bevy_abi_mod_host::{
    AbiModPlugin, ModErrorPolicy, ModHost, ModId, ModNamedSystemSet, ModQuarantine, ModRegistry,
    ModSystemError, ModSystemFaulted, ModWorldExt,
};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_cmd_spawn_empty, host_sys_commands, host_sys_res_mut_ptr,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BamCommands, BamComponent, BamSystemCtx, BamSystemDesc,
    BamTermDesc, FfiEntity, FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::error::{BevyError, ErrorContext, FallbackErrorHandler};
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{LogLevel, Schedule, ScheduleBuildSettings, ScheduleLabel};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "weak_test::TraceLog")]
struct TraceLog {
    steps: [u32; 8],
    len: u32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "weak_test::SharedCounter")]
struct SharedCounter {
    value: u32,
}

unsafe extern "C" fn push_step_cb(sys_ctx: *mut BamSystemCtx, user_data: *mut c_void) -> FfiResult {
    let step_tag = user_data as usize as u32;
    let handle = BamComponent::from_raw((user_data as usize >> 32) as u32);
    let ptr = unsafe { host_sys_res_mut_ptr(sys_ctx, handle, core::ptr::null_mut()) };
    if ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let trace = unsafe { &mut *(ptr as *mut TraceLog) };
    let idx = trace.len as usize;
    if idx < trace.steps.len() {
        trace.steps[idx] = step_tag;
        trace.len += 1;
    }
    FfiResult::OK
}

unsafe extern "C" fn bump_counter_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let handle = BamComponent::from_raw(user_data as usize as u32);
    let ptr = unsafe { host_sys_res_mut_ptr(sys_ctx, handle, core::ptr::null_mut()) };
    if ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let counter = unsafe { &mut *(ptr as *mut SharedCounter) };
    counter.value += 1;
    FfiResult::OK
}

fn encode_step_user_data(handle: BamComponent, step: u32) -> *mut c_void {
    (((handle.0 as usize) << 32) | (step as usize)) as *mut c_void
}

unsafe extern "C" fn spawn_then_fault_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    if !user_data.is_null() {
        unsafe { (*(user_data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    }
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if rc.is_err() {
        return rc;
    }
    let mut ent = FfiEntity::NULL;
    let spawn_rc = unsafe { host_cmd_spawn_empty(cmds, &mut ent) };
    if spawn_rc.is_err() {
        return spawn_rc;
    }
    // Return an error after recording a deferred spawn: uncommitted commands must be discarded!
    FfiResult::INVALID_STATE
}

static CAPTURED_FALLBACK_ERRORS: Mutex<Vec<(String, String, Option<ModId>, FfiResult)>> =
    Mutex::new(Vec::new());

fn capture_fallback_error(err: BevyError, ctx: ErrorContext) {
    let mod_err = err.downcast_ref::<ModSystemError>().cloned();
    let (mod_id, status) = mod_err
        .map(|e| (e.mod_id, e.status))
        .unwrap_or((None, FfiResult::OK));
    CAPTURED_FALLBACK_ERRORS.lock().unwrap().push((
        ctx.name().to_string(),
        err.to_string(),
        mod_id,
        status,
    ));
}

#[test]
fn weak_ordering_and_ambiguity_suppression_work_with_strict_error_detection() {
    let mut world = World::new();
    let trace_h = world.register_mod_resource::<TraceLog>().unwrap();
    let counter_h = world.register_mod_resource::<SharedCounter>().unwrap();
    let reg = world.resource::<ModRegistry>().clone();
    world.insert_resource(TraceLog::default());
    world.insert_resource(SharedCounter::default());

    // `sys_a` writes `TraceLog` (step 1) and weakly orders `.before_weak("unloaded_mod::missing")`
    // and `.before_weak("mod_a::sys_b")`. Referencing an unloaded mod must succeed cleanly!
    let sys_a = GuestSystem::new(
        "sys_a",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_write(trace_h)],
        }],
        push_step_cb,
        encode_step_user_data(trace_h, 1),
        reg.clone(),
    )
    .with_weak_ordering(
        vec!["unloaded_mod::before_everything".to_string()],
        vec![
            "mod_a::sys_b".to_string(),
            "unloaded_mod::after_everything".to_string(),
        ],
    );

    // `sys_b` writes `TraceLog` (step 2) and weakly orders `.after_weak("mod_a::sys_a")` +
    // `.before_weak("mod_a::sys_c")`.
    let sys_b = GuestSystem::new(
        "sys_b",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_write(trace_h)],
        }],
        push_step_cb,
        encode_step_user_data(trace_h, 2),
        reg.clone(),
    )
    .with_weak_ordering(
        vec!["mod_a::sys_a".to_string()],
        vec!["mod_a::sys_c".to_string()],
    );

    // `sys_c` writes `TraceLog` (step 3) without any ambiguity with `sys_a`/`sys_b` because weak
    // edges between conflicting systems materialize ordering edges in Bevy 0.20!
    let sys_c = GuestSystem::new(
        "sys_c",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_write(trace_h)],
        }],
        push_step_cb,
        encode_step_user_data(trace_h, 3),
        reg.clone(),
    )
    .with_weak_ordering(vec!["mod_a::sys_b".to_string()], vec![]);

    // Two conflicting `SharedCounter` writers with NO ordering edge between them, suppressed via
    // `.ambiguous_with(...)` and `with_ambiguous_with_all(true)`!
    let ambig_1 = GuestSystem::new(
        "ambig_1",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_write(counter_h)],
        }],
        bump_counter_cb,
        counter_h.0 as usize as *mut c_void,
        reg.clone(),
    )
    .with_ambiguous_with(vec!["mod_b::ambig_2".to_string()]);

    let ambig_2 = GuestSystem::new(
        "ambig_2",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::resource_write(counter_h)],
        }],
        bump_counter_cb,
        counter_h.0 as usize as *mut c_void,
        reg,
    )
    .with_ambiguous_with_all(true);

    assert!(ambig_2.is_ambiguous_with_all());

    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems(configure_guest_system_in_world(&world, "mod_a", 0, sys_a));
    schedule.add_systems(configure_guest_system_in_world(&world, "mod_a", 0, sys_b));
    schedule.add_systems(configure_guest_system_in_world(&world, "mod_a", 0, sys_c));
    schedule.add_systems(configure_guest_system_in_world(&world, "mod_b", 0, ambig_1));
    schedule.add_systems(configure_guest_system_in_world(&world, "mod_b", 0, ambig_2));

    schedule
        .initialize(&mut world)
        .expect("weak ordering + ambiguity suppression must pass LogLevel::Error");
    schedule.run(&mut world);

    let trace = world.resource::<TraceLog>();
    assert_eq!(trace.len, 3);
    assert_eq!(&trace.steps[..3], &[1, 2, 3]);
    assert_eq!(world.resource::<SharedCounter>().value, 2);
}

#[test]
fn bam_system_desc_weak_and_ambiguity_builders_preserve_abi_fields() {
    let after_w = [c"other::early".as_ptr()];
    let before_w = [c"other::late".as_ptr()];
    let ambig = [c"other::peer".as_ptr()];
    let desc = BamSystemDesc::new(
        c"my_sys".as_ptr(),
        core::ptr::null(),
        0,
        bump_counter_cb,
        core::ptr::null_mut(),
    )
    .with_after_weak(after_w.as_ptr(), after_w.len())
    .with_before_weak(before_w.as_ptr(), before_w.len())
    .with_ambiguous_with(ambig.as_ptr(), ambig.len())
    .with_ambiguous_with_all();

    assert_eq!(desc.validate(), FfiResult::OK);
    assert!(desc.is_ambiguous_with_all());
    assert_eq!(
        desc.flags & BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL,
        BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL
    );
    assert_eq!(desc.after_weak_count, 1);
    assert_eq!(desc.before_weak_count, 1);
    assert_eq!(desc.ambiguous_with_count, 1);
}

#[test]
fn delegate_to_bevy_fallback_invokes_fallback_error_handler_with_context_and_discards_commands() {
    CAPTURED_FALLBACK_ERRORS.lock().unwrap().clear();

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(
        ModErrorPolicy::DelegateToBevyFallback,
    ));
    app.insert_resource(FallbackErrorHandler(capture_fallback_error));

    let baseline_entities = app.world().entities().len();
    let mod_id = ModId(77);
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "fallback_fault_sys",
        vec![],
        spawn_then_fault_cb,
        Arc::as_ptr(&call_count) as *mut c_void,
        reg,
    )
    .with_mod_id(mod_id);

    app.world_mut()
        .resource_mut::<ModQuarantine>()
        .register_system(mod_id, sys.system_name_arc(), sys.enabled_flag());
    app.add_systems(Update, sys);

    app.update();

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    // Uncommitted spawn command was discarded before `apply_deferred`.
    assert_eq!(app.world().entities().len(), baseline_entities);

    // Fallback handler stays in charge without forced quarantine.
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(mod_id, "fallback_fault_sys")
    );

    // `ModSystemFaulted` emitted once.
    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(msgs.len(), 1);
    let mut cursor = msgs.get_cursor();
    let events: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
    assert_eq!(events[0].mod_id, Some(mod_id));
    assert_eq!(events[0].system_name, "fallback_fault_sys");
    assert_eq!(events[0].status, FfiResult::INVALID_STATE);

    // Captured `BevyError` includes `.with_context(...)` and downcasts to `ModSystemError`.
    let captured = CAPTURED_FALLBACK_ERRORS.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "fallback_fault_sys");
    assert!(
        captured[0]
            .1
            .contains("guest mod Some(ModId(77)) system \"fallback_fault_sys\" faulted with"),
        "missing expected context in BevyError: {}",
        captured[0].1
    );
    assert_eq!(captured[0].2, Some(mod_id));
    assert_eq!(captured[0].3, FfiResult::INVALID_STATE);

    // Running a second update still invokes the system (not auto-quarantined).
    drop(captured);
    app.update();
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[test]
fn multi_seed_shuffle_seed_determinism_with_weak_chain_and_ambiguity() {
    let seeds: [u64; 18] = [
        0,
        1,
        7,
        42,
        1337,
        0xDEAD_BEEF,
        0xCAFE_BABE_0000_0001,
        0x1234_5678_9ABC_DEF0,
        999_983,
        u64::MAX / 3,
        u64::MAX - 1,
        101,
        202,
        303,
        404,
        505,
        606,
        707,
    ];

    for seed in seeds {
        let mut world = World::new();
        let trace_h = world.register_mod_resource::<TraceLog>().unwrap();
        let counter_h = world.register_mod_resource::<SharedCounter>().unwrap();
        let reg = world.resource::<ModRegistry>().clone();
        world.insert_resource(TraceLog::default());
        world.insert_resource(SharedCounter::default());
        world.insert_resource(ModHost::new());

        let mut schedule = Schedule::default();
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            shuffle_seed: Some(seed),
            ..Default::default()
        });

        // 4-step ordered pipeline using `ModNamedSystemSet` + `chain_weak` + `after_weak` against
        // a non-existent optional mod set, plus two ambiguous counter systems.
        let mut sets = Vec::with_capacity(4);
        for step in 1..=4u32 {
            let sys_name = format!("stage_{step}");
            sets.push(ModNamedSystemSet::new("pipeline", sys_name.clone()));
            let mut sys = GuestSystem::new(
                sys_name,
                vec![BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::resource_write(trace_h)],
                }],
                push_step_cb,
                encode_step_user_data(trace_h, step),
                reg.clone(),
            );
            if step == 1 {
                sys = sys.with_weak_ordering(vec!["optional_mod::pre_hook".to_string()], vec![]);
            }
            schedule.add_systems(configure_guest_system_in_world(&world, "pipeline", 0, sys));
        }

        let counter_a = GuestSystem::new(
            "counter_a",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::resource_write(counter_h)],
            }],
            bump_counter_cb,
            counter_h.0 as usize as *mut c_void,
            reg.clone(),
        )
        .with_ambiguous_with(vec!["aux::counter_b".to_string()]);

        let counter_b = GuestSystem::new(
            "counter_b",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::resource_write(counter_h)],
            }],
            bump_counter_cb,
            counter_h.0 as usize as *mut c_void,
            reg,
        )
        .with_ambiguous_with_all(true);

        schedule.add_systems(configure_guest_system_in_world(&world, "aux", 0, counter_a));
        schedule.add_systems(configure_guest_system_in_world(&world, "aux", 0, counter_b));

        // Chain the 4 pipeline sets weakly using `configure_weak_set_chain` and `chain_weak`.
        world.insert_resource(bevy_ecs::schedule::Schedules::new());
        configure_weak_set_chain(&mut world, Update.intern(), &sets);
        for pair in sets.windows(2) {
            schedule.configure_sets(bevy_ecs::schedule::IntoScheduleConfigs::chain_weak((
                pair[0].clone(),
                pair[1].clone(),
            )));
        }

        schedule
            .initialize(&mut world)
            .unwrap_or_else(|e| panic!("seed {seed} failed schedule build: {e:?}"));
        schedule.run(&mut world);

        let trace = world.resource::<TraceLog>();
        assert_eq!(trace.len, 4, "seed {seed} trace length");
        assert_eq!(
            &trace.steps[..4],
            &[1, 2, 3, 4],
            "seed {seed} produced non-deterministic pipeline order"
        );
        assert_eq!(
            world.resource::<SharedCounter>().value,
            2,
            "seed {seed} counter sum"
        );
    }
}

use std::sync::Arc;
