//! Raw deferred command dispatchers and validated command queue wrappers.

use core::ffi::{CStr, c_char};
use core::marker::PhantomData;

use bevy_abi_mod_sys::{
    AbiPod, CmdInsertDefaultFn, CmdRemoveComponentFn, FfiEntity, FfiResult, RawCommandQueue,
};

use crate::resolve::{CommandBridgeVTable, CommandBridgeVTableExt};

/// Dispatches `cmd_spawn_empty` on a raw queue handle.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle for `vtable`.
#[inline]
pub unsafe fn raw_cmd_spawn_empty(
    raw: *mut RawCommandQueue,
    vtable: &CommandBridgeVTable,
) -> FfiEntity {
    if raw.is_null() {
        return FfiEntity(0);
    }
    if let Some(f) = vtable.spawn_empty {
        // SAFETY: Calling resolved function pointer with non-null raw queue handle.
        unsafe { f(raw) }
    } else {
        FfiEntity(0)
    }
}

/// Dispatches `cmd_despawn` on a raw queue handle.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle for `vtable`.
#[inline]
pub unsafe fn raw_cmd_despawn(
    raw: *mut RawCommandQueue,
    vtable: &CommandBridgeVTable,
    entity: FfiEntity,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if let Some(f) = vtable.despawn {
        // SAFETY: Calling resolved function pointer with non-null raw queue handle and entity.
        unsafe { f(raw, entity) }
    } else {
        FfiResult::NULL_POINTER
    }
}

/// Dispatches `cmd_insert_component` with raw pointer arguments.
///
/// # Safety
/// - If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle for `vtable`.
/// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
/// - `data` must point to at least `len` contiguous initialized bytes matching the component layout.
#[inline]
pub unsafe fn raw_cmd_insert_bytes(
    raw: *mut RawCommandQueue,
    vtable: &CommandBridgeVTable,
    entity: FfiEntity,
    component_name: *const c_char,
    data: *const u8,
    len: usize,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if let Some(f) = vtable.insert_component {
        // SAFETY: Forwarding valid raw pointers per caller's safety contract.
        unsafe { f(raw, entity, component_name, data, len) }
    } else {
        FfiResult::NULL_POINTER
    }
}

/// Dispatches `cmd_insert_component` for a typed [`AbiPod`] slice.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle for `vtable`.
#[inline]
pub unsafe fn raw_cmd_insert_slice<T: AbiPod>(
    raw: *mut RawCommandQueue,
    vtable: &CommandBridgeVTable,
    entity: FfiEntity,
    component_name: &CStr,
    data: &[T],
) -> FfiResult {
    // SAFETY: `CStr` is nul-terminated and `&[T]` (`T: AbiPod`) is a contiguous initialized byte range
    // with no uninitialized padding bytes.
    unsafe {
        raw_cmd_insert_bytes(
            raw,
            vtable,
            entity,
            component_name.as_ptr(),
            data.as_ptr().cast::<u8>(),
            core::mem::size_of_val(data),
        )
    }
}

/// Dispatches `cmd_remove_component` with a raw C string pointer.
///
/// # Safety
/// - If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle.
/// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
#[inline]
pub unsafe fn raw_cmd_remove_component(
    raw: *mut RawCommandQueue,
    remove_fn: Option<CmdRemoveComponentFn>,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if let Some(f) = remove_fn {
        // SAFETY: `raw` is non-null and `component_name` satisfies caller contract.
        unsafe { f(raw, entity, component_name) }
    } else {
        FfiResult::NULL_POINTER
    }
}

/// Dispatches `cmd_remove_component` with a validated `&CStr`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle.
#[inline]
pub unsafe fn raw_cmd_remove_component_cstr(
    raw: *mut RawCommandQueue,
    remove_fn: Option<CmdRemoveComponentFn>,
    entity: FfiEntity,
    component_name: &CStr,
) -> FfiResult {
    // SAFETY: `CStr` guarantees a valid nul-terminated C string pointer.
    unsafe { raw_cmd_remove_component(raw, remove_fn, entity, component_name.as_ptr()) }
}

/// Dispatches `cmd_insert_default` with a raw C string pointer.
///
/// # Safety
/// - If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle.
/// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
#[inline]
pub unsafe fn raw_cmd_insert_default(
    raw: *mut RawCommandQueue,
    insert_default_fn: Option<CmdInsertDefaultFn>,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if let Some(f) = insert_default_fn {
        // SAFETY: `raw` is non-null and `component_name` satisfies caller contract.
        unsafe { f(raw, entity, component_name) }
    } else {
        FfiResult::NULL_POINTER
    }
}

/// Dispatches `cmd_insert_default` with a validated `&CStr`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawCommandQueue`] handle.
#[inline]
pub unsafe fn raw_cmd_insert_default_cstr(
    raw: *mut RawCommandQueue,
    insert_default_fn: Option<CmdInsertDefaultFn>,
    entity: FfiEntity,
    component_name: &CStr,
) -> FfiResult {
    // SAFETY: `CStr` guarantees a valid nul-terminated C string pointer.
    unsafe { raw_cmd_insert_default(raw, insert_default_fn, entity, component_name.as_ptr()) }
}

/// Unforgeable validated wrapper around a host [`RawCommandQueue`] handle.
///
/// Safe code cannot construct a `ValidatedCommandQueue` from arbitrary pointers; it can only be
/// created inside `bevy_abi_mod_guest_sys` system trampolines or via explicit `unsafe` constructors.
pub struct ValidatedCommandQueue<'a> {
    raw: *mut RawCommandQueue,
    vtable: CommandBridgeVTable,
    remove_component: Option<CmdRemoveComponentFn>,
    insert_default: Option<CmdInsertDefaultFn>,
    _marker: PhantomData<&'a mut ()>,
}

impl<'a> ValidatedCommandQueue<'a> {
    /// Wraps a raw host command queue handle and resolved VTable.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawCommandQueue`] handle
    ///   matching `vtable` for the duration of `'a`.
    /// - Every non-None function pointer in `vtable` must be safe to call with `raw`.
    #[inline]
    pub unsafe fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        Self {
            raw,
            vtable,
            remove_component: None,
            insert_default: None,
            _marker: PhantomData,
        }
    }

    /// Wraps a raw host command queue handle and extended command bridge VTable.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawCommandQueue`] handle
    ///   matching `ext` for the duration of `'a`.
    /// - Every non-None function pointer in `ext` must be safe to call with `raw`.
    #[inline]
    pub unsafe fn with_ext(raw: *mut RawCommandQueue, ext: CommandBridgeVTableExt) -> Self {
        Self {
            raw,
            vtable: ext.base,
            remove_component: ext.remove_component,
            insert_default: ext.insert_default,
            _marker: PhantomData,
        }
    }

    /// Attaches an optional `cmd_remove_component` function pointer to this queue wrapper.
    ///
    /// # Safety
    /// If `remove_fn` is `Some`, it must be a valid host `cmd_remove_component` symbol compatible
    /// with this queue's underlying [`RawCommandQueue`] handle.
    #[inline]
    pub unsafe fn with_remove_component(mut self, remove_fn: Option<CmdRemoveComponentFn>) -> Self {
        self.remove_component = remove_fn;
        self
    }

    /// Attaches an optional `cmd_insert_default` function pointer to this queue wrapper.
    ///
    /// # Safety
    /// If `insert_default_fn` is `Some`, it must be a valid host `cmd_insert_default` symbol
    /// compatible with this queue's underlying [`RawCommandQueue`] handle.
    #[inline]
    pub unsafe fn with_insert_default(
        mut self,
        insert_default_fn: Option<CmdInsertDefaultFn>,
    ) -> Self {
        self.insert_default = insert_default_fn;
        self
    }

    /// Returns `true` if the underlying [`RawCommandQueue`] pointer is null.
    #[inline]
    pub fn is_null(&self) -> bool {
        self.raw.is_null()
    }

    /// Enqueues creation of a new empty entity and returns its [`FfiEntity`].
    #[inline]
    pub fn spawn_empty(&mut self) -> FfiEntity {
        // SAFETY: `self.raw` and `self.vtable` were validated on construction and exclusively borrowed via `&mut self`.
        unsafe { raw_cmd_spawn_empty(self.raw, &self.vtable) }
    }

    /// Enqueues despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        // SAFETY: `self.raw` and `self.vtable` were validated on construction and exclusively borrowed via `&mut self`.
        unsafe { raw_cmd_despawn(self.raw, &self.vtable, entity) }
    }

    /// Enqueues insertion of raw component bytes onto `entity`.
    ///
    /// # Safety
    /// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
    /// - `data` must point to at least `len` contiguous initialized bytes matching the component layout.
    /// - If `len > 0`, `data` must not be null and must not be concurrently mutated during this call.
    #[inline]
    pub unsafe fn insert_bytes(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
        data: *const u8,
        len: usize,
    ) -> FfiResult {
        // SAFETY: `self.raw` and `self.vtable` were validated on construction; caller upholds raw pointer invariants.
        unsafe { raw_cmd_insert_bytes(self.raw, &self.vtable, entity, component_name, data, len) }
    }

    /// Safely enqueues insertion of a typed [`AbiPod`] slice onto `entity`.
    #[inline]
    pub fn insert_slice<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        data: &[T],
    ) -> FfiResult {
        // SAFETY: `self.raw` and `self.vtable` were validated on construction and `T: AbiPod`.
        unsafe { raw_cmd_insert_slice(self.raw, &self.vtable, entity, component_name, data) }
    }

    /// Safely enqueues insertion of a single typed [`AbiPod`] value onto `entity`.
    #[inline]
    pub fn insert_value<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_slice(entity, component_name, core::slice::from_ref(value))
    }

    /// Enqueues deferred removal of a component by raw C string name from `entity`.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    pub unsafe fn remove_component_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: `self.raw` was validated on construction; caller guarantees `component_name` is a valid C string.
        unsafe { raw_cmd_remove_component(self.raw, self.remove_component, entity, component_name) }
    }

    /// Safely enqueues deferred removal of `component_name` from `entity`.
    #[inline]
    pub fn remove_component(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        // SAFETY: `self.raw` was validated on construction and `component_name` is a valid `CStr`.
        unsafe {
            raw_cmd_remove_component_cstr(self.raw, self.remove_component, entity, component_name)
        }
    }

    /// Enqueues deferred insertion of a host-constructed default component by raw C string name.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    pub unsafe fn insert_default_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: `self.raw` was validated on construction; caller guarantees `component_name` is a valid C string.
        unsafe { raw_cmd_insert_default(self.raw, self.insert_default, entity, component_name) }
    }

    /// Safely enqueues deferred insertion of a host-constructed default component onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        // SAFETY: `self.raw` was validated on construction and `component_name` is a valid `CStr`.
        unsafe {
            raw_cmd_insert_default_cstr(self.raw, self.insert_default, entity, component_name)
        }
    }
}

/// Bridge trait implemented by the safe SDK's `GuestCommandQueue` to expose raw `unsafe` helpers.
pub trait ValidatedCommandQueueAdapter<'a>: Sized {
    /// Wraps an unforgeable [`ValidatedCommandQueue`] into `Self`.
    fn from_validated_queue(inner: ValidatedCommandQueue<'a>) -> Self;
    /// Borrows the underlying [`ValidatedCommandQueue`] mutably.
    fn as_validated_queue_mut(&mut self) -> &mut ValidatedCommandQueue<'a>;
    /// Consumes `self` and returns the underlying [`ValidatedCommandQueue`].
    fn into_validated_queue(self) -> ValidatedCommandQueue<'a>;
}

/// Raw `unsafe` constructors and raw-pointer command operations for `GuestCommandQueue`.
pub trait GuestCommandQueueRawExt<'a>: ValidatedCommandQueueAdapter<'a> {
    /// Creates a command queue wrapper from a raw handle and resolved VTable.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawCommandQueue`] handle
    ///   matching `vtable` for `'a`.
    /// - Every non-None function pointer in `vtable` must be safe to call with `raw`.
    #[inline]
    unsafe fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        // SAFETY: Forwarding caller's raw command queue invariants to `ValidatedCommandQueue::new`.
        Self::from_validated_queue(unsafe { ValidatedCommandQueue::new(raw, vtable) })
    }

    /// Creates a command queue wrapper from a raw handle and extended VTable.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawCommandQueue`] handle
    ///   matching `ext` for `'a`.
    /// - Every non-None function pointer in `ext` must be safe to call with `raw`.
    #[inline]
    unsafe fn with_ext(raw: *mut RawCommandQueue, ext: CommandBridgeVTableExt) -> Self {
        // SAFETY: Forwarding caller's raw command queue invariants to `ValidatedCommandQueue::with_ext`.
        Self::from_validated_queue(unsafe { ValidatedCommandQueue::with_ext(raw, ext) })
    }

    /// Attaches an optional `cmd_remove_component` function pointer to this queue wrapper.
    ///
    /// # Safety
    /// If `remove_fn` is `Some`, it must be a valid host `cmd_remove_component` symbol compatible
    /// with this queue's underlying [`RawCommandQueue`] handle.
    #[inline]
    unsafe fn with_remove_component(self, remove_fn: Option<CmdRemoveComponentFn>) -> Self {
        // SAFETY: Forwarding caller's symbol safety contract.
        Self::from_validated_queue(unsafe {
            self.into_validated_queue().with_remove_component(remove_fn)
        })
    }

    /// Attaches an optional `cmd_insert_default` function pointer to this queue wrapper.
    ///
    /// # Safety
    /// If `insert_default_fn` is `Some`, it must be a valid host `cmd_insert_default` symbol
    /// compatible with this queue's underlying [`RawCommandQueue`] handle.
    #[inline]
    unsafe fn with_insert_default(self, insert_default_fn: Option<CmdInsertDefaultFn>) -> Self {
        // SAFETY: Forwarding caller's symbol safety contract.
        Self::from_validated_queue(unsafe {
            self.into_validated_queue()
                .with_insert_default(insert_default_fn)
        })
    }

    /// Enqueues insertion of raw component bytes for `entity`.
    ///
    /// # Safety
    /// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
    /// - `data` must point to at least `len` contiguous initialized bytes matching the component layout.
    /// - If `len > 0`, `data` must not be null and must not be concurrently mutated during this call.
    #[inline]
    unsafe fn insert_bytes(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
        data: *const u8,
        len: usize,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw pointer invariants.
        unsafe {
            self.as_validated_queue_mut()
                .insert_bytes(entity, component_name, data, len)
        }
    }

    /// Enqueues deferred removal of a component by raw C string name from `entity`.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    unsafe fn remove_component_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw C string invariant.
        unsafe {
            self.as_validated_queue_mut()
                .remove_component_raw(entity, component_name)
        }
    }

    /// Enqueues deferred insertion of a host-constructed default component by raw C string name.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    unsafe fn insert_default_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw C string invariant.
        unsafe {
            self.as_validated_queue_mut()
                .insert_default_raw(entity, component_name)
        }
    }
}

impl<'a, T: ValidatedCommandQueueAdapter<'a>> GuestCommandQueueRawExt<'a> for T {}
