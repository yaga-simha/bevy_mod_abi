//! Example 3: Rust Guest Mod for Cross-Language Interoperability
//!
//! Operates concurrently with the C99 mod. Mutates ColorRgba based on
//! Position coordinates updated by the C99 mod.

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
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());
static COLOR_VTABLE: AtomicPtr<ColorRgbaVTable> = AtomicPtr::new(core::ptr::null_mut());

unsafe fn rust_color_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let host_api_ptr = HOST_API.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VTABLE.load(Ordering::Acquire);
    let color_vt_ptr = COLOR_VTABLE.load(Ordering::Acquire);

    if host_api_ptr.is_null() || pos_vt_ptr.is_null() || color_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*host_api_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };
    let color_vt = unsafe { &*color_vt_ptr };

    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        // Query 0: ColorRgba (WRITE)
        let color_base = chunk_iter.column_mutable(0);
        // Query 1: Position (READ)
        let pos_base = chunk_iter.column_readonly(1);

        if color_base.is_null() || pos_base.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for i in 0..count {
            let c_ptr = unsafe { color_base.add(i * color_vt.size) } as *mut c_void;
            let p_ptr = unsafe { pos_base.add(i * pos_vt.size) } as *const c_void;

            let px = unsafe { (pos_vt.get_x)(p_ptr) };
            let py = unsafe { (pos_vt.get_y)(p_ptr) };
            let pz = unsafe { (pos_vt.get_z)(p_ptr) };

            // Dynamic color shading reflecting drone 3D position
            let new_r = ((px * 0.05).abs() + 0.1).min(1.0);
            let new_g = ((py * 0.05).abs() + 0.2).min(1.0);
            let new_b = ((pz * 0.05).abs() + 0.3).min(1.0);
            let new_a = 1.0;

            unsafe {
                (color_vt.set_r)(c_ptr, new_r);
                (color_vt.set_g)(c_ptr, new_g);
                (color_vt.set_b)(c_ptr, new_b);
                (color_vt.set_a)(c_ptr, new_a);
            }

            chunk_iter.mark_changed(0, i);
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_rust_color_system calls rust_color_system);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    if host_api.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API.store(host_api as *mut HostApi, Ordering::Release);

    let pos_sym = unsafe { (get_proc_address)(c"Position_vtable".as_ptr()) };
    let color_sym = unsafe { (get_proc_address)(c"ColorRgba_vtable".as_ptr()) };

    if pos_sym.is_null() || color_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }
    POS_VTABLE.store(pos_sym as *mut PositionVTable, Ordering::Release);
    COLOR_VTABLE.store(color_sym as *mut ColorRgbaVTable, Ordering::Release);

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
        name: c"rust_color_system".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_rust_color_system,
        user_data: core::ptr::null_mut(),
    };

    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    COLOR_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
