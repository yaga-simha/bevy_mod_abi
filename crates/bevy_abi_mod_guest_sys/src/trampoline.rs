//! C-ABI system/run-condition closure trampolines and validated handshake state.

use core::ffi::{CStr, c_void};
use core::marker::PhantomData;
use std::sync::Mutex;

use bevy_abi_mod_sys::{
    ComponentAccessDesc, FFI_BOOL_FALSE, FfiBool, FfiResult, GetProcAddressFn, HostApi,
    IterResetFn, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    RegisterConditionalMultiQuerySystemFn, RegisterConditionalSystemFn, RegisterMultiQuerySystemFn,
    SystemDescriptor, rust_bool_to_ffi,
};

use crate::alloc::{ErasedAllocation, catch_guest_panic, drop_boxed_allocation};
use crate::commands::ValidatedCommandQueue;
use crate::component::register_runtime_component;
use crate::iter::ValidatedIter;
use crate::resolve::{
    CommandBridgeVTable, CommandBridgeVTableExt, ResourceBridgeVTable, resolve_iter_reset,
    resolve_register_conditional_multi_query, resolve_register_conditional_system,
    resolve_register_multi_query, resolve_symbol_cstr, resolve_vtable_raw, validate_host_api,
};

/// Maximum number of simultaneous queries supported per multi-query guest system without heap allocation.
pub const MAX_GUEST_MULTI_QUERIES: usize = 8;

/// Macro to wrap a guest Rust system function into a C-ABI `DynamicSystemCallback`
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

/// Macro to wrap a guest Rust multi-query system function into a C-ABI `MultiQuerySystemCallback`
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
/// C-ABI `RunConditionCallback` (`unsafe extern "C" fn(*mut c_void) -> FfiBool`) that catches
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

/// Unforgeable validated handshake context produced from `bevy_abi_mod_init` arguments.
#[derive(Clone, Copy)]
pub struct ValidatedInit<'a> {
    get_proc_address: GetProcAddressFn,
    raw_host_api: *const HostApi,
    host_api_val: HostApi,
    host_context: *mut c_void,
    cmd_vtable_ext: CommandBridgeVTableExt,
    res_vtable: ResourceBridgeVTable,
    iter_reset: Option<IterResetFn>,
    register_multi_query: Option<RegisterMultiQuerySystemFn>,
    register_conditional_system: Option<RegisterConditionalSystemFn>,
    register_conditional_multi_query: Option<RegisterConditionalMultiQuerySystemFn>,
    _marker: PhantomData<&'a HostApi>,
}

impl<'a> ValidatedInit<'a> {
    /// Validates raw `bevy_abi_mod_init` arguments and resolves all host bridge VTables.
    ///
    /// # Safety
    /// - `get_proc_address` must be a valid host symbol-resolver function pointer.
    /// - If `host_api` is non-null, it must point to an aligned [`HostApi`] prefix with at least 4
    ///   readable bytes (`struct_size`) and, when `struct_size >= MIN_GUEST_APP_HOST_API_SIZE`,
    ///   valid function pointers outliving all systems registered during this load session.
    /// - If `host_context` is non-null, it must point to the host's active `bevy_abi_mod_init`
    ///   context for `'a`.
    pub unsafe fn from_raw(
        get_proc_address: GetProcAddressFn,
        host_api: *const HostApi,
        host_context: *mut c_void,
    ) -> Result<Self, FfiResult> {
        // SAFETY: Caller guarantees `host_api` and `host_context` satisfy handshake invariants.
        let host_api_val = unsafe { validate_host_api(host_api, host_context)? };

        // SAFETY: Caller guarantees `get_proc_address` is a valid host symbol resolver.
        let (
            cmd_vtable_ext,
            res_vtable,
            iter_reset,
            register_multi_query,
            register_conditional_system,
            register_conditional_multi_query,
        ) = unsafe {
            (
                CommandBridgeVTableExt::resolve(get_proc_address),
                ResourceBridgeVTable::resolve(get_proc_address),
                resolve_iter_reset(get_proc_address),
                resolve_register_multi_query(get_proc_address),
                resolve_register_conditional_system(get_proc_address),
                resolve_register_conditional_multi_query(get_proc_address),
            )
        };

        Ok(Self {
            get_proc_address,
            raw_host_api: host_api,
            host_api_val,
            host_context,
            cmd_vtable_ext,
            res_vtable,
            iter_reset,
            register_multi_query,
            register_conditional_system,
            register_conditional_multi_query,
            _marker: PhantomData,
        })
    }

    /// Returns a reference to the validated [`HostApi`] table.
    #[inline]
    pub fn host_api(&self) -> &HostApi {
        &self.host_api_val
    }

    /// Returns the raw `host_context` pointer supplied to `bevy_abi_mod_init`.
    #[inline]
    pub fn host_context(&self) -> *mut c_void {
        self.host_context
    }

    /// Returns the validated [`GetProcAddressFn`] symbol resolver.
    #[inline]
    pub fn get_proc_address(&self) -> GetProcAddressFn {
        self.get_proc_address
    }

    /// Returns the pre-resolved [`CommandBridgeVTable`].
    #[inline]
    pub fn command_vtable(&self) -> CommandBridgeVTable {
        self.cmd_vtable_ext.base
    }

    /// Returns the pre-resolved [`CommandBridgeVTableExt`].
    #[inline]
    pub fn command_vtable_ext(&self) -> CommandBridgeVTableExt {
        self.cmd_vtable_ext
    }

    /// Returns the pre-resolved [`ResourceBridgeVTable`].
    #[inline]
    pub fn resource_vtable(&self) -> ResourceBridgeVTable {
        self.res_vtable
    }

    /// Resolves a raw symbol pointer by nul-terminated name via `get_proc_address`.
    #[inline]
    pub fn resolve_symbol(&self, symbol_name: &CStr) -> *const c_void {
        // SAFETY: `self.get_proc_address` was validated in `ValidatedInit::from_raw`.
        unsafe { resolve_symbol_cstr(self.get_proc_address, symbol_name) }
    }

    /// Resolves a typed static component VTable reference (for example `c"Position_vtable"`).
    ///
    /// # Safety
    /// Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
    /// under `symbol_name` and outlives `'a`.
    #[inline]
    pub unsafe fn resolve_vtable<V: Copy + 'static>(&self, symbol_name: &CStr) -> Option<&'a V> {
        // SAFETY: `self.get_proc_address` was validated in `from_raw`; caller guarantees `V` layout.
        unsafe { resolve_vtable_raw::<V>(self.get_proc_address, symbol_name) }
    }

    /// Registers a runtime dynamic component `T` under `name` on this host context.
    #[inline]
    pub fn register_component<T: 'static>(&self, name: &CStr) -> FfiResult {
        // SAFETY: `self.raw_host_api` and `self.host_context` were validated in `ValidatedInit::from_raw`.
        unsafe { register_runtime_component::<T>(self.raw_host_api, self.host_context, name) }
    }

    /// Extracts [`GuestInitHandles`] for closure registration helpers.
    #[inline]
    pub fn handles(&self) -> GuestInitHandles<'_> {
        GuestInitHandles {
            host_api: &self.host_api_val,
            host_context: self.host_context,
            cmd_vtable_ext: self.cmd_vtable_ext,
            res_vtable: self.res_vtable,
            iter_reset: self.iter_reset,
            register_multi_query: self.register_multi_query,
            register_conditional_system: self.register_conditional_system,
            register_conditional_multi_query: self.register_conditional_multi_query,
        }
    }
}

/// Validated safe handles delivered to a single-query guest system closure trampoline.
pub struct ValidatedSystemInvocation<'a> {
    /// Validated chunk iterator for this system execution.
    pub iter: ValidatedIter<'a>,
    /// Validated deferred command queue for this system execution.
    pub commands: ValidatedCommandQueue<'a>,
    /// Validated host API table.
    pub host_api: &'a HostApi,
}

/// Validated safe handles delivered to a multi-query guest system closure trampoline.
pub struct ValidatedMultiQueryInvocation<'a> {
    /// Array of validated chunk iterators (`0..iter_count` populated with `Some`).
    pub iters: [Option<ValidatedIter<'a>>; MAX_GUEST_MULTI_QUERIES],
    /// Number of active query iterators.
    pub iter_count: usize,
    /// Validated deferred command queue for this system execution.
    pub commands: ValidatedCommandQueue<'a>,
    /// Validated host API table.
    pub host_api: &'a HostApi,
    /// Pre-resolved resource bridge VTable.
    pub res_vtable: ResourceBridgeVTable,
}

struct SystemClosureState<F> {
    host_api: HostApi,
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
    F: for<'a> FnMut(ValidatedSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    catch_guest_panic(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to an initialized `SystemClosureState<F>` owned by `GUEST_ALLOCATIONS`
        // that remains alive until `bevy_abi_mod_shutdown` runs after all systems are deactivated.
        let state = unsafe { &*user_data.cast::<SystemClosureState<F>>() };
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        let api = &state.host_api;
        // SAFETY: `iter` and `commands` are scoped to this C-ABI system invocation and match `state`'s
        // validated `HostApi` and resolved bridge VTables.
        let (validated_iter, validated_commands) = unsafe {
            (
                ValidatedIter::with_bridges(iter, api, state.iter_reset, state.res_vtable),
                ValidatedCommandQueue::with_ext(commands, state.cmd_vtable_ext),
            )
        };
        (*cb_guard)(ValidatedSystemInvocation {
            iter: validated_iter,
            commands: validated_commands,
            host_api: api,
        })
    })
}

struct MultiQueryClosureState<F> {
    host_api: HostApi,
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
    F: for<'a> FnMut(ValidatedMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
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
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        let api = &state.host_api;

        let mut validated_iters: [Option<ValidatedIter<'_>>; MAX_GUEST_MULTI_QUERIES] =
            core::array::from_fn(|_| None);
        for (slot, item) in validated_iters.iter_mut().enumerate().take(iter_count) {
            // SAFETY: `iters` is non-null and holds `iter_count` valid `RawChunkIter` handles for this call.
            let raw_iter = unsafe { *iters.add(slot) };
            // SAFETY: `raw_iter` belongs to query group `slot` of this system invocation.
            *item =
                Some(unsafe { ValidatedIter::for_multi_query(raw_iter, api, state.iter_reset) });
        }

        // SAFETY: `commands` is scoped to this C-ABI multi-query system invocation.
        let validated_commands =
            unsafe { ValidatedCommandQueue::with_ext(commands, state.cmd_vtable_ext) };

        (*cb_guard)(ValidatedMultiQueryInvocation {
            iters: validated_iters,
            iter_count,
            commands: validated_commands,
            host_api: api,
            res_vtable: state.res_vtable,
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
    F: for<'a> FnMut(ValidatedSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    let state = Box::new(SystemClosureState {
        host_api: *handles.host_api,
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
        staged.push(ErasedAllocation::from_raw_parts(
            state_ptr.cast::<c_void>(),
            drop_boxed_allocation::<SystemClosureState<F>>,
        ));
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
    F: for<'a> FnMut(ValidatedSystemInvocation<'a>) -> FfiResult + Send + 'static,
{
    let state = Box::new(SystemClosureState {
        host_api: *handles.host_api,
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
        staged.push(ErasedAllocation::from_raw_parts(
            state_ptr.cast::<c_void>(),
            drop_boxed_allocation::<SystemClosureState<F>>,
        ));
        staged.push(ErasedAllocation::from_raw_parts(
            cond_ptr.cast::<c_void>(),
            drop_boxed_allocation::<RunConditionClosureState<C>>,
        ));
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
    F: for<'a> FnMut(ValidatedMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
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
        host_api: *handles.host_api,
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
        staged.push(ErasedAllocation::from_raw_parts(
            state_ptr.cast::<c_void>(),
            drop_boxed_allocation::<MultiQueryClosureState<F>>,
        ));
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
    F: for<'a> FnMut(ValidatedMultiQueryInvocation<'a>) -> FfiResult + Send + 'static,
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
        host_api: *handles.host_api,
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
        staged.push(ErasedAllocation::from_raw_parts(
            state_ptr.cast::<c_void>(),
            drop_boxed_allocation::<MultiQueryClosureState<F>>,
        ));
        staged.push(ErasedAllocation::from_raw_parts(
            cond_ptr.cast::<c_void>(),
            drop_boxed_allocation::<RunConditionClosureState<C>>,
        ));
    } else {
        // SAFETY: Neither `state_ptr` nor `cond_ptr` was retained by the host on registration error.
        unsafe {
            drop(Box::from_raw(cond_ptr));
            drop(Box::from_raw(state_ptr));
        }
    }
    status
}

/// Validates C-ABI handshake arguments inside [`catch_guest_panic`] and invokes `f` with the
/// resulting [`ValidatedInit`].
///
/// # Safety
/// Must be called with valid handshake arguments supplied to `bevy_abi_mod_init`.
pub unsafe fn run_with_validated_init<F>(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
    f: F,
) -> FfiResult
where
    F: FnOnce(ValidatedInit<'_>) -> FfiResult,
{
    catch_guest_panic(|| {
        // SAFETY: Caller guarantees `get_proc_address`, `host_api`, and `host_context` satisfy
        // the `bevy_abi_mod_init` handshake invariants.
        let init = unsafe { ValidatedInit::from_raw(get_proc_address, host_api, host_context) };
        match init {
            Ok(validated) => f(validated),
            Err(err) => err,
        }
    })
}

/// Bridge trait implemented by the safe SDK's `GuestApp` to expose raw `unsafe` handshake helpers.
pub trait ValidatedAppAdapter<'a>: Sized {
    /// Constructs `Self` from an unforgeable [`ValidatedInit`].
    fn from_validated_init(init: ValidatedInit<'a>) -> Self;
    /// Borrows the underlying [`ValidatedInit`].
    fn as_validated_init(&self) -> &ValidatedInit<'a>;
}

/// Raw `unsafe` handshake and VTable resolution extension for `GuestApp`.
pub trait GuestAppRawExt<'a>: ValidatedAppAdapter<'a> {
    /// Constructs `Self` from the raw C-ABI arguments passed to `bevy_abi_mod_init`.
    ///
    /// # Safety
    /// - `get_proc_address` must be a valid host symbol-resolver function pointer.
    /// - If non-null, `host_api` must point to an aligned [`HostApi`] prefix outliving all
    ///   registered system executions for this mod load session.
    /// - If non-null, `host_context` must point to the host's initialization context for `'a`.
    unsafe fn from_raw(
        get_proc_address: GetProcAddressFn,
        host_api: *const HostApi,
        host_context: *mut c_void,
    ) -> Result<Self, FfiResult> {
        // SAFETY: Forwarding caller's C-ABI handshake invariants to `ValidatedInit::from_raw`.
        let init = unsafe { ValidatedInit::from_raw(get_proc_address, host_api, host_context)? };
        Ok(Self::from_validated_init(init))
    }

    /// Resolves a typed static component VTable reference (for example `c"Position_vtable"`).
    ///
    /// # Safety
    /// Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
    /// under `symbol_name` and outlives `'a`.
    #[inline]
    unsafe fn resolve_vtable<V: Copy + 'static>(&self, symbol_name: &CStr) -> Option<&'a V> {
        // SAFETY: Forwarding caller's VTable layout guarantee to `ValidatedInit::resolve_vtable`.
        unsafe { self.as_validated_init().resolve_vtable::<V>(symbol_name) }
    }
}

impl<'a, T: ValidatedAppAdapter<'a>> GuestAppRawExt<'a> for T {}

/// Lifecycle adapter enabling transactional C-ABI initialization for a safe `GuestApp` builder type.
pub trait GuestAppLifecycle {
    /// Builder type parameterized by the initialization lifetime.
    type App<'a>;

    /// Constructs the builder from an unforgeable [`ValidatedInit`].
    fn build_from_validated<'a>(init: ValidatedInit<'a>) -> Self::App<'a>;

    /// Commits staged allocations upon successful initialization.
    fn commit_app(app: Self::App<'_>);
}

/// Executes a [`GuestAppLifecycle`] setup closure inside [`catch_guest_panic`], committing staged
/// allocations on [`FfiResult::OK`] and rolling them back on error or panic.
///
/// # Safety
/// Must be called with valid handshake arguments supplied to `bevy_abi_mod_init`.
pub unsafe fn run_lifecycle_init<L, F>(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
    build_fn: F,
) -> FfiResult
where
    L: GuestAppLifecycle,
    F: for<'a> FnOnce(&mut L::App<'a>) -> FfiResult,
{
    // SAFETY: Forwarding C-ABI handshake arguments to `run_with_validated_init`.
    unsafe {
        run_with_validated_init(get_proc_address, host_api, host_context, |init| {
            let mut app = L::build_from_validated(init);
            let status = build_fn(&mut app);
            if status.is_ok() {
                L::commit_app(app);
            }
            status
        })
    }
}

/// Extension trait providing `GuestApp::run_init` over raw C-ABI handshake arguments.
pub trait GuestAppInitExt: GuestAppLifecycle + Sized {
    /// Validates raw handshake arguments inside [`catch_guest_panic`], runs `build_fn`, and commits
    /// staged allocations on [`FfiResult::OK`].
    ///
    /// # Safety
    /// Must be called with valid handshake arguments supplied to `bevy_abi_mod_init`.
    #[inline]
    unsafe fn run_init<F>(
        get_proc_address: GetProcAddressFn,
        host_api: *const HostApi,
        host_context: *mut c_void,
        build_fn: F,
    ) -> FfiResult
    where
        F: for<'a> FnOnce(&mut Self::App<'a>) -> FfiResult,
    {
        // SAFETY: Forwarding C-ABI handshake invariants to `run_lifecycle_init`.
        unsafe { run_lifecycle_init::<Self, F>(get_proc_address, host_api, host_context, build_fn) }
    }
}

impl<T: GuestAppLifecycle> GuestAppInitExt for T {}
