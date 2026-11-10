//! Example 2: Multi-System Pipeline Guest Mod
//!
//! Demonstrates 3 interacting systems, all 4 query filter modes (READ, WRITE,
//! FILTER_ONLY, WITHOUT), deferred commands (spawn_empty, insert_value, despawn),
//! and change detection.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};
use guest_sdk::prelude::*;

#[repr(C)]
pub struct PositionVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct VelocityVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct HealthVTable {
    pub size: usize,
    pub align: usize,
    pub get_current: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_current: unsafe extern "C" fn(*mut c_void, f32),
    pub get_max: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_max: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DroneTag {
    pub squad: u32,
}

#[repr(C)]
pub struct PipelineState {
    pub tick_counter: u32,
    pub reinforcements_spawned: u32,
    pub casualties_despawned: u32,
}

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());
static VEL_VTABLE: AtomicPtr<VelocityVTable> = AtomicPtr::new(core::ptr::null_mut());
static HEALTH_VTABLE: AtomicPtr<HealthVTable> = AtomicPtr::new(core::ptr::null_mut());
static CMD_VTABLE: AtomicPtr<CommandBridgeVTable> = AtomicPtr::new(core::ptr::null_mut());
static GLOBAL_STATE: AtomicPtr<PipelineState> = AtomicPtr::new(core::ptr::null_mut());

/// System 1: KinematicsMovementSystem
/// Queries: Position (WRITE, col 0), Velocity (READ, col 1)
unsafe fn kinematics_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VTABLE.load(Ordering::Acquire);
    let vel_vt_ptr = VEL_VTABLE.load(Ordering::Acquire);

    if api_ptr.is_null() || pos_vt_ptr.is_null() || vel_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };
    let vel_vt = unsafe { &*vel_vt_ptr };

    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }
        let pos_col = chunk_iter.column_mutable(0);
        let vel_col = chunk_iter.column_readonly(1);
        if pos_col.is_null() || vel_col.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let pos_ptr = unsafe { pos_col.add(row * pos_vt.size) } as *mut c_void;
            let vel_ptr = unsafe { vel_col.add(row * vel_vt.size) } as *const c_void;

            let px = unsafe { (pos_vt.get_x)(pos_ptr) };
            let py = unsafe { (pos_vt.get_y)(pos_ptr) };
            let vx = unsafe { (vel_vt.get_x)(vel_ptr) };
            let vy = unsafe { (vel_vt.get_y)(vel_ptr) };

            unsafe {
                (pos_vt.set_x)(pos_ptr, px + vx * 0.1);
                (pos_vt.set_y)(pos_ptr, py + vy * 0.1);
            }
            chunk_iter.mark_changed(0, row);
        }
    }
    FfiResult::OK
}

/// System 2: FleetLifecycleSystem (Attrition, Despawn, and Reinforcement Spawner)
/// Queries: Health (WRITE, col 0), Position (READ, col 1), PlayerTag (WITHOUT, col 2)
unsafe fn fleet_lifecycle_system(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let health_vt_ptr = HEALTH_VTABLE.load(Ordering::Acquire);
    let cmd_vt_ptr = CMD_VTABLE.load(Ordering::Acquire);

    if api_ptr.is_null() || health_vt_ptr.is_null() || cmd_vt_ptr.is_null() || user_data.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let health_vt = unsafe { &*health_vt_ptr };
    let cmd_vtable = unsafe { *cmd_vt_ptr };
    let state = unsafe { &mut *(user_data as *mut PipelineState) };

    state.tick_counter += 1;
    let mut queue = GuestCommandQueue::new(commands, cmd_vtable);
    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }
        let health_col = chunk_iter.column_mutable(0);
        let entities = chunk_iter.entities();
        if health_col.is_null() || entities.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let entity = unsafe { *entities.add(row) };
            let health_ptr = unsafe { health_col.add(row * health_vt.size) } as *mut c_void;

            let cur_health = unsafe { (health_vt.get_current)(health_ptr) };
            let new_health = cur_health - 10.0;

            if new_health <= 0.0 {
                queue.despawn(entity);
                state.casualties_despawned += 1;
            } else {
                unsafe { (health_vt.set_current)(health_ptr, new_health) };
                chunk_iter.mark_changed(0, row);
            }
        }
    }

    // Reinforcement Spawner Logic: spawn Drone Gamma on tick 1 with 50 HP
    if state.tick_counter == 1 && state.reinforcements_spawned == 0 {
        let drone = queue.spawn_empty();
        queue.insert_value(drone, c"Position", &Position { x: 30.0, y: 0.0, z: 0.0 });
        queue.insert_value(drone, c"Velocity", &Velocity { x: 2.0, y: 1.0, z: 0.0 });
        queue.insert_value(drone, c"Health", &Health { current: 50.0, max: 50.0 });
        queue.insert_value(drone, c"DroneTag", &DroneTag { squad: 2 });
        state.reinforcements_spawned += 1;
    }

    FfiResult::OK
}

/// System 3: PlayerRegenSystem
/// Queries: Health (WRITE, col 0), PlayerTag (FILTER_ONLY, col 1)
unsafe fn player_regen_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let health_vt_ptr = HEALTH_VTABLE.load(Ordering::Acquire);

    if api_ptr.is_null() || health_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let health_vt = unsafe { &*health_vt_ptr };

    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }
        let health_col = chunk_iter.column_mutable(0);
        if health_col.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let health_ptr = unsafe { health_col.add(row * health_vt.size) } as *mut c_void;
            let cur_health = unsafe { (health_vt.get_current)(health_ptr) };
            let max_health = unsafe { (health_vt.get_max)(health_ptr) };

            let healed = (cur_health + 5.0).min(max_health);
            unsafe { (health_vt.set_current)(health_ptr, healed) };
            chunk_iter.mark_changed(0, row);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_kinematics_system calls kinematics_system);
wrap_system_callback!(fn c_fleet_lifecycle_system calls fleet_lifecycle_system);
wrap_system_callback!(fn c_player_regen_system calls player_regen_system);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    if host_api.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API.store(host_api as *mut HostApi, Ordering::Release);

    let pos_sym = unsafe { get_proc_address(c"Position_vtable".as_ptr()) };
    let vel_sym = unsafe { get_proc_address(c"Velocity_vtable".as_ptr()) };
    let health_sym = unsafe { get_proc_address(c"Health_vtable".as_ptr()) };

    if pos_sym.is_null() || vel_sym.is_null() || health_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }

    POS_VTABLE.store(pos_sym as *mut PositionVTable, Ordering::Release);
    VEL_VTABLE.store(vel_sym as *mut VelocityVTable, Ordering::Release);
    HEALTH_VTABLE.store(health_sym as *mut HealthVTable, Ordering::Release);

    let cmd_vt = Box::into_raw(Box::new(CommandBridgeVTable::resolve(get_proc_address)));
    CMD_VTABLE.store(cmd_vt, Ordering::Release);

    let state = Box::into_raw(Box::new(PipelineState {
        tick_counter: 0,
        reinforcements_spawned: 0,
        casualties_despawned: 0,
    }));
    GLOBAL_STATE.store(state, Ordering::Release);

    let api = unsafe { &*host_api };

    // Register System 1: KinematicsMovementSystem
    let q1 = [
        ComponentAccessDesc {
            name: c"Position".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"Velocity".as_ptr(),
            mode: ComponentAccessMode::READ,
        },
    ];
    let desc1 = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"KinematicsMovementSystem".as_ptr(),
        queries: q1.as_ptr(),
        query_count: q1.len(),
        callback: c_kinematics_system,
        user_data: core::ptr::null_mut(),
    };
    let res1 = unsafe { (api.register_system)(host_context, &desc1) };
    if res1.is_err() {
        return res1;
    }

    // Register System 2: FleetLifecycleSystem (Attrition + Despawn + Spawner)
    let q2 = [
        ComponentAccessDesc {
            name: c"Health".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"Position".as_ptr(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDesc {
            name: c"PlayerTag".as_ptr(),
            mode: ComponentAccessMode::WITHOUT, // WITHOUT Filter!
        },
    ];
    let desc2 = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"FleetLifecycleSystem".as_ptr(),
        queries: q2.as_ptr(),
        query_count: q2.len(),
        callback: c_fleet_lifecycle_system,
        user_data: state as *mut c_void,
    };
    let res2 = unsafe { (api.register_system)(host_context, &desc2) };
    if res2.is_err() {
        return res2;
    }

    // Register System 3: PlayerRegenSystem (FILTER_ONLY PlayerTag)
    let q3 = [
        ComponentAccessDesc {
            name: c"Health".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"PlayerTag".as_ptr(),
            mode: ComponentAccessMode::FILTER_ONLY, // FILTER_ONLY (With<PlayerTag>)!
        },
    ];
    let desc3 = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"PlayerRegenSystem".as_ptr(),
        queries: q3.as_ptr(),
        query_count: q3.len(),
        callback: c_player_regen_system,
        user_data: core::ptr::null_mut(),
    };
    let res3 = unsafe { (api.register_system)(host_context, &desc3) };
    if res3.is_err() {
        return res3;
    }

    FfiResult::OK
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    VEL_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    HEALTH_VTABLE.store(core::ptr::null_mut(), Ordering::Release);

    let cmd_vt_ptr = CMD_VTABLE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !cmd_vt_ptr.is_null() {
        drop(unsafe { Box::from_raw(cmd_vt_ptr) });
    }

    let state_ptr = GLOBAL_STATE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !state_ptr.is_null() {
        drop(unsafe { Box::from_raw(state_ptr) });
    }

    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
