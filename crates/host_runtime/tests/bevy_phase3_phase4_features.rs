#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use abi_contract::{
    ComponentAccessDesc, ComponentUuid, ConditionalSystemDescriptor, ExposableComponent, FfiBool,
    FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter, RawCommandQueue, StrView,
    SystemDescriptor, FFI_BOOL_FALSE, FFI_BOOL_TRUE,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::query::{Added, Changed};
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings};
use bevy_ecs::system::Res;
use common::{ensure_guest_mods_built, reset_rust_mod_panic};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use host_runtime::{
    host_cmd_insert_component, host_cmd_insert_default, host_cmd_remove_component,
    host_cmd_spawn_empty, host_get_proc_address, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_iter_resource_mutable, host_iter_resource_readonly, host_register_component,
    host_register_conditional_system, host_register_system, AbiModPlugin,
    ComponentCollisionError, ModAppExt, ModInitContext, ModRegistry,
};
use libloading::{Library, Symbol};
use mod_derive::ModExposable;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

// ============================================================================
// Shared Host ECS Components & Resources for Phase 3 + Phase 4 Tests
// ============================================================================

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Pos3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Vel3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Sensor {
    pub signal: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
#[component(storage = "SparseSet")]
pub struct SparseBoost {
    pub factor: f32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct RemovableMarker {
    pub tag: u32,
}

#[derive(Component, ModExposable, Debug, Clone, PartialEq, Default)]
pub struct DefaultableTag {
    pub label: String,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq)]
pub struct NoDefaultComp {
    pub code: u64,
}

#[repr(C)]
#[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct SimConfig {
    pub dt: f32,
    pub step_count: u32,
    pub banner_len_observed: u32,
}

#[derive(Resource, ModExposable, Debug, Clone, PartialEq, Default)]
pub struct WorldBanner {
    pub title: String,
}

// ============================================================================
// Asymmetric Mod Interop Types (Exact Name & Layout Match with Rust + C99 Mods)
// ============================================================================

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4Transform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4CTransform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4Sensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4CSensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4FreshTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4CFreshTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4Removable {
    pub dummy: u32,
}

#[repr(C)]
#[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4CRemovable {
    pub dummy: u32,
}

#[derive(Component, ModExposable, Debug, Clone, PartialEq, Default)]
pub struct NameTag {
    pub label: String,
}

#[repr(C)]
#[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4RustRes {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub spawned_default_entity: u64,
}

#[repr(C)]
#[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
pub struct Phase4CRes {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub spawned_default_entity: u64,
}

fn register_inline_system(app: &mut App, desc: &SystemDescriptor) -> FfiResult {
    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx = ModInitContext::new(registry);
    ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
    let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
    // SAFETY: `ctx_ptr` and `desc` are valid stack pointers for the duration of registration.
    let status = unsafe { host_register_system(ctx_ptr, desc) };
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

fn register_inline_conditional_system(
    app: &mut App,
    desc: &ConditionalSystemDescriptor,
) -> FfiResult {
    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut ctx = ModInitContext::new(registry);
    ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
    let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
    // SAFETY: `ctx_ptr` and `desc` are valid stack pointers for the duration of registration.
    let status = unsafe { host_register_conditional_system(ctx_ptr, desc) };
    for sys in ctx.registered_systems.drain(..) {
        app.add_systems(Update, sys);
    }
    status
}

// ============================================================================
// 1. OPTIONAL_READ & OPTIONAL_WRITE Across Dense Table + SparseSet Archetypes
// ============================================================================

#[test]
fn test_optional_read_and_optional_write_across_table_and_sparse_archetypes() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();
    app.register_mod_component::<Vel3>();
    app.register_mod_component::<SparseBoost>();

    // Archetype A: (Pos3, Vel3)
    let e_pos_vel = app
        .world_mut()
        .spawn((
            Pos3 {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            Vel3 {
                x: 5.0,
                y: 0.0,
                z: 0.0,
            },
        ))
        .id();

    // Archetype B: (Pos3,) -- missing BOTH Vel3 and SparseBoost!
    let e_pos_only = app
        .world_mut()
        .spawn(Pos3 {
            x: 100.0,
            y: 0.0,
            z: 0.0,
        })
        .id();

    // Archetype C: (Pos3, Vel3, SparseBoost) -- has dense Vel3 AND SparseSet SparseBoost!
    let e_all = app
        .world_mut()
        .spawn((
            Pos3 {
                x: 1000.0,
                y: 0.0,
                z: 0.0,
            },
            Vel3 {
                x: 2.0,
                y: 0.0,
                z: 0.0,
            },
            SparseBoost { factor: 3.0 },
        ))
        .id();

    // Track which entities hadChanged<SparseBoost> in a post-check Bevy system
    let changed_boost_entities: Arc<Mutex<Vec<Entity>>> = Arc::new(Mutex::new(Vec::new()));

    unsafe extern "C" fn optional_kinematics_cb(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            // SAFETY: Valid `iter` passed by `DynamicCSystem`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            // Column 1 is OPTIONAL_READ (`Vel3`): mutable borrow MUST be rejected!
            // SAFETY: Valid `iter`.
            assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());

            // SAFETY: Column 0 is WRITE `Pos3`, Column 1 is OPTIONAL_READ `Vel3`,
            // Column 2 is OPTIONAL_WRITE `SparseBoost`.
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
            let opt_vel_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const Vel3;
            let opt_boost_ptr = unsafe { host_iter_column_mutable(iter, 2) } as *mut SparseBoost;
            assert!(!pos_ptr.is_null());

            for i in 0..count {
                // SAFETY: `i < count` within active chunk bounds.
                unsafe {
                    let vx = if !opt_vel_ptr.is_null() {
                        (*opt_vel_ptr.add(i)).x
                    } else {
                        1.0
                    };
                    let mult = if !opt_boost_ptr.is_null() {
                        let b = &mut *opt_boost_ptr.add(i);
                        let old = b.factor;
                        b.factor += 1.0;
                        old
                    } else {
                        1.0
                    };
                    (*pos_ptr.add(i)).x += vx * mult;
                }
            }
        }
        FfiResult::OK
    }

    // Prime initial spawn ticks with one update BEFORE registering the guest system so we can
    // isolate SparseBoost change ticks caused strictly by `OPTIONAL_WRITE`.
    app.update();

    let queries = [
        ComponentAccessDesc::write(c"Pos3".as_ptr()),
        ComponentAccessDesc::optional_read(c"Vel3".as_ptr()),
        ComponentAccessDesc::optional_write(c"SparseBoost".as_ptr()),
    ];
    let desc = SystemDescriptor::new(
        c"optional_kinematics".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        optional_kinematics_cb,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

    let boost_log = Arc::clone(&changed_boost_entities);
    app.add_systems(
        Update,
        move |q: bevy_ecs::system::Query<Entity, Changed<SparseBoost>>| {
            let mut guard = boost_log.lock().unwrap();
            guard.clear();
            for e in &q {
                guard.push(e);
            }
        },
    );

    app.update();

    let world = app.world();
    // Archetype A: 10.0 + 5.0 * 1.0 = 15.0
    assert_eq!(world.get::<Pos3>(e_pos_vel).unwrap().x, 15.0);
    // Archetype B (no Vel3, no SparseBoost): 100.0 + 1.0 * 1.0 = 101.0
    assert_eq!(world.get::<Pos3>(e_pos_only).unwrap().x, 101.0);
    // Archetype C (Vel3=2.0, SparseBoost=3.0 -> 4.0): 1000.0 + 2.0 * 3.0 = 1006.0
    assert_eq!(world.get::<Pos3>(e_all).unwrap().x, 1006.0);
    assert_eq!(world.get::<SparseBoost>(e_all).unwrap().factor, 4.0);
    assert_eq!(*changed_boost_entities.lock().unwrap(), vec![e_all]);
}

// ============================================================================
// 2. CHANGED & ADDED Row-Level Filtering, Chunk Coalescing & Differential Parity
// ============================================================================

#[derive(Default)]
struct TickFilterLog {
    changed_entities: Vec<u64>,
    chunk_lengths: Vec<usize>,
    added_entities: Vec<u64>,
}

#[test]
fn test_changed_and_added_row_level_coalescing_and_differential_bevy_parity() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();
    app.register_mod_component::<Vel3>();
    app.register_mod_component::<Sensor>();
    app.register_mod_component::<SparseBoost>();

    let mut entities = Vec::new();
    for i in 0..12u32 {
        let e = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Sensor { signal: i },
            ))
            .id();
        entities.push(e);
    }

    let c_abi_log = Arc::new(Mutex::new(TickFilterLog::default()));
    let native_changed_log: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let native_added_log: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));

    unsafe extern "C" fn changed_pos_cb(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `Mutex<TickFilterLog>` valid for the test lifetime.
        let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
        let mut guard = log_mutex.lock().unwrap();
        loop {
            // SAFETY: Valid `iter`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            guard.chunk_lengths.push(count);
            // Column 0 is WRITE Pos3, Column 1 is CHANGED Pos3 (non-data tick filter when paired with WRITE!)
            // SAFETY: Valid `iter`.
            assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
            let ents = unsafe { host_iter_entities(iter) };
            let pos_mut = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
            assert!(!ents.is_null() && !pos_mut.is_null());
            for row in 0..count {
                // SAFETY: `row < count`.
                unsafe {
                    guard.changed_entities.push((*ents.add(row)).0);
                    // Mutate y on matched changed rows — must NOT self-retrigger on the next frame!
                    (*pos_mut.add(row)).y += 1.0;
                }
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn added_sensor_cb(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `Mutex<TickFilterLog>` valid for the test lifetime.
        let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
        let mut guard = log_mutex.lock().unwrap();
        loop {
            // SAFETY: Valid `iter`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let ents = unsafe { host_iter_entities(iter) };
            let sensor_ro = unsafe { host_iter_column_readonly(iter, 0) } as *const Sensor;
            assert!(!ents.is_null() && !sensor_ro.is_null());
            for row in 0..count {
                // SAFETY: `row < count`.
                unsafe {
                    let _sig = (*sensor_ro.add(row)).signal;
                    guard.added_entities.push((*ents.add(row)).0);
                }
            }
        }
        FfiResult::OK
    }

    let log_ptr = Arc::as_ptr(&c_abi_log) as *mut c_void;

    // System A: `[WRITE(Pos3), CHANGED(Pos3)]` (self-compatible mutable + Changed<Pos3>!)
    let changed_queries = [
        ComponentAccessDesc::write(c"Pos3".as_ptr()),
        ComponentAccessDesc::changed(c"Pos3".as_ptr()),
    ];
    let changed_desc = SystemDescriptor::new(
        c"changed_pos_sys".as_ptr(),
        changed_queries.as_ptr(),
        changed_queries.len(),
        changed_pos_cb,
        log_ptr,
    );
    assert_eq!(register_inline_system(&mut app, &changed_desc), FfiResult::OK);

    // System B: `[ADDED(Sensor)]`
    let added_queries = [ComponentAccessDesc::added(c"Sensor".as_ptr())];
    let added_desc = SystemDescriptor::new(
        c"added_sensor_sys".as_ptr(),
        added_queries.as_ptr(),
        added_queries.len(),
        added_sensor_cb,
        log_ptr,
    );
    assert_eq!(register_inline_system(&mut app, &added_desc), FfiResult::OK);

    // Native Bevy reference systems for differential parity on `Added<Sensor>`
    let native_added_ref = Arc::clone(&native_added_log);
    app.add_systems(
        Update,
        move |q_added: bevy_ecs::system::Query<Entity, Added<Sensor>>| {
            let mut guard = native_added_ref.lock().unwrap();
            for e in &q_added {
                guard.insert(e.to_bits());
            }
        },
    );
    let _ = &native_changed_log;

    // Frame 1: All 12 initial entities match BOTH `Changed<Pos3>` (single 12-row chunk) AND `Added<Sensor>`!
    app.update();
    {
        let mut guard = c_abi_log.lock().unwrap();
        let c_changed: BTreeSet<u64> = guard.changed_entities.drain(..).collect();
        let c_added: BTreeSet<u64> = guard.added_entities.drain(..).collect();
        let chunks = std::mem::take(&mut guard.chunk_lengths);
        let expected_all: BTreeSet<u64> = entities.iter().map(|e| e.to_bits()).collect();
        assert_eq!(c_changed, expected_all);
        assert_eq!(chunks, vec![12]);
        let native_added = std::mem::take(&mut *native_added_log.lock().unwrap());
        assert_eq!(c_added, native_added);
        assert_eq!(c_added.len(), 12);
    }

    // Frame 2: No external mutations! Even though `changed_pos_sys` mutated `Pos3.y` in Frame 1,
    // its own `this_run` tick MUST NOT re-trigger `Changed<Pos3>` on Frame 2!
    app.update();
    {
        let guard = c_abi_log.lock().unwrap();
        assert!(
            guard.changed_entities.is_empty(),
            "System must NOT observe its own prior-frame mutation as Changed<Pos3>!"
        );
        assert!(guard.chunk_lengths.is_empty());
        assert!(guard.added_entities.is_empty());
        assert!(native_added_log.lock().unwrap().is_empty());
    }

    // Frame 3: Mutate non-contiguous row runs in the 12-row table:
    // rows [1, 2, 3] (len 3), row [7] (len 1), rows [10, 11] (len 2)!
    // Also mutate Sensor on row 5 (NOT Added!) and spawn 2 brand-new entities (Added + Changed)!
    let mutated_indices = [1usize, 2, 3, 7, 10, 11];
    for &idx in &mutated_indices {
        app.world_mut().get_mut::<Pos3>(entities[idx]).unwrap().x += 50.0;
    }
    app.world_mut().get_mut::<Sensor>(entities[5]).unwrap().signal = 999;

    let e_new_1 = app
        .world_mut()
        .spawn((
            Pos3 {
                x: 500.0,
                y: 0.0,
                z: 0.0,
            },
            Sensor { signal: 100 },
            Vel3::default(), // Different archetype/table so original 12-row table stays intact!
        ))
        .id();
    let e_new_2 = app
        .world_mut()
        .spawn((
            Pos3 {
                x: 600.0,
                y: 0.0,
                z: 0.0,
            },
            Sensor { signal: 101 },
            Vel3::default(),
        ))
        .id();

    app.update();
    {
        let mut guard = c_abi_log.lock().unwrap();
        let c_changed: BTreeSet<u64> = guard.changed_entities.drain(..).collect();
        let c_added: BTreeSet<u64> = guard.added_entities.drain(..).collect();
        let chunks = std::mem::take(&mut guard.chunk_lengths);

        let mut expected_changed: BTreeSet<u64> =
            mutated_indices.iter().map(|&i| entities[i].to_bits()).collect();
        expected_changed.insert(e_new_1.to_bits());
        expected_changed.insert(e_new_2.to_bits());

        assert_eq!(c_changed, expected_changed);
        // Original 12-row table coalesced into runs [3, 1, 2], plus new 2-row table [2]!
        assert_eq!(chunks, vec![3, 1, 2, 2]);

        let native_added = std::mem::take(&mut *native_added_log.lock().unwrap());
        let expected_added: BTreeSet<u64> =
            [e_new_1.to_bits(), e_new_2.to_bits()].into_iter().collect();
        // Mutating `Sensor` on `entities[5]` MUST NOT trigger `Added<Sensor>`!
        assert_eq!(c_added, expected_added);
        assert_eq!(c_added, native_added);
    }
}

#[test]
fn test_sparse_set_changed_and_added_filters() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<SparseBoost>();

    let e1 = app.world_mut().spawn(SparseBoost { factor: 1.0 }).id();
    let e2 = app.world_mut().spawn(SparseBoost { factor: 2.0 }).id();
    let e3 = app.world_mut().spawn(SparseBoost { factor: 3.0 }).id();

    let seen_changed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    unsafe extern "C" fn sparse_changed_cb(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `Mutex<Vec<u64>>`.
        let vec_mutex = unsafe { &*(user_data as *const Mutex<Vec<u64>>) };
        let mut guard = vec_mutex.lock().unwrap();
        loop {
            // SAFETY: Valid `iter`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let ents = unsafe { host_iter_entities(iter) };
            let boost_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const SparseBoost;
            assert!(!ents.is_null() && !boost_ptr.is_null());
            for r in 0..count {
                // SAFETY: `r < count`.
                unsafe {
                    guard.push((*ents.add(r)).0);
                }
            }
        }
        FfiResult::OK
    }

    let queries = [ComponentAccessDesc::changed(c"SparseBoost".as_ptr())];
    let desc = SystemDescriptor::new(
        c"sparse_changed_sys".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        sparse_changed_cb,
        Arc::as_ptr(&seen_changed) as *mut c_void,
    );
    assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

    // Frame 1: all 3 seen
    app.update();
    assert_eq!(seen_changed.lock().unwrap().drain(..).count(), 3);

    // Frame 2: none changed
    app.update();
    assert!(seen_changed.lock().unwrap().is_empty());

    // Frame 3: mutate only `e2`!
    app.world_mut().get_mut::<SparseBoost>(e2).unwrap().factor = 99.0;
    let _ = (e1, e3);
    app.update();
    assert_eq!(
        seen_changed.lock().unwrap().drain(..).collect::<Vec<_>>(),
        vec![e2.to_bits()]
    );
}

// ============================================================================
// 3. ECS Resources (POD + Opaque VTable, Change Ticks, & Scheduler Conflicts)
// ============================================================================

#[repr(C)]
struct WorldBannerVTableMirror {
    size: usize,
    align: usize,
    get_title: unsafe extern "C" fn(*const c_void) -> StrView,
    set_title: unsafe extern "C" fn(*mut c_void, *const u8, usize),
}

static WRITE_SIM_CONFIG_TRIGGER: AtomicU8 = AtomicU8::new(1);

#[test]
fn test_ecs_resources_pod_and_opaque_vtable_and_change_ticks() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<SimConfig>();
    app.register_mod_resource::<WorldBanner>();
    app.insert_resource(SimConfig {
        dt: 0.5,
        step_count: 0,
        banner_len_observed: 0,
    });
    app.insert_resource(WorldBanner {
        title: "BevyAbiMod".to_string(),
    });

    // Clear initial resource insertion change ticks before registering systems
    app.update();

    unsafe extern "C" fn pure_resource_sys(
        iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        // Undeclared resource MUST return null!
        // SAFETY: Valid `iter`.
        assert!(unsafe { host_iter_resource_readonly(iter, c"NonExistentRes".as_ptr()) }.is_null());

        // `WorldBanner` was declared `RESOURCE_READ`: mutable borrow MUST return null!
        // SAFETY: Valid `iter`.
        assert!(unsafe { host_iter_resource_mutable(iter, c"WorldBanner".as_ptr()) }.is_null());

        // Attempting `cmd_insert_component` on a Resource MUST be rejected with INVALID_COMPONENT!
        let dummy = SimConfig::default();
        // SAFETY: Valid `commands` and `dummy` bytes.
        let bad_cmd = unsafe {
            host_cmd_insert_component(
                commands,
                FfiEntity::from_bits(1),
                c"SimConfig".as_ptr(),
                (&dummy as *const SimConfig).cast::<u8>(),
                core::mem::size_of::<SimConfig>(),
            )
        };
        assert_eq!(bad_cmd, FfiResult::INVALID_COMPONENT);

        // Read `WorldBanner` via its generated VTable!
        // SAFETY: Valid symbol lookup bound to current World's ModRegistry.
        let banner_vt_ptr = unsafe { host_get_proc_address(c"WorldBanner_vtable".as_ptr()) }
            as *const WorldBannerVTableMirror;
        assert!(!banner_vt_ptr.is_null());
        // SAFETY: Valid `WorldBanner` read-only pointer.
        let banner_ptr = unsafe { host_iter_resource_readonly(iter, c"WorldBanner".as_ptr()) };
        assert!(!banner_ptr.is_null());
        // SAFETY: Calling host-generated `get_title` VTable accessor.
        let view = unsafe { ((*banner_vt_ptr).get_title)(banner_ptr.cast::<c_void>()) };

        if WRITE_SIM_CONFIG_TRIGGER.load(Ordering::SeqCst) != 0 {
            // Mutate `SimConfig` -> marks Bevy resource change tick!
            // SAFETY: Declared `RESOURCE_WRITE` on `SimConfig`.
            let cfg_ptr = unsafe { host_iter_resource_mutable(iter, c"SimConfig".as_ptr()) }
                as *mut SimConfig;
            assert!(!cfg_ptr.is_null());
            // SAFETY: Exclusive borrow of `SimConfig`.
            unsafe {
                (*cfg_ptr).step_count += 1;
                (*cfg_ptr).banner_len_observed = view.len as u32;
            }
        } else {
            // Read-only access even when `RESOURCE_WRITE` is declared -> MUST NOT mark changed!
            // SAFETY: Valid read-only access.
            let cfg_ro = unsafe { host_iter_resource_readonly(iter, c"SimConfig".as_ptr()) }
                as *const SimConfig;
            assert!(!cfg_ro.is_null());
        }

        // Pure resource-only system has 0 entity chunks!
        // SAFETY: Valid `iter`.
        assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
        FfiResult::OK
    }

    WRITE_SIM_CONFIG_TRIGGER.store(1, Ordering::SeqCst);

    let queries = [
        ComponentAccessDesc::resource_write(c"SimConfig".as_ptr()),
        ComponentAccessDesc::resource_read(c"WorldBanner".as_ptr()),
    ];
    let desc = SystemDescriptor::new(
        c"pure_resource_sys".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        pure_resource_sys,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

    // Frame 1: `WRITE_SIM_CONFIG_TRIGGER = 1` -> mutates `SimConfig`!
    app.update();
    assert_eq!(
        *app.world().resource::<SimConfig>(),
        SimConfig {
            dt: 0.5,
            step_count: 1,
            banner_len_observed: "BevyAbiMod".len() as u32,
        }
    );

    // Add a native Bevy observer system ordered AFTER `ModSystemSet::All` to inspect `Res<SimConfig>::is_changed()`!
    let native_saw_changed = Arc::new(Mutex::new(false));
    let saw_clone = Arc::clone(&native_saw_changed);
    use bevy_ecs::schedule::IntoScheduleConfigs;
    app.add_systems(
        Update,
        (move |res: Res<SimConfig>| {
            use bevy_ecs::change_detection::DetectChanges;
            *saw_clone.lock().unwrap() = res.is_changed();
        })
        .after(host_runtime::ModSystemSet::All),
    );

    // Frame 2: `WRITE_SIM_CONFIG_TRIGGER = 1` -> `is_changed()` MUST be `true`!
    app.update();
    assert!(*native_saw_changed.lock().unwrap());
    assert_eq!(app.world().resource::<SimConfig>().step_count, 2);

    // Frame 3: `WRITE_SIM_CONFIG_TRIGGER = 0` (calls only `res_readonly`) -> `is_changed()` MUST be `false`!
    WRITE_SIM_CONFIG_TRIGGER.store(0, Ordering::SeqCst);
    app.update();
    assert!(
        !*native_saw_changed.lock().unwrap(),
        "Calling only `res_readonly` must not mark Bevy Resource as changed!"
    );
    assert_eq!(app.world().resource::<SimConfig>().step_count, 2);
}

#[test]
#[should_panic]
fn test_ecs_resource_write_conflicts_with_native_bevy_res_under_strict_ambiguity() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<SimConfig>();
    app.insert_resource(SimConfig::default());

    app.edit_schedule(Update, |schedule| {
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            ..Default::default()
        });
    });

    unsafe extern "C" fn noop_res_writer(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    let queries = [ComponentAccessDesc::resource_write(c"SimConfig".as_ptr())];
    let desc = SystemDescriptor::new(
        c"unord_res_writer".as_ptr(),
        queries.as_ptr(),
        queries.len(),
        noop_res_writer,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

    // Unordered native Bevy `Res<SimConfig>` reader MUST be detected as conflicting with C-ABI `RESOURCE_WRITE`!
    app.add_systems(Update, |res: Res<SimConfig>| {
        let _ = res.dt;
    });

    app.update();
}

// ============================================================================
// 4. Guest `run_if` Condition Callbacks (Tick Preservation & Panic Containment)
// ============================================================================

#[test]
fn test_run_if_condition_skips_execution_preserves_changed_ticks_and_contains_panics() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();

    let e1 = app.world_mut().spawn(Pos3 { x: 1.0, y: 0.0, z: 0.0 }).id();
    let e2 = app.world_mut().spawn(Pos3 { x: 2.0, y: 0.0, z: 0.0 }).id();

    // Mode: 0 -> return FALSE (0), 1 -> return non-canonical TRUE (42), 2 -> guest panic caught at C-ABI boundary!
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

    // Mirrors `guest_sdk::wrap_run_condition!`: Rust `extern "C" fn` cannot unwind across the
    // foreign ABI boundary, so guest-side `catch_unwind` contains panics and yields `FFI_BOOL_FALSE`.
    unsafe extern "C" fn test_run_if(user_data: *mut c_void) -> FfiBool {
        // SAFETY: `user_data` points to `CondCtx`.
        let ctx = unsafe { &*(user_data as *const CondCtx) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match ctx.mode.load(Ordering::SeqCst) {
                0 => FFI_BOOL_FALSE,
                1 => 42, // Non-canonical C truthy byte!
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
        // SAFETY: `user_data` points to `SysCtx`.
        let ctx = unsafe { &*(user_data as *const SysCtx) };
        ctx.exec_count.fetch_add(1, Ordering::SeqCst);
        let mut guard = ctx.observed.lock().unwrap();
        loop {
            // SAFETY: Valid `iter`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let ents = unsafe { host_iter_entities(iter) };
            for r in 0..count {
                // SAFETY: `r < count`.
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

    // Frame 1: `mode = 1` (returns 42) -> executes, sees both initial entities!
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);
    assert_eq!(observed_changed.lock().unwrap().drain(..).count(), 2);

    // Frame 2: `mode = 0` (returns 0) AND mutate `e1`! System is skipped!
    cond_mode.store(0, Ordering::SeqCst);
    app.world_mut().get_mut::<Pos3>(e1).unwrap().x = 99.0;
    let _ = e2;
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);
    assert!(observed_changed.lock().unwrap().is_empty());

    // Frame 3: `mode = 0` again (2nd skipped frame, no mutations)
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);

    // Frame 4: `mode = 1` -> executes and MUST still observe `e1`'s Frame 2 mutation!
    cond_mode.store(1, Ordering::SeqCst);
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        observed_changed.lock().unwrap().drain(..).collect::<Vec<_>>(),
        vec![e1.to_bits()]
    );

    // Frame 5: `mode = 2` -> `run_if` panics inside guest closure; guest C-ABI boundary catches
    // unwind and returns `FFI_BOOL_FALSE`, skipping system execution without aborting the host!
    // Mutate `e2` during this skipped frame to prove `last_run` is STILL preserved!
    app.world_mut().get_mut::<Pos3>(e2).unwrap().z = 77.0;
    cond_mode.store(2, Ordering::SeqCst);
    app.update();
    assert!(panic_caught.load(Ordering::SeqCst));
    assert_eq!(exec_count.load(Ordering::SeqCst), 2);
    assert!(observed_changed.lock().unwrap().is_empty());

    // Frame 6: `mode = 1` restored -> executes cleanly and observes `e2`'s Frame 5 mutation!
    cond_mode.store(1, Ordering::SeqCst);
    app.update();
    assert_eq!(exec_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        observed_changed.lock().unwrap().drain(..).collect::<Vec<_>>(),
        vec![e2.to_bits()]
    );

    drop(sys_ctx);
    drop(cond_ctx);
}

// ============================================================================
// 5. Deferred `cmd_remove_component` & `cmd_insert_default` (Opaque String)
// ============================================================================

#[repr(C)]
struct DefaultableTagVTableMirror {
    size: usize,
    align: usize,
    get_label: unsafe extern "C" fn(*const c_void) -> StrView,
    set_label: unsafe extern "C" fn(*mut c_void, *const u8, usize),
}

#[test]
fn test_cmd_remove_component_and_cmd_insert_default_opaque_string_workflow() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();
    app.register_mod_component::<RemovableMarker>();
    app.register_mod_component::<NoDefaultComp>();
    app.register_mod_component_with_default::<DefaultableTag>();

    let existing = app
        .world_mut()
        .spawn((
            Pos3 {
                x: 7.0,
                y: 0.0,
                z: 0.0,
            },
            RemovableMarker { tag: 123 },
        ))
        .id();

    struct CmdTestCtx {
        existing_entity: FfiEntity,
        spawned_entity: AtomicUsize,
        did_spawn: AtomicU8,
    }

    let ctx = Box::new(CmdTestCtx {
        existing_entity: FfiEntity::from_bits(existing.to_bits()),
        spawned_entity: AtomicUsize::new(0),
        did_spawn: AtomicU8::new(0),
    });

    unsafe extern "C" fn spawner_and_remover_sys(
        _iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `CmdTestCtx`.
        let ctx = unsafe { &*(user_data as *const CmdTestCtx) };
        if ctx.did_spawn.swap(1, Ordering::SeqCst) != 0 {
            return FfiResult::OK;
        }

        // 1. Remove `RemovableMarker` from `existing_entity`
        // SAFETY: Valid `commands` and CStr.
        let rem_status = unsafe {
            host_cmd_remove_component(commands, ctx.existing_entity, c"RemovableMarker".as_ptr())
        };
        assert_eq!(rem_status, FfiResult::OK);

        // 2. Spawn a brand-new entity and attach `DefaultableTag` via `host_cmd_insert_default`!
        // SAFETY: Valid `commands`.
        let spawned = unsafe { host_cmd_spawn_empty(commands) };
        assert_ne!(spawned.0, 0);

        // Raw byte insertion on non-POD `DefaultableTag` (`String`) MUST still be rejected per D3!
        let fake_bytes = [0u8; core::mem::size_of::<DefaultableTag>()];
        // SAFETY: Valid arguments testing permission rejection.
        let raw_status = unsafe {
            host_cmd_insert_component(
                commands,
                spawned,
                c"DefaultableTag".as_ptr(),
                fake_bytes.as_ptr(),
                fake_bytes.len(),
            )
        };
        assert_eq!(raw_status, FfiResult::PERMISSION_DENIED);

        // `host_cmd_insert_default` on `NoDefaultComp` (registered without Default) MUST return PERMISSION_DENIED!
        // SAFETY: Valid arguments testing permission rejection.
        let no_def_status =
            unsafe { host_cmd_insert_default(commands, spawned, c"NoDefaultComp".as_ptr()) };
        assert_eq!(no_def_status, FfiResult::PERMISSION_DENIED);

        // `host_cmd_insert_default` on `DefaultableTag` MUST succeed!
        // SAFETY: Valid `commands` and CStr.
        let def_status =
            unsafe { host_cmd_insert_default(commands, spawned, c"DefaultableTag".as_ptr()) };
        assert_eq!(def_status, FfiResult::OK);

        ctx.spawned_entity.store(spawned.0 as usize, Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe extern "C" fn tag_vtable_mutator_sys(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: Valid symbol lookup bound to current World's ModRegistry.
        let vt_ptr = unsafe { host_get_proc_address(c"DefaultableTag_vtable".as_ptr()) }
            as *const DefaultableTagVTableMirror;
        assert!(!vt_ptr.is_null());
        loop {
            // SAFETY: Valid `iter`.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let base = unsafe { host_iter_column_mutable(iter, 0) };
            assert!(!base.is_null());
            let label = b"created_via_default_and_vtable";
            for r in 0..count {
                // SAFETY: `r < count`, stride equals `(*vt_ptr).size`.
                unsafe {
                    let elem = base.add(r * (*vt_ptr).size).cast::<c_void>();
                    ((*vt_ptr).set_label)(elem, label.as_ptr(), label.len());
                }
            }
        }
        FfiResult::OK
    }

    let spawn_desc = SystemDescriptor::new(
        c"spawner_and_remover_sys".as_ptr(),
        core::ptr::null(),
        0,
        spawner_and_remover_sys,
        (&*ctx as *const CmdTestCtx) as *mut c_void,
    );
    assert_eq!(register_inline_system(&mut app, &spawn_desc), FfiResult::OK);

    let tag_queries = [ComponentAccessDesc::write(c"DefaultableTag".as_ptr())];
    let mut_desc = SystemDescriptor::new(
        c"tag_vtable_mutator_sys".as_ptr(),
        tag_queries.as_ptr(),
        tag_queries.len(),
        tag_vtable_mutator_sys,
        core::ptr::null_mut(),
    );
    assert_eq!(register_inline_system(&mut app, &mut_desc), FfiResult::OK);

    // Frame 1: Queues `remove_component` + `spawn_empty` + `insert_default`, applied at end of frame!
    app.update();
    assert!(
        app.world().get::<RemovableMarker>(existing).is_none(),
        "RemovableMarker must be removed after deferred command flush!"
    );
    assert_eq!(app.world().get::<Pos3>(existing).unwrap().x, 7.0);

    let spawned_bits = ctx.spawned_entity.load(Ordering::SeqCst) as u64;
    let spawned_entity = Entity::from_bits(spawned_bits);
    assert!(app.world().get::<DefaultableTag>(spawned_entity).is_some());

    // Frame 2: `tag_vtable_mutator_sys` mutates the default-inserted `DefaultableTag` via VTable!
    app.update();
    assert_eq!(
        app.world().get::<DefaultableTag>(spawned_entity).unwrap().label,
        "created_via_default_and_vtable"
    );

    drop(ctx);
}

// ============================================================================
// 6. Component UUID & Metadata Collision Detection (Phase 3.2)
// ============================================================================

#[repr(C)]
#[derive(Component)]
struct SameNameDifferentSize {
    a: f64,
    b: f64,
    c: f64,
}

impl ExposableComponent for SameNameDifferentSize {
    const COMPONENT_NAME: &'static str = "Pos3"; // Collides with `Pos3` (size 12, align 4) vs (size 24, align 8)!
    const COMPONENT_CSTR: &'static core::ffi::CStr = c"Pos3";
    const UUID: ComponentUuid = ComponentUuid::from_name("Pos3");
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [abi_contract::SymbolEntry] {
        &[]
    }
}

#[repr(C)]
#[derive(Component)]
struct DifferentNameSameUuid {
    x: f32,
    y: f32,
    z: f32,
}

impl ExposableComponent for DifferentNameSameUuid {
    const COMPONENT_NAME: &'static str = "ForgedPos3Alias";
    const COMPONENT_CSTR: &'static core::ffi::CStr = c"ForgedPos3Alias";
    const UUID: ComponentUuid = Pos3::UUID; // Collides with `Pos3::UUID` under a distinct name!
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [abi_contract::SymbolEntry] {
        &[]
    }
}

#[test]
fn test_component_uuid_and_metadata_collision_detection() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // 1. Initial registration + idempotent re-registration succeed
    assert!(app.try_register_mod_component::<Pos3>().is_ok());
    assert!(app.try_register_mod_component::<Pos3>().is_ok());

    // 2. Same name with mismatched size/align -> MetadataMismatch
    let err_meta = app
        .try_register_mod_component::<SameNameDifferentSize>()
        .expect_err("Mismatched size/align under existing name must fail!");
    assert!(matches!(
        err_meta,
        ComponentCollisionError::MetadataMismatch { .. }
    ));

    // 3. Different name hashing/declaring an existing UUID -> UuidCollision
    let err_uuid = app
        .try_register_mod_component::<DifferentNameSameUuid>()
        .expect_err("Duplicate ComponentUuid under distinct name must fail!");
    assert!(matches!(
        err_uuid,
        ComponentCollisionError::UuidCollision { .. }
    ));

    // 4. Re-registering an entity component as a Resource -> MetadataMismatch (`is_resource` differs!)
    #[repr(C)]
    #[derive(Resource)]
    struct Pos3ResourceCollision {
        x: f32,
        y: f32,
        z: f32,
    }
    impl ExposableComponent for Pos3ResourceCollision {
        const COMPONENT_NAME: &'static str = "Pos3";
        const COMPONENT_CSTR: &'static core::ffi::CStr = c"Pos3";
        const UUID: ComponentUuid = Pos3::UUID;
        type VTable = ();
        fn vtable() -> &'static Self::VTable {
            &()
        }
        fn symbols() -> &'static [abi_contract::SymbolEntry] {
            &[]
        }
    }
    let err_res_mismatch = app
        .try_register_mod_resource::<Pos3ResourceCollision>()
        .expect_err("Registering an entity component name as a Resource must fail!");
    assert!(matches!(
        err_res_mismatch,
        ComponentCollisionError::MetadataMismatch { .. }
    ));

    // 5. Dynamic `host_register_component` C-ABI collision guard
    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let mut init_ctx = ModInitContext::new(registry);
    init_ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
    let _active_guard = init_ctx.make_active();
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

    // Dynamic registration with forged UUID colliding with `Pos3::UUID` -> INVALID_COMPONENT!
    let forged_desc = ModComponentDescriptor {
        struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
        name: c"DynamicForgedUuid".as_ptr(),
        uuid: Pos3::UUID,
        size: 8,
        align: 4,
        drop_fn: None,
        vtable_ptr: core::ptr::null(),
    };
    // SAFETY: Valid `ctx_ptr` and `forged_desc`.
    assert_eq!(
        unsafe { host_register_component(ctx_ptr, &forged_desc) },
        FfiResult::INVALID_COMPONENT
    );

    // Valid dynamic registration + idempotent re-registration -> OK, then layout mismatch -> INVALID_COMPONENT!
    let valid_dyn = ModComponentDescriptor {
        struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
        name: c"ValidDynComp".as_ptr(),
        uuid: ComponentUuid::from_name("ValidDynComp"),
        size: 8,
        align: 4,
        drop_fn: None,
        vtable_ptr: core::ptr::null(),
    };
    // SAFETY: Valid `ctx_ptr` and `valid_dyn`.
    assert_eq!(
        unsafe { host_register_component(ctx_ptr, &valid_dyn) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_register_component(ctx_ptr, &valid_dyn) },
        FfiResult::OK
    );

    let mismatched_dyn = ModComponentDescriptor {
        size: 16,
        ..valid_dyn
    };
    // SAFETY: Valid `ctx_ptr` and `mismatched_dyn`.
    assert_eq!(
        unsafe { host_register_component(ctx_ptr, &mismatched_dyn) },
        FfiResult::INVALID_COMPONENT
    );
}

#[test]
#[should_panic(expected = "collision")]
fn test_conflicting_static_register_mod_component_panics_with_clear_message() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Pos3>();
    app.register_mod_component::<DifferentNameSameUuid>();
}

// ============================================================================
// 7. End-to-End Cross-Language Integration: Compiled Rust + C99 `.dylib`/`.so`
// ============================================================================

#[test]
fn test_end_to_end_asymmetric_rust_and_c99_dylibs_phase3_phase4() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Register shared & per-language Phase 3/4 components and resources
    app.register_mod_component::<common::Position>();
    app.register_mod_component::<common::Velocity>();
    app.register_mod_component_with_default::<NameTag>();

    // Rust mod Phase 3/4 components + resource
    app.register_mod_component::<Phase4Transform>();
    app.register_mod_component::<Phase4Sensor>();
    app.register_mod_component::<Phase4FreshTag>();
    app.register_mod_component::<Phase4Removable>();
    app.register_mod_resource::<Phase4RustRes>();
    app.insert_resource(Phase4RustRes {
        scale: 2.0,
        matched_rows: 0,
        optional_vel_hits: 0,
        spawned_default_entity: 0,
    });

    // C99 mod Phase 3/4 components + resource
    app.register_mod_component::<Phase4CTransform>();
    app.register_mod_component::<Phase4CSensor>();
    app.register_mod_component::<Phase4CFreshTag>();
    app.register_mod_component::<Phase4CRemovable>();
    app.register_mod_resource::<Phase4CRes>();
    app.insert_resource(Phase4CRes {
        scale: 3.0,
        matched_rows: 0,
        optional_vel_hits: 0,
        spawned_default_entity: 0,
    });

    // Load BOTH compiled mods in STRICT mode to prove 100% symbol & component resolution!
    let rust_mod_id = app
        .try_load_mod_strict(&rust_dylib)
        .expect("Strict load of asymmetric_rust_mod must succeed");
    let c_mod_id = app
        .try_load_mod_strict(&c_dylib)
        .expect("Strict load of asymmetric_c_mod must succeed");

    // Open handles to toggle `run_if` predicates across the C-ABI boundary
    // SAFETY: Loading already-loaded shared libraries for atomic toggle symbols.
    let rust_lib = unsafe { Library::new(&rust_dylib).unwrap() };
    // SAFETY: Loading already-loaded shared library for atomic toggle symbol.
    let c_lib = unsafe { Library::new(&c_dylib).unwrap() };
    // SAFETY: Symbol matches `unsafe extern "C" fn(FfiBool)`.
    let set_rust_enabled: Symbol<unsafe extern "C" fn(FfiBool)> = unsafe {
        rust_lib
            .get(b"asymmetric_rust_mod_set_phase4_enabled\0")
            .unwrap()
    };
    // SAFETY: Symbol matches `unsafe extern "C" fn(FfiBool)`.
    let set_c_enabled: Symbol<unsafe extern "C" fn(FfiBool)> = unsafe {
        c_lib
            .get(b"asymmetric_c_mod_set_phase4_enabled\0")
            .unwrap()
    };
    // SAFETY: Setting atomic flags to TRUE before Frame 1.
    unsafe {
        set_rust_enabled(FFI_BOOL_TRUE);
        set_c_enabled(FFI_BOOL_TRUE);
    }

    // Spawn Rust test entities:
    // - `r_with_vel`: HAS optional `Velocity { x: 10.0, .. }` + `Phase4Sensor` + `Phase4FreshTag` + `Phase4Removable`
    // - `r_no_vel`: LACKS `Velocity` (`Option<&Velocity> == None`!) + `Phase4Sensor` + `Phase4FreshTag` + `Phase4Removable`
    let r_with_vel = app
        .world_mut()
        .spawn((
            Phase4Transform {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            common::Velocity {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            Phase4Sensor { val: 1 },
            Phase4FreshTag { code: 1 },
            Phase4Removable { dummy: 9 },
        ))
        .id();
    let r_no_vel = app
        .world_mut()
        .spawn((
            Phase4Transform {
                x: 100.0,
                y: 0.0,
                z: 0.0,
            },
            Phase4Sensor { val: 2 },
            Phase4FreshTag { code: 1 },
            Phase4Removable { dummy: 9 },
        ))
        .id();

    // Spawn C99 test entities:
    // - `c_with_vel`: HAS optional `Velocity { y: 5.0, .. }` + `Phase4CSensor` + `Phase4CFreshTag` + `Phase4CRemovable`
    // - `c_no_vel`: LACKS `Velocity` + `Phase4CSensor` + `Phase4CFreshTag` + `Phase4CRemovable`
    let c_with_vel = app
        .world_mut()
        .spawn((
            Phase4CTransform {
                x: 0.0,
                y: 2.0,
                z: 0.0,
            },
            common::Velocity {
                x: 0.0,
                y: 5.0,
                z: 0.0,
            },
            Phase4CSensor { val: 1 },
            Phase4CFreshTag { code: 1 },
            Phase4CRemovable { dummy: 9 },
        ))
        .id();
    let c_no_vel = app
        .world_mut()
        .spawn((
            Phase4CTransform {
                x: 0.0,
                y: 200.0,
                z: 0.0,
            },
            Phase4CSensor { val: 2 },
            Phase4CFreshTag { code: 1 },
            Phase4CRemovable { dummy: 9 },
        ))
        .id();

    // Frame 1: Both Rust & C99 Phase 3/4 systems execute!
    app.update();

    {
        let w = app.world();
        // Rust mod (`scale = 2.0`):
        // - `r_with_vel`: x = 1.0 + 10.0 * 2.0 = 21.0
        // - `r_no_vel`:   x = 100.0 + 1.0 * 2.0 = 102.0
        assert_eq!(w.get::<Phase4Transform>(r_with_vel).unwrap().x, 21.0);
        assert_eq!(w.get::<Phase4Transform>(r_no_vel).unwrap().x, 102.0);
        assert!(w.get::<Phase4Removable>(r_with_vel).is_none());
        assert!(w.get::<Phase4Removable>(r_no_vel).is_none());

        let r_res = w.resource::<Phase4RustRes>();
        assert_eq!(r_res.matched_rows, 2);
        assert_eq!(r_res.optional_vel_hits, 1);
        assert_ne!(r_res.spawned_default_entity, 0);
        let r_spawned = Entity::from_bits(r_res.spawned_default_entity);
        assert!(w.get::<NameTag>(r_spawned).is_some());

        // C99 mod (`scale = 3.0`):
        // - `c_with_vel`: y = 2.0 + 5.0 * 3.0 = 17.0
        // - `c_no_vel`:   y = 200.0 + 1.0 * 3.0 = 203.0
        assert_eq!(w.get::<Phase4CTransform>(c_with_vel).unwrap().y, 17.0);
        assert_eq!(w.get::<Phase4CTransform>(c_no_vel).unwrap().y, 203.0);
        assert!(w.get::<Phase4CRemovable>(c_with_vel).is_none());
        assert!(w.get::<Phase4CRemovable>(c_no_vel).is_none());

        let c_res = w.resource::<Phase4CRes>();
        assert_eq!(c_res.matched_rows, 2);
        assert_eq!(c_res.optional_vel_hits, 1);
        assert_ne!(c_res.spawned_default_entity, 0);
        let c_spawned = Entity::from_bits(c_res.spawned_default_entity);
        assert!(w.get::<NameTag>(c_spawned).is_some());
    }

    // Frame 2: Mutate `Phase4Sensor` ONLY (without re-adding `Phase4FreshTag`!).
    // Because the query requires BOTH `Changed<Phase4Sensor>` AND `Added<Phase4FreshTag>`,
    // mutating `Phase4Sensor` alone MUST match 0 rows!
    app.world_mut().get_mut::<Phase4Sensor>(r_with_vel).unwrap().val = 77;
    app.world_mut().get_mut::<Phase4CSensor>(c_with_vel).unwrap().val = 77;
    app.update();
    assert_eq!(app.world().resource::<Phase4RustRes>().matched_rows, 2);
    assert_eq!(app.world().resource::<Phase4CRes>().matched_rows, 2);

    // Frame 3: Toggle `run_if` to FALSE (`0`) on BOTH Rust and C99 mods, then spawn brand-new
    // entities (`r_delayed`, `c_delayed`)! While `run_if == false`, neither mod executes!
    // SAFETY: Toggling atomic flags to FALSE.
    unsafe {
        set_rust_enabled(FFI_BOOL_FALSE);
        set_c_enabled(FFI_BOOL_FALSE);
    }
    let r_delayed = app
        .world_mut()
        .spawn((
            Phase4Transform {
                x: 50.0,
                y: 0.0,
                z: 0.0,
            },
            Phase4Sensor { val: 5 },
            Phase4FreshTag { code: 5 },
        ))
        .id();
    let c_delayed = app
        .world_mut()
        .spawn((
            Phase4CTransform {
                x: 0.0,
                y: 60.0,
                z: 0.0,
            },
            Phase4CSensor { val: 5 },
            Phase4CFreshTag { code: 5 },
        ))
        .id();

    app.update();
    assert_eq!(app.world().resource::<Phase4RustRes>().matched_rows, 2);
    assert_eq!(app.world().resource::<Phase4CRes>().matched_rows, 2);
    assert_eq!(app.world().get::<Phase4Transform>(r_delayed).unwrap().x, 50.0);
    assert_eq!(app.world().get::<Phase4CTransform>(c_delayed).unwrap().y, 60.0);

    // Frame 4: Re-enable `run_if` (`FFI_BOOL_TRUE`)! Because `last_run` was preserved across
    // the skipped Frame 3, `r_delayed` and `c_delayed` MUST STILL match `Added + Changed` now!
    // SAFETY: Toggling atomic flags back to TRUE.
    unsafe {
        set_rust_enabled(FFI_BOOL_TRUE);
        set_c_enabled(FFI_BOOL_TRUE);
    }
    app.update();
    assert_eq!(app.world().resource::<Phase4RustRes>().matched_rows, 3);
    assert_eq!(app.world().resource::<Phase4CRes>().matched_rows, 3);
    assert_eq!(app.world().get::<Phase4Transform>(r_delayed).unwrap().x, 52.0);
    assert_eq!(app.world().get::<Phase4CTransform>(c_delayed).unwrap().y, 63.0);

    // Cleanly unload both mods!
    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
}

