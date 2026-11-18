//! Host FFI integration tests for Task #11: Batch Commands & Persistent / Non-POD `Messages`:
//! - `HOST_API.cmd_spawn_batch` / `host_cmd_spawn_batch` (SoA columnar batch spawn + rollback)
//! - `HOST_API.cmd_insert_if_new` / `host_cmd_insert_if_new` (`InsertMode::Keep` + dynamic `drop_fn` accounting)
//! - `HOST_API.cmd_despawn_all` / `host_cmd_despawn_all` (`With`/`Without`/`Allow`/`Or`, `DefaultQueryFilters`, `IsResource` & `Observer` protection)
//! - `ModRegistry::register_non_pod_message` + VTable witness `export_fields` / `import_field` round-trip over `host_msg_read_batch` & `host_msg_send`

use crate::support::{Position, Velocity};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::drop_trampoline::drop_slot_live_values;
use bevy_abi_mod_host_sys::system::query_group::PersistentMessageCursors;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HOST_API, HostCommands, HostCommandsStorage, HostWireBridge,
    LoadContext, ModRegistry, host_cmd_despawn_all, host_cmd_insert_if_new, host_cmd_spawn_batch,
    host_msg_read_batch, host_msg_send, host_msg_write_batch, host_register_component,
    host_sys_commands, host_sys_query,
};
use bevy_abi_mod_sys::{
    AccessMode, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc,
    FfiBool, FfiEntity, FfiResult, FfiSlice, ModComponent as _, StrView,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::observer::On;
use bevy_ecs::prelude::{Event, Resource};
use bevy_ecs::system::System as BevySystem;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MAX_SPAWN_BATCH: usize = 64;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::BatchTag")]
struct BatchTag;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::FilterA")]
struct FilterA;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::FilterB")]
struct FilterB;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::FilterC")]
struct FilterC;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::FilterD")]
struct FilterD;

#[derive(Component, Clone, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "wave2_test::ChatMsg")]
struct ChatMsg {
    sender: String,
    tags: Vec<u32>,
    seq: u64,
}

impl Message for ChatMsg {}

#[derive(Resource, Default)]
struct ProtectedHostRes;

#[derive(Event)]
struct DummyObservedEvent;

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

// ============================================================================
// 1. `cmd_spawn_batch` (SoA columnar spawn, ZSTs, ChildOf, validation)
// ============================================================================

#[test]
fn cmd_spawn_batch_soa_populates_columns_and_validates_inputs() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let tag_h = app
        .register_mod_component::<BatchTag>(ComponentOptions::new())
        .unwrap();
    let child_of_h = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(bevy_abi_mod_sys::BAM_CHILD_OF_NAME)
        .expect("AbiModPlugin pre-registers ChildOf")
        .handle;

    let parent_ent = app.world_mut().spawn_empty().id();
    let parent_ffi = FfiEntity::from_bits(parent_ent.to_bits());

    #[derive(Default)]
    struct BatchState {
        pos_h: BamComponent,
        vel_h: BamComponent,
        tag_h: BamComponent,
        child_of_h: BamComponent,
        parent_ffi: FfiEntity,
        spawned: Mutex<Vec<Entity>>,
        zero_rc: Mutex<FfiResult>,
        no_out_rc: Mutex<FfiResult>,
        dup_rc: Mutex<FfiResult>,
        cap_rc: Mutex<FfiResult>,
        bad_child_rc: Mutex<FfiResult>,
    }

    unsafe extern "C" fn spawn_batch_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let st = unsafe { &*(user_data as *const BatchState) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
        if rc.is_err() {
            return rc;
        }

        const N: usize = 64;
        let positions: Vec<Position> = (0..N)
            .map(|i| Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: -1.0,
            })
            .collect();
        let velocities: Vec<Velocity> = (0..N)
            .map(|i| Velocity {
                x: 0.5,
                y: i as f32 * 0.25,
                z: 0.0,
            })
            .collect();
        let parents = vec![st.parent_ffi; N];
        let mut out_entities = vec![FfiEntity::NULL; N];

        let comps = [st.pos_h, st.vel_h, st.tag_h, st.child_of_h];
        let ptrs = [
            positions.as_ptr().cast::<u8>(),
            velocities.as_ptr().cast::<u8>(),
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const(),
            parents.as_ptr().cast::<u8>(),
        ];
        let spawn_fn = HOST_API.cmd_spawn_batch.expect("HOST_API.cmd_spawn_batch");
        let rc = unsafe {
            spawn_fn(
                cmds,
                comps.as_ptr(),
                ptrs.as_ptr(),
                comps.len(),
                N,
                out_entities.as_mut_ptr(),
            )
        };
        if rc.is_err() {
            return rc;
        }
        *st.spawned.lock().unwrap() = out_entities
            .iter()
            .map(|e| Entity::from_bits(e.to_bits()))
            .collect();

        // `entity_count == 0` succeeds as a no-op.
        *st.zero_rc.lock().unwrap() = unsafe {
            host_cmd_spawn_batch(
                cmds,
                comps.as_ptr(),
                ptrs.as_ptr(),
                comps.len(),
                0,
                core::ptr::null_mut(),
            )
        };

        // `out_entities == null` with `entity_count > 0` still spawns cleanly.
        let one_pos = [Position {
            x: 777.0,
            y: 888.0,
            z: 999.0,
        }];
        let one_comp = [st.pos_h];
        let one_ptr = [one_pos.as_ptr().cast::<u8>()];
        *st.no_out_rc.lock().unwrap() = unsafe {
            host_cmd_spawn_batch(
                cmds,
                one_comp.as_ptr(),
                one_ptr.as_ptr(),
                1,
                1,
                core::ptr::null_mut(),
            )
        };

        // Duplicate component rejected.
        let dup_comps = [st.pos_h, st.pos_h];
        let dup_ptrs = [one_pos.as_ptr().cast::<u8>(), one_pos.as_ptr().cast::<u8>()];
        *st.dup_rc.lock().unwrap() = unsafe {
            host_cmd_spawn_batch(
                cmds,
                dup_comps.as_ptr(),
                dup_ptrs.as_ptr(),
                2,
                1,
                core::ptr::null_mut(),
            )
        };

        // Over MAX_SPAWN_BATCH_COMPONENTS rejected.
        let over_comps = [st.pos_h; MAX_SPAWN_BATCH + 1];
        let over_ptrs = [one_pos.as_ptr().cast::<u8>(); MAX_SPAWN_BATCH + 1];
        *st.cap_rc.lock().unwrap() = unsafe {
            host_cmd_spawn_batch(
                cmds,
                over_comps.as_ptr(),
                over_ptrs.as_ptr(),
                over_comps.len(),
                1,
                core::ptr::null_mut(),
            )
        };

        // Invalid ChildOf entity rejected before allocating entities.
        let bad_parents = [FfiEntity::NULL];
        let bad_comps = [st.child_of_h];
        let bad_ptrs = [bad_parents.as_ptr().cast::<u8>()];
        *st.bad_child_rc.lock().unwrap() = unsafe {
            host_cmd_spawn_batch(
                cmds,
                bad_comps.as_ptr(),
                bad_ptrs.as_ptr(),
                1,
                1,
                core::ptr::null_mut(),
            )
        };

        FfiResult::OK
    }

    let state = Arc::new(BatchState {
        pos_h,
        vel_h,
        tag_h,
        child_of_h,
        parent_ffi,
        ..Default::default()
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "spawn_batch_test",
        vec![],
        spawn_batch_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    assert_eq!(*state.zero_rc.lock().unwrap(), FfiResult::OK);
    assert_eq!(*state.no_out_rc.lock().unwrap(), FfiResult::OK);
    assert_eq!(*state.dup_rc.lock().unwrap(), FfiResult::INVALID_ARGUMENT);
    assert_eq!(*state.cap_rc.lock().unwrap(), FfiResult::CAPACITY_EXCEEDED);
    assert_eq!(
        *state.bad_child_rc.lock().unwrap(),
        FfiResult::INVALID_ENTITY
    );

    let spawned = state.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 64);
    for (i, &ent) in spawned.iter().enumerate() {
        let er = app.world().entity(ent);
        assert_eq!(
            er.get::<Position>(),
            Some(&Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: -1.0,
            })
        );
        assert_eq!(
            er.get::<Velocity>(),
            Some(&Velocity {
                x: 0.5,
                y: i as f32 * 0.25,
                z: 0.0,
            })
        );
        assert!(er.contains::<BatchTag>());
        assert_eq!(er.get::<ChildOf>().map(|c| c.parent()), Some(parent_ent));
    }

    // Verify the `out_entities = null` spawn landed too.
    let mut found_777 = false;
    let mut q = app.world_mut().query::<&Position>();
    for pos in q.iter(app.world()) {
        if pos.x == 777.0 {
            found_777 = true;
        }
    }
    assert!(found_777);
}

// ============================================================================
// 2. `cmd_insert_if_new` + dynamic `drop_fn` cleanup & `live_values` parity
// ============================================================================

#[test]
fn cmd_insert_if_new_preserves_existing_and_drops_skipped_dynamic_payloads() {
    static DROP_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn track_drop(ptr: *mut c_void) {
        assert!(!ptr.is_null());
        DROP_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    DROP_CALLS.store(0, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();

    let dyn_name = "wave2_test::TrackedDynamic";
    let dyn_uuid = ComponentUuid::from_parts(dyn_name, 8, 8, 1);
    let mut dyn_h = BamComponent::INVALID;
    {
        let reg = app.world().resource::<ModRegistry>().clone();
        let active = Arc::new(AtomicBool::new(true));
        let mut lctx = LoadContext::with_world_and_mod_id(
            reg,
            active,
            app.world_mut() as *mut bevy_ecs::world::World,
            None,
        );
        let desc = ComponentDesc::new(c"wave2_test::TrackedDynamic".as_ptr(), dyn_uuid, 8, 8)
            .with_drop(track_drop);
        assert_eq!(
            unsafe { host_register_component(lctx.as_ffi_ptr(), &desc, &mut dyn_h) },
            FfiResult::OK
        );
        let updated = lctx.registry.clone();
        lctx.finish_init();
        *app.world_mut().resource_mut::<ModRegistry>() = updated;
    }

    let dyn_meta = app
        .world()
        .resource::<ModRegistry>()
        .get_by_handle(dyn_h)
        .unwrap()
        .clone();
    let drop_slot = dyn_meta.drop_slot.expect("dynamic drop_slot");
    let baseline_live = drop_slot_live_values(drop_slot).unwrap();

    let e_with = app
        .world_mut()
        .spawn(Position {
            x: 10.0,
            y: 20.0,
            z: 30.0,
        })
        .id();
    let e_without = app.world_mut().spawn_empty().id();
    let e_dead = app.world_mut().spawn_empty().id();

    struct InsertIfNewCtx {
        pos_h: BamComponent,
        dyn_h: BamComponent,
        e_with: Entity,
        e_without: Entity,
        e_dead: Entity,
        phase: AtomicUsize,
    }

    unsafe extern "C" fn insert_if_new_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const InsertIfNewCtx) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
        if rc.is_err() {
            return rc;
        }
        let phase = ctx.phase.fetch_add(1, Ordering::SeqCst);
        if phase == 0 {
            let init_val: u64 = 111;
            return unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_with.to_bits()),
                    ctx.dyn_h,
                    (&raw const init_val).cast::<u8>(),
                    8,
                )
            };
        }

        let overwrite_pos = Position {
            x: 999.0,
            y: 999.0,
            z: 999.0,
        };
        let insert_pos = Position {
            x: 42.0,
            y: 43.0,
            z: 44.0,
        };
        let dyn_skip: u64 = 222;
        let dyn_keep: u64 = 333;
        let dyn_dead: u64 = 444;

        // `e_with` already has Position and TrackedDynamic -> both skipped; `dyn_skip` dropped!
        assert_eq!(
            unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_with.to_bits()),
                    ctx.pos_h,
                    (&raw const overwrite_pos).cast::<u8>(),
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_with.to_bits()),
                    ctx.dyn_h,
                    (&raw const dyn_skip).cast::<u8>(),
                    8,
                )
            },
            FfiResult::OK
        );

        // `e_without` lacks both -> both inserted!
        assert_eq!(
            unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_without.to_bits()),
                    ctx.pos_h,
                    (&raw const insert_pos).cast::<u8>(),
                    core::mem::size_of::<Position>(),
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_without.to_bits()),
                    ctx.dyn_h,
                    (&raw const dyn_keep).cast::<u8>(),
                    8,
                )
            },
            FfiResult::OK
        );

        // `e_dead` does not exist -> skipped and `dyn_dead` dropped!
        assert_eq!(
            unsafe {
                host_cmd_insert_if_new(
                    cmds,
                    FfiEntity::from_bits(ctx.e_dead.to_bits()),
                    ctx.dyn_h,
                    (&raw const dyn_dead).cast::<u8>(),
                    8,
                )
            },
            FfiResult::OK
        );
        FfiResult::OK
    }

    let state = Arc::new(InsertIfNewCtx {
        pos_h,
        dyn_h,
        e_with,
        e_without,
        e_dead,
        phase: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "insert_if_new_test",
        vec![],
        insert_if_new_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg.clone(),
    );
    app.add_systems(Update, sys);

    // Phase 0: initial insert_if_new on `e_with`.
    app.update();
    assert_eq!(DROP_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(drop_slot_live_values(drop_slot).unwrap(), baseline_live + 1);

    // Despawn `e_dead` so phase 1 tests missing-entity uncommitted cleanup.
    app.world_mut().despawn(e_dead);

    // Also test uncommitted `cmd_spawn_batch` with dynamic component cleared before apply.
    {
        let snap = reg.snapshot();
        let alloc = app.world().entity_allocator();
        let mut batch_storage = HostCommandsStorage::default();
        let batch_vals = [555u64, 666u64];
        {
            let mut bcmds = HostCommands::new(&mut batch_storage, alloc, snap);
            let comps = [dyn_h];
            let ptrs = [batch_vals.as_ptr().cast::<u8>()];
            assert_eq!(
                unsafe {
                    host_cmd_spawn_batch(
                        bcmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        2,
                        core::ptr::null_mut(),
                    )
                },
                FfiResult::OK
            );
        }
        batch_storage.clear();
        assert_eq!(DROP_CALLS.load(Ordering::SeqCst), 2);
    }

    // Phase 1: insert_if_new across existing, absent, and dead targets.
    app.update();

    assert_eq!(
        app.world().entity(e_with).get::<Position>(),
        Some(&Position {
            x: 10.0,
            y: 20.0,
            z: 30.0
        })
    );
    assert_eq!(
        app.world().entity(e_without).get::<Position>(),
        Some(&Position {
            x: 42.0,
            y: 43.0,
            z: 44.0
        })
    );
    assert!(
        app.world()
            .entity(e_without)
            .contains_id(dyn_meta.component_id)
    );

    // 2 from cleared SoA batch + 2 from skipped `insert_if_new` (`e_with`, `e_dead`) = 4.
    assert_eq!(DROP_CALLS.load(Ordering::SeqCst), 4);
    assert_eq!(drop_slot_live_values(drop_slot).unwrap(), baseline_live + 2);

    // Despawning both live entities drops the 2 remaining live values cleanly without underflow.
    app.world_mut().despawn(e_with);
    app.world_mut().despawn(e_without);
    assert_eq!(DROP_CALLS.load(Ordering::SeqCst), 6);
    assert_eq!(drop_slot_live_values(drop_slot).unwrap(), baseline_live);
}

// ============================================================================
// 3. `cmd_despawn_all` (`With`/`Without`/`Allow`/`Or`, `IsResource` & `Observer` protection)
// ============================================================================

#[test]
fn cmd_despawn_all_respects_filters_allow_or_groups_and_protects_resources_observers() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let a_h = app
        .register_mod_component::<FilterA>(ComponentOptions::new())
        .unwrap();
    let b_h = app
        .register_mod_component::<FilterB>(ComponentOptions::new())
        .unwrap();
    let c_h = app
        .register_mod_component::<FilterC>(ComponentOptions::new())
        .unwrap();
    let d_h = app
        .register_mod_component::<FilterD>(ComponentOptions::new())
        .unwrap();
    let disabled_h = register_disabled_bridge(&mut app);

    // Spawn an observer entity (also carrying FilterA + FilterB!) and a resource entity
    // (also carrying FilterA + FilterB!). `despawn_all` MUST NEVER despawn either.
    let observer_ent = app
        .world_mut()
        .add_observer(|_on: On<DummyObservedEvent>| {})
        .id();
    app.world_mut()
        .entity_mut(observer_ent)
        .insert((FilterA, FilterB));
    app.world_mut().init_resource::<ProtectedHostRes>();
    let res_cid = app
        .world()
        .components()
        .component_id::<ProtectedHostRes>()
        .unwrap();
    let res_ent = app
        .world()
        .resource_entities()
        .get(res_cid)
        .expect("ProtectedHostRes resource entity");
    app.world_mut()
        .entity_mut(res_ent)
        .insert((FilterA, FilterB));

    let e_match_b = app.world_mut().spawn((FilterA, FilterB)).id();
    let e_match_c = app.world_mut().spawn((FilterA, FilterC)).id();
    let e_excluded_d = app.world_mut().spawn((FilterA, FilterB, FilterD)).id();
    let e_only_a = app.world_mut().spawn(FilterA).id();
    let e_disabled_ab = app.world_mut().spawn((FilterA, FilterB, Disabled)).id();

    struct DespawnCtx {
        a_h: BamComponent,
        b_h: BamComponent,
        c_h: BamComponent,
        d_h: BamComponent,
        disabled_h: BamComponent,
        phase: AtomicUsize,
        empty_rc: Mutex<FfiResult>,
        write_rc: Mutex<FfiResult>,
        allow_or_rc: Mutex<FfiResult>,
    }

    unsafe extern "C" fn despawn_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const DespawnCtx) };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
        if rc.is_err() {
            return rc;
        }
        let phase = ctx.phase.fetch_add(1, Ordering::SeqCst);
        if phase == 0 {
            *ctx.empty_rc.lock().unwrap() =
                unsafe { host_cmd_despawn_all(cmds, core::ptr::null(), 0) };
            let bad_write = [BamTermDesc::new(ctx.a_h, AccessMode::WRITE)];
            *ctx.write_rc.lock().unwrap() =
                unsafe { host_cmd_despawn_all(cmds, bad_write.as_ptr(), 1) };
            let bad_allow_or =
                [BamTermDesc::new(ctx.disabled_h, AccessMode::ALLOW).with_or_group(1)];
            *ctx.allow_or_rc.lock().unwrap() =
                unsafe { host_cmd_despawn_all(cmds, bad_allow_or.as_ptr(), 1) };

            let terms = [
                BamTermDesc::new(ctx.a_h, AccessMode::FILTER_ONLY),
                BamTermDesc::new(ctx.d_h, AccessMode::WITHOUT),
                BamTermDesc::new(ctx.b_h, AccessMode::FILTER_ONLY).with_or_group(1),
                BamTermDesc::new(ctx.c_h, AccessMode::FILTER_ONLY).with_or_group(1),
            ];
            return unsafe { host_cmd_despawn_all(cmds, terms.as_ptr(), terms.len()) };
        }

        let terms = [
            BamTermDesc::new(ctx.a_h, AccessMode::FILTER_ONLY),
            BamTermDesc::new(ctx.b_h, AccessMode::FILTER_ONLY),
            BamTermDesc::new(ctx.disabled_h, AccessMode::ALLOW),
        ];
        unsafe { host_cmd_despawn_all(cmds, terms.as_ptr(), terms.len()) }
    }

    let state = Arc::new(DespawnCtx {
        a_h,
        b_h,
        c_h,
        d_h,
        disabled_h,
        phase: AtomicUsize::new(0),
        empty_rc: Mutex::new(FfiResult::OK),
        write_rc: Mutex::new(FfiResult::OK),
        allow_or_rc: Mutex::new(FfiResult::OK),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "despawn_all_test",
        vec![],
        despawn_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);

    // Pass 1: Despawn `With<FilterA>, Without<FilterD>, Or<(With<FilterB>, With<FilterC>)>`
    // (DefaultQueryFilters leaves `e_disabled_ab` alive; IsResource & Observer stay alive).
    app.update();
    assert_eq!(*state.empty_rc.lock().unwrap(), FfiResult::INVALID_ARGUMENT);
    assert_eq!(*state.write_rc.lock().unwrap(), FfiResult::INVALID_ARGUMENT);
    assert_eq!(
        *state.allow_or_rc.lock().unwrap(),
        FfiResult::INVALID_ARGUMENT
    );

    assert!(app.world().get_entity(e_match_b).is_err());
    assert!(app.world().get_entity(e_match_c).is_err());
    assert!(app.world().get_entity(e_excluded_d).is_ok());
    assert!(app.world().get_entity(e_only_a).is_ok());
    assert!(app.world().get_entity(e_disabled_ab).is_ok());
    assert!(app.world().get_entity(observer_ent).is_ok());
    assert!(app.world().get_entity(res_ent).is_ok());

    // Pass 2: Despawn `With<FilterA>, With<FilterB>, Allow<Disabled>` -> hits `e_disabled_ab`
    // while STILL sparing `observer_ent` and `res_ent`!
    app.update();

    assert!(app.world().get_entity(e_disabled_ab).is_err());
    assert!(app.world().get_entity(e_excluded_d).is_err());
    assert!(app.world().get_entity(e_only_a).is_ok());
    assert!(app.world().get_entity(observer_ent).is_ok());
    assert!(app.world().get_entity(res_ent).is_ok());
}

// ============================================================================
// 4. VTable-backed Non-POD `Messages<ChatMsg>` (`String` + `Vec<u32>`)
// ============================================================================

fn encode_witness_packet<T: bevy_abi_mod_sys::ModComponent>(msg: &T) -> Vec<u8> {
    type ExportFieldsFn = unsafe extern "C" fn(
        src_ptr: *const c_void,
        sink: Option<
            unsafe extern "C" fn(
                sink_ctx: *mut c_void,
                field_name: StrView,
                type_tag: StrView,
                payload: FfiSlice<u8>,
            ) -> FfiResult,
        >,
        sink_ctx: *mut c_void,
    ) -> FfiResult;

    let sym_name = format!("bam.v0.{}.export_fields", T::CANONICAL_NAME);
    let ptr = T::resolve_symbol(&sym_name).expect("export_fields symbol");
    let export_fn: ExportFieldsFn = unsafe { core::mem::transmute(ptr) };

    struct PacketAcc {
        buf: Vec<u8>,
        count: u32,
    }

    unsafe extern "C" fn sink(
        ctx: *mut c_void,
        name: StrView,
        tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult {
        let acc = unsafe { &mut *ctx.cast::<PacketAcc>() };
        let nb = unsafe { name.try_as_slice().unwrap() };
        let tb = unsafe { tag.try_as_slice().unwrap() };
        let vb = unsafe { payload.try_as_slice().unwrap() };
        acc.count += 1;
        acc.buf.extend_from_slice(&(nb.len() as u32).to_le_bytes());
        acc.buf.extend_from_slice(nb);
        acc.buf.extend_from_slice(&(tb.len() as u32).to_le_bytes());
        acc.buf.extend_from_slice(tb);
        acc.buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        acc.buf.extend_from_slice(vb);
        FfiResult::OK
    }

    let mut acc = PacketAcc {
        buf: vec![0u8; 4],
        count: 0,
    };
    let rc = unsafe {
        export_fn(
            core::ptr::from_ref(msg).cast::<c_void>(),
            Some(sink),
            (&raw mut acc).cast::<c_void>(),
        )
    };
    assert_eq!(rc, FfiResult::OK);
    acc.buf[0..4].copy_from_slice(&acc.count.to_le_bytes());
    acc.buf
}

fn decode_host_chat_msg(host_ptr: *const c_void) -> ChatMsg {
    type ExportFieldsFn = unsafe extern "C" fn(
        src_ptr: *const c_void,
        sink: Option<
            unsafe extern "C" fn(
                sink_ctx: *mut c_void,
                field_name: StrView,
                type_tag: StrView,
                payload: FfiSlice<u8>,
            ) -> FfiResult,
        >,
        sink_ctx: *mut c_void,
    ) -> FfiResult;
    type ImportFieldFn = unsafe extern "C" fn(
        dst_ptr: *mut c_void,
        field_name: StrView,
        type_tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult;

    let export_ptr =
        ChatMsg::resolve_symbol(&format!("bam.v0.{}.export_fields", ChatMsg::CANONICAL_NAME))
            .unwrap();
    let import_ptr =
        ChatMsg::resolve_symbol(&format!("bam.v0.{}.import_field", ChatMsg::CANONICAL_NAME))
            .unwrap();
    let export_fn: ExportFieldsFn = unsafe { core::mem::transmute(export_ptr) };
    let import_fn: ImportFieldFn = unsafe { core::mem::transmute(import_ptr) };

    struct Ctx {
        dst: *mut c_void,
        import_fn: ImportFieldFn,
    }
    unsafe extern "C" fn fwd(
        ctx: *mut c_void,
        name: StrView,
        tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult {
        let c = unsafe { &*ctx.cast::<Ctx>() };
        unsafe { (c.import_fn)(c.dst, name, tag, payload) }
    }

    let mut out = ChatMsg::default();
    let mut ctx = Ctx {
        dst: (&raw mut out).cast::<c_void>(),
        import_fn,
    };
    let rc = unsafe { export_fn(host_ptr, Some(fwd), (&raw mut ctx).cast::<c_void>()) };
    assert_eq!(rc, FfiResult::OK);
    out
}

#[test]
fn non_pod_messages_round_trip_and_batch_cursors_work_across_ffi() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.world_mut().init_resource::<Messages<ChatMsg>>();
    let msg_res_id = app
        .world()
        .components()
        .component_id::<Messages<ChatMsg>>()
        .expect("Messages<ChatMsg> id");

    let msg_h = app
        .world_mut()
        .resource_mut::<ModRegistry>()
        .register_non_pod_message::<ChatMsg>(msg_res_id)
        .expect("register non-POD ChatMsg");

    // Verify `ModRegistry::insert_mod_component_symbols` published witness symbols.
    let snap = app.world().resource::<ModRegistry>().snapshot();
    assert!(
        snap.resolve_symbol("bam.v0.wave2_test::ChatMsg.export_fields")
            .is_some()
    );
    assert!(
        snap.resolve_symbol("bam.v0.wave2_test::ChatMsg.import_field")
            .is_some()
    );
    assert!(!snap.get_by_handle(msg_h).unwrap().is_abi_pod);

    // Host writes 3 non-POD ChatMsgs.
    {
        let mut msgs = app.world_mut().resource_mut::<Messages<ChatMsg>>();
        msgs.write(ChatMsg {
            sender: "alice".to_owned(),
            tags: vec![1, 2],
            seq: 10,
        });
        msgs.write(ChatMsg {
            sender: "bob".to_owned(),
            tags: vec![3, 4, 5],
            seq: 20,
        });
        msgs.write(ChatMsg {
            sender: "carol".to_owned(),
            tags: vec![],
            seq: 30,
        });
    }

    #[derive(Default)]
    struct MsgTestState {
        read_msgs: Mutex<Vec<ChatMsg>>,
        write_batch_rc: Mutex<FfiResult>,
        bad_pkt_rc: Mutex<FfiResult>,
        bad_utf8_rc: Mutex<FfiResult>,
    }

    unsafe extern "C" fn reader_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let st = unsafe { &*(user_data as *const MsgTestState) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        // Batch 1: read at most 2 pointers.
        let mut ptrs = [0usize; 2];
        let mut count = 0usize;
        let rc =
            unsafe { host_msg_read_batch(iter, 0, ptrs.as_mut_ptr().cast::<u8>(), 2, &mut count) };
        if rc.is_err() {
            return rc;
        }
        assert_eq!(count, 2);
        let mut out = st.read_msgs.lock().unwrap();
        for &p in &ptrs[..count] {
            out.push(decode_host_chat_msg(p as *const c_void));
        }

        // Batch 2: read remaining 1 pointer without skipping any!
        let rc =
            unsafe { host_msg_read_batch(iter, 0, ptrs.as_mut_ptr().cast::<u8>(), 2, &mut count) };
        if rc.is_err() {
            return rc;
        }
        assert_eq!(count, 1);
        out.push(decode_host_chat_msg(ptrs[0] as *const c_void));
        FfiResult::OK
    }

    unsafe extern "C" fn writer_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let st = unsafe { &*(user_data as *const MsgTestState) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        // `host_msg_write_batch` is POD-only and must reject non-POD (`INVALID_COMPONENT`).
        let dummy = [0u8; 8];
        *st.write_batch_rc.lock().unwrap() =
            unsafe { host_msg_write_batch(iter, 0, dummy.as_ptr(), 1) };

        // Truncated packet rejected (`INVALID_ARGUMENT`).
        let truncated = [1u8, 0, 0, 0, 5, 0];
        *st.bad_pkt_rc.lock().unwrap() =
            unsafe { host_msg_send(iter, 0, truncated.as_ptr(), truncated.len()) };

        // Invalid UTF-8 field name rejected (`INVALID_UTF8`).
        let mut bad_utf8 = Vec::new();
        bad_utf8.extend_from_slice(&1u32.to_le_bytes());
        bad_utf8.extend_from_slice(&1u32.to_le_bytes());
        bad_utf8.push(0xff);
        *st.bad_utf8_rc.lock().unwrap() =
            unsafe { host_msg_send(iter, 0, bad_utf8.as_ptr(), bad_utf8.len()) };

        let outgoing = ChatMsg {
            sender: "guest_mod".to_owned(),
            tags: vec![42, 77, 99],
            seq: 9001,
        };
        let pkt = encode_witness_packet(&outgoing);
        unsafe { host_msg_send(iter, 0, pkt.as_ptr(), pkt.len()) }
    }

    let state = Arc::new(MsgTestState::default());
    let reg = app.world().resource::<ModRegistry>().clone();

    let mut sys_read = GuestSystem::new(
        "chat_reader",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            msg_h,
            AccessMode::MESSAGE_READ,
        )])],
        reader_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg.clone(),
    );
    let mut sys_write = GuestSystem::new(
        "chat_writer",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            msg_h,
            AccessMode::MESSAGE_WRITE,
        )])],
        writer_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );

    sys_read.initialize(app.world_mut());
    sys_write.initialize(app.world_mut());
    let _ = sys_read.run((), app.world_mut());
    let _ = sys_write.run((), app.world_mut());

    let read_msgs = state.read_msgs.lock().unwrap();
    assert_eq!(
        *read_msgs,
        vec![
            ChatMsg {
                sender: "alice".to_owned(),
                tags: vec![1, 2],
                seq: 10
            },
            ChatMsg {
                sender: "bob".to_owned(),
                tags: vec![3, 4, 5],
                seq: 20
            },
            ChatMsg {
                sender: "carol".to_owned(),
                tags: vec![],
                seq: 30
            },
        ]
    );
    assert_eq!(
        *state.write_batch_rc.lock().unwrap(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        *state.bad_pkt_rc.lock().unwrap(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(*state.bad_utf8_rc.lock().unwrap(), FfiResult::INVALID_UTF8);

    // Host observes the 1 valid non-POD message sent by guest_mod (`seq == 9001`).
    let mut host_cursor = bevy_ecs::message::MessageCursor::<ChatMsg>::default();
    let msgs = app.world().resource::<Messages<ChatMsg>>();
    let all: Vec<ChatMsg> = host_cursor.read(msgs).cloned().collect();
    assert_eq!(all.len(), 4);
    assert_eq!(
        all[3],
        ChatMsg {
            sender: "guest_mod".to_owned(),
            tags: vec![42, 77, 99],
            seq: 9001,
        }
    );

    let _ = core::any::TypeId::of::<PersistentMessageCursors>();
}
