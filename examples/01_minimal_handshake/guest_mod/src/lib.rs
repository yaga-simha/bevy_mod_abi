//! Example 1: Minimal Guest Mod (`minimal_guest_mod`)
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

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());

unsafe fn position_nudge_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    let vt_ptr = POS_VTABLE.load(Ordering::Acquire);
    if api_ptr.is_null() || vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let vt = unsafe { &*vt_ptr };

    let mut chunk_iter = GuestChunkIter::new(iter, api);
    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }
        let col = chunk_iter.column_mutable(0);
        if col.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for row in 0..count {
            let ptr = unsafe { col.add(row * vt.size) } as *mut c_void;
            let x = unsafe { (vt.get_x)(ptr) };
            let y = unsafe { (vt.get_y)(ptr) };
            let z = unsafe { (vt.get_z)(ptr) };

            unsafe {
                (vt.set_x)(ptr, x + 1.0);
                (vt.set_y)(ptr, y + 2.0);
                (vt.set_z)(ptr, z + 3.0);
            }
            chunk_iter.mark_changed(0, row);
        }
    }
    FfiResult::OK
}

wrap_system_callback!(fn c_position_nudge calls position_nudge_system);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    HOST_API.store(host_api as *mut HostApi, Ordering::Release);

    let vt = unsafe { get_proc_address(c"Position_vtable".as_ptr()) };
    if vt.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }
    POS_VTABLE.store(vt as *mut PositionVTable, Ordering::Release);

    let queries = [ComponentAccessDesc::write(c"Position".as_ptr())];
    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"PositionNudgeSystem".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: c_position_nudge,
        user_data: core::ptr::null_mut(),
    };

    let api = unsafe { &*host_api };
    unsafe { (api.register_system)(host_context, &desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
