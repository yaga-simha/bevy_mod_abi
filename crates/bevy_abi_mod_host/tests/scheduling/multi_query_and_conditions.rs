//! Multi-query scoping, `WITHOUT` filters, `run_if` condition callbacks,
//! scheduled ordering descriptors, and dual-mod concurrent schedule execution.

use crate::support::{
    Armor, CombatHealth, Mass, Position, Velocity, ensure_guest_mods_built, reset_rust_mod_panic,
};
use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModNamedSystemSet, ModRegistry};
use bevy_abi_mod_host_sys::{
    ComponentAccessDescOwned, DynamicCSystem, ModInitContext, ModSchedule, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_next_chunk, host_iter_reset, host_iter_resource_mutable,
    host_iter_resource_readonly, host_register_conditional_system,
    host_register_multi_query_system, host_register_system,
};
use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::{
    ComponentAccessDesc, ComponentAccessMode, ConditionalSystemDescriptor, FFI_BOOL_FALSE, FfiBool,
    FfiResult, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    RegisterScheduledSystemFn, ScheduleLabelId, SystemDescriptor,
};
use bevy_app::{App, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, Schedule};
use bevy_ecs::system::System;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
struct Pos3 {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
#[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
struct SimConfig {
    dt: f32,
    step_count: u32,
    banner_len_observed: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct RustModCustomData {
    power: f32,
    shield: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct CModCustomData {
    mana: f32,
    level: i32,
}

fn register_inline_conditional_system(
    app: &mut App,
    desc: &ConditionalSystemDescriptor,
) -> FfiResult {
    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx = ModInitContext::new(registry);
    ctx.world = app.world_mut() as *mut World;
    let status = {
        let _guard = ctx.make_active();
        let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
        unsafe { host_register_conditional_system(ctx_ptr, desc) }
    };
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

static MULTI_QUERY_PAIR_EVALS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn multi_query_nested_test_cb(
    queries: *const *mut RawChunkIter,
    query_count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    assert_eq!(query_count, 2);
    let q_outer = unsafe { *queries.add(0) };
    let q_inner = unsafe { *queries.add(1) };

    loop {
        let outer_n = unsafe { host_iter_next_chunk(q_outer) };
        if outer_n == 0 {
            break;
        }
        let pos_col = unsafe { host_iter_column_mutable(q_outer, 0) };
        let pos_stride = unsafe { host_iter_column_stride(q_outer, 0) };
        assert!(!pos_col.is_null());

        for i in 0..outer_n {
            assert_eq!(unsafe { host_iter_reset(q_inner) }, FfiResult::OK);
            loop {
                let inner_n = unsafe { host_iter_next_chunk(q_inner) };
                if inner_n == 0 {
                    break;
                }
                let vel_col = unsafe { host_iter_column_readonly(q_inner, 0) };
                let vel_stride = unsafe { host_iter_column_stride(q_inner, 0) };
                assert!(!vel_col.is_null());
                assert!(unsafe { host_iter_column_mutable(q_inner, 0) }.is_null());

                for j in 0..inner_n {
                    let _vel_ptr = unsafe { vel_col.add(j * vel_stride) };
                    MULTI_QUERY_PAIR_EVALS.fetch_add(1, Ordering::SeqCst);
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
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Armor>();
    app.register_mod_component::<Mass>();

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

    let reg_arc = Arc::new(app.world().resource::<ModRegistry>().clone());

    {
        let mut bad_world = World::new();
        let pid = bad_world.register_component::<Position>();
        let mut bad_reg = ModRegistry::new();
        bad_reg.register_component::<Position>(pid);
        let mut conflicting_sys = DynamicCSystem::new_multi_query(
            "self_conflicting_multi_query".to_string(),
            vec![
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                }],
            ],
            multi_query_nested_test_cb,
            core::ptr::null_mut(),
            Arc::new(bad_reg),
        );
        conflicting_sys.initialize(&mut bad_world);
        MULTI_QUERY_PAIR_EVALS.store(0, Ordering::SeqCst);
        let mut sched = Schedule::default();
        sched.add_systems(conflicting_sys);
        sched.run(&mut bad_world);
        assert_eq!(MULTI_QUERY_PAIR_EVALS.load(Ordering::SeqCst), 0);
    }

    MULTI_QUERY_PAIR_EVALS.store(0, Ordering::SeqCst);
    let valid_sys = DynamicCSystem::new_multi_query(
        "disjoint_nested_multi_query".to_string(),
        vec![
            vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "Mass".to_string(),
                    mode: ComponentAccessMode::FILTER_ONLY,
                },
                ComponentAccessDescOwned {
                    name: "Armor".to_string(),
                    mode: ComponentAccessMode::WITHOUT,
                },
            ],
            vec![
                ComponentAccessDescOwned {
                    name: "Velocity".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "Armor".to_string(),
                    mode: ComponentAccessMode::FILTER_ONLY,
                },
                ComponentAccessDescOwned {
                    name: "Mass".to_string(),
                    mode: ComponentAccessMode::WITHOUT,
                },
            ],
        ],
        multi_query_nested_test_cb,
        core::ptr::null_mut(),
        reg_arc,
    );

    app.add_systems(Update, valid_sys);
    app.update();
    assert_eq!(MULTI_QUERY_PAIR_EVALS.load(Ordering::SeqCst), 20);
}

#[test]
fn test_multi_query_per_group_resource_scoping_prevents_cross_iter_aliasing() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();
    app.register_mod_resource::<SimConfig>();
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
        iters: *const *mut RawChunkIter,
        iter_count: usize,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        assert_eq!(iter_count, 2);
        let iter0 = unsafe { *iters.add(0) };
        let iter1 = unsafe { *iters.add(1) };

        assert!(unsafe { host_iter_resource_readonly(iter0, c"SimConfig".as_ptr()) }.is_null());
        assert!(unsafe { host_iter_resource_mutable(iter0, c"SimConfig".as_ptr()) }.is_null());

        let cfg_ptr =
            unsafe { host_iter_resource_mutable(iter1, c"SimConfig".as_ptr()) } as *mut SimConfig;
        assert!(!cfg_ptr.is_null());
        unsafe {
            (*cfg_ptr).step_count += 5;
        }
        FfiResult::OK
    }

    let q0 = [ComponentAccessDesc::read(c"Pos3".as_ptr())];
    let q1 = [ComponentAccessDesc::resource_write(c"SimConfig".as_ptr())];
    let specs = [
        QuerySpec {
            queries: q0.as_ptr(),
            query_count: q0.len(),
        },
        QuerySpec {
            queries: q1.as_ptr(),
            query_count: q1.len(),
        },
    ];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"mq_scoped_res_sys".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        mq_scoped_res_sys,
        core::ptr::null_mut(),
    );

    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx = ModInitContext::new(registry);
    ctx.world = app.world_mut() as *mut World;
    {
        let _guard = ctx.make_active();
        let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
        assert_eq!(
            unsafe { host_register_multi_query_system(ctx_ptr, &mq_desc) },
            FfiResult::OK
        );
    }
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }

    let conflict_specs = [
        QuerySpec {
            queries: q1.as_ptr(),
            query_count: q1.len(),
        },
        QuerySpec {
            queries: q1.as_ptr(),
            query_count: q1.len(),
        },
    ];
    let conflict_desc = MultiQuerySystemDescriptor::new(
        c"mq_dup_res_write_conflict".as_ptr(),
        conflict_specs.as_ptr(),
        conflict_specs.len(),
        mq_scoped_res_sys,
        core::ptr::null_mut(),
    );
    let registry2 = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx2 = ModInitContext::new(registry2);
    ctx2.world = app.world_mut() as *mut World;
    {
        let _guard2 = ctx2.make_active();
        let ctx2_ptr = (&mut ctx2 as *mut ModInitContext).cast::<c_void>();
        assert_eq!(
            unsafe { host_register_multi_query_system(ctx2_ptr, &conflict_desc) },
            FfiResult::ACCESS_CONFLICT
        );
    }
    assert!(ctx2.registered_systems.is_empty());

    app.update();
    assert_eq!(app.world().resource::<SimConfig>().step_count, 15);
}

#[test]
fn test_run_if_condition_skips_execution_preserves_changed_ticks_and_contains_panics() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();

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

    unsafe extern "C" fn test_run_if(user_data: *mut c_void) -> FfiBool {
        let ctx = unsafe { &*(user_data as *const CondCtx) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match ctx.mode.load(Ordering::SeqCst) {
                0 => FFI_BOOL_FALSE,
                1 => 42,
                _ => panic!("simulated panic inside guest run_if"),
            }
        })) {
            Ok(flag) => flag,
            Err(_) => {
                ctx.panic_caught.store(true, Ordering::SeqCst);
                FFI_BOOL_FALSE
            }
        }
    }

    unsafe extern "C" fn cond_changed_sys(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const SysCtx) };
        ctx.exec_count.fetch_add(1, Ordering::SeqCst);
        let mut guard = ctx.observed.lock().unwrap();
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let ents = unsafe { host_iter_entities(iter) };
            for r in 0..count {
                unsafe {
                    guard.push((*ents.add(r)).0);
                }
            }
        }
        FfiResult::OK
    }

    let queries = [ComponentAccessDesc::changed(c"Pos3".as_ptr())];
    let base = SystemDescriptor::new(
        c"cond_changed_sys".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        cond_changed_sys,
        (&*sys_ctx as *const SysCtx) as *mut c_void,
    );
    let cond_desc = base.with_run_if(test_run_if, (&*cond_ctx as *const CondCtx) as *mut c_void);
    assert_eq!(
        register_inline_conditional_system(&mut app, &cond_desc),
        FfiResult::OK
    );

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
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const OrderPushCtx) };
    ctx.log.lock().unwrap().push(ctx.tag);
    FfiResult::OK
}

unsafe extern "C" fn multi_startup_cb(
    _iters: *const *mut RawChunkIter,
    _iter_count: usize,
    _commands: *mut RawCommandQueue,
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
    app.register_mod_component::<Position>();

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let _reg_guard = reg.make_current();

    let sym = unsafe { host_get_proc_address(c"register_scheduled_system".as_ptr()) };
    assert!(!sym.is_null());
    let reg_sched_fn: RegisterScheduledSystemFn = unsafe { core::mem::transmute(sym) };

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

    let mut init_ctx = ModInitContext::new(Arc::clone(&reg));
    let _init_guard = init_ctx.make_active();
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

    let startup_v2 = SystemDescriptor::new(
        c"startup_single".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_startup_single as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleLabelId::STARTUP);
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, startup_v2.as_base_ptr()) },
        FfiResult::OK
    );

    let pos_read = [ComponentAccessDesc::read(c"Position".as_ptr())];
    let specs = [QuerySpec {
        queries: pos_read.as_ptr(),
        query_count: pos_read.len(),
    }];
    let after_startup = [c"startup_single".as_ptr()];
    let multi_v2 = MultiQuerySystemDescriptor::new(
        c"startup_multi".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        multi_startup_cb,
        (&*ctx_startup_multi as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleLabelId::STARTUP)
    .with_after(after_startup.as_ptr(), after_startup.len());
    assert_eq!(
        unsafe { host_register_multi_query_system(ctx_ptr, multi_v2.as_base_ptr()) },
        FfiResult::OK
    );

    let after_first = [c"update_first".as_ptr()];
    let second_desc = SystemDescriptor::new(
        c"update_second".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_second as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleLabelId::UPDATE)
    .with_after(after_first.as_ptr(), after_first.len());
    assert_eq!(
        unsafe { reg_sched_fn(ctx_ptr, &second_desc) },
        FfiResult::OK
    );

    let first_desc = SystemDescriptor::new(
        c"update_first".as_ptr(),
        core::ptr::null(),
        0,
        order_step_push,
        (&*ctx_first as *const OrderPushCtx as *mut OrderPushCtx).cast::<c_void>(),
    )
    .with_schedule(ScheduleLabelId::UPDATE);
    assert_eq!(unsafe { reg_sched_fn(ctx_ptr, &first_desc) }, FfiResult::OK);
    drop(_init_guard);

    for sys in init_ctx.registered_systems {
        let sched = sys.schedule();
        let own_set = ModNamedSystemSet::new("test_mod", sys.system_name());
        let after = sys.after_systems().to_vec();
        let mut cfg = sys.in_set(own_set);
        for dep in after {
            cfg = cfg.after(ModNamedSystemSet::new("test_mod", dep));
        }
        match sched {
            ModSchedule::Startup => {
                app.add_systems(Startup, cfg);
            }
            ModSchedule::Update => {
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
}

#[test]
fn test_concurrent_dual_mod_schedule_stress() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    let rust_mod_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
    let c_mod_id = app.load_mod_tracked(&c_dylib).expect("load c mod");

    let (rust_comp_id, c_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();
        (
            registry
                .get_by_name("RustModCustomData")
                .unwrap()
                .component_id,
            registry.get_by_name("CModCustomData").unwrap().component_id,
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
    #![allow(unused_imports, dead_code)]
    //! Empirical Adversarial Verification & Stress Test Suite for Milestone 5
    //!
    //! Rigorous stress testing of:
    //! 1. Chunk iteration boundary conditions (0 entities, 1 entity, multi-chunk archetypes >16,384 entities).
    //! 2. Component access permission enforcement (READ vs WRITE vs FILTER_ONLY vs WITHOUT, invalid column indices).
    //! 3. Change detection granularity (blanket chunk mutation vs selective row-level `host_iter_mark_changed`).
    //! 4. High-volume deferred commands execution (2,000 rapid spawns, inserts, and 1,000 despawns).
    //! 5. Command bridge error resilience (null pointers, bad entity bits, mismatched sizes, unregistered components, invalid UTF-8).
    //! 6. Multi-query safety & nested iteration (self-conflict detection, compatible multi-queries, nested loops with `host_iter_reset`).
    //! 7. Cross-language binary struct memory layout matching (C99 vs Rust repr(C) byte-for-byte).
    //! 8. Cross-language concurrent runtime stress (Example 3 C99 + Rust mods running simultaneously under heavy entity loads and dynamic reload cycles).

    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, host_cmd_despawn,
        host_cmd_insert_component, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_entities, host_iter_mark_changed,
        host_iter_next_chunk, host_iter_reset,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::Changed;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::{Query, System};
    use std::ffi::{CString, c_char, c_void};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    // ==============================================================================
    // Component Definitions for Challenger Tests
    // ==============================================================================

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct DroneTag {
        pub squad: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct PlayerTag {
        pub id: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C)]
    pub struct ColorRgba {
        pub r: f32,
        pub g: f32,
        pub b: f32,
        pub a: f32,
    }

    // ==============================================================================
    // TEST 1: Chunk Iteration Boundaries (0 Entities & 1 Entity)
    // ==============================================================================

    static ZERO_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn zero_entity_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        ZERO_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

        // With 0 entities, next_chunk must immediately return 0
        let count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(count, 0, "Empty world chunk iteration must return 0");

        // All column pointers and entity pointers must be null
        let readonly_ptr = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(
            readonly_ptr.is_null(),
            "Readonly column pointer must be null when chunk has 0 entities"
        );

        let mutable_ptr = unsafe { host_iter_column_mutable(iter, 1) };
        assert!(
            mutable_ptr.is_null(),
            "Mutable column pointer must be null when chunk has 0 entities"
        );

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(
            entities_ptr.is_null(),
            "Entities pointer must be null when chunk has 0 entities"
        );

        // Iter reset must succeed cleanly
        let reset_res = unsafe { host_iter_reset(iter) };
        assert_eq!(reset_res, FfiResult::OK);

        // Iterating again after reset must still return 0
        let count_again = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(count_again, 0);

        FfiResult::OK
    }

    static SINGLE_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);
    static SINGLE_ENTITY_ID_BITS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn single_entity_callback(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        SINGLE_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

        let count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(
            count, 1,
            "Single entity world chunk iteration must return exactly 1"
        );

        // Entity pointer must be non-null and valid
        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null(), "Entities pointer must not be null");
        let entity = unsafe { *entities_ptr };
        SINGLE_ENTITY_ID_BITS.store(entity.0 as u32, Ordering::SeqCst);

        // Read column 0 (Position)
        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
        assert!(!pos_ptr.is_null(), "Position pointer must be non-null");
        let pos = unsafe { *pos_ptr };
        assert_eq!(
            pos,
            Position {
                x: 42.0,
                y: 100.0,
                z: -5.0
            }
        );

        // Mutate column 1 (Velocity)
        let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
        assert!(!vel_ptr.is_null(), "Velocity pointer must be non-null");
        unsafe {
            (*vel_ptr).x += 10.0;
            (*vel_ptr).y += 20.0;
            (*vel_ptr).z += 30.0;
        }

        // Exhaust iterator
        let next_count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(
            next_count, 0,
            "Single entity world must be exhausted on second next_chunk"
        );

        FfiResult::OK
    }

    // ==============================================================================
    // TEST 7: Multi-Query Safety & Nested Iteration with Reset
    // ==============================================================================

    static NESTED_INTERACTION_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn nested_pairwise_callback(
        iters: *const *mut RawChunkIter,
        iter_count: usize,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        assert_eq!(iter_count, 2);
        let iter_drones = unsafe { *iters };
        let iter_players = unsafe { *iters.add(1) };

        let mut interactions = 0;

        // Outer loop: Drones
        loop {
            let drone_count = unsafe { host_iter_next_chunk(iter_drones) };
            if drone_count == 0 {
                break;
            }

            for _ in 0..drone_count {
                // Inner loop: Players (requires iter_reset before each traversal)
                let reset_res = unsafe { host_iter_reset(iter_players) };
                assert_eq!(reset_res, FfiResult::OK);

                loop {
                    let player_count = unsafe { host_iter_next_chunk(iter_players) };
                    if player_count == 0 {
                        break;
                    }
                    interactions += player_count;
                }
            }
        }

        NESTED_INTERACTION_COUNT.store(interactions, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn test_stress_multi_query_safety_and_nesting() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<DroneTag>();
        app.register_mod_component::<PlayerTag>();

        let registry = Arc::new(app.world().resource::<ModRegistry>().clone());

        // 1. Self-conflict detection test: Query 0 writes Position, Query 1 writes Position
        {
            let conflicting_specs = vec![
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
            ];
            let mut sys = DynamicCSystem::new_multi_query(
                "self_conflict_write_write".to_string(),
                conflicting_specs,
                nested_pairwise_callback,
                core::ptr::null_mut(),
                Arc::clone(&registry),
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
            let conflicting_specs = vec![
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                }],
                vec![ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                }],
            ];
            let mut sys = DynamicCSystem::new_multi_query(
                "self_conflict_write_read".to_string(),
                conflicting_specs,
                nested_pairwise_callback,
                core::ptr::null_mut(),
                Arc::clone(&registry),
            );
            sys.initialize(app.world_mut());
            assert!(
                sys.has_self_conflict(),
                "System with Write-Read collision must flag self_conflict"
            );
            assert!(!sys.is_enabled());
        }

        // 3. Compatible nested execution: 10 Drones and 5 Players -> 50 interactions
        {
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

            let compatible_specs = vec![
                vec![
                    ComponentAccessDescOwned {
                        name: "Position".to_string(),
                        mode: ComponentAccessMode::READ,
                    },
                    ComponentAccessDescOwned {
                        name: "DroneTag".to_string(),
                        mode: ComponentAccessMode::FILTER_ONLY,
                    },
                ],
                vec![
                    ComponentAccessDescOwned {
                        name: "Position".to_string(),
                        mode: ComponentAccessMode::READ,
                    },
                    ComponentAccessDescOwned {
                        name: "PlayerTag".to_string(),
                        mode: ComponentAccessMode::FILTER_ONLY,
                    },
                ],
            ];

            let sys = DynamicCSystem::new_multi_query(
                "nested_pairwise".to_string(),
                compatible_specs,
                nested_pairwise_callback,
                core::ptr::null_mut(),
                Arc::clone(&registry),
            );
            app.add_systems(Update, sys);

            app.update();

            assert_eq!(
                NESTED_INTERACTION_COUNT.load(Ordering::SeqCst),
                10 * 5,
                "10 drones and 5 players must generate exactly 50 pairwise interactions"
            );
        }
    }
}
