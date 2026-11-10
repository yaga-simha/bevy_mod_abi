//! Mod 2: Dynamic Rainbow Spectrum Mod (`mod_rainbow`)
//!
//! Generates dynamic rainbow hues across entities based on spatial position
//! and simulation tick, creating an animated iridescent wave.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use guest_sdk::prelude::*;

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;

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

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static COLOR_VTABLE: AtomicPtr<ColorRgbaVTable> = AtomicPtr::new(core::ptr::null_mut());
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());
static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
fn hsv_to_rgb(hue: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let h_prime = (hue % 360.0) / 60.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if h_prime < 1.0 {
        (c, x, 0.0)
    } else if h_prime < 2.0 {
        (x, c, 0.0)
    } else if h_prime < 3.0 {
        (0.0, c, x)
    } else if h_prime < 4.0 {
        (0.0, x, c)
    } else if h_prime < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

unsafe fn rainbow_spectrum_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let color_vt_ptr = COLOR_VTABLE.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VTABLE.load(Ordering::Acquire);

    if api_ptr.is_null() || color_vt_ptr.is_null() || pos_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let color_vt = unsafe { &*color_vt_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };

    let tick = TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        let color_col = chunk_iter.column_mutable(0);
        let pos_col = chunk_iter.column_readonly(1);
        if color_col.is_null() || pos_col.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for row in 0..count {
            let color_ptr = unsafe { color_col.add(row * color_vt.size) } as *mut c_void;
            let pos_ptr = unsafe { pos_col.add(row * pos_vt.size) } as *const c_void;

            let px = unsafe { (pos_vt.get_x)(pos_ptr) };
            let py = unsafe { (pos_vt.get_y)(pos_ptr) };

            // Spatial wave + temporal phase progression
            let phase =
                ((px / ARENA_WIDTH) * 1.5 + (py / ARENA_HEIGHT) * 1.0 + (tick as f32 * 0.03)) % 1.0;
            let hue = (phase.abs() * 360.0) % 360.0;
            let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

            unsafe {
                (color_vt.set_r)(color_ptr, r);
                (color_vt.set_g)(color_ptr, g);
                (color_vt.set_b)(color_ptr, b);
                (color_vt.set_a)(color_ptr, 1.0);
            }

            chunk_iter.mark_changed(0, row);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_rainbow_spectrum_system calls rainbow_spectrum_system);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    if host_api.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API.store(host_api as *mut HostApi, Ordering::Release);

    let color_sym = unsafe { get_proc_address(c"ColorRgba_vtable".as_ptr()) };
    let pos_sym = unsafe { get_proc_address(c"Position_vtable".as_ptr()) };

    if color_sym.is_null() || pos_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }

    COLOR_VTABLE.store(color_sym as *mut ColorRgbaVTable, Ordering::Release);
    POS_VTABLE.store(pos_sym as *mut PositionVTable, Ordering::Release);

    let api = unsafe { &*host_api };

    let queries = [
        ComponentAccessDesc {
            name: c"ColorRgba".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"Position".as_ptr(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"RainbowSpectrumSystem".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_rainbow_spectrum_system,
        user_data: core::ptr::null_mut(),
    };

    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    COLOR_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
