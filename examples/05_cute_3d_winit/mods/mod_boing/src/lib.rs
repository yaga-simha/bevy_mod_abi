//! Mod 1: Jelly Boing Hop (`mod_boing`)
//!
//! Makes every slime spirit playfully hop across the 3D meadow!
//! - Applies 3D gravity and bouncy elastic ground hops.
//! - Computes adorable squash-and-stretch `Scale3D` deformation (pancake squash on landing,
//!   tall jelly stretch at jump launch/apex).
//! - Flops bunny ears with vertical velocity and rosy-blushes cheeks upon landing!

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use guest_sdk::prelude::*;

const MEADOW_RADIUS: f32 = 8.2;
const GRAVITY: f32 = 16.0;
const DT: f32 = 1.0 / 60.0;

#[repr(C)]
pub struct Vec3VTable {
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
pub struct CuteSpiritVTable {
    pub size: usize,
    pub align: usize,
    pub get_yaw: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_yaw: unsafe extern "C" fn(*mut c_void, f32),
    pub get_ear_wiggle: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_ear_wiggle: unsafe extern "C" fn(*mut c_void, f32),
    pub get_blush: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_blush: unsafe extern "C" fn(*mut c_void, f32),
}

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static POS_VT: AtomicPtr<Vec3VTable> = AtomicPtr::new(core::ptr::null_mut());
static VEL_VT: AtomicPtr<Vec3VTable> = AtomicPtr::new(core::ptr::null_mut());
static SCALE_VT: AtomicPtr<Vec3VTable> = AtomicPtr::new(core::ptr::null_mut());
static SPIRIT_VT: AtomicPtr<CuteSpiritVTable> = AtomicPtr::new(core::ptr::null_mut());
static TICK: AtomicU64 = AtomicU64::new(0);

unsafe fn jelly_boing_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VT.load(Ordering::Acquire);
    let vel_vt_ptr = VEL_VT.load(Ordering::Acquire);
    let scale_vt_ptr = SCALE_VT.load(Ordering::Acquire);
    let spirit_vt_ptr = SPIRIT_VT.load(Ordering::Acquire);

    if api_ptr.is_null()
        || pos_vt_ptr.is_null()
        || vel_vt_ptr.is_null()
        || scale_vt_ptr.is_null()
        || spirit_vt_ptr.is_null()
    {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*api_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };
    let vel_vt = unsafe { &*vel_vt_ptr };
    let scale_vt = unsafe { &*scale_vt_ptr };
    let spirit_vt = unsafe { &*spirit_vt_ptr };

    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        let pos_col = chunk_iter.column_mutable(0);
        let vel_col = chunk_iter.column_mutable(1);
        let scale_col = chunk_iter.column_mutable(2);
        let spirit_col = chunk_iter.column_mutable(3);

        if pos_col.is_null() || vel_col.is_null() || scale_col.is_null() || spirit_col.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let pos_ptr = unsafe { pos_col.add(row * pos_vt.size) } as *mut c_void;
            let vel_ptr = unsafe { vel_col.add(row * vel_vt.size) } as *mut c_void;
            let scale_ptr = unsafe { scale_col.add(row * scale_vt.size) } as *mut c_void;
            let spirit_ptr = unsafe { spirit_col.add(row * spirit_vt.size) } as *mut c_void;

            let mut px = unsafe { (pos_vt.get_x)(pos_ptr) };
            let mut py = unsafe { (pos_vt.get_y)(pos_ptr) };
            let mut pz = unsafe { (pos_vt.get_z)(pos_ptr) };

            let mut vx = unsafe { (vel_vt.get_x)(vel_ptr) };
            let mut vy = unsafe { (vel_vt.get_y)(vel_ptr) };
            let mut vz = unsafe { (vel_vt.get_z)(vel_ptr) };

            let mut blush = unsafe { (spirit_vt.get_blush)(spirit_ptr) };

            // Gravity & vertical hop integration
            vy -= GRAVITY * DT;
            py += vy * DT;

            let phase = (row as f32) * 1.618 + (tick as f32) * 0.07;

            if py <= 0.0 {
                py = 0.0;
                // Joyful rhythmic hop impulse!
                let hop_strength = 5.4 + 1.8 * phase.sin().abs();
                vy = hop_strength;

                // Steer gently back toward the meadow center if wandering far
                let dist_sq = px * px + pz * pz;
                if dist_sq > (MEADOW_RADIUS * 0.65) * (MEADOW_RADIUS * 0.65) {
                    let dist = dist_sq.sqrt().max(0.01);
                    vx = (-px / dist) * 2.4 + phase.cos() * 0.8;
                    vz = (-pz / dist) * 2.4 + phase.sin() * 0.8;
                } else {
                    vx = vx * 0.85 + phase.cos() * 0.65;
                    vz = vz * 0.85 + phase.sin() * 0.65;
                }

                // Cute rosy blush on landing boing!
                blush = 1.0;
            } else {
                blush = (blush - DT * 1.4).max(0.32);
            }

            // Keep inside circular meadow boundary
            let r = (px * px + pz * pz).sqrt();
            if r > MEADOW_RADIUS {
                let nx = px / r;
                let nz = pz / r;
                px = nx * MEADOW_RADIUS;
                pz = nz * MEADOW_RADIUS;
                vx = -vx * 0.8;
                vz = -vz * 0.8;
            }

            // Volume-preserving jelly squash & stretch!
            // Near ground with high impact -> wide pancake squash; rising fast -> tall stretch
            let stretch = if py < 0.18 {
                0.68
            } else {
                (1.0 + vy * 0.055).clamp(0.76, 1.38)
            };
            let inv_sqrt = 1.0 / stretch.sqrt();
            let sx = inv_sqrt;
            let sy = stretch;
            let sz = inv_sqrt;

            // Face direction of horizontal travel + floppy bunny ears
            let speed_xz = (vx * vx + vz * vz).sqrt();
            let yaw = if speed_xz > 0.05 {
                vx.atan2(vz)
            } else {
                unsafe { (spirit_vt.get_yaw)(spirit_ptr) }
            };
            let ear_wiggle = (-vy * 0.09).clamp(-0.55, 0.55) + 0.12 * (phase * 2.0).sin();

            unsafe {
                (pos_vt.set_x)(pos_ptr, px);
                (pos_vt.set_y)(pos_ptr, py);
                (pos_vt.set_z)(pos_ptr, pz);

                (vel_vt.set_x)(vel_ptr, vx);
                (vel_vt.set_y)(vel_ptr, vy);
                (vel_vt.set_z)(vel_ptr, vz);

                (scale_vt.set_x)(scale_ptr, sx);
                (scale_vt.set_y)(scale_ptr, sy);
                (scale_vt.set_z)(scale_ptr, sz);

                (spirit_vt.set_yaw)(spirit_ptr, yaw);
                (spirit_vt.set_ear_wiggle)(spirit_ptr, ear_wiggle);
                (spirit_vt.set_blush)(spirit_ptr, blush);
            }

            chunk_iter.mark_changed(0, row);
            chunk_iter.mark_changed(1, row);
            chunk_iter.mark_changed(2, row);
            chunk_iter.mark_changed(3, row);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_jelly_boing_system calls jelly_boing_system);

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
    let scale_sym = unsafe { get_proc_address(c"Scale3D_vtable".as_ptr()) };
    let spirit_sym = unsafe { get_proc_address(c"CuteSpirit_vtable".as_ptr()) };

    if pos_sym.is_null() || vel_sym.is_null() || scale_sym.is_null() || spirit_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }

    POS_VT.store(pos_sym as *mut Vec3VTable, Ordering::Release);
    VEL_VT.store(vel_sym as *mut Vec3VTable, Ordering::Release);
    SCALE_VT.store(scale_sym as *mut Vec3VTable, Ordering::Release);
    SPIRIT_VT.store(spirit_sym as *mut CuteSpiritVTable, Ordering::Release);

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
        ComponentAccessDesc {
            name: c"Scale3D".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"CuteSpirit".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
    ];

    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"JellyBoingSystem".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_jelly_boing_system,
        user_data: core::ptr::null_mut(),
    };

    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VT.store(core::ptr::null_mut(), Ordering::Release);
    VEL_VT.store(core::ptr::null_mut(), Ordering::Release);
    SCALE_VT.store(core::ptr::null_mut(), Ordering::Release);
    SPIRIT_VT.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
