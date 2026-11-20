//! Adversarial and behavioral test suite for Phase 3 Item #17 — Per-Mod Resource Quotas &
//! Frame-Time Watchdogs (`ModBudgetPolicy`, `ModBudgetBreach`, `ModBudgetTelemetry`,
//! `ModBudgetExt`).

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HOST_API, HostSceneBackend, RegistrySnapshot,
};
use bevy_abi_mod_sys::{BamCommands, BamIter, BamSystemCtx};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::{ResMut, Resource};
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "budget_test::Payload64")]
struct Payload64 {
    bytes: [u8; 64],
}

impl Default for Payload64 {
    fn default() -> Self {
        Self { bytes: [0u8; 64] }
    }
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(summary_tick)]
#[mod_component(name = "budget_test::SummaryPos")]
struct SummaryPos {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "budget_test::ExtraTag")]
struct ExtraTag {
    id: u32,
}

struct NoopSceneBackend;

impl HostSceneBackend for NoopSceneBackend {
    fn spawn_scene(
        &self,
        _world: &mut World,
        _snapshot: &RegistrySnapshot,
        _root_entity: Entity,
        _parent: Option<Entity>,
        _scene_handle: &FfiAssetHandle,
    ) -> Result<(), FfiResult> {
        Ok(())
    }
}

#[derive(Resource, Default)]
struct HostTickCounter(usize);

fn host_tick_system(mut counter: ResMut<'_, HostTickCounter>) {
    counter.0 += 1;
}

#[test]
fn spawn_flood_is_capped_across_all_spawn_commands_and_resets_per_frame() {
    struct FloodState {
        payload_h: BamComponent,
        ok_count: AtomicUsize,
        rejected_count: AtomicUsize,
        batch_5_status: AtomicUsize,
        batch_1_status: AtomicUsize,
    }

    unsafe extern "C" fn spawn_flood_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `FloodState` owned by the test for the app's lifetime.
        let state = unsafe { &*(user_data as *const FloodState) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is active for this callback invocation.
        let st = unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) };
        if st.is_err() {
            return st;
        }

        let payload = Payload64 { bytes: [0xAB; 64] };
        let comps = [state.payload_h];
        let ptrs = [(&raw const payload).cast::<u8>()];
        let scene_h = FfiAssetHandle::from_index(BamComponent::from_raw(1), 1, 0);
        let mut ent = FfiEntity::NULL;

        // 3x cmd_spawn_empty (3/10)
        for _ in 0..3 {
            // SAFETY: Valid `cmds` and writable `ent`.
            let r = unsafe { (HOST_API.cmd_spawn_empty.unwrap())(cmds, &mut ent) };
            assert_eq!(r, FfiResult::OK);
            state.ok_count.fetch_add(1, Ordering::SeqCst);
        }
        // 3x cmd_spawn (6/10)
        for _ in 0..3 {
            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `ent`.
            let r = unsafe {
                (HOST_API.cmd_spawn.unwrap())(cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut ent)
            };
            assert_eq!(r, FfiResult::OK);
            state.ok_count.fetch_add(1, Ordering::SeqCst);
        }
        // 2x cmd_spawn_persistent (8/10)
        for _ in 0..2 {
            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `ent`.
            let r = unsafe {
                (HOST_API.cmd_spawn_persistent.unwrap())(
                    cmds,
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    1,
                    &mut ent,
                )
            };
            assert_eq!(r, FfiResult::OK);
            state.ok_count.fetch_add(1, Ordering::SeqCst);
        }
        // 1x cmd_spawn_scene (9/10)
        // SAFETY: Valid `cmds`, `scene_h`, and `ent`.
        let r_scene = unsafe {
            (HOST_API.cmd_spawn_scene.unwrap())(cmds, &scene_h, FfiEntity::NULL, &mut ent)
        };
        assert_eq!(r_scene, FfiResult::OK);
        state.ok_count.fetch_add(1, Ordering::SeqCst);

        // Attempt cmd_spawn_batch of 5 -> 9 + 5 = 14 > 10 => CAPACITY_EXCEEDED!
        let batch5 = [payload; 5];
        let batch5_ptrs = [batch5.as_ptr().cast::<u8>()];
        let mut out5 = [FfiEntity::NULL; 5];
        // SAFETY: Valid `cmds`, `comps`, `batch5_ptrs`, and `out5`.
        let r_b5 = unsafe {
            (HOST_API.cmd_spawn_batch.unwrap())(
                cmds,
                comps.as_ptr(),
                batch5_ptrs.as_ptr(),
                1,
                5,
                out5.as_mut_ptr(),
            )
        };
        state
            .batch_5_status
            .store(r_b5.0 as usize, Ordering::SeqCst);
        if r_b5 == FfiResult::CAPACITY_EXCEEDED {
            state.rejected_count.fetch_add(1, Ordering::SeqCst);
        }

        // Now cmd_spawn_batch of 1 -> 9 + 1 = 10 <= 10 => OK!
        let mut out1 = [FfiEntity::NULL; 1];
        // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `out1`.
        let r_b1 = unsafe {
            (HOST_API.cmd_spawn_batch.unwrap())(
                cmds,
                comps.as_ptr(),
                ptrs.as_ptr(),
                1,
                1,
                out1.as_mut_ptr(),
            )
        };
        state
            .batch_1_status
            .store(r_b1.0 as usize, Ordering::SeqCst);
        if r_b1.is_ok() {
            state.ok_count.fetch_add(1, Ordering::SeqCst);
        }

        // Now flood across every spawn command: ALL must fail with CAPACITY_EXCEEDED!
        for _ in 0..500 {
            // SAFETY: Valid `cmds` and `ent`.
            let r1 = unsafe { (HOST_API.cmd_spawn_empty.unwrap())(cmds, &mut ent) };
            assert_eq!(r1, FfiResult::CAPACITY_EXCEEDED);
            state.rejected_count.fetch_add(1, Ordering::SeqCst);

            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `ent`.
            let r2 = unsafe {
                (HOST_API.cmd_spawn.unwrap())(cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut ent)
            };
            assert_eq!(r2, FfiResult::CAPACITY_EXCEEDED);
            state.rejected_count.fetch_add(1, Ordering::SeqCst);
        }
        for _ in 0..100 {
            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `ent`.
            let r3 = unsafe {
                (HOST_API.cmd_spawn_persistent.unwrap())(
                    cmds,
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    1,
                    &mut ent,
                )
            };
            assert_eq!(r3, FfiResult::CAPACITY_EXCEEDED);
            state.rejected_count.fetch_add(1, Ordering::SeqCst);

            // SAFETY: Valid `cmds`, `scene_h`, and `ent`.
            let r4 = unsafe {
                (HOST_API.cmd_spawn_scene.unwrap())(cmds, &scene_h, FfiEntity::NULL, &mut ent)
            };
            assert_eq!(r4, FfiResult::CAPACITY_EXCEEDED);
            state.rejected_count.fetch_add(1, Ordering::SeqCst);
        }
        for _ in 0..50 {
            // SAFETY: Valid `cmds`, `comps`, `batch5_ptrs`, and `out5`.
            let r5 = unsafe {
                (HOST_API.cmd_spawn_batch.unwrap())(
                    cmds,
                    comps.as_ptr(),
                    batch5_ptrs.as_ptr(),
                    1,
                    5,
                    out5.as_mut_ptr(),
                )
            };
            assert_eq!(r5, FfiResult::CAPACITY_EXCEEDED);
            state.rejected_count.fetch_add(1, Ordering::SeqCst);
        }

        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_budget_policy(
        ModBudgetPolicy::default().with_max_spawns_per_frame(Some(10)),
    ));
    let payload_h = app
        .register_mod_component::<Payload64>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    app.world_mut()
        .resource_mut::<ModRegistry>()
        .set_scene_backend(Arc::new(NoopSceneBackend));

    let state = Box::new(FloodState {
        payload_h,
        ok_count: AtomicUsize::new(0),
        rejected_count: AtomicUsize::new(0),
        batch_5_status: AtomicUsize::new(0),
        batch_1_status: AtomicUsize::new(999),
    });
    let state_ptr = (&*state as *const FloodState) as *mut c_void;

    let reg = app.world().resource::<ModRegistry>().clone();
    let mod_id = ModId(101);
    let sys = GuestSystem::new("spawn_flood", Vec::new(), spawn_flood_cb, state_ptr, reg)
        .with_mod_id(mod_id)
        .with_mod_namespace("flood_mod");
    app.add_systems(Update, sys);

    let count_non_resource_entities = |world: &mut World| {
        let mut qb = bevy_ecs::query::QueryBuilder::<Entity>::new(world);
        qb.without_id(bevy_ecs::component::IS_RESOURCE);
        qb.build().iter(world).count()
    };

    // Frame 1: 10 accepted, 1251 rejected with CAPACITY_EXCEEDED.
    let entities_before = count_non_resource_entities(app.world_mut());
    app.update();
    let entities_after_f1 = count_non_resource_entities(app.world_mut());
    assert_eq!(entities_after_f1 - entities_before, 10);
    assert_eq!(state.ok_count.load(Ordering::SeqCst), 10);
    assert_eq!(
        state.batch_5_status.load(Ordering::SeqCst),
        FfiResult::CAPACITY_EXCEEDED.0 as usize
    );
    assert_eq!(
        state.batch_1_status.load(Ordering::SeqCst),
        FfiResult::OK.0 as usize
    );
    assert_eq!(state.rejected_count.load(Ordering::SeqCst), 1251);

    let breaches = app.world().resource::<Messages<ModBudgetBreach>>();
    assert_eq!(breaches.len(), 1);
    let mut cursor = breaches.get_cursor();
    let b = cursor.read(breaches).next().unwrap();
    assert_eq!(b.mod_id, Some(mod_id));
    assert_eq!(b.mod_name, "flood_mod");
    assert_eq!(b.system_name, "spawn_flood");
    assert_eq!(b.kind, ModBudgetBreachKind::SpawnsPerFrameExceeded);
    assert_eq!(b.observed, 10);
    assert_eq!(b.limit, 10);
    assert_eq!(b.count, 1251);
    assert!(!b.quarantined);

    let snapshots = app.system_budget_snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].spawns_last_frame, 10);
    assert_eq!(snapshots[0].breach_count, 1251);
    assert_eq!(
        app.world()
            .resource::<ModBudgetTelemetry>()
            .mod_spawns_last_frame(mod_id),
        10
    );

    // Frame 2: per-frame spawn budget resets cleanly, allowing 10 more spawns and capping again.
    app.update();
    let entities_after_f2 = count_non_resource_entities(app.world_mut());
    assert_eq!(entities_after_f2 - entities_after_f1, 10);
    assert_eq!(state.ok_count.load(Ordering::SeqCst), 20);
    assert_eq!(state.rejected_count.load(Ordering::SeqCst), 2502);
}

#[test]
fn cross_system_spawn_cap_is_shared_per_mod_per_frame() {
    struct SpawnerCfg {
        requested: usize,
        succeeded: AtomicUsize,
        rejected: AtomicUsize,
    }

    unsafe extern "C" fn spawn_n_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `SpawnerCfg` owned by the test.
        let cfg = unsafe { &*(user_data as *const SpawnerCfg) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is active for this invocation.
        let st = unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) };
        if st.is_err() {
            return st;
        }
        let mut ent = FfiEntity::NULL;
        for _ in 0..cfg.requested {
            // SAFETY: Valid `cmds` and writable `ent`.
            let r = unsafe { (HOST_API.cmd_spawn_empty.unwrap())(cmds, &mut ent) };
            if r.is_ok() {
                cfg.succeeded.fetch_add(1, Ordering::SeqCst);
            } else {
                assert_eq!(r, FfiResult::CAPACITY_EXCEEDED);
                cfg.rejected.fetch_add(1, Ordering::SeqCst);
            }
        }
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.set_mod_budget_policy(ModBudgetPolicy::default().with_max_spawns_per_frame(Some(6)));

    let cfg_a = Box::new(SpawnerCfg {
        requested: 4,
        succeeded: AtomicUsize::new(0),
        rejected: AtomicUsize::new(0),
    });
    let cfg_b = Box::new(SpawnerCfg {
        requested: 5,
        succeeded: AtomicUsize::new(0),
        rejected: AtomicUsize::new(0),
    });
    let cfg_other = Box::new(SpawnerCfg {
        requested: 6,
        succeeded: AtomicUsize::new(0),
        rejected: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let sys_a = GuestSystem::new(
        "spawner_a",
        Vec::new(),
        spawn_n_cb,
        (&*cfg_a as *const SpawnerCfg) as *mut c_void,
        reg.clone(),
    )
    .with_mod_id(ModId(11))
    .with_mod_namespace("mod_alpha");

    let sys_b = GuestSystem::new(
        "spawner_b",
        Vec::new(),
        spawn_n_cb,
        (&*cfg_b as *const SpawnerCfg) as *mut c_void,
        reg.clone(),
    )
    .with_mod_id(ModId(11))
    .with_mod_namespace("mod_alpha");

    let sys_other = GuestSystem::new(
        "spawner_other",
        Vec::new(),
        spawn_n_cb,
        (&*cfg_other as *const SpawnerCfg) as *mut c_void,
        reg,
    )
    .with_mod_id(ModId(12))
    .with_mod_namespace("mod_beta");

    app.add_systems(Update, ((sys_a, sys_b).chain(), sys_other));
    app.update();

    assert_eq!(cfg_a.succeeded.load(Ordering::SeqCst), 4);
    assert_eq!(cfg_a.rejected.load(Ordering::SeqCst), 0);
    assert_eq!(cfg_b.succeeded.load(Ordering::SeqCst), 2);
    assert_eq!(cfg_b.rejected.load(Ordering::SeqCst), 3);
    assert_eq!(cfg_other.succeeded.load(Ordering::SeqCst), 6);
    assert_eq!(cfg_other.rejected.load(Ordering::SeqCst), 0);

    let telemetry = app.world().resource::<ModBudgetTelemetry>();
    assert_eq!(telemetry.mod_spawns_last_frame(ModId(11)), 6);
    assert_eq!(telemetry.mod_spawns_last_frame(ModId(12)), 6);
    assert_eq!(telemetry.mod_snapshots(ModId(11)).len(), 2);
}

#[test]
fn arena_flood_returns_capacity_exceeded_without_growing_host_memory() {
    struct ArenaFloodState {
        target: FfiEntity,
        payload_h: BamComponent,
        ok_inserts: AtomicUsize,
        rejected_inserts: AtomicUsize,
    }

    unsafe extern "C" fn arena_flood_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `ArenaFloodState` owned by the test.
        let state = unsafe { &*(user_data as *const ArenaFloodState) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is active.
        let st = unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) };
        if st.is_err() {
            return st;
        }

        let payload = Payload64 { bytes: [0x5A; 64] };
        let ptr = (&raw const payload).cast::<u8>();

        // Cap is 512 bytes (8 x 64-byte payloads). Enqueue 1,000 inserts!
        for _ in 0..1_000 {
            // SAFETY: Valid `cmds`, `state.target`, and 64-byte `Payload64`.
            let r = unsafe {
                (HOST_API.cmd_insert.unwrap())(cmds, state.target, state.payload_h, ptr, 64)
            };
            if r.is_ok() {
                state.ok_inserts.fetch_add(1, Ordering::SeqCst);
            } else {
                assert_eq!(r, FfiResult::CAPACITY_EXCEEDED);
                state.rejected_inserts.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Also test `cmd_insert_if_new` and `cmd_spawn` with payload when arena is full.
        // SAFETY: Valid `cmds` and payload.
        let r_if_new = unsafe {
            (HOST_API.cmd_insert_if_new.unwrap())(cmds, state.target, state.payload_h, ptr, 64)
        };
        assert_eq!(r_if_new, FfiResult::CAPACITY_EXCEEDED);
        state.rejected_inserts.fetch_add(1, Ordering::SeqCst);

        let comps = [state.payload_h];
        let ptrs = [ptr];
        let mut out_e = FfiEntity::NULL;
        // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `out_e`.
        let r_spawn = unsafe {
            (HOST_API.cmd_spawn.unwrap())(cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut out_e)
        };
        assert_eq!(r_spawn, FfiResult::CAPACITY_EXCEEDED);
        state.rejected_inserts.fetch_add(1, Ordering::SeqCst);

        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.set_mod_budget_policy(
        ModBudgetPolicy::default().with_max_arena_bytes_per_system(Some(512)),
    );

    let payload_h = app
        .register_mod_component::<Payload64>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let target_ent = app.world_mut().spawn_empty().id();

    let state = Box::new(ArenaFloodState {
        target: FfiEntity::from_bits(target_ent.to_bits()),
        payload_h,
        ok_inserts: AtomicUsize::new(0),
        rejected_inserts: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let mod_id = ModId(202);
    let sys = GuestSystem::new(
        "arena_flood",
        Vec::new(),
        arena_flood_cb,
        (&*state as *const ArenaFloodState) as *mut c_void,
        reg,
    )
    .with_mod_id(mod_id)
    .with_mod_namespace("arena_mod");
    app.add_systems(Update, sys);

    app.update();

    assert_eq!(state.ok_inserts.load(Ordering::SeqCst), 8);
    assert_eq!(state.rejected_inserts.load(Ordering::SeqCst), 994);
    assert_eq!(
        app.world().get::<Payload64>(target_ent),
        Some(&Payload64 { bytes: [0x5A; 64] })
    );

    let breaches = app.world().resource::<Messages<ModBudgetBreach>>();
    assert_eq!(breaches.len(), 1);
    let mut cursor = breaches.get_cursor();
    let b = cursor.read(breaches).next().unwrap();
    assert_eq!(b.mod_id, Some(mod_id));
    assert_eq!(b.kind, ModBudgetBreachKind::ArenaBytesExceeded);
    assert_eq!(b.observed, 512);
    assert_eq!(b.limit, 512);
    assert_eq!(b.count, 994);
    assert!(!b.quarantined);

    let snap = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(mod_id), "arena_flood")
        .unwrap();
    assert_eq!(snap.arena_bytes_peak, 512);
    assert_eq!(snap.breach_count, 994);
    assert!(!snap.quarantined);
}

#[test]
fn runaway_loop_watchdog_auto_quarantines_emits_messages_preserves_siblings_and_reenables() {
    struct RunawayState {
        should_stall: AtomicBool,
        calls: AtomicUsize,
    }

    unsafe extern "C" fn runaway_cb(
        _sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `RunawayState` owned by the test.
        let state = unsafe { &*(user_data as *const RunawayState) };
        state.calls.fetch_add(1, Ordering::SeqCst);
        if state.should_stall.load(Ordering::Acquire) {
            // Busy-wait / sleep reliably longer than the 500us p99 budget ceiling.
            let target = Duration::from_millis(4);
            let start = Instant::now();
            std::thread::sleep(Duration::from_millis(3));
            while start.elapsed() < target {
                core::hint::spin_loop();
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn healthy_sibling_cb(
        _sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicUsize` owned by the test.
        let counter = unsafe { &*(user_data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_budget_policy(
        ModBudgetPolicy::default()
            .with_max_p99_system_micros(Some(500))
            .with_window_size(4)
            .with_max_consecutive_breaches(2),
    ));
    app.init_resource::<HostTickCounter>();

    let runaway_state = Box::new(RunawayState {
        should_stall: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    let sibling_calls = Box::new(AtomicUsize::new(0));

    let reg = app.world().resource::<ModRegistry>().clone();
    let bad_mod = ModId(301);
    let good_mod = ModId(302);

    let runaway_sys = GuestSystem::new(
        "runaway_loop",
        Vec::new(),
        runaway_cb,
        (&*runaway_state as *const RunawayState) as *mut c_void,
        reg.clone(),
    )
    .with_mod_id(bad_mod)
    .with_mod_namespace("bad_mod");

    let sibling_sys = GuestSystem::new(
        "healthy_sibling",
        Vec::new(),
        healthy_sibling_cb,
        (&*sibling_calls as *const AtomicUsize) as *mut c_void,
        reg,
    )
    .with_mod_id(good_mod)
    .with_mod_namespace("good_mod");

    app.add_systems(Update, (runaway_sys, sibling_sys, host_tick_system));

    // Frame 1: first p99 overrun (1/2 consecutive breaches) -> not quarantined yet.
    app.update();
    assert_eq!(runaway_state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.world().resource::<HostTickCounter>().0, 1);
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bad_mod, "runaway_loop")
    );
    assert_eq!(app.world().resource::<Messages<ModBudgetBreach>>().len(), 0);

    // Frame 2: second consecutive p99 overrun (2/2) -> auto-quarantines cleanly!
    app.update();
    assert_eq!(runaway_state.calls.load(Ordering::SeqCst), 2);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 2);
    assert_eq!(app.world().resource::<HostTickCounter>().0, 2);
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bad_mod, "runaway_loop")
    );
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(good_mod, "healthy_sibling")
    );

    // Structured `ModBudgetBreach` + `ModSystemFaulted` must both be emitted!
    {
        let breaches = app.world().resource::<Messages<ModBudgetBreach>>();
        assert_eq!(breaches.len(), 1);
        let mut cursor = breaches.get_cursor();
        let b = cursor.read(breaches).next().unwrap();
        assert_eq!(b.mod_id, Some(bad_mod));
        assert_eq!(b.mod_name, "bad_mod");
        assert_eq!(b.system_name, "runaway_loop");
        assert_eq!(b.kind, ModBudgetBreachKind::FrameTimeP99Exceeded);
        assert!(b.observed > 500, "expected p99 > 500us, got {}", b.observed);
        assert_eq!(b.limit, 500);
        assert_eq!(b.count, 2);
        assert!(b.quarantined);
    }
    {
        let faults = app.world().resource::<Messages<ModSystemFaulted>>();
        assert_eq!(faults.len(), 1);
        let mut cursor = faults.get_cursor();
        let f = cursor.read(faults).next().unwrap();
        assert_eq!(f.mod_id, Some(bad_mod));
        assert_eq!(f.system_name, "runaway_loop");
        assert_eq!(f.status, FfiResult::CAPACITY_EXCEEDED);
        assert!(!f.panicked);
        assert!(
            f.last_error
                .as_deref()
                .is_some_and(|s| s.contains("frame-time budget"))
        );
    }

    let snap = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(bad_mod), "runaway_loop")
        .unwrap();
    assert!(snap.p99_us > 500);
    assert!(snap.p50_us > 500);
    assert_eq!(snap.breach_count, 1);
    assert!(snap.quarantined);

    // Frames 3 & 4: runaway system stays quarantined; sibling guest & host systems keep running!
    app.update();
    app.update();
    assert_eq!(runaway_state.calls.load(Ordering::SeqCst), 2);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 4);
    assert_eq!(app.world().resource::<HostTickCounter>().0, 4);

    // Fix runaway condition and re-enable via `ModQuarantine::reenable` -> resumes cleanly!
    runaway_state.should_stall.store(false, Ordering::Release);
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .reenable(bad_mod, "runaway_loop")
    );
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bad_mod, "runaway_loop")
    );

    // Frames 5, 6, 7: executes cleanly every frame without false re-quarantine from stale window.
    for expected_calls in 3..=5 {
        app.update();
        assert_eq!(runaway_state.calls.load(Ordering::SeqCst), expected_calls);
        assert!(
            !app.world()
                .resource::<ModQuarantine>()
                .is_quarantined(bad_mod, "runaway_loop")
        );
    }

    // Re-trigger runaway loop -> re-quarantines cleanly after 2 consecutive breaches again!
    runaway_state.should_stall.store(true, Ordering::Release);
    app.update();
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bad_mod, "runaway_loop")
    );
    app.update();
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(bad_mod, "runaway_loop")
    );
    assert_eq!(runaway_state.calls.load(Ordering::SeqCst), 7);
    let post_snap = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(bad_mod), "runaway_loop")
        .unwrap();
    assert_eq!(post_snap.breach_count, 2);
    assert!(post_snap.quarantined);
}

#[test]
fn per_mod_overrides_and_runtime_policy_updates_work_immediately() {
    struct Probe {
        attempts: usize,
        ok: AtomicUsize,
    }

    unsafe extern "C" fn try_spawns_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `Probe` owned by the test.
        let p = unsafe { &*(user_data as *const Probe) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is active.
        if unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        let mut ent = FfiEntity::NULL;
        let mut ok = 0usize;
        for _ in 0..p.attempts {
            // SAFETY: Valid `cmds` and writable `ent`.
            if unsafe { (HOST_API.cmd_spawn_empty.unwrap())(cmds, &mut ent) }.is_ok() {
                ok += 1;
            }
        }
        p.ok.store(ok, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.set_mod_budget_policy(ModBudgetPolicy::default().with_max_spawns_per_frame(Some(10)));
    app.set_mod_name_budget_override(
        "tight_mod",
        ModBudgetLimits::default().with_max_spawns_per_frame(Some(2)),
    );
    app.set_mod_budget_override(
        ModId(30),
        ModBudgetLimits::default().with_max_spawns_per_frame(Some(5)),
    );

    let p_id_override = Box::new(Probe {
        attempts: 12,
        ok: AtomicUsize::new(0),
    });
    let p_name_override = Box::new(Probe {
        attempts: 12,
        ok: AtomicUsize::new(0),
    });
    let p_default = Box::new(Probe {
        attempts: 12,
        ok: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let s1 = GuestSystem::new(
        "s1",
        Vec::new(),
        try_spawns_cb,
        (&*p_id_override as *const Probe) as *mut c_void,
        reg.clone(),
    )
    .with_mod_id(ModId(30))
    .with_mod_namespace("tight_mod");
    let s2 = GuestSystem::new(
        "s2",
        Vec::new(),
        try_spawns_cb,
        (&*p_name_override as *const Probe) as *mut c_void,
        reg.clone(),
    )
    .with_mod_id(ModId(31))
    .with_mod_namespace("tight_mod");
    let s3 = GuestSystem::new(
        "s3",
        Vec::new(),
        try_spawns_cb,
        (&*p_default as *const Probe) as *mut c_void,
        reg,
    )
    .with_mod_id(ModId(32))
    .with_mod_namespace("normal_mod");

    app.add_systems(Update, (s1, s2, s3));
    app.update();

    assert_eq!(p_id_override.ok.load(Ordering::SeqCst), 5);
    assert_eq!(p_name_override.ok.load(Ordering::SeqCst), 2);
    assert_eq!(p_default.ok.load(Ordering::SeqCst), 10);

    // Replace policy via raw `app.insert_resource(ModBudgetPolicy { .. })` -> `First` syncs before `Update`!
    app.insert_resource(ModBudgetPolicy {
        max_spawns_per_frame: Some(3),
        ..Default::default()
    });
    app.update();
    assert_eq!(p_id_override.ok.load(Ordering::SeqCst), 3);
    assert_eq!(p_name_override.ok.load(Ordering::SeqCst), 3);
    assert_eq!(p_default.ok.load(Ordering::SeqCst), 3);
}

#[test]
fn telemetry_reports_matched_entities_tables_and_summary_tick_skips() {
    unsafe extern "C" fn iterate_changed_cb(
        sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        // SAFETY: Valid `sys_ctx` and writable `iter`.
        if unsafe { (HOST_API.sys_query.unwrap())(sys_ctx, 0, &mut iter) }.is_ok()
            && !iter.is_null()
        {
            let mut chunk_len = 0usize;
            // SAFETY: Valid `iter` and writable `chunk_len`.
            while unsafe { (HOST_API.iter_next_chunk.unwrap())(iter, &mut chunk_len) }.get() {}
        }
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<SummaryPos>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let _tag_h = app
        .register_mod_component::<ExtraTag>(ComponentOptions::new().with_raw_insert())
        .unwrap();

    // Table 1: 4 entities with `(SummaryPos,)`.
    let mut table1_entities = Vec::new();
    for i in 0..4 {
        table1_entities.push(
            app.world_mut()
                .spawn(SummaryPos {
                    x: i as f32,
                    y: 0.0,
                })
                .id(),
        );
    }
    // Table 2: 6 entities with `(SummaryPos, ExtraTag)`.
    for i in 0..6 {
        app.world_mut().spawn((
            SummaryPos {
                x: i as f32,
                y: 1.0,
            },
            ExtraTag { id: i },
        ));
    }

    let mod_id = ModId(401);
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "changed_tracker",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::CHANGED,
        )])],
        iterate_changed_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(mod_id)
    .with_mod_namespace("summary_mod");
    app.add_systems(Update, sys);

    // Frame 1: all 10 entities newly added -> 2 matched tables, 0 skipped, 10 matched entities.
    app.update();
    let s1 = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(mod_id), "changed_tracker")
        .unwrap();
    assert_eq!(s1.matched_tables, 2);
    assert_eq!(s1.skipped_summary_tick_tables, 0);
    assert_eq!(s1.matched_entities, 10);

    // Frame 2: zero changes -> both tables skipped via `summary_tick`, 0 matched entities!
    app.update();
    let s2 = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(mod_id), "changed_tracker")
        .unwrap();
    assert_eq!(s2.matched_tables, 2);
    assert_eq!(s2.skipped_summary_tick_tables, 2);
    assert_eq!(s2.matched_entities, 0);

    // Frame 3: mutate 1 entity in Table 1 -> Table 2 skipped via `summary_tick`, 1 matched entity!
    app.world_mut()
        .get_mut::<SummaryPos>(table1_entities[0])
        .unwrap()
        .x = 99.0;
    app.update();
    let s3 = app
        .world()
        .resource::<ModBudgetTelemetry>()
        .system_snapshot(Some(mod_id), "changed_tracker")
        .unwrap();
    assert_eq!(s3.matched_tables, 2);
    assert_eq!(s3.skipped_summary_tick_tables, 1);
    assert_eq!(s3.matched_entities, 1);
}

#[cfg(feature = "diagnostics")]
#[test]
fn diagnostics_publishes_budget_breaches_p99_spawns_and_arena_peak() {
    use bevy_abi_mod_host::diagnostics::{
        TOTAL_ARENA_BYTES_PEAK_PATH, TOTAL_BUDGET_BREACH_COUNT_PATH, TOTAL_SPAWNS_QUEUED_PATH,
        mod_arena_bytes_peak_path, mod_budget_breach_count_path, mod_p99_time_path,
        mod_spawns_queued_path,
    };
    use bevy_diagnostic::{DiagnosticsPlugin, DiagnosticsStore};

    unsafe extern "C" fn diag_budget_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let payload_h = BamComponent::from_raw(user_data as usize as u32);
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is active.
        if unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        let payload = Payload64 { bytes: [1; 64] };
        let comps = [payload_h];
        let ptrs = [(&raw const payload).cast::<u8>()];
        let mut ent = FfiEntity::NULL;
        // 2 spawns succeed, 3rd rejected (`max_spawns_per_frame = 2`).
        for _ in 0..3 {
            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `ent`.
            let _ = unsafe {
                (HOST_API.cmd_spawn.unwrap())(cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut ent)
            };
        }
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins(DiagnosticsPlugin);
    app.add_plugins(AbiModPlugin::with_budget_policy(
        ModBudgetPolicy::default().with_max_spawns_per_frame(Some(2)),
    ));
    let payload_h = app
        .register_mod_component::<Payload64>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "diag_budget",
        Vec::new(),
        diag_budget_cb,
        (payload_h.0 as usize) as *mut c_void,
        reg,
    )
    .with_mod_id(ModId(505))
    .with_mod_namespace("diag_budget_mod");
    app.add_systems(Update, sys);

    app.update();

    let store = app.world().resource::<DiagnosticsStore>();
    assert_eq!(
        store
            .get(&TOTAL_BUDGET_BREACH_COUNT_PATH)
            .and_then(|d| d.value()),
        Some(1.0)
    );
    assert_eq!(
        store.get(&TOTAL_SPAWNS_QUEUED_PATH).and_then(|d| d.value()),
        Some(2.0)
    );
    assert_eq!(
        store
            .get(&TOTAL_ARENA_BYTES_PEAK_PATH)
            .and_then(|d| d.value()),
        Some(128.0)
    );
    assert_eq!(
        store
            .get(&mod_budget_breach_count_path("diag_budget_mod"))
            .and_then(|d| d.value()),
        Some(1.0)
    );
    assert!(
        store
            .get(&mod_p99_time_path("diag_budget_mod"))
            .and_then(|d| d.value())
            .is_some()
    );
    assert_eq!(
        store
            .get(&mod_spawns_queued_path("diag_budget_mod"))
            .and_then(|d| d.value()),
        Some(2.0)
    );
    assert_eq!(
        store
            .get(&mod_arena_bytes_peak_path("diag_budget_mod"))
            .and_then(|d| d.value()),
        Some(128.0)
    );
}

#[test]
fn policy_override_precedence_prefers_mod_id_then_name_then_defaults() {
    let mut policy = ModBudgetPolicy::default()
        .with_max_arena_bytes_per_system(Some(4096))
        .with_max_spawns_per_frame(Some(100))
        .with_max_p99_system_micros(Some(2000))
        .with_window_size(16)
        .with_max_consecutive_breaches(4)
        .with_mod_name_override(
            "alpha",
            ModBudgetLimits::default().with_max_spawns_per_frame(Some(25)),
        )
        .with_mod_override(
            ModId(7),
            ModBudgetLimits::default().with_max_spawns_per_frame(Some(5)),
        );

    assert_eq!(
        policy
            .effective_limits(Some(ModId(7)), Some("alpha"))
            .max_spawns_per_frame,
        Some(5)
    );
    assert_eq!(
        policy
            .effective_limits(Some(ModId(8)), Some("alpha"))
            .max_spawns_per_frame,
        Some(25)
    );
    assert_eq!(
        policy
            .effective_limits(Some(ModId(8)), Some("beta"))
            .max_spawns_per_frame,
        Some(100)
    );

    assert!(policy.clear_mod_override(ModId(7)).is_some());
    assert_eq!(
        policy
            .effective_limits(Some(ModId(7)), Some("alpha"))
            .max_spawns_per_frame,
        Some(25)
    );
    assert!(policy.clear_mod_name_override("alpha").is_some());
    assert_eq!(
        policy
            .effective_limits(Some(ModId(7)), Some("alpha"))
            .max_spawns_per_frame,
        Some(100)
    );
}

#[test]
fn budget_limits_zero_window_and_breaches_clamp_to_one() {
    let lim = ModBudgetLimits::unlimited()
        .with_window_size(0)
        .with_max_consecutive_breaches(0);
    assert_eq!(lim.effective_window_size(), 1);
    assert_eq!(lim.effective_max_consecutive_breaches(), 1);
}
