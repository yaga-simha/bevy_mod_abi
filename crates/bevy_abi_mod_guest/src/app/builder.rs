//! High-level guest initialization builder managing staged closures and rollback safety.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_guest_sys::{
    CommandBridgeVTable, CommandBridgeVTableExt, ErasedAllocation, ResourceBridgeVTable,
    ValidatedAppAdapter, ValidatedInit, ValidatedMultiQueryInvocation, ValidatedSystemInvocation,
    abort_allocations, commit_allocations, register_conditional_multi_query_closure,
    register_conditional_system_closure, register_multi_query_closure, register_system_closure,
};
use bevy_abi_mod_sys::{ComponentAccessDesc, FfiResult, GetProcAddressFn, HostApi};

use crate::app::context::SystemContext;
use crate::app::multi_query::MultiQueryContext;
use crate::commands::GuestCommandQueue;
use crate::component::GuestComponent;
use crate::iter::GuestChunkIter;

/// High-level mod initialization builder managing system closures, runtime component registration,
/// and transactional guest-heap allocation cleanup.
///
/// Obtain `&mut GuestApp<'_>` safely via [`export_mod!`](crate::export_mod). Constructing a
/// `GuestApp` directly from raw C-ABI pointers without `unsafe` is rejected:
///
/// ```compile_fail
/// use core::ffi::{c_char, c_void};
/// use bevy_abi_mod_guest::prelude::*;
///
/// unsafe extern "C" fn stub_gpa(_name: *const c_char) -> *const c_void {
///     core::ptr::null()
/// }
///
/// let _app = GuestApp::from_raw(stub_gpa, core::ptr::null(), core::ptr::null_mut());
/// ```
pub struct GuestApp<'a> {
    init: ValidatedInit<'a>,
    staged_allocations: Vec<ErasedAllocation>,
}

impl<'a> GuestApp<'a> {
    #[inline]
    pub(crate) fn from_validated(init: ValidatedInit<'a>) -> Self {
        Self {
            init,
            staged_allocations: Vec::new(),
        }
    }

    /// Returns the validated [`HostApi`] table.
    #[inline]
    pub fn host_api(&self) -> &HostApi {
        self.init.host_api()
    }

    /// Returns the raw `host_context` pointer for interoperability with low-level registration helpers.
    #[inline]
    pub fn host_context(&self) -> *mut c_void {
        self.init.host_context()
    }

    /// Returns the host's [`GetProcAddressFn`] symbol resolver.
    #[inline]
    pub fn get_proc_address(&self) -> GetProcAddressFn {
        self.init.get_proc_address()
    }

    /// Returns the pre-resolved [`CommandBridgeVTable`] for this world.
    #[inline]
    pub fn command_vtable(&self) -> CommandBridgeVTable {
        self.init.command_vtable()
    }

    /// Returns the pre-resolved [`CommandBridgeVTableExt`] (including `remove_component` and `insert_default`).
    #[inline]
    pub fn command_vtable_ext(&self) -> CommandBridgeVTableExt {
        self.init.command_vtable_ext()
    }

    /// Returns the pre-resolved [`ResourceBridgeVTable`] (`res_readonly`, `res_mutable`) for this world.
    #[inline]
    pub fn resource_vtable(&self) -> ResourceBridgeVTable {
        self.init.resource_vtable()
    }

    /// Resolves a raw symbol pointer by nul-terminated name via `get_proc_address`.
    #[inline]
    pub fn resolve_symbol(&self, symbol_name: &CStr) -> *const c_void {
        self.init.resolve_symbol(symbol_name)
    }

    /// Registers a runtime dynamic component `T` under `name`.
    #[inline]
    pub fn register_component<T: 'static>(&mut self, name: &CStr) -> FfiResult {
        self.init.register_component::<T>(name)
    }

    /// Registers a runtime dynamic component implementing [`GuestComponent`] using `T::NAME`.
    #[inline]
    pub fn register_guest_component<T: GuestComponent>(&mut self) -> FfiResult {
        self.register_component::<T>(T::NAME)
    }

    /// Allocates a guest-owned value on the heap and returns a stable `*mut T` suitable for
    /// `SystemDescriptor::user_data`, automatically freeing it on `bevy_abi_mod_shutdown`
    /// (or immediately if `bevy_abi_mod_init` fails).
    pub fn alloc_user_data<T: Send + 'static>(&mut self, value: T) -> *mut T {
        let (alloc, raw) = ErasedAllocation::new_boxed(value);
        self.staged_allocations.push(alloc);
        raw
    }

    /// Registers a custom cleanup closure to execute automatically during `bevy_abi_mod_shutdown`
    /// (or on rollback if `bevy_abi_mod_init` fails).
    pub fn on_shutdown<F>(&mut self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.staged_allocations
            .push(ErasedAllocation::new_shutdown_hook(cleanup));
    }

    /// Registers a panic-safe, leak-free Rust system closure with the host ECS schedule.
    pub fn add_system<F>(
        &mut self,
        name: &CStr,
        queries: &[ComponentAccessDesc],
        mut system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
    {
        register_system_closure(
            self.init.handles(),
            &mut self.staged_allocations,
            name,
            queries,
            move |inv: ValidatedSystemInvocation<'_>| {
                let mut ctx = SystemContext {
                    iter: GuestChunkIter::from_validated(inv.iter),
                    commands: GuestCommandQueue::from_validated(inv.commands),
                    host_api: inv.host_api,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a panic-safe, leak-free Rust system closure gated by a `run_if` predicate closure.
    pub fn add_system_if<F, C>(
        &mut self,
        name: &CStr,
        queries: &[ComponentAccessDesc],
        run_if: C,
        mut system: F,
    ) -> FfiResult
    where
        C: FnMut() -> bool + Send + 'static,
        F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
    {
        register_conditional_system_closure(
            self.init.handles(),
            &mut self.staged_allocations,
            name,
            queries,
            run_if,
            move |inv: ValidatedSystemInvocation<'_>| {
                let mut ctx = SystemContext {
                    iter: GuestChunkIter::from_validated(inv.iter),
                    commands: GuestCommandQueue::from_validated(inv.commands),
                    host_api: inv.host_api,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a multi-query Rust system closure (up to [`MAX_GUEST_MULTI_QUERIES`](crate::MAX_GUEST_MULTI_QUERIES) query groups).
    pub fn add_multi_query_system<F>(
        &mut self,
        name: &CStr,
        query_groups: &[&[ComponentAccessDesc]],
        mut system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
    {
        register_multi_query_closure(
            self.init.handles(),
            &mut self.staged_allocations,
            name,
            query_groups,
            move |inv: ValidatedMultiQueryInvocation<'_>| {
                let mut ctx = MultiQueryContext {
                    iters: inv.iters.map(|opt| opt.map(GuestChunkIter::from_validated)),
                    iter_count: inv.iter_count,
                    commands: GuestCommandQueue::from_validated(inv.commands),
                    host_api: inv.host_api,
                    res_vtable: inv.res_vtable,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a multi-query Rust system closure gated by a `run_if` predicate closure.
    pub fn add_multi_query_system_if<F, C>(
        &mut self,
        name: &CStr,
        query_groups: &[&[ComponentAccessDesc]],
        run_if: C,
        mut system: F,
    ) -> FfiResult
    where
        C: FnMut() -> bool + Send + 'static,
        F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
    {
        register_conditional_multi_query_closure(
            self.init.handles(),
            &mut self.staged_allocations,
            name,
            query_groups,
            run_if,
            move |inv: ValidatedMultiQueryInvocation<'_>| {
                let mut ctx = MultiQueryContext {
                    iters: inv.iters.map(|opt| opt.map(GuestChunkIter::from_validated)),
                    iter_count: inv.iter_count,
                    commands: GuestCommandQueue::from_validated(inv.commands),
                    host_api: inv.host_api,
                    res_vtable: inv.res_vtable,
                };
                system(&mut ctx)
            },
        )
    }

    /// Commits all staged allocations from this initialization session to the guest lifecycle
    /// registry so they remain alive until `bevy_abi_mod_shutdown`.
    pub fn commit(mut self) {
        commit_allocations(&mut self.staged_allocations);
    }
}

impl<'a> ValidatedAppAdapter<'a> for GuestApp<'a> {
    #[inline]
    fn from_validated_init(init: ValidatedInit<'a>) -> Self {
        Self::from_validated(init)
    }

    #[inline]
    fn as_validated_init(&self) -> &ValidatedInit<'a> {
        &self.init
    }
}

impl Drop for GuestApp<'_> {
    fn drop(&mut self) {
        if !self.staged_allocations.is_empty() {
            let aborted = core::mem::take(&mut self.staged_allocations);
            abort_allocations(aborted);
        }
    }
}
