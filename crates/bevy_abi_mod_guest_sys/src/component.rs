//! Runtime dynamic component registration over the C-ABI boundary.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_sys::{
    ComponentDropFn, ComponentUuid, FfiResult, HostApi, ModComponentDescriptor,
};

use crate::resolve::MIN_REGISTER_COMPONENT_HOST_API_SIZE;

unsafe extern "C" fn generic_drop_trampoline<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `ptr` is non-null and points to an initialized instance of `T`.
            unsafe {
                core::ptr::drop_in_place(ptr.cast::<T>());
            }
        }));
    }
}

/// Registers a runtime dynamic component with the host Bevy ECS World.
///
/// # Safety
/// - `host_api` must point to a valid, initialized [`HostApi`] table provided by the host engine
///   (or be null, which returns [`FfiResult::NULL_POINTER`]).
/// - `host_context` must point to the valid initialization context passed to `bevy_abi_mod_init`
///   (or be null, which returns [`FfiResult::NULL_POINTER`]).
/// - Must only be called during mod initialization (`bevy_abi_mod_init`) while the host retains
///   mutable access to the Bevy `World`.
#[inline]
pub unsafe fn register_runtime_component<T: 'static>(
    host_api: *const HostApi,
    host_context: *mut c_void,
    name: &CStr,
) -> FfiResult {
    if host_api.is_null() || host_context.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Reading the first 4-byte `struct_size` header of non-null `host_api` before trailing fields.
    let struct_size = unsafe { *host_api.cast::<u32>() } as usize;
    if struct_size < MIN_REGISTER_COMPONENT_HOST_API_SIZE {
        return FfiResult::INVALID_VERSION;
    }

    // SAFETY: `host_api` is non-null and `struct_size` covers `register_component`.
    let register_component_fn =
        unsafe { core::ptr::addr_of!((*host_api).register_component).read() };

    let Ok(name_str) = name.to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    let uuid = ComponentUuid::from_name(name_str);
    let size = core::mem::size_of::<T>();
    let align = core::mem::align_of::<T>();

    let drop_fn: Option<ComponentDropFn> = if core::mem::needs_drop::<T>() {
        Some(generic_drop_trampoline::<T>)
    } else {
        None
    };

    let desc = ModComponentDescriptor {
        struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
        name: name.as_ptr(),
        uuid,
        size,
        align,
        drop_fn,
        vtable_ptr: core::ptr::null(),
    };

    // SAFETY: Forwarding valid descriptor and `host_context` to host registration callback.
    unsafe { register_component_fn(host_context, &desc) }
}
