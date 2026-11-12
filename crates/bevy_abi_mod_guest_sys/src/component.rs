//! Dynamic component registration (`AbiPod` and raw `drop_fn` escape hatch) and handle resolution.

use core::ffi::{CStr, c_void};
use std::ffi::CString;

use bevy_abi_mod_sys::{AbiPod, BamComponent, BamLoadCtx, ComponentDesc, ComponentUuid, FfiResult};

use crate::{host::Host, instance::GuestInstance, panic::catch_guest_unit};

/// Panic-safe `extern "C"` destructor trampoline for non-POD dynamic components registered via
/// [`register_dynamic_component_with_drop`].
///
/// # Safety
/// If `ptr` is non-null, it must be properly aligned for `T` and point to an initialized `T`
/// value ready to be dropped in place once.
pub unsafe extern "C" fn generic_drop_trampoline<T>(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: Verified `ptr` is non-null and aligned for `T`; host invokes `drop_fn` once
        // in place when removing or despawning the dynamic component.
        unsafe {
            core::ptr::drop_in_place(ptr.cast::<T>());
        }
    });
}

/// Registers a caller-constructed [`ComponentDesc`] on `load_ctx` and returns its 1-based
/// [`BamComponent`] handle.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] during `bevy_abi_mod_init`.
/// - `desc` must satisfy [`ComponentDesc::validate`], and its `name`, `drop_fn`, and `vtable_ptr`
///   must remain valid for the lifetime of the host `World` (or until `dlclose` after `World` drop).
pub unsafe fn register_component_raw(
    load_ctx: *mut BamLoadCtx,
    host: &Host,
    desc: &ComponentDesc,
) -> Result<BamComponent, FfiResult> {
    if load_ctx.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    let Some(register_fn) = host.api().register_component else {
        return Err(FfiResult::NULL_POINTER);
    };
    let mut out = BamComponent::INVALID;
    // SAFETY: `load_ctx` is non-null and active per caller contract, `desc` passed validation, and
    // `out` is writable stack storage.
    let status = unsafe { register_fn(load_ctx, desc, &mut out) };
    if !status.is_ok() {
        return Err(status);
    }
    if !out.is_valid() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    Ok(out)
}

/// Registers a cross-language `T: AbiPod` dynamic component (`drop_fn = None`), storing an owned
/// copy of `name` inside `instance` and returning the assigned [`BamComponent`] handle.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance`.
/// - If `vtable_ptr` is non-null, it must point to a static [`bevy_abi_mod_sys::VTableHeader`]-prefixed
///   VTable matching `(uuid, size_of::<T>(), align_of::<T>())`.
pub unsafe fn register_pod_component<T: AbiPod>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &CStr,
    uuid: ComponentUuid,
    flags: u32,
    vtable_ptr: *const c_void,
) -> Result<BamComponent, FfiResult> {
    if name.to_bytes().is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if name.to_str().is_err() {
        return Err(FfiResult::INVALID_UTF8);
    }
    let owned_name = CString::from(name);
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` was just allocated by `instance.track_allocation` and stays valid for the
    // lifetime of `instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let desc = ComponentDesc::new(
        name_ptr,
        uuid,
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
    )
    .with_flags(flags)
    .with_vtable(vtable_ptr);
    let host = *instance.host();
    // SAFETY: `load_ctx` and `vtable_ptr` are valid per caller contract; `name_ptr` is owned by `instance`.
    unsafe { register_component_raw(load_ctx, &host, &desc) }
}

/// Raw escape hatch registering a non-POD dynamic component `T` with [`generic_drop_trampoline`]
/// when `core::mem::needs_drop::<T>()` is `true`.
///
/// Safe `bevy_abi_mod_guest` APIs intentionally restrict `register_component` to `T: AbiPod` so
/// guest components cannot smuggle Rust heap allocations across raw `memcpy` insertions or trigger
/// destructor use-after-unload if an entity outlives library unloading. Use this low-level helper
/// only when you control component construction, single-ownership moves, and library lifetime.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance`.
/// - `T` must be safe to relocate bitwise within Bevy archetype tables (`!PhantomPinned`), and
///   every byte written via `cmd_insert` for this component handle must represent a uniquely owned,
///   valid `T` (never duplicated via `Copy` if `T` owns heap memory).
/// - The guest shared library must remain mapped in memory until every entity carrying `T` has been
///   dropped by the host `World`.
/// - If `vtable_ptr` is non-null, it must point to a valid static VTable for `T`.
pub unsafe fn register_dynamic_component_with_drop<T: 'static>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &CStr,
    uuid: ComponentUuid,
    flags: u32,
    vtable_ptr: *const c_void,
) -> Result<BamComponent, FfiResult> {
    if name.to_bytes().is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if name.to_str().is_err() {
        return Err(FfiResult::INVALID_UTF8);
    }
    let owned_name = CString::from(name);
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` was just allocated by `instance.track_allocation` and stays valid for the
    // lifetime of `instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let mut desc = ComponentDesc::new(
        name_ptr,
        uuid,
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
    )
    .with_flags(flags)
    .with_vtable(vtable_ptr);
    if core::mem::needs_drop::<T>() {
        desc = desc.with_drop(generic_drop_trampoline::<T>);
    }
    let host = *instance.host();
    // SAFETY: `load_ctx` and `vtable_ptr` satisfy caller invariants; `generic_drop_trampoline::<T>`
    // catches panics and drops `T` in place.
    unsafe { register_component_raw(load_ctx, &host, &desc) }
}

/// Resolves an existing host or guest component/resource handle by `name`, `uuid`, or both.
///
/// When both `name` and `uuid` are `Some`, the host verifies that the named registration's UUID
/// equals `uuid`, returning [`FfiResult::TYPE_MISMATCH`] on layout/identity disagreement.
pub fn resolve_component(
    host: &Host,
    name: Option<&CStr>,
    uuid: Option<ComponentUuid>,
) -> Result<BamComponent, FfiResult> {
    if name.is_none() && uuid.is_none() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if let Some(cstr) = name
        && cstr.to_bytes().is_empty()
    {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let Some(resolve_fn) = host.api().resolve_component else {
        return Err(FfiResult::NULL_POINTER);
    };
    let name_ptr = name.map_or(core::ptr::null(), CStr::as_ptr);
    let uuid_val;
    let uuid_ptr = match uuid {
        Some(u) => {
            uuid_val = u;
            &raw const uuid_val
        }
        None => core::ptr::null(),
    };
    let mut out = BamComponent::INVALID;
    // SAFETY: `host.host_ctx()` is a valid active `BamHostCtx`, `name_ptr` is null or a valid
    // NUL-terminated `CStr`, `uuid_ptr` is null or points to stack `uuid_val`, and `out` is writable.
    let status = unsafe { resolve_fn(host.host_ctx(), name_ptr, uuid_ptr, &mut out) };
    if !status.is_ok() {
        return Err(status);
    }
    if !out.is_valid() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    Ok(out)
}
