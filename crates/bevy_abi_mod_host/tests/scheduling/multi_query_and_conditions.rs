//! Multi-query scoping, `WITHOUT` filters, `run_if` condition callbacks,
//! scheduled ordering descriptors, and dual-mod concurrent schedule execution (`ABI 0.2.0`).

use crate::support::{
    Armor, C_MOD_CUSTOM_DATA_NAME, CModCustomData, CombatHealth, Mass, Position,
    RUST_MOD_CUSTOM_DATA_NAME, RustModCustomData, Velocity, ensure_guest_mods_built,
};
use bevy_abi_mod_host::{
    AbiModPlugin, ComponentOptions, LoadOptions, ModAppExt, ModNamedSystemSet, ModRegistry,
};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, LoadContext, host_iter_column_info, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_entities, host_iter_next_chunk, host_iter_reset,
    host_register_system, host_res_mut_ptr, host_res_ptr, host_sys_query,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    BamColumnInfo, BamIter, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, FfiBool,
    FfiResult, ScheduleId,
};
use bevy_app::{App, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, Schedule};
use bevy_ecs::system::System;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
#[mod_component(name = "sched_mq::Pos3")]
struct Pos3 {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
#[mod_component(name = "sched_mq::SimConfig")]
struct SimConfig {
    dt: f32,
    step_count: u32,
    banner_len_observed: u32,
}

fn register_inline_system(app: &mut App, desc: &BamSystemDesc) -> FfiResult {
    let registry = app.world().resource::<ModRegistry>().clone();
    let mut ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::new(AtomicBool::new(true)),
        app.world_mut() as *mut World,
        None,
    );
    let status = unsafe { host_register_system(ctx.as_ffi_ptr(), desc) };
    ctx.finish_init();
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

unsafe extern "C" fn multi_query_nested_test_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let pair_evals = unsafe { &*(user_data as *const AtomicUsize) };
    let mut q_outer: *mut BamIter = core::ptr::null_mut();
    let mut q_inner: *mut BamIter = core::ptr::null_mut();
    let mut q_oob: *mut BamIter = core::ptr::null_mut();

    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 0, &mut q_outer) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 1, &mut q_inner) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 2, &mut q_oob) },
        FfiResult::OUT_OF_RANGE
    );

    let mut outer_info = MaybeUninit::<BamColumnInfo>::uninit();
    let mut inner_info = MaybeUninit::<BamColumnInfo>::uninit();
    assert_eq!(
        unsafe { host_iter_column_info(q_outer, 0, outer_info.as_mut_ptr()) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_iter_column_info(q_inner, 0, inner_info.as_mut_ptr()) },
        FfiResult::OK
    );
    let pos_stride = unsafe { outer_info.assume_init().size };
    let vel_stride = unsafe { inner_info.assume_init().size };

    let mut outer_n = 0usize;
    while unsafe { host_iter_next_chunk(q_outer, &mut outer_n) }.get() {
        let pos_col = unsafe { host_iter_column_mutable(q_outer, 0) };
        assert!(!pos_col.is_null());

        for i in 0..outer_n {
            assert_eq!(unsafe { host_iter_reset(q_inner) }, FfiResult::OK);
            let mut inner_n = 0usize;
            while unsafe { host_iter_next_chunk(q_inner, &mut inner_n) }.get() {
                let vel_col = unsafe { host_iter_column_readonly(q_inner, 0) };
                assert!(!vel_col.is_null());
                assert!(unsafe { host_iter_column_mutable(q_inner, 0) }.is_null());

                for j in 0..inner_n {
                    let _vel_ptr = unsafe { vel_col.add(j * vel_stride) };
                    pair_evals.fetch_add(1, Ordering::SeqCst);
                }
            }
            let _pos_ptr = unsafe { pos_col.add(i * pos_stride) };
        }
    }
    FfiResult::OK
}

#[test]
fn test_multi_query_without_filter_and_intra_system_conflict_detection() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let armor_h = app
        .register_mod_component::<Armor>(ComponentOptions::new())
        .unwrap();
    let mass_h = app
        .register_mod_component::<Mass>(ComponentOptions::new())
        .unwrap();

    for i in 0..5 {
        if i % 2 == 0 {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Mass { value: 10.0 },
            ));
        } else {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Mass { value: 10.0 },
                CombatHealth {
                    current: 100,
                    max: 100,
                },
            ));
        }
    }

    for _ in 0..3 {
        app.world_mut().spawn((
            Position {
                x: 99.0,
                y: 0.0,
                z: 0.0,
            },
            Mass { value: 10.0 },
            Armor { defense: 50 },
        ));
    }

    for i in 0..4 {
        app.world_mut().spawn((
            Velocity {
                x: 1.0,
                y: i as f32,
                z: 0.0,
            },
            Armor { defense: 10 },
        ));
    }

    let reg = app.world().resource::<ModRegistry>().clone();
    let pair_evals = Arc::new(AtomicUsize::new(0));

    {
        let mut bad_world = World::new();
        let pid = bad_world.register_component::<Position>();
        let mut bad_reg = ModRegistry::new();
        let bad_pos_h = bad_reg
            .register_component::<Position>(pid, ComponentOptions::new())
            .unwrap();
        let mut conflicting_sys = GuestSystem::new(
            "self_conflicting_multi_query",
            vec![
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::write(bad_pos_h)],
                },
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::read(bad_pos_h)],
                },
            ],
            multi_query_nested_test_cb,
            Arc::as_ptr(&pair_evals) as *mut c_void,
            bad_reg,
        );
        conflicting_sys.initialize(&mut bad_world);
        pair_evals.store(0, Ordering::SeqCst);
        let mut sched = Schedule::default();
        sched.add_systems(conflicting_sys);
        sched.run(&mut bad_world);
        assert_eq!(pair_evals.load(Ordering::SeqCst), 0);
    }

    pair_evals.store(0, Ordering::SeqCst);
    let valid_sys = GuestSystem::new(
        "disjoint_nested_multi_query",
        vec![
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::write(pos_h),
                    BamTermDesc::filter_only(mass_h),
                    BamTermDesc::without(armor_h),
                ],
            },
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::read(vel_h),
                    BamTermDesc::filter_only(armor_h),
                    BamTermDesc::without(mass_h),
                ],
            },
        ],
        multi_query_nested_test_cb,
        Arc::as_ptr(&pair_evals) as *mut c_void,
        reg,
    );

    app.add_systems(Update, valid_sys);
    app.update();
    assert_eq!(pair_evals.load(Ordering::SeqCst), 20);
}

#[test]
fn test_multi_query_per_group_resource_scoping_prevents_cross_iter_aliasing() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Pos3>(ComponentOptions::new())
        .unwrap();
    let sim_h = app.register_mod_resource::<SimConfig>().unwrap();
    app.insert_resource(SimConfig {
        dt: 0.25,
        step_count: 10,
        banner_len_observed: 0,
    });
    app.world_mut().spawn(Pos3 {
        x: 4.0,
        y: 0.0,
        z: 0.0,
    });

    unsafe extern "C" fn mq_scoped_res_sys(
        sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let mut iter0: *mut BamIter = core::ptr::null_mut();
        let mut iter1: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter0) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 1, &mut iter1) },
            FfiResult::OK
        );

        // `iter0` declared only `Pos3` (entity term at index 0) — resource access on `iter0` MUST return null!
        assert!(unsafe { host_res_ptr(iter0, 0) }.is_null());
        assert!(unsafe { host_res_mut_ptr(iter0, 0) }.is_null());

        // `iter1` declared `SimConfig` (`RESOURCE_WRITE` at term index 0).
        let cfg_ptr = unsafe { host_res_mut_ptr(iter1, 0) } as *mut SimConfig;
        assert!(!cfg_ptr.is_null());
        unsafe {
            (*cfg_ptr).step_count += 5;
        }
        FfiResult::OK
    }

    let q0 = [BamTermDesc::read(pos_h)];
    let q1 = [BamTermDesc::resource_write(sim_h)];
    let specs = [
        BamQueryDesc::new(q0.as_ptr(), q0.len()),
        BamQueryDesc::new(q1.as_ptr(), q1.len()),
    ];
    let mq_desc = BamSystemDesc::new(
        c"mq_scoped_res_sys".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        mq_scoped_res_sys,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &mq_desc), FfiResult::OK);

    let conflict_specs = [
        BamQueryDesc::new(q1.as_ptr(), q1.len()),
        BamQueryDesc::new(q1.as_ptr(), q1.len()),
    ];
    let conflict_desc = BamSystemDesc::new(
        c"mq_dup_res_write_conflict".as_ptr(),
        conflict_specs.as_ptr(),
        conflict_specs.len(),
        mq_scoped_res_sys,
        core::ptr::null_mut(),
    );
    let registry2 = app.world().resource::<ModRegistry>().clone();
    let mut ctx2 = LoadContext::with_world_and_mod_id(
        registry2,
        Arc::new(AtomicBool::new(true)),
        app.world_mut() as *mut World,
        None,
    );
    assert_eq!(
        unsafe { host_register_system(ctx2.as_ffi_ptr(), &conflict_desc) },
        FfiResult::ACCESS_CONFLICT
    );
    ctx2.finish_init();
    assert!(ctx2.registered_systems.is_empty());

    app.update();
    assert_eq!(app.world().resource::<SimConfig>().step_count, 15);
}

#[test]
fn test_run_if_condition_skips_execution_preserves_changed_ticks_and_contains_panics() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Pos3>(ComponentOptions::new())
        .unwrap();

    let e1 = app
        .world_mut()
        .spawn(Pos3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        })
        .id();
    let e2 = app
        .world_mut()
        .spawn(Pos3 {
            x: 2.0,
            y: 0.0,
            z: 0.0,
        })
        .id();

    let cond_mode = Arc::new(AtomicU8::new(1));
    let panic_caught = Arc::new(AtomicBool::new(false));
    let exec_count = Arc::new(AtomicUsize::new(0));
    let observed_changed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    struct CondCtx {
        mode: Arc<AtomicU8>,
        panic_caught: Arc<AtomicBool>,
    }
    struct SysCtx {
        exec_count: Arc<AtomicUsize>,
        observed: Arc<Mutex<Vec<u64>>>,
    }

    let cond_ctx = Box::new(CondCtx {
        mode: Arc::clone(&cond_mode),
        panic_caught: Arc::clone(&panic_caught),
    });
    let sys_ctx = Box::new(SysCtx {
        exec_count: Arc::clone(&exec_count),
        observed: Arc::clone(&observed_changed),
    });

    // S-09: Guest `run_if` trampolines in `bevy_abi_mod_guest_sys` catch panics on the guest side
    // and return `FfiBool::FALSE` across the `extern "C"` boundary without unwinding.
    unsafe extern "C" fn test_run_if(user_data: *mut c_void) -> FfiBool {
        let ctx = unsafe { &*(user_data as *const CondCtx) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match ctx.mode.load(Ordering::SeqCst) {
                0 => FfiBool::FALSE,
                // Non-zero byte (`42`) verifies `FfiBool` accepts any non-zero C truthy value (`C-12`).
                1 => FfiBool(42),
                _ => panic!("simulated panic caught inside guest run_if trampoline"),
            }
        })) {
            Ok(flag) => flag,
            Err(_) => {
                ctx.panic_caught.store(true, Ordering::SeqCst);
                FfiBool::FALSE
            }
        }
    }

    unsafe extern "C" fn cond_changed_sys(
        raw_sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const SysCtx) };
        ctx.exec_count.fetch_add(1, Ordering::SeqCst);
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let st = unsafe { host_sys_query(raw_sys_ctx, 0, &mut iter) };
        if st.is_err() {
            return st;
        }
        let mut guard = ctx.observed.lock().unwrap();
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let ents = unsafe { host_iter_entities(iter) };
            for r in 0..count {
                unsafe {
                    guard.push((*ents.add(r)).0);
                }
            }
        }
        FfiResult::OK
    }

    let terms = [BamTermDesc::changed(pos_h)];
    let queries = [BamQueryDesc::new(terms.as_ptr(), terms.len())];
    let cond_desc = BamSystemDesc::new(
        c"cond_changed_sys".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        cond_changed_sys,
        (&*sys_ctx as *const SysCtx) as *mut c_void,
    )
    .with_run_if(test_run_if, (&*cond_ctx as *const CondCtx) as *mut c_void);

    assert_eq!(register_inline_system(&mut app, &cond_desc), FfiResult::OK);

    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);
    assert_eq!(observed_changed.lock().unwrap().drain(..).count(), 2);

    cond_mode.store(0, Ordering::SeqCst);
    app.world_mut().get_mut::<Pos3>(e1).unwrap().x = 99.0;
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);
    assert!(observed_changed.lock().unwrap().is_empty());

    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);

    cond_mode.store(1, Ordering::SeqCst);
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        observed_changed
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![e1.to_bits()]
    );

    app.world_mut().get_mut::<Pos3>(e2).unwrap().z = 77.0;
    cond_mode.store(2, Ordering::SeqCst);
    app.update();
    assert!(panic_caught.load(Ordering::SeqCst));
    assert_eq!(exec_count.load(Ordering::SeqCst), 2);
    assert!(observed_changed.lock().unwrap().is_empty());

    cond_mode.store(1, Ordering::SeqCst);
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        observed_changed
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![e2.to_bits()]
    );

    drop(sys_ctx);
    drop(cond_ctx);
}

struct OrderPushCtx {
    tag: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

unsafe extern "C" fn order_step_push(
    _sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const OrderPushCtx) };
    ctx.log.lock().unwrap().push(ctx.tag);
    FfiResult::OK
}

#[test]
fn scheduled_system_and_multi_query_descriptors_with_ordering_work() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();

    let reg = app.world().resource::<ModRegistry>().clone();

    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let ctx_startup_single = Box::new(OrderPushCtx {
        tag: "startup_single",
        log: Arc::clone(&log),
    });
    let ctx_startup_multi = Box::new(OrderPushCtx {
        tag: "startup_multi",
        log: Arc::clone(&log),
    });
    let ctx_second = Box::new(OrderPushCtx {
        tag: "update_second",
        log: Arc::clone(&log),
    });
    let ctx_first = Box::new(OrderPushCtx {
        tag: "update_first",
        log: Arc::clone(&log),
    });

    let mut load_ctx = LoadContext::new(reg);

    let startup_single = BamSystemDesc::new(
        c"startup_single".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_startup_single as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleId::STARTUP);
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &startup_single) },
        FfiResult::OK
    );

    let pos_read = [BamTermDesc::read(pos_h)];
    let specs = [BamQueryDesc::new(pos_read.as_ptr(), pos_read.len())];
    let after_startup = [c"startup_single".as_ptr()];
    let startup_multi = BamSystemDesc::new(
        c"startup_multi".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        order_step_push,
        (&*ctx_startup_multi as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleId::STARTUP)
    .with_after(after_startup.as_ptr(), after_startup.len());
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &startup_multi) },
        FfiResult::OK
    );

    let after_first = [c"update_first".as_ptr()];
    let second_desc = BamSystemDesc::new(
        c"update_second".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_second as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_after(after_first.as_ptr(), after_first.len());
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &second_desc) },
        FfiResult::OK
    );

    let before_second = [c"update_second".as_ptr()];
    let first_desc = BamSystemDesc::new(
        c"update_first".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_first as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_before(before_second.as_ptr(), before_second.len());
    assert_eq!(
        unsafe { host_register_system(load_ctx.as_ffi_ptr(), &first_desc) },
        FfiResult::OK
    );
    load_ctx.finish_init();

    for sys in load_ctx.registered_systems.drain(..) {
        let sched = sys.schedule();
        let own_set = ModNamedSystemSet::new("test_mod", sys.system_name());
        let after = sys.after_systems().to_vec();
        let before = sys.before_systems().to_vec();
        let mut cfg = sys.in_set(own_set);
        for dep in after {
            cfg = cfg.after(ModNamedSystemSet::new("test_mod", dep));
        }
        for dep in before {
            cfg = cfg.before(ModNamedSystemSet::new("test_mod", dep));
        }
        match sched {
            ScheduleId::STARTUP => {
                app.add_systems(Startup, cfg);
            }
            ScheduleId::UPDATE => {
                app.add_systems(Update, cfg);
            }
            _ => unreachable!(),
        }
    }

    app.update();
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "startup_single",
            "startup_multi",
            "update_first",
            "update_second",
        ]
    );

    app.update();
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "startup_single",
            "startup_multi",
            "update_first",
            "update_second",
            "update_first",
            "update_second",
        ]
    );

    drop(ctx_startup_single);
    drop(ctx_startup_multi);
    drop(ctx_second);
    drop(ctx_first);
}

#[test]
fn test_concurrent_dual_mod_schedule_stress() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    // T-03 + L-03: Load both mods with `shadow_copy: true` so parallel tests never share `.bss`.
    let rust_mod_id = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load rust mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load c mod");

    let (rust_comp_id, c_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();
        (
            registry
                .get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
            registry
                .get_by_name(C_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
        )
    };

    const PER_GROUP: usize = 1_000;
    let mut group_a = Vec::with_capacity(PER_GROUP);
    let mut group_b = Vec::with_capacity(PER_GROUP);
    let mut group_c = Vec::with_capacity(PER_GROUP);

    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_data = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        group_a.push(e);
    }

    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_data = CModCustomData {
            mana: 20.0,
            level: 1,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        group_b.push(e);
    }

    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_r = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let init_c = CModCustomData {
            mana: 20.0,
            level: 1,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_r, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(init_c, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        group_c.push(e);
    }

    for _ in 0..50 {
        app.update();
    }

    for &e in &group_a {
        let data = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data.power, 600.0);
        assert_eq!(data.shield, 100);
    }

    for &e in &group_b {
        let data = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 270.0);
        assert_eq!(data.level, 51);
    }

    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 600.0);
        assert_eq!(data_r.shield, 100);
        assert_eq!(data_c.mana, 270.0);
        assert_eq!(data_c.level, 51);
    }

    app.unload_mod(rust_mod_id).expect("Rust mod unload failed");
    for _ in 0..10 {
        app.update();
    }

    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 600.0);
        assert_eq!(data_r.shield, 100);
        assert_eq!(data_c.mana, 320.0);
        assert_eq!(data_c.level, 61);
    }

    app.unload_mod(c_mod_id).expect("C mod unload failed");
    for _ in 0..10 {
        app.update();
    }

    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 600.0);
        assert_eq!(data_c.mana, 320.0);
    }

    for e in group_a.into_iter().chain(group_b).chain(group_c) {
        app.world_mut().despawn(e);
    }
}

mod multi_query_nesting_stress {
    use crate::support::{Position, Velocity};
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_entities, host_iter_next_chunk, host_iter_reset, host_sys_query,
    };
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{BamIter, BamSystemCtx, BamTermDesc, FfiResult};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::system::System;
    use core::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C)]
    #[mod_component(name = "sched_mq_stress::DroneTag")]
    pub struct DroneTag {
        pub squad: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C)]
    #[mod_component(name = "sched_mq_stress::PlayerTag")]
    pub struct PlayerTag {
        pub id: u32,
    }

    unsafe extern "C" fn zero_entity_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let visits = unsafe { &*(user_data as *const AtomicUsize) };
        visits.fetch_add(1, Ordering::SeqCst);

        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 99usize;
        assert!(
            !unsafe { host_iter_next_chunk(iter, &mut count) }.get(),
            "Empty world chunk iteration must return false"
        );
        assert_eq!(count, 0, "Empty world chunk iteration must write count = 0");

        assert!(unsafe { host_iter_column_readonly(iter, 0) }.is_null());
        assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());
        assert!(unsafe { host_iter_entities(iter) }.is_null());

        assert_eq!(unsafe { host_iter_reset(iter) }, FfiResult::OK);
        assert!(!unsafe { host_iter_next_chunk(iter, &mut count) }.get());
        assert_eq!(count, 0);

        FfiResult::OK
    }

    struct SingleEntityProbe {
        visits: AtomicUsize,
        entity_bits: AtomicU64,
    }

    unsafe extern "C" fn single_entity_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let probe = unsafe { &*(user_data as *const SingleEntityProbe) };
        probe.visits.fetch_add(1, Ordering::SeqCst);

        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        assert!(unsafe { host_iter_next_chunk(iter, &mut count) }.get());
        assert_eq!(count, 1);

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null());
        let entity = unsafe { *entities_ptr };
        probe.entity_bits.store(entity.0, Ordering::SeqCst);

        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
        assert!(!pos_ptr.is_null());
        let pos = unsafe { *pos_ptr };
        assert_eq!(
            pos,
            Position {
                x: 42.0,
                y: 100.0,
                z: -5.0
            }
        );

        let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
        assert!(!vel_ptr.is_null());
        unsafe {
            (*vel_ptr).x += 10.0;
            (*vel_ptr).y += 20.0;
            (*vel_ptr).z += 30.0;
        }

        assert!(!unsafe { host_iter_next_chunk(iter, &mut count) }.get());
        assert_eq!(count, 0);

        FfiResult::OK
    }

    unsafe extern "C" fn nested_pairwise_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let out_count = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter_drones: *mut BamIter = core::ptr::null_mut();
        let mut iter_players: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter_drones) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 1, &mut iter_players) },
            FfiResult::OK
        );

        let mut interactions = 0usize;
        let mut drone_count = 0usize;
        while unsafe { host_iter_next_chunk(iter_drones, &mut drone_count) }.get() {
            for _ in 0..drone_count {
                assert_eq!(unsafe { host_iter_reset(iter_players) }, FfiResult::OK);
                let mut player_count = 0usize;
                while unsafe { host_iter_next_chunk(iter_players, &mut player_count) }.get() {
                    interactions += player_count;
                }
            }
        }

        out_count.store(interactions, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_zero_and_single_entity_chunk_iteration_boundaries() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let reg = app.world().resource::<ModRegistry>().clone();

        let zero_visits = Arc::new(AtomicUsize::new(0));
        let zero_sys = GuestSystem::new(
            "zero_entity_sys",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(pos_h), BamTermDesc::write(vel_h)],
            }],
            zero_entity_callback,
            Arc::as_ptr(&zero_visits) as *mut c_void,
            reg,
        );
        app.add_systems(Update, zero_sys);
        app.update();
        assert_eq!(zero_visits.load(Ordering::SeqCst), 1);

        let mut app_single = App::new();
        app_single.add_plugins(AbiModPlugin);
        let pos_h2 = app_single
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let vel_h2 = app_single
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let spawned = app_single
            .world_mut()
            .spawn((
                Position {
                    x: 42.0,
                    y: 100.0,
                    z: -5.0,
                },
                Velocity {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
            ))
            .id();

        let probe = Arc::new(SingleEntityProbe {
            visits: AtomicUsize::new(0),
            entity_bits: AtomicU64::new(0),
        });
        let reg2 = app_single.world().resource::<ModRegistry>().clone();
        let single_sys = GuestSystem::new(
            "single_entity_sys",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(pos_h2), BamTermDesc::write(vel_h2)],
            }],
            single_entity_callback,
            Arc::as_ptr(&probe) as *mut c_void,
            reg2,
        );
        app_single.add_systems(Update, single_sys);
        app_single.update();

        assert_eq!(probe.visits.load(Ordering::SeqCst), 1);
        assert_eq!(probe.entity_bits.load(Ordering::SeqCst), spawned.to_bits());
        assert_eq!(
            *app_single.world().get::<Velocity>(spawned).unwrap(),
            Velocity {
                x: 11.0,
                y: 22.0,
                z: 33.0,
            }
        );
    }

    #[test]
    fn test_stress_multi_query_safety_and_nesting() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let _vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let drone_h = app
            .register_mod_component::<DroneTag>(ComponentOptions::new())
            .unwrap();
        let player_h = app
            .register_mod_component::<PlayerTag>(ComponentOptions::new())
            .unwrap();

        let registry = app.world().resource::<ModRegistry>().clone();
        let interactions = Arc::new(AtomicUsize::new(0));

        // 1. Self-conflict detection test: Query 0 writes Position, Query 1 writes Position
        {
            let mut sys = GuestSystem::new(
                "self_conflict_write_write",
                vec![
                    BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![BamTermDesc::write(pos_h)],
                    },
                    BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![BamTermDesc::write(pos_h)],
                    },
                ],
                nested_pairwise_callback,
                Arc::as_ptr(&interactions) as *mut c_void,
                registry.clone(),
            );
            sys.initialize(app.world_mut());
            assert!(
                sys.has_self_conflict(),
                "System with Write-Write collision must flag self_conflict"
            );
            assert!(
                !sys.is_enabled(),
                "Conflicting system must be automatically disabled"
            );
        }

        // 2. Self-conflict detection test: Query 0 writes Position, Query 1 reads Position
        {
            let mut sys = GuestSystem::new(
                "self_conflict_write_read",
                vec![
                    BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![BamTermDesc::write(pos_h)],
                    },
                    BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![BamTermDesc::read(pos_h)],
                    },
                ],
                nested_pairwise_callback,
                Arc::as_ptr(&interactions) as *mut c_void,
                registry.clone(),
            );
            sys.initialize(app.world_mut());
            assert!(
                sys.has_self_conflict(),
                "System with Write-Read collision must flag self_conflict"
            );
            assert!(!sys.is_enabled());
        }

        // 3. Compatible nested execution: 10 Drones and 5 Players -> 50 interactions
        for i in 0..10 {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                DroneTag { squad: 1 },
            ));
        }
        for i in 0..5 {
            app.world_mut().spawn((
                Position {
                    x: 100.0 + i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                PlayerTag { id: i as u32 },
            ));
        }

        let sys = GuestSystem::new(
            "nested_pairwise",
            vec![
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::read(pos_h), BamTermDesc::filter_only(drone_h)],
                },
                BamQuerySpecOwned {
                    flags: 0,
                    terms: vec![BamTermDesc::read(pos_h), BamTermDesc::filter_only(player_h)],
                },
            ],
            nested_pairwise_callback,
            Arc::as_ptr(&interactions) as *mut c_void,
            registry,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(
            interactions.load(Ordering::SeqCst),
            10 * 5,
            "10 drones and 5 players must generate exactly 50 pairwise interactions"
        );
    }
}
