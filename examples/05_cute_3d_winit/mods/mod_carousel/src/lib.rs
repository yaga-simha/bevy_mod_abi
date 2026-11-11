//! Mod 2: Pastel Starlight Carousel (`mod_carousel`)
//!
//! Guides the slime spirits into an enchanted two-ring 3D merry-go-round waltz!
//! - Smoothly steers slimes onto concentric orbital carousel rings with gentle bobbing waves.
//! - Cycles their jelly bodies through soft kawaii macaron pastel hues (strawberry pink,
//!   peach, lemon cream, mint, sky blue, lavender).
//! - Synchronizes joyful ear wiggles and rosy cheeks to the waltz beat.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use guest_sdk::prelude::*;

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
pub struct ColorRgbaVTable {
    pub size: usize,
    pub align: usize,
    pub get_r: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_r: unsafe extern "C" fn(*mut c_void, f32),
    pub get_g: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_g: unsafe extern "C" fn(*mut c_void, f32),
    pub get_b: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_b: unsafe extern "C" fn(*mut c_void, f32),
    pub get_a: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_a: unsafe extern "C" fn(*mut c_void, f32),
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
static COLOR_VT: AtomicPtr<ColorRgbaVTable> = AtomicPtr::new(core::ptr::null_mut());
static SPIRIT_VT: AtomicPtr<CuteSpiritVTable> = AtomicPtr::new(core::ptr::null_mut());
static TICK: AtomicU64 = AtomicU64::new(0);

#[inline]
fn pastel_hsv_to_rgb(hue_deg: f32) -> (f32, f32, f32) {
    // Low-saturation, high-value macaron pastel palette
    let s = 0.46_f32;
    let v = 0.98_f32;
    let c = v * s;
    let h = ((hue_deg % 360.0) + 360.0) % 360.0 / 60.0;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if h < 1.0 {
        (c, x, 0.0)
    } else if h < 2.0 {
        (x, c, 0.0)
    } else if h < 3.0 {
        (0.0, c, x)
    } else if h < 4.0 {
        (0.0, x, c)
    } else if h < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

unsafe fn starlight_carousel_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VT.load(Ordering::Acquire);
    let vel_vt_ptr = VEL_VT.load(Ordering::Acquire);
    let scale_vt_ptr = SCALE_VT.load(Ordering::Acquire);
    let color_vt_ptr = COLOR_VT.load(Ordering::Acquire);
    let spirit_vt_ptr = SPIRIT_VT.load(Ordering::Acquire);

    if api_ptr.is_null()
        || pos_vt_ptr.is_null()
        || vel_vt_ptr.is_null()
        || scale_vt_ptr.is_null()
        || color_vt_ptr.is_null()
        || spirit_vt_ptr.is_null()
    {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*api_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };
    let vel_vt = unsafe { &*vel_vt_ptr };
    let scale_vt = unsafe { &*scale_vt_ptr };
    let color_vt = unsafe { &*color_vt_ptr };
    let spirit_vt = unsafe { &*spirit_vt_ptr };

    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    let t = (tick as f32) * DT;

    let mut chunk_iter = GuestChunkIter::new(iter, api);
    let mut global_idx: usize = 0;

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        let pos_col = chunk_iter.column_mutable(0);
        let vel_col = chunk_iter.column_mutable(1);
        let scale_col = chunk_iter.column_mutable(2);
        let color_col = chunk_iter.column_mutable(3);
        let spirit_col = chunk_iter.column_mutable(4);

        if pos_col.is_null()
            || vel_col.is_null()
            || scale_col.is_null()
            || color_col.is_null()
            || spirit_col.is_null()
        {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let idx = global_idx;
            global_idx += 1;

            let pos_ptr = unsafe { pos_col.add(row * pos_vt.size) } as *mut c_void;
            let vel_ptr = unsafe { vel_col.add(row * vel_vt.size) } as *mut c_void;
            let scale_ptr = unsafe { scale_col.add(row * scale_vt.size) } as *mut c_void;
            let color_ptr = unsafe { color_col.add(row * color_vt.size) } as *mut c_void;
            let spirit_ptr = unsafe { spirit_col.add(row * spirit_vt.size) } as *mut c_void;

            let px = unsafe { (pos_vt.get_x)(pos_ptr) };
            let py = unsafe { (pos_vt.get_y)(pos_ptr) };
            let pz = unsafe { (pos_vt.get_z)(pos_ptr) };

            // Inner & outer counter-rotating carousel rings
            let outer_ring = idx.is_multiple_of(2);
            let ring_radius = if outer_ring { 5.6 } else { 3.1 };
            let angular_speed = if outer_ring { 0.85 } else { -1.15 };
            let slot_angle = (idx as f32) * core::f32::consts::FRAC_PI_6 + t * angular_speed;

            let target_x = ring_radius * slot_angle.cos();
            let target_z = ring_radius * slot_angle.sin();

            // Carousel horse up-and-down wave bobbing
            let bob_phase = t * 3.6 + (idx as f32) * 0.9;
            let target_y = 0.65 + 0.55 * bob_phase.sin();

            // Smooth spring velocity steering onto the carousel orbit
            let vx = (target_x - px) * 3.8;
            let vz = (target_z - pz) * 3.8;
            let vy = (target_y - py) * 5.0;

            let new_py = (py + vy * DT).max(0.0);

            // Gentle rhythmic jelly pulse
            let pulse = 1.0 + 0.12 * (bob_phase * 2.0).sin();
            let inv_pulse = 1.0 / pulse.sqrt();

            // Pastel macaron rainbow wave
            let hue = (idx as f32) * 22.0 + t * 75.0;
            let (r, g, b) = pastel_hsv_to_rgb(hue);

            // Face tangent along the carousel ring + cheerful ear dance
            let tangent_x = -slot_angle.sin() * angular_speed.signum();
            let tangent_z = slot_angle.cos() * angular_speed.signum();
            let yaw = tangent_x.atan2(tangent_z);
            let ear_wiggle = 0.35 * (t * 6.0 + idx as f32).sin();
            let blush = 0.65 + 0.35 * (t * 3.0 + idx as f32).cos().abs();

            unsafe {
                (pos_vt.set_y)(pos_ptr, new_py);

                (vel_vt.set_x)(vel_ptr, vx);
                (vel_vt.set_y)(vel_ptr, 0.0);
                (vel_vt.set_z)(vel_ptr, vz);

                (scale_vt.set_x)(scale_ptr, inv_pulse);
                (scale_vt.set_y)(scale_ptr, pulse);
                (scale_vt.set_z)(scale_ptr, inv_pulse);

                (color_vt.set_r)(color_ptr, r);
                (color_vt.set_g)(color_ptr, g);
                (color_vt.set_b)(color_ptr, b);
                (color_vt.set_a)(color_ptr, 1.0);

                (spirit_vt.set_yaw)(spirit_ptr, yaw);
                (spirit_vt.set_ear_wiggle)(spirit_ptr, ear_wiggle);
                (spirit_vt.set_blush)(spirit_ptr, blush);
            }

            chunk_iter.mark_changed(0, row);
            chunk_iter.mark_changed(1, row);
            chunk_iter.mark_changed(2, row);
            chunk_iter.mark_changed(3, row);
            chunk_iter.mark_changed(4, row);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_starlight_carousel_system calls starlight_carousel_system);

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
    let color_sym = unsafe { get_proc_address(c"ColorRgba_vtable".as_ptr()) };
    let spirit_sym = unsafe { get_proc_address(c"CuteSpirit_vtable".as_ptr()) };

    if pos_sym.is_null()
        || vel_sym.is_null()
        || scale_sym.is_null()
        || color_sym.is_null()
        || spirit_sym.is_null()
    {
        return FfiResult::INVALID_COMPONENT;
    }

    POS_VT.store(pos_sym as *mut Vec3VTable, Ordering::Release);
    VEL_VT.store(vel_sym as *mut Vec3VTable, Ordering::Release);
    SCALE_VT.store(scale_sym as *mut Vec3VTable, Ordering::Release);
    COLOR_VT.store(color_sym as *mut ColorRgbaVTable, Ordering::Release);
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
            name: c"ColorRgba".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"CuteSpirit".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
    ];

    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"StarlightCarouselSystem".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_starlight_carousel_system,
        user_data: core::ptr::null_mut(),
    };

    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VT.store(core::ptr::null_mut(), Ordering::Release);
    VEL_VT.store(core::ptr::null_mut(), Ordering::Release);
    SCALE_VT.store(core::ptr::null_mut(), Ordering::Release);
    COLOR_VT.store(core::ptr::null_mut(), Ordering::Release);
    SPIRIT_VT.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
