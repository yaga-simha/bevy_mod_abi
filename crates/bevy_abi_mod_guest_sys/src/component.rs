//! Dynamic component registration (`AbiPod` and raw `drop_fn` escape hatch) and handle resolution.

use core::ffi::{CStr, c_void};
use std::ffi::CString;

use bevy_abi_mod_sys::{
    AbiPod, BamComponent, BamLoadCtx, ComponentCloneFn, ComponentDesc, ComponentDropFn,
    ComponentHookFn, ComponentUuid, FfiEntity, FfiResult,
};

use crate::{
    host::Host,
    instance::GuestInstance,
    panic::{catch_guest_result, catch_guest_unit},
    trampoline::ReentrantCell,
};

type PodHookCallback<T> = Box<dyn FnMut(FfiEntity, &T) + Send + 'static>;

/// Optional lifecycle callbacks, clone capability, and required component dependencies for a
/// dynamic POD component.
pub struct PodComponentHooks<T: AbiPod> {
    /// Optional callback invoked when `T` is added to an entity (`on_add`).
    pub on_add: Option<PodHookCallback<T>>,
    /// Optional callback invoked when `T` is inserted onto an entity (`on_insert`).
    pub on_insert: Option<PodHookCallback<T>>,
    /// Optional callback invoked before `T` is overwritten or removed (`on_discard`).
    pub on_discard: Option<PodHookCallback<T>>,
    /// Optional callback invoked before `T` is removed from an entity (`on_remove`).
    pub on_remove: Option<PodHookCallback<T>>,
    /// Optional callback invoked when an entity carrying `T` is despawned (`on_despawn`).
    pub on_despawn: Option<PodHookCallback<T>>,
    /// Whether to wire a bitwise `ComponentCloneFn` trampoline for `EntityCloner` support.
    pub enable_clone: bool,
    /// Optional 1-based [`BamComponent`] handles automatically inserted (`#[require(...)]`) when
    /// `T` is added to an entity lacking them.
    pub required_components: Vec<BamComponent>,
}

impl<T: AbiPod> Default for PodComponentHooks<T> {
    fn default() -> Self {
        Self {
            on_add: None,
            on_insert: None,
            on_discard: None,
            on_remove: None,
            on_despawn: None,
            enable_clone: false,
            required_components: Vec::new(),
        }
    }
}

impl<T: AbiPod> PodComponentHooks<T> {
    /// Creates an empty [`PodComponentHooks`] builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the `on_add` lifecycle callback.
    pub fn with_on_add<F: FnMut(FfiEntity, &T) + Send + 'static>(mut self, f: F) -> Self {
        self.on_add = Some(Box::new(f));
        self
    }

    /// Sets the `on_insert` lifecycle callback.
    pub fn with_on_insert<F: FnMut(FfiEntity, &T) + Send + 'static>(mut self, f: F) -> Self {
        self.on_insert = Some(Box::new(f));
        self
    }

    /// Sets the `on_discard` lifecycle callback.
    pub fn with_on_discard<F: FnMut(FfiEntity, &T) + Send + 'static>(mut self, f: F) -> Self {
        self.on_discard = Some(Box::new(f));
        self
    }

    /// Sets the `on_remove` lifecycle callback.
    pub fn with_on_remove<F: FnMut(FfiEntity, &T) + Send + 'static>(mut self, f: F) -> Self {
        self.on_remove = Some(Box::new(f));
        self
    }

    /// Sets the `on_despawn` lifecycle callback.
    pub fn with_on_despawn<F: FnMut(FfiEntity, &T) + Send + 'static>(mut self, f: F) -> Self {
        self.on_despawn = Some(Box::new(f));
        self
    }

    /// Enables or disables bitwise `ComponentCloneFn` registration for `T`.
    #[inline]
    pub fn with_clone(mut self, enabled: bool) -> Self {
        self.enable_clone = enabled;
        self
    }

    /// Appends required component handles (`#[require(...)]`) to this registration.
    pub fn with_required_components(mut self, components: &[BamComponent]) -> Self {
        self.required_components.extend_from_slice(components);
        self
    }
}

struct HookTrampolineState<T: AbiPod> {
    on_add: Option<ReentrantCell<PodHookCallback<T>>>,
    on_insert: Option<ReentrantCell<PodHookCallback<T>>>,
    on_discard: Option<ReentrantCell<PodHookCallback<T>>>,
    on_remove: Option<ReentrantCell<PodHookCallback<T>>>,
    on_despawn: Option<ReentrantCell<PodHookCallback<T>>>,
}

fn dispatch_pod_hook<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    cell_opt: &Option<ReentrantCell<PodHookCallback<T>>>,
) {
    let Some(cell) = cell_opt else {
        return;
    };
    let val_ref: &T = if core::mem::size_of::<T>() == 0 {
        // SAFETY: Zero-sized `T: AbiPod` may be referenced via `NonNull::dangling()`.
        unsafe { core::ptr::NonNull::<T>::dangling().as_ref() }
    } else {
        if component_ptr.is_null()
            || !(component_ptr as usize).is_multiple_of(core::mem::align_of::<T>())
        {
            return;
        }
        // SAFETY: Host guarantees `component_ptr` points to an initialized `T` on `entity`
        // for the duration of the lifecycle hook call.
        unsafe { &*component_ptr.cast::<T>() }
    };
    let _ = cell.try_with_mut(|cb| cb(entity, val_ref));
}

unsafe extern "C" fn pod_on_add_trampoline<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: `user_data` was allocated via `GuestInstance::track_allocation` for
        // `HookTrampolineState<T>`, and the host clears `DynamicComponentHookMap` and disarms
        // `is_active` prior to `bevy_abi_mod_shutdown`.
        let state = unsafe { &*user_data.cast::<HookTrampolineState<T>>() };
        dispatch_pod_hook::<T>(entity, component_ptr, &state.on_add);
    });
}

unsafe extern "C" fn pod_on_insert_trampoline<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: `user_data` points to `HookTrampolineState<T>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<HookTrampolineState<T>>() };
        dispatch_pod_hook::<T>(entity, component_ptr, &state.on_insert);
    });
}

unsafe extern "C" fn pod_on_discard_trampoline<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: `user_data` points to `HookTrampolineState<T>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<HookTrampolineState<T>>() };
        dispatch_pod_hook::<T>(entity, component_ptr, &state.on_discard);
    });
}

unsafe extern "C" fn pod_on_remove_trampoline<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: `user_data` was allocated via `GuestInstance::track_allocation` for
        // `HookTrampolineState<T>`, and the host clears `DynamicComponentHookMap` and disarms
        // `is_active` prior to `bevy_abi_mod_shutdown`.
        let state = unsafe { &*user_data.cast::<HookTrampolineState<T>>() };
        dispatch_pod_hook::<T>(entity, component_ptr, &state.on_remove);
    });
}

unsafe extern "C" fn pod_on_despawn_trampoline<T: AbiPod>(
    entity: FfiEntity,
    component_ptr: *const c_void,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: `user_data` points to `HookTrampolineState<T>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<HookTrampolineState<T>>() };
        dispatch_pod_hook::<T>(entity, component_ptr, &state.on_despawn);
    });
}

/// Panic-safe `extern "C"` bitwise [`ComponentCloneFn`] trampoline for `T: AbiPod`.
///
/// # Safety
/// When `size_of::<T>() > 0`, `src` and `dst` must be non-null, non-overlapping, and valid for
/// `size_of::<T>()` bytes (`src` readable, `dst` writable).
pub unsafe extern "C" fn pod_clone_trampoline<T: AbiPod>(
    src: *const c_void,
    dst: *mut c_void,
    _user_data: *mut c_void,
) -> FfiResult {
    catch_guest_result(|| {
        let size = core::mem::size_of::<T>();
        if size == 0 {
            return FfiResult::OK;
        }
        if src.is_null() || dst.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `T: AbiPod` is `Copy` without drop glue; copying `size` bytes via `u8` pointers
        // preserves uninitialized padding provenance cleanly without forming `&[u8]`.
        unsafe {
            core::ptr::copy_nonoverlapping(src.cast::<u8>(), dst.cast::<u8>(), size);
        }
        FfiResult::OK
    })
}

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

/// Registers a cross-language `T: AbiPod` dynamic component with optional `on_add` and `on_remove`
/// hooks, storing both `name` and the panic-safe hook state inside `instance`.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance`.
/// - If `vtable_ptr` is non-null, it must point to a static [`bevy_abi_mod_sys::VTableHeader`]-prefixed
///   VTable matching `(uuid, size_of::<T>(), align_of::<T>())`.
pub unsafe fn register_pod_component_with_hooks<T: AbiPod>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &CStr,
    uuid: ComponentUuid,
    flags: u32,
    vtable_ptr: *const c_void,
    hooks: PodComponentHooks<T>,
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

    let has_on_add = hooks.on_add.is_some();
    let has_on_insert = hooks.on_insert.is_some();
    let has_on_discard = hooks.on_discard.is_some();
    let has_on_remove = hooks.on_remove.is_some();
    let has_on_despawn = hooks.on_despawn.is_some();
    if has_on_add || has_on_insert || has_on_discard || has_on_remove || has_on_despawn {
        let state = HookTrampolineState {
            on_add: hooks.on_add.map(ReentrantCell::new),
            on_insert: hooks.on_insert.map(ReentrantCell::new),
            on_discard: hooks.on_discard.map(ReentrantCell::new),
            on_remove: hooks.on_remove.map(ReentrantCell::new),
            on_despawn: hooks.on_despawn.map(ReentrantCell::new),
        };
        let raw_state = instance.track_allocation(Box::new(state));
        desc = desc.with_hooks(
            has_on_add.then_some(pod_on_add_trampoline::<T> as ComponentHookFn),
            has_on_remove.then_some(pod_on_remove_trampoline::<T> as ComponentHookFn),
            raw_state.cast::<c_void>(),
        );
        if has_on_insert {
            desc = desc.with_on_insert(pod_on_insert_trampoline::<T> as ComponentHookFn);
        }
        if has_on_discard {
            desc = desc.with_on_discard(pod_on_discard_trampoline::<T> as ComponentHookFn);
        }
        if has_on_despawn {
            desc = desc.with_on_despawn(pod_on_despawn_trampoline::<T> as ComponentHookFn);
        }
    }
    if hooks.enable_clone {
        desc = desc.with_clone_fn(pod_clone_trampoline::<T> as ComponentCloneFn);
    }
    if !hooks.required_components.is_empty() {
        let boxed_req: Box<[BamComponent]> = hooks.required_components.into_boxed_slice();
        let req_len = boxed_req.len();
        let raw_req = instance.track_allocation(Box::new(boxed_req));
        // SAFETY: `raw_req` stays pinned inside `instance` until `bevy_abi_mod_shutdown`.
        let req_ptr = unsafe { (*raw_req).as_ptr() };
        desc = desc.with_required_components(req_ptr, req_len);
    }

    let host = *instance.host();
    // SAFETY: `load_ctx` and `vtable_ptr` satisfy caller invariants; `name_ptr` and `raw_state`
    // remain valid in `instance` until host disarms hooks and runs `bevy_abi_mod_shutdown`.
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

/// Registers a runtime layout-defined dynamic component (`name`, `size`, `align`, `drop_fn`)
/// backed by `ComponentDescriptor::new_with_layout` on the host, storing an owned copy of `name`
/// inside `instance` and returning its 1-based [`BamComponent`] handle.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance`.
/// - If `drop_fn` is `Some`, it must be safe for the host to invoke on instances of `(size, align)`
///   on entity despawn/remove, and the guest library must stay mapped until all instances drop.
pub unsafe fn register_component_with_layout(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &str,
    size: usize,
    align: usize,
    drop_fn: Option<ComponentDropFn>,
) -> Result<BamComponent, FfiResult> {
    if name.is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let Ok(owned_name) = CString::new(name) else {
        return Err(FfiResult::INVALID_ARGUMENT);
    };
    let uuid = ComponentUuid::from_parts(name, size, align, 0);
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` was allocated via `instance.track_allocation` and stays valid for
    // the lifetime of `instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let mut desc = ComponentDesc::new(name_ptr, uuid, size, align);
    if let Some(df) = drop_fn {
        desc = desc.with_drop(df);
    }
    let host = *instance.host();
    // SAFETY: `load_ctx` and `desc` satisfy caller and C-ABI invariants.
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
