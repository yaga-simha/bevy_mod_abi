#![deny(improper_ctypes_definitions)]

//! # Low-Level Guest FFI Internals for `bevy_abi_mod`
//!
//! `bevy_abi_mod_guest_sys` isolates all guest-side `unsafe` C-ABI trampolines, `GetProcAddressFn`
//! symbol resolution, guest-heap allocation registries, raw pointer/stride/alignment validation,
//! and raw command/resource dispatchers.
//!
//! This crate has **ZERO Bevy dependencies** — it depends exclusively on `bevy_abi_mod_sys`.

use core::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

pub use bevy_abi_mod_sys;
pub use bevy_abi_mod_sys::{
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH, AbiPod,
    CmdDespawnFn, CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
    ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
    ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, DynamicComponentDescriptor,
    DynamicSystemCallback, ExposableComponent, ExtensibleStruct, FFI_BOOL_FALSE, FFI_BOOL_TRUE,
    FfiBool, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, IterResetFn, IterResourceMutableFn, IterResourceReadonlyFn,
    ModAbiVersionFn, ModComponentDescriptor, ModInitFn, ModShutdownFn, MultiQuerySystemCallback,
    MultiQuerySystemDescriptor, MultiQuerySystemDescriptorV2, MultiQuerySystemDescriptorV3,
    QuerySpec, RawChunkIter, RawCommandQueue, RegisterComponentFn,
    RegisterConditionalMultiQuerySystemFn, RegisterConditionalSystemFn, RegisterMultiQuerySystemFn,
    RegisterSystemFn, RunConditionCallback, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
    ScheduledSystemDescriptor, StrView, SymbolEntry, SystemDescriptor, SystemDescriptorV2,
    SystemDescriptorV3, ffi_bool_to_rust, rust_bool_to_ffi,
};

/// Maximum number of simultaneous queries supported per multi-query guest system without heap allocation.
pub const MAX_GUEST_MULTI_QUERIES: usize = 8;

/// Executes a closure inside `std::panic::catch_unwind` within the guest's own Rust runtime,
/// converting any unwinding panic into [`FfiResult::PANIC_CAUGHT`] before crossing `extern "C"`.
#[inline]
pub fn catch_guest_panic<F>(f: F) -> FfiResult
where
    F: FnOnce() -> FfiResult,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => FfiResult::PANIC_CAUGHT,
    }
}

// ============================================================================
// Guest-Owned Allocation & Lifecycle Registry
// ============================================================================

/// Opaque type-erased guest-heap allocation tracked for transactional rollback or LIFO shutdown.
pub struct ErasedAllocation {
    ptr: *mut c_void,
    drop_fn: unsafe fn(*mut c_void),
}

// SAFETY: Every allocation tracked in `ErasedAllocation` is constructed from a `Send + 'static`
// value and is dropped at most once during rollback or `bevy_abi_mod_shutdown`.
unsafe impl Send for ErasedAllocation {}

unsafe fn drop_boxed_allocation<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        // SAFETY: `ptr` was allocated via `Box::into_raw(Box::new(...))` of type `T`
        // inside this guest `.cdylib`'s allocator and is dropped at most once.
        drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
    }
}

struct ShutdownHook<F: FnOnce()> {
    hook: Option<F>,
}

impl<F: FnOnce()> Drop for ShutdownHook<F> {
    fn drop(&mut self) {
        if let Some(f) = self.hook.take() {
            f();
        }
    }
}

impl ErasedAllocation {
    /// Allocates `value` on the guest heap, returning its tracked [`ErasedAllocation`] and raw `*mut T`.
    pub fn new_boxed<T: Send + 'static>(value: T) -> (Self, *mut T) {
        let raw = Box::into_raw(Box::new(value));
        (
            Self {
                ptr: raw.cast::<c_void>(),
                drop_fn: drop_boxed_allocation::<T>,
            },
            raw,
        )
    }

    /// Wraps a shutdown closure into a tracked [`ErasedAllocation`] that invokes `cleanup` when dropped.
    pub fn new_shutdown_hook<F: FnOnce() + Send + 'static>(cleanup: F) -> Self {
        let raw = Box::into_raw(Box::new(ShutdownHook {
            hook: Some(cleanup),
        }));
        Self {
            ptr: raw.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<ShutdownHook<F>>,
        }
    }
}

static GUEST_ALLOCATIONS: Mutex<Vec<ErasedAllocation>> = Mutex::new(Vec::new());

fn drop_allocation_list_lifo(mut list: Vec<ErasedAllocation>) {
    while let Some(alloc) = list.pop() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: Each `ErasedAllocation` owns a unique guest-heap allocation paired with
            // its matching monomorphized `drop_fn`.
            unsafe { (alloc.drop_fn)(alloc.ptr) };
        }));
    }
}

/// Commits staged allocations from a successful `GuestApp` initialization into `GUEST_ALLOCATIONS`.
pub fn commit_allocations(allocs: &mut Vec<ErasedAllocation>) {
    if allocs.is_empty() {
        return;
    }
    let mut guard = GUEST_ALLOCATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.append(allocs);
}

/// Rolls back and drops staged allocations in LIFO order when `GuestApp` initialization aborts.
pub fn abort_allocations(allocs: Vec<ErasedAllocation>) {
    if !allocs.is_empty() {
        drop_allocation_list_lifo(allocs);
    }
}

/// Drops all guest-side system closures, `user_data` allocations, and shutdown hooks
/// registered via `GuestApp` in reverse registration (LIFO) order.
pub fn cleanup_registered_guest_state() {
    let drained = {
        let mut guard = GUEST_ALLOCATIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core::mem::take(&mut *guard)
    };
    drop_allocation_list_lifo(drained);
}

/// Macro to wrap a guest Rust system function into a C-ABI [`DynamicSystemCallback`]
/// (`unsafe extern "C" fn`) that catches guest panics and returns [`FfiResult::PANIC_CAUGHT`].
#[macro_export]
macro_rules! wrap_system_callback {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            iter: *mut $crate::RawChunkIter,
            commands: *mut $crate::RawCommandQueue,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                // SAFETY: Forwarding opaque handles received from the host executor.
                unsafe { $inner_fn(iter, commands, user_data) }
            })
        }
    };
}

/// Macro to wrap a guest Rust multi-query system function into a C-ABI [`MultiQuerySystemCallback`]
/// (`unsafe extern "C" fn`) that catches guest panics and returns [`FfiResult::PANIC_CAUGHT`].
#[macro_export]
macro_rules! wrap_multi_query_callback {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            iters: *const *mut $crate::RawChunkIter,
            iter_count: usize,
            commands: *mut $crate::RawCommandQueue,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                // SAFETY: Forwarding opaque handles received from the host executor.
                unsafe { $inner_fn(iters, iter_count, commands, user_data) }
            })
        }
    };
}

/// Macro to wrap a guest Rust run-condition function (`unsafe fn(*mut c_void) -> bool`) into a
/// C-ABI [`RunConditionCallback`] (`unsafe extern "C" fn(*mut c_void) -> FfiBool`) that catches
/// guest panics and returns [`FFI_BOOL_FALSE`] (`0`) instead of unwinding across `extern "C"`.
#[macro_export]
macro_rules! wrap_run_condition {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiBool {
            match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                // SAFETY: Forwarding `user_data` received from the host executor.
                let should_run: bool = unsafe { $inner_fn(user_data) };
                $crate::rust_bool_to_ffi(should_run)
            })) {
                Ok(flag) => flag,
                Err(_) => $crate::FFI_BOOL_FALSE,
            }
        }
    };
}

// ============================================================================
// Symbol Resolution & Bridge VTables
// ============================================================================

/// Resolves a raw symbol pointer from `get_proc_address` using a validated [`CStr`].
#[inline]
pub fn resolve_symbol_cstr(
    get_proc_address: GetProcAddressFn,
    symbol_name: &CStr,
) -> *const c_void {
    // SAFETY: `symbol_name` is a valid nul-terminated C string (`&CStr`).
    unsafe { get_proc_address(symbol_name.as_ptr()) }
}

/// Resolves a typed static component VTable reference (e.g. `c"Position_vtable"`).
///
/// # Safety
/// Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
/// under `symbol_name` and outlives `'a`.
#[inline]
pub unsafe fn resolve_vtable_raw<'a, V: Copy + 'static>(
    get_proc_address: GetProcAddressFn,
    symbol_name: &CStr,
) -> Option<&'a V> {
    let ptr = resolve_symbol_cstr(get_proc_address, symbol_name);
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<V>()) {
        None
    } else {
        // SAFETY: Caller guarantees `symbol_name` points to a valid static `V` outliving `'a`.
        Some(unsafe { &*ptr.cast::<V>() })
    }
}

/// Validates raw C-ABI `bevy_abi_mod_init` handshake pointers and semver ABI compatibility.
///
/// # Safety
/// If `host_api` is non-null, it must point to at least 4 readable bytes (`struct_size`) and,
/// when `struct_size >= size_of::<HostApi>()`, a valid `HostApi` outliving `'a`.
#[inline]
pub unsafe fn validate_host_api<'a>(
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> Result<&'a HostApi, FfiResult> {
    if host_api.is_null() || host_context.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }

    // SAFETY: Reading 4-byte `struct_size` header at offset 0 of non-null `host_api`.
    let struct_size = unsafe { *host_api.cast::<u32>() } as usize;
    if struct_size < core::mem::size_of::<HostApi>() {
        return Err(FfiResult::INVALID_VERSION);
    }

    // SAFETY: `struct_size >= size_of::<HostApi>()` covers the entire `HostApi` struct.
    let api_ref = unsafe { &*host_api };
    let host_major = (api_ref.abi_version >> 24) & 0xFF;
    let host_minor = (api_ref.abi_version >> 16) & 0xFF;
    if host_major != ABI_VERSION_MAJOR || host_minor < ABI_VERSION_MINOR {
        return Err(FfiResult::INVALID_VERSION);
    }

    Ok(api_ref)
}

/// Cached function pointers for host command queue operations.
#[derive(Clone, Copy, Default, Debug)]
pub struct CommandBridgeVTable {
    /// Function pointer for `cmd_spawn_empty`.
    pub spawn_empty: Option<CmdSpawnEmptyFn>,
    /// Function pointer for `cmd_despawn`.
    pub despawn: Option<CmdDespawnFn>,
    /// Function pointer for `cmd_insert_component`.
    pub insert_component: Option<CmdInsertComponentFn>,
}

impl CommandBridgeVTable {
    /// Resolves command bridge functions from the host's symbol provider.
    pub fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        let mut spawn_ptr = resolve_symbol_cstr(get_proc_address, c"cmd_spawn_empty");
        if spawn_ptr.is_null() {
            spawn_ptr = resolve_symbol_cstr(get_proc_address, c"host_cmd_spawn_empty");
        }
        let mut despawn_ptr = resolve_symbol_cstr(get_proc_address, c"cmd_despawn");
        if despawn_ptr.is_null() {
            despawn_ptr = resolve_symbol_cstr(get_proc_address, c"host_cmd_despawn");
        }
        let mut insert_ptr = resolve_symbol_cstr(get_proc_address, c"cmd_insert_component");
        if insert_ptr.is_null() {
            insert_ptr = resolve_symbol_cstr(get_proc_address, c"host_cmd_insert_component");
        }

        Self {
            spawn_empty: if !spawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdSpawnEmptyFn signature.
                Some(unsafe { core::mem::transmute::<*const c_void, CmdSpawnEmptyFn>(spawn_ptr) })
            } else {
                None
            },
            despawn: if !despawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdDespawnFn signature.
                Some(unsafe { core::mem::transmute::<*const c_void, CmdDespawnFn>(despawn_ptr) })
            } else {
                None
            },
            insert_component: if !insert_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdInsertComponentFn signature.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, CmdInsertComponentFn>(insert_ptr)
                })
            } else {
                None
            },
        }
    }
}

/// Extended cached function pointers for host command queue operations, including Phase 3/4
/// `cmd_remove_component` and `cmd_insert_default` while preserving literal compatibility for
/// [`CommandBridgeVTable`].
#[derive(Clone, Copy, Default, Debug)]
pub struct CommandBridgeVTableExt {
    /// Core v1 command pointers (`spawn_empty`, `despawn`, `insert_component`).
    pub base: CommandBridgeVTable,
    /// Function pointer for `cmd_remove_component`.
    pub remove_component: Option<CmdRemoveComponentFn>,
    /// Function pointer for `cmd_insert_default`.
    pub insert_default: Option<CmdInsertDefaultFn>,
}

impl CommandBridgeVTableExt {
    /// Resolves all command bridge functions (including `cmd_remove_component` and `cmd_insert_default`)
    /// from the host's symbol provider.
    pub fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        let base = CommandBridgeVTable::resolve(get_proc_address);
        let mut remove_ptr = resolve_symbol_cstr(get_proc_address, c"cmd_remove_component");
        if remove_ptr.is_null() {
            remove_ptr = resolve_symbol_cstr(get_proc_address, c"host_cmd_remove_component");
        }
        let mut default_ptr = resolve_symbol_cstr(get_proc_address, c"cmd_insert_default");
        if default_ptr.is_null() {
            default_ptr = resolve_symbol_cstr(get_proc_address, c"host_cmd_insert_default");
        }

        Self {
            base,
            remove_component: if !remove_ptr.is_null() {
                // SAFETY: Symbol `"cmd_remove_component"` resolved from host matches `CmdRemoveComponentFn`.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, CmdRemoveComponentFn>(remove_ptr)
                })
            } else {
                None
            },
            insert_default: if !default_ptr.is_null() {
                // SAFETY: Symbol `"cmd_insert_default"` resolved from host matches `CmdInsertDefaultFn`.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, CmdInsertDefaultFn>(default_ptr)
                })
            } else {
                None
            },
        }
    }
}

/// Cached function pointers for host ECS resource read/write operations (`Res<R>` / `ResMut<R>`).
#[derive(Clone, Copy, Default, Debug)]
pub struct ResourceBridgeVTable {
    /// Function pointer for `res_readonly` (`iter_resource_readonly`).
    pub res_readonly: Option<IterResourceReadonlyFn>,
    /// Function pointer for `res_mutable` (`iter_resource_mutable`).
    pub res_mutable: Option<IterResourceMutableFn>,
}

impl ResourceBridgeVTable {
    /// Resolves resource access functions (`res_readonly`, `res_mutable`) from `get_proc_address`.
    pub fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        let mut ro_ptr = resolve_symbol_cstr(get_proc_address, c"res_readonly");
        if ro_ptr.is_null() {
            ro_ptr = resolve_symbol_cstr(get_proc_address, c"iter_resource_readonly");
        }
        if ro_ptr.is_null() {
            ro_ptr = resolve_symbol_cstr(get_proc_address, c"host_iter_resource_readonly");
        }

        let mut rw_ptr = resolve_symbol_cstr(get_proc_address, c"res_mutable");
        if rw_ptr.is_null() {
            rw_ptr = resolve_symbol_cstr(get_proc_address, c"iter_resource_mutable");
        }
        if rw_ptr.is_null() {
            rw_ptr = resolve_symbol_cstr(get_proc_address, c"host_iter_resource_mutable");
        }

        Self {
            res_readonly: if !ro_ptr.is_null() {
                // SAFETY: Symbol `"res_readonly"` resolved from host matches `IterResourceReadonlyFn`.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, IterResourceReadonlyFn>(ro_ptr)
                })
            } else {
                None
            },
            res_mutable: if !rw_ptr.is_null() {
                // SAFETY: Symbol `"res_mutable"` resolved from host matches `IterResourceMutableFn`.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, IterResourceMutableFn>(rw_ptr)
                })
            } else {
                None
            },
        }
    }
}

/// Resolves the `iter_reset` host function pointer via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_iter_reset(get_proc_address: GetProcAddressFn) -> Option<IterResetFn> {
    let mut ptr = resolve_symbol_cstr(get_proc_address, c"iter_reset");
    if ptr.is_null() {
        ptr = resolve_symbol_cstr(get_proc_address, c"host_iter_reset");
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"iter_reset"` resolved from host `GetProcAddressFn` has signature `IterResetFn`.
        Some(unsafe { core::mem::transmute::<*const c_void, IterResetFn>(ptr) })
    }
}

/// Resolves `register_multi_query_system` via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_register_multi_query(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterMultiQuerySystemFn> {
    let mut ptr = resolve_symbol_cstr(get_proc_address, c"register_multi_query_system");
    if ptr.is_null() {
        ptr = resolve_symbol_cstr(get_proc_address, c"host_register_multi_query_system");
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"register_multi_query_system"` resolved from host matches `RegisterMultiQuerySystemFn`.
        Some(unsafe { core::mem::transmute::<*const c_void, RegisterMultiQuerySystemFn>(ptr) })
    }
}

/// Resolves `register_conditional_system` via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_register_conditional_system(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterConditionalSystemFn> {
    let mut ptr = resolve_symbol_cstr(get_proc_address, c"register_conditional_system");
    if ptr.is_null() {
        ptr = resolve_symbol_cstr(get_proc_address, c"host_register_conditional_system");
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"register_conditional_system"` resolved from host matches `RegisterConditionalSystemFn`.
        Some(unsafe { core::mem::transmute::<*const c_void, RegisterConditionalSystemFn>(ptr) })
    }
}

/// Resolves `register_conditional_multi_query_system` via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_register_conditional_multi_query(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterConditionalMultiQuerySystemFn> {
    let mut ptr = resolve_symbol_cstr(get_proc_address, c"register_conditional_multi_query_system");
    if ptr.is_null() {
        ptr = resolve_symbol_cstr(
            get_proc_address,
            c"host_register_conditional_multi_query_system",
        );
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"register_conditional_multi_query_system"` matches `RegisterConditionalMultiQuerySystemFn`.
        Some(unsafe {
            core::mem::transmute::<*const c_void, RegisterConditionalMultiQuerySystemFn>(ptr)
        })
    }
}

// ============================================================================
// Raw Deferred Command Dispatchers
// ============================================================================

/// Dispatches `cmd_spawn_empty` on a raw queue handle.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawCommandQueue` handle for `vtable`.
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
/// If `raw` is non-null, it must be a valid host `RawCommandQueue` handle for `vtable`.
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

/// Dispatches `cmd_insert_component` for a typed POD slice.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawCommandQueue` handle for `vtable`.
#[inline]
pub unsafe fn raw_cmd_insert_slice<T: Copy>(
    raw: *mut RawCommandQueue,
    vtable: &CommandBridgeVTable,
    entity: FfiEntity,
    component_name: &CStr,
    data: &[T],
) -> FfiResult {
    // SAFETY: `CStr` is nul-terminated and `&[T]` is a valid contiguous byte range.
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
/// `component_name` must point to a valid, nul-terminated UTF-8 C string.
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
/// If `raw` is non-null, it must be a valid host `RawCommandQueue` handle.
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
/// `component_name` must point to a valid, nul-terminated UTF-8 C string.
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
/// If `raw` is non-null, it must be a valid host `RawCommandQueue` handle.
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

// ============================================================================
// Raw Chunk Iterator, Slice, and Resource Helpers
// ============================================================================

/// Returns `true` if the two half-open byte ranges `[addr_a, addr_a + len_a)` and `[addr_b, addr_b + len_b)` overlap.
#[inline]
pub fn byte_ranges_overlap(addr_a: usize, len_a: usize, addr_b: usize, len_b: usize) -> bool {
    if len_a == 0 || len_b == 0 {
        return false;
    }
    let end_a = addr_a.saturating_add(len_a);
    let end_b = addr_b.saturating_add(len_b);
    addr_a < end_b && addr_b < end_a
}

/// Validates and constructs an `&[FfiEntity]` slice from a raw entity column pointer and row count.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized `FfiEntity` values
/// valid for `'a`.
#[inline]
pub unsafe fn validated_entities_slice<'a>(ptr: *const FfiEntity, count: usize) -> &'a [FfiEntity] {
    if count == 0 || ptr.is_null() {
        return &[];
    }
    let elem_size = core::mem::size_of::<FfiEntity>();
    let elem_align = core::mem::align_of::<FfiEntity>();
    if !(ptr as usize).is_multiple_of(elem_align) || count > (isize::MAX as usize) {
        return &[];
    }
    let Some(total_bytes) = count.checked_mul(elem_size) else {
        return &[];
    };
    if total_bytes > (isize::MAX as usize) || (ptr as usize).checked_add(total_bytes).is_none() {
        return &[];
    }
    // SAFETY:
    // - `ptr` is non-null and aligned for `FfiEntity`.
    // - `total_bytes <= isize::MAX as usize` without address-space wrap-around.
    // - Caller ties `'a` to the active chunk borrow on `GuestChunkIter`.
    unsafe { core::slice::from_raw_parts(ptr, count) }
}

/// Calls `reset_fn(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_reset(raw: *mut RawChunkIter, reset_fn: Option<IterResetFn>) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(reset_fn) = reset_fn else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: `raw` is non-null and exclusively borrowed by the caller.
    unsafe { reset_fn(raw) }
}

/// Calls `api.iter_next_chunk(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_next_chunk(raw: *mut RawChunkIter, api: &HostApi) -> usize {
    if raw.is_null() {
        return 0;
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_next_chunk)(raw) }
}

/// Calls `api.iter_column_readonly(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_column_readonly(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
) -> *const u8 {
    if raw.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_column_readonly)(raw, index) }
}

/// Calls `api.iter_column_mutable(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_column_mutable(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
) -> *mut u8 {
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_column_mutable)(raw, index) }
}

/// Calls `api.iter_mark_changed(raw, index, row)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_mark_changed(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
    row: usize,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_mark_changed)(raw, index, row) }
}

/// Calls `api.iter_entities(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_entities(raw: *mut RawChunkIter, api: &HostApi) -> *const FfiEntity {
    if raw.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_entities)(raw) }
}

/// Calls `api.iter_column_stride(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_column_stride(raw: *mut RawChunkIter, api: &HostApi, index: usize) -> usize {
    if raw.is_null() {
        return 0;
    }
    // SAFETY: Delegating to HostApi function pointer with non-null `raw`.
    unsafe { (api.iter_column_stride)(raw, index) }
}

/// Resolves a read-only resource byte pointer from `raw` via `res_vtable`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_resource_readonly(
    raw: *mut RawChunkIter,
    res_vtable: &ResourceBridgeVTable,
    resource_name: &CStr,
) -> *const u8 {
    if raw.is_null() {
        return core::ptr::null();
    }
    let Some(f) = res_vtable.res_readonly else {
        return core::ptr::null();
    };
    // SAFETY: `raw` is non-null and `resource_name` is a valid nul-terminated `CStr`.
    unsafe { f(raw, resource_name.as_ptr()) }
}

/// Resolves a mutable resource byte pointer from `raw` via `res_vtable`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host `RawChunkIter` handle.
#[inline]
pub unsafe fn raw_iter_resource_mutable(
    raw: *mut RawChunkIter,
    res_vtable: &ResourceBridgeVTable,
    resource_name: &CStr,
) -> *mut u8 {
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    let Some(f) = res_vtable.res_mutable else {
        return core::ptr::null_mut();
    };
    // SAFETY: `raw` is non-null and exclusively borrowed, and `resource_name` is a valid `CStr`.
    unsafe { f(raw, resource_name.as_ptr()) }
}

/// Validates alignment and non-nullness of `ptr` and converts it into `&'a R`.
///
/// # Safety
/// If `ptr` is non-null, it must point to an initialized `R` valid for shared reads over `'a`.
#[inline]
pub unsafe fn raw_deref_resource<'a, R: Copy>(ptr: *const u8) -> Option<&'a R> {
    if ptr.is_null() {
        return None;
    }
    if core::mem::size_of::<R>() == 0 {
        // SAFETY: ZST requires no backing bytes; lifetime is tied to caller's `'a`.
        return Some(unsafe { &*core::ptr::NonNull::<R>::dangling().as_ptr() });
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
        return None;
    }
    // SAFETY: `ptr` is non-null, aligned for `R`, and backed by host resource storage for `'a`.
    Some(unsafe { &*ptr.cast::<R>() })
}

/// Validates alignment and non-nullness of `ptr` and converts it into `&'a mut R`.
///
/// # Safety
/// If `ptr` is non-null, it must point to an initialized `R` exclusively borrowed over `'a`.
#[inline]
pub unsafe fn raw_deref_resource_mut<'a, R: Copy>(ptr: *mut u8) -> Option<&'a mut R> {
    if ptr.is_null() {
        return None;
    }
    if core::mem::size_of::<R>() == 0 {
        // SAFETY: ZST requires no backing bytes; exclusive borrow tied to caller's `'a`.
        return Some(unsafe { &mut *core::ptr::NonNull::<R>::dangling().as_ptr() });
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
        return None;
    }
    // SAFETY: `ptr` is non-null, aligned for `R`, and exclusively borrowed for `'a`.
    Some(unsafe { &mut *ptr.cast::<R>() })
}

/// Bounds-checked element pointer offset into a read-only column buffer.
///
/// # Safety
/// If `ptr` is non-null and `index < count`, `ptr` must belong to an allocation of at least
/// `(index + 1) * stride` bytes.
#[inline]
pub unsafe fn raw_slice_get_ptr(
    ptr: *const u8,
    stride: usize,
    count: usize,
    index: usize,
) -> Option<*const u8> {
    if index < count && !ptr.is_null() {
        let offset = index.checked_mul(stride)?;
        if offset > (isize::MAX as usize) {
            return None;
        }
        (ptr as usize).checked_add(offset)?;
        // SAFETY: Bounds checked (`index < count`), `offset <= isize::MAX` without address wrapping.
        unsafe { Some(ptr.add(offset)) }
    } else {
        None
    }
}

/// Bounds-checked element pointer offset into a mutable column buffer.
///
/// # Safety
/// If `ptr` is non-null and `index < count`, `ptr` must belong to an allocation of at least
/// `(index + 1) * stride` bytes.
#[inline]
pub unsafe fn raw_slice_get_ptr_mut(
    ptr: *mut u8,
    stride: usize,
    count: usize,
    index: usize,
) -> Option<*mut u8> {
    if index < count && !ptr.is_null() {
        let offset = index.checked_mul(stride)?;
        if offset > (isize::MAX as usize) {
            return None;
        }
        (ptr as usize).checked_add(offset)?;
        // SAFETY: Bounds checked (`index < count`), `offset <= isize::MAX` without address wrapping.
        unsafe { Some(ptr.add(offset)) }
    } else {
        None
    }
}

/// Validates `(ptr, stride, count)` and constructs `&'a [T]`.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized `T` elements
/// valid for shared reads over `'a`.
#[inline]
pub unsafe fn raw_slice_as_typed<'a, T: Copy>(
    ptr: *const u8,
    stride: usize,
    count: usize,
) -> Option<&'a [T]> {
    if stride != core::mem::size_of::<T>() {
        return None;
    }
    if count == 0 {
        return Some(&[]);
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    if count > (isize::MAX as usize) {
        return None;
    }
    let total_bytes = match count.checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
        _ => return None,
    };
    (ptr as usize).checked_add(total_bytes)?;
    // SAFETY: `ptr` is non-null, aligned for `T`, contiguous (`stride == size_of::<T>()`),
    // and `total_bytes <= isize::MAX` without address-space wrap-around.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<T>(), count) })
}

/// Validates `(ptr, stride, count)` and constructs `&'a mut [T]`.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized `T` elements
/// exclusively borrowed over `'a`.
#[inline]
pub unsafe fn raw_slice_as_typed_mut<'a, T: Copy>(
    ptr: *mut u8,
    stride: usize,
    count: usize,
) -> Option<&'a mut [T]> {
    if stride != core::mem::size_of::<T>() {
        return None;
    }
    if count == 0 {
        return Some(&mut []);
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    if count > (isize::MAX as usize) {
        return None;
    }
    let total_bytes = match count.checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
        _ => return None,
    };
    (ptr as usize).checked_add(total_bytes)?;
    // SAFETY: `ptr` is non-null, aligned for `T`, contiguous (`stride == size_of::<T>()`),
    // `total_bytes <= isize::MAX` without wrap-around, and exclusively borrowed by caller for `'a`.
    Some(unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), count) })
}

// ============================================================================
// Runtime Component Registration
// ============================================================================

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

#[inline]
const fn min_host_api_v1_size() -> usize {
    core::mem::offset_of!(HostApi, register_component) + core::mem::size_of::<RegisterComponentFn>()
}

/// Registers a runtime dynamic component with the host Bevy ECS World.
///
/// # Safety
/// - `host_api` must point to a valid, initialized [`HostApi`] table provided by the host engine.
/// - `host_context` must point to the valid initialization context passed to `bevy_abi_mod_init`.
/// - This function must only be called during mod initialization (`bevy_abi_mod_init`) while the host
///   retains mutable access to the Bevy `World`.
#[inline]
pub unsafe fn register_runtime_component<T: 'static>(
    host_api: *const HostApi,
    host_context: *mut c_void,
    name: &CStr,
) -> FfiResult {
    if host_api.is_null() || host_context.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Reading the first 4-byte `struct_size` header of `HostApi` before reading trailing fields.
    let struct_size = unsafe { *host_api.cast::<u32>() } as usize;
    if struct_size < min_host_api_v1_size() {
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

// ============================================================================
// Raw System & Run-Condition Closure Registration Trampolines
// ============================================================================

/// Validated raw arguments delivered to a single-query guest system closure trampoline.
pub struct RawSystemInvocation<'a> {
    /// Raw chunk iterator handle for this invocation.
    pub iter: *mut RawChunkIter,
    /// Raw deferred command queue handle for this invocation.
    pub commands: *mut RawCommandQueue,
    /// Validated host API table.
    pub host_api: &'a HostApi,
    /// Pre-resolved extended command bridge VTable.
    pub cmd_vtable_ext: CommandBridgeVTableExt,
    /// Pre-resolved resource bridge VTable.
    pub res_vtable: ResourceBridgeVTable,
    /// Optional `iter_reset` symbol.
    pub iter_reset: Option<IterResetFn>,
}

/// Validated raw arguments delivered to a multi-query guest system closure trampoline.
pub struct RawMultiQueryInvocation<'a> {
    /// Stack array of raw chunk iterator pointers (`0..iter_count` initialized).
    pub raw_iters: [*mut RawChunkIter; MAX_GUEST_MULTI_QUERIES],
    /// Number of active query iterators.
    pub iter_count: usize,
    /// Raw deferred command queue handle for this invocation.
    pub commands: *mut RawCommandQueue,
    /// Validated host API table.
    pub host_api: &'a HostApi,
    /// Pre-resolved extended command bridge VTable.
    pub cmd_vtable_ext: CommandBridgeVTableExt,
    /// Pre-resolved resource bridge VTable.
    pub res_vtable: ResourceBridgeVTable,
    /// Optional `iter_reset` symbol.
    pub iter_reset: Option<IterResetFn>,
}

struct SystemClosureState<F> {
    host_api: *const HostApi,
    cmd_vtable_ext: CommandBridgeVTableExt,
    res_vtable: ResourceBridgeVTable,
    iter_reset: Option<IterResetFn>,
    callback: Mutex<F>,
}

unsafe extern "C" fn c_system_closure_trampoline<F>(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult
where
    F: for<'a> FnMut(RawSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    catch_guest_panic(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to an initialized `SystemClosureState<F>` owned by `GUEST_ALLOCATIONS`
        // that remains alive until `bevy_abi_mod_shutdown` runs after all systems are deactivated.
        let state = unsafe { &*user_data.cast::<SystemClosureState<F>>() };
        if state.host_api.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        // SAFETY: `state.host_api` was validated during `GuestApp` initialization and remains valid
        // for the duration of the system callback.
        let api = unsafe { &*state.host_api };
        (*cb_guard)(RawSystemInvocation {
            iter,
            commands,
            host_api: api,
            cmd_vtable_ext: state.cmd_vtable_ext,
            res_vtable: state.res_vtable,
            iter_reset: state.iter_reset,
        })
    })
}

struct MultiQueryClosureState<F> {
    host_api: *const HostApi,
    cmd_vtable_ext: CommandBridgeVTableExt,
    res_vtable: ResourceBridgeVTable,
    iter_reset: Option<IterResetFn>,
    callback: Mutex<F>,
}

unsafe extern "C" fn c_multi_query_closure_trampoline<F>(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult
where
    F: for<'a> FnMut(RawMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
{
    catch_guest_panic(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if iter_count > MAX_GUEST_MULTI_QUERIES {
            return FfiResult::INVALID_COMPONENT;
        }
        if iter_count > 0 && iters.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to an initialized `MultiQueryClosureState<F>` owned by `GUEST_ALLOCATIONS`.
        let state = unsafe { &*user_data.cast::<MultiQueryClosureState<F>>() };
        if state.host_api.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        // SAFETY: `state.host_api` is valid for the duration of the callback.
        let api = unsafe { &*state.host_api };

        let mut raw_iters = [core::ptr::null_mut::<RawChunkIter>(); MAX_GUEST_MULTI_QUERIES];
        for (slot, item) in raw_iters.iter_mut().enumerate().take(iter_count) {
            // SAFETY: `iters` is non-null and holds `iter_count` pointers per `MultiQuerySystemCallback` contract.
            *item = unsafe { *iters.add(slot) };
        }

        (*cb_guard)(RawMultiQueryInvocation {
            raw_iters,
            iter_count,
            commands,
            host_api: api,
            cmd_vtable_ext: state.cmd_vtable_ext,
            res_vtable: state.res_vtable,
            iter_reset: state.iter_reset,
        })
    })
}

struct RunConditionClosureState<C> {
    predicate: Mutex<C>,
}

unsafe extern "C" fn c_run_condition_closure_trampoline<C>(user_data: *mut c_void) -> FfiBool
where
    C: FnMut() -> bool + Send + 'static,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            return FFI_BOOL_FALSE;
        }
        // SAFETY: `user_data` points to an initialized `RunConditionClosureState<C>` owned by `GUEST_ALLOCATIONS`.
        let state = unsafe { &*user_data.cast::<RunConditionClosureState<C>>() };
        let Ok(mut guard) = state.predicate.try_lock() else {
            return FFI_BOOL_FALSE;
        };
        rust_bool_to_ffi((*guard)())
    })) {
        Ok(flag) => flag,
        Err(_) => FFI_BOOL_FALSE,
    }
}

/// Host context bundle used when registering guest closures.
#[derive(Clone, Copy)]
pub struct GuestInitHandles<'a> {
    /// Validated host API table.
    pub host_api: &'a HostApi,
    /// Raw initialization context pointer supplied to `bevy_abi_mod_init`.
    pub host_context: *mut c_void,
    /// Pre-resolved command bridge VTable.
    pub cmd_vtable_ext: CommandBridgeVTableExt,
    /// Pre-resolved resource bridge VTable.
    pub res_vtable: ResourceBridgeVTable,
    /// Optional `iter_reset` symbol.
    pub iter_reset: Option<IterResetFn>,
    /// Optional `register_multi_query_system` symbol.
    pub register_multi_query: Option<RegisterMultiQuerySystemFn>,
    /// Optional `register_conditional_system` symbol.
    pub register_conditional_system: Option<RegisterConditionalSystemFn>,
    /// Optional `register_conditional_multi_query_system` symbol.
    pub register_conditional_multi_query: Option<RegisterConditionalMultiQuerySystemFn>,
}

/// Registers a single-query guest closure with the host, staging its allocation on success.
pub fn register_system_closure<F>(
    handles: GuestInitHandles<'_>,
    staged: &mut Vec<ErasedAllocation>,
    name: &CStr,
    queries: &[ComponentAccessDesc],
    callback: F,
) -> FfiResult
where
    F: for<'a> FnMut(RawSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    let state = Box::new(SystemClosureState {
        host_api: handles.host_api as *const HostApi,
        cmd_vtable_ext: handles.cmd_vtable_ext,
        res_vtable: handles.res_vtable,
        iter_reset: handles.iter_reset,
        callback: Mutex::new(callback),
    });
    let state_ptr = Box::into_raw(state);

    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: name.as_ptr(),
        queries: if queries.is_empty() {
            core::ptr::null()
        } else {
            queries.as_ptr()
        },
        query_count: queries.len(),
        callback: c_system_closure_trampoline::<F>,
        user_data: state_ptr.cast::<c_void>(),
    };

    // SAFETY: `handles.host_context` and `desc` are valid for the duration of `register_system`.
    let status = unsafe { (handles.host_api.register_system)(handles.host_context, &desc) };
    if status.is_ok() {
        staged.push(ErasedAllocation {
            ptr: state_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<SystemClosureState<F>>,
        });
    } else {
        // SAFETY: `state_ptr` was just allocated via `Box::into_raw` above and was not retained.
        drop(unsafe { Box::from_raw(state_ptr) });
    }
    status
}

/// Registers a conditional single-query guest closure with the host, staging its allocations on success.
pub fn register_conditional_system_closure<F, C>(
    handles: GuestInitHandles<'_>,
    staged: &mut Vec<ErasedAllocation>,
    name: &CStr,
    queries: &[ComponentAccessDesc],
    run_if: C,
    callback: F,
) -> FfiResult
where
    C: FnMut() -> bool + Send + 'static,
    F: for<'a> FnMut(RawSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    let state = Box::new(SystemClosureState {
        host_api: handles.host_api as *const HostApi,
        cmd_vtable_ext: handles.cmd_vtable_ext,
        res_vtable: handles.res_vtable,
        iter_reset: handles.iter_reset,
        callback: Mutex::new(callback),
    });
    let state_ptr = Box::into_raw(state);

    let cond_state = Box::new(RunConditionClosureState {
        predicate: Mutex::new(run_if),
    });
    let cond_ptr = Box::into_raw(cond_state);

    let base = SystemDescriptor::new(
        name.as_ptr(),
        if queries.is_empty() {
            core::ptr::null()
        } else {
            queries.as_ptr()
        },
        queries.len(),
        c_system_closure_trampoline::<F>,
        state_ptr.cast::<c_void>(),
    );
    let desc = base.with_run_if(
        c_run_condition_closure_trampoline::<C>,
        cond_ptr.cast::<c_void>(),
    );

    let status = if let Some(reg_cond) = handles.register_conditional_system {
        // SAFETY: `handles.host_context` and `desc` are valid for the duration of `reg_cond`.
        unsafe { reg_cond(handles.host_context, &desc) }
    } else {
        // SAFETY: `ConditionalSystemDescriptor` starts with the exact `SystemDescriptor` prefix.
        unsafe { (handles.host_api.register_system)(handles.host_context, desc.as_base_ptr()) }
    };

    if status.is_ok() {
        staged.push(ErasedAllocation {
            ptr: state_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<SystemClosureState<F>>,
        });
        staged.push(ErasedAllocation {
            ptr: cond_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<RunConditionClosureState<C>>,
        });
    } else {
        // SAFETY: Neither `state_ptr` nor `cond_ptr` was retained by the host on registration error.
        unsafe {
            drop(Box::from_raw(cond_ptr));
            drop(Box::from_raw(state_ptr));
        }
    }
    status
}

/// Registers a multi-query guest closure with the host, staging its allocation on success.
pub fn register_multi_query_closure<F>(
    handles: GuestInitHandles<'_>,
    staged: &mut Vec<ErasedAllocation>,
    name: &CStr,
    query_groups: &[&[ComponentAccessDesc]],
    callback: F,
) -> FfiResult
where
    F: for<'a> FnMut(RawMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
{
    if query_groups.len() > MAX_GUEST_MULTI_QUERIES {
        return FfiResult::INVALID_COMPONENT;
    }
    let Some(register_mq) = handles.register_multi_query else {
        return FfiResult::NULL_POINTER;
    };

    let mut raw_specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }; MAX_GUEST_MULTI_QUERIES];
    for (slot, group) in raw_specs.iter_mut().zip(query_groups.iter()) {
        *slot = QuerySpec {
            queries: if group.is_empty() {
                core::ptr::null()
            } else {
                group.as_ptr()
            },
            query_count: group.len(),
        };
    }

    let state = Box::new(MultiQueryClosureState {
        host_api: handles.host_api as *const HostApi,
        cmd_vtable_ext: handles.cmd_vtable_ext,
        res_vtable: handles.res_vtable,
        iter_reset: handles.iter_reset,
        callback: Mutex::new(callback),
    });
    let state_ptr = Box::into_raw(state);

    let desc = MultiQuerySystemDescriptor {
        struct_size: core::mem::size_of::<MultiQuerySystemDescriptor>() as u32,
        name: name.as_ptr(),
        query_specs: if query_groups.is_empty() {
            core::ptr::null()
        } else {
            raw_specs.as_ptr()
        },
        query_spec_count: query_groups.len(),
        callback: c_multi_query_closure_trampoline::<F>,
        user_data: state_ptr.cast::<c_void>(),
    };

    // SAFETY: `handles.host_context` and `desc` (backed by stack `raw_specs`) remain valid for the call.
    let status = unsafe { register_mq(handles.host_context, &desc) };
    if status.is_ok() {
        staged.push(ErasedAllocation {
            ptr: state_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<MultiQueryClosureState<F>>,
        });
    } else {
        // SAFETY: `state_ptr` was just allocated above and was not retained by the host.
        drop(unsafe { Box::from_raw(state_ptr) });
    }
    status
}

/// Registers a conditional multi-query guest closure with the host, staging its allocations on success.
pub fn register_conditional_multi_query_closure<F, C>(
    handles: GuestInitHandles<'_>,
    staged: &mut Vec<ErasedAllocation>,
    name: &CStr,
    query_groups: &[&[ComponentAccessDesc]],
    run_if: C,
    callback: F,
) -> FfiResult
where
    C: FnMut() -> bool + Send + 'static,
    F: for<'a> FnMut(RawMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
{
    if query_groups.len() > MAX_GUEST_MULTI_QUERIES {
        return FfiResult::INVALID_COMPONENT;
    }
    if handles.register_conditional_multi_query.is_none() && handles.register_multi_query.is_none()
    {
        return FfiResult::NULL_POINTER;
    }

    let mut raw_specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }; MAX_GUEST_MULTI_QUERIES];
    for (slot, group) in raw_specs.iter_mut().zip(query_groups.iter()) {
        *slot = QuerySpec {
            queries: if group.is_empty() {
                core::ptr::null()
            } else {
                group.as_ptr()
            },
            query_count: group.len(),
        };
    }

    let state = Box::new(MultiQueryClosureState {
        host_api: handles.host_api as *const HostApi,
        cmd_vtable_ext: handles.cmd_vtable_ext,
        res_vtable: handles.res_vtable,
        iter_reset: handles.iter_reset,
        callback: Mutex::new(callback),
    });
    let state_ptr = Box::into_raw(state);

    let cond_state = Box::new(RunConditionClosureState {
        predicate: Mutex::new(run_if),
    });
    let cond_ptr = Box::into_raw(cond_state);

    let base = MultiQuerySystemDescriptor::new(
        name.as_ptr(),
        if query_groups.is_empty() {
            core::ptr::null()
        } else {
            raw_specs.as_ptr()
        },
        query_groups.len(),
        c_multi_query_closure_trampoline::<F>,
        state_ptr.cast::<c_void>(),
    );
    let desc = base.with_run_if(
        c_run_condition_closure_trampoline::<C>,
        cond_ptr.cast::<c_void>(),
    );

    let status = if let Some(reg_cond_mq) = handles.register_conditional_multi_query {
        // SAFETY: `handles.host_context` and `desc` remain valid for the duration of `reg_cond_mq`.
        unsafe { reg_cond_mq(handles.host_context, &desc) }
    } else if let Some(reg_mq) = handles.register_multi_query {
        // SAFETY: `ConditionalMultiQuerySystemDescriptor` starts with `MultiQuerySystemDescriptor`.
        unsafe { reg_mq(handles.host_context, desc.as_base_ptr()) }
    } else {
        FfiResult::NULL_POINTER
    };

    if status.is_ok() {
        staged.push(ErasedAllocation {
            ptr: state_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<MultiQueryClosureState<F>>,
        });
        staged.push(ErasedAllocation {
            ptr: cond_ptr.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<RunConditionClosureState<C>>,
        });
    } else {
        // SAFETY: Neither `state_ptr` nor `cond_ptr` was retained by the host on registration error.
        unsafe {
            drop(Box::from_raw(cond_ptr));
            drop(Box::from_raw(state_ptr));
        }
    }
    status
}
