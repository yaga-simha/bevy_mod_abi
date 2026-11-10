//! Mod 1: Gravity & Kinetic Bouncer Mod (`mod_gravity`)
//!
//! Applies downward gravitational acceleration and handles elastic floor/wall
//! collisions with kinetic energy restitution and horizontal friction.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};
use guest_sdk::prelude::*;

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;
const FLOOR_Y: f32 = 1.0;
const CEIL_Y: f32 = ARENA_HEIGHT - 1.0;
const MIN_X: f32 = 1.0;
const MAX_X: f32 = ARENA_WIDTH - 1.0;

const GRAVITY: f32 = 40.0;
const RESTITUTION: f32 = 0.85;
const FRICTION: f32 = 0.98;
const DT: f32 = 1.0 / 60.0;

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

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());
static VEL_VTABLE: AtomicPtr<VelocityVTable> = AtomicPtr::new(core::ptr::null_mut());

unsafe fn gravity_physics_system(
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
        let vel_col = chunk_iter.column_mutable(1);
        if pos_col.is_null() || vel_col.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let pos_ptr = unsafe { pos_col.add(row * pos_vt.size) } as *mut c_void;
            let vel_ptr = unsafe { vel_col.add(row * vel_vt.size) } as *mut c_void;

            let mut px = unsafe { (pos_vt.get_x)(pos_ptr) };
            let mut py = unsafe { (pos_vt.get_y)(pos_ptr) };
            let mut vx = unsafe { (vel_vt.get_x)(vel_ptr) };
            let mut vy = unsafe { (vel_vt.get_y)(vel_ptr) };

            // Apply downward gravitational acceleration
            vy -= GRAVITY * DT;

            // Floor bounce
            if py <= FLOOR_Y {
                py = FLOOR_Y;
                vy = -vy * RESTITUTION;
                vx *= FRICTION;
            } else if py >= CEIL_Y {
                py = CEIL_Y;
                vy = -vy * RESTITUTION;
            }

            // Wall bounce
            if px <= MIN_X {
                px = MIN_X;
                vx = -vx * RESTITUTION;
            } else if px >= MAX_X {
                px = MAX_X;
                vx = -vx * RESTITUTION;
            }

            unsafe {
                (pos_vt.set_x)(pos_ptr, px);
                (pos_vt.set_y)(pos_ptr, py);
                (vel_vt.set_x)(vel_ptr, vx);
                (vel_vt.set_y)(vel_ptr, vy);
            }

            chunk_iter.mark_changed(0, row);
            chunk_iter.mark_changed(1, row);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_gravity_physics_system calls gravity_physics_system);

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

    if pos_sym.is_null() || vel_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }

    POS_VTABLE.store(pos_sym as *mut PositionVTable, Ordering::Release);
    VEL_VTABLE.store(vel_sym as *mut VelocityVTable, Ordering::Release);

    let api = unsafe { &*host_api };

    let queries = [
        ComponentAccessDesc {
            name: c"Position".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"Velocity".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
    ];

    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"GravityPhysicsSystem".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_gravity_physics_system,
        user_data: core::ptr::null_mut(),
    };

    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    VEL_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
