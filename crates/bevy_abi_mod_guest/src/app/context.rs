//! Unified system execution context (`SystemContext`) and run-condition context (`RunConditionContext`).

use core::ffi::{CStr, c_void};

use bevy_abi_mod_guest_sys::{Host, ValidatedIter, ValidatedSystemInvocation, resolve_component};
use bevy_abi_mod_sys::{
    AbiPod, BamComponent, BamHostApi, BamSystemCtx, ComponentUuid, FfiResult,
    MAX_QUERIES_PER_SYSTEM,
};

use crate::{
    commands::GuestCommands,
    component::{GuestComponent, resolve_guest_component},
    iter::ChunkIter,
};

/// Safe context passed to `.run_if(...)` condition closures.
///
/// Note that Bevy evaluates run conditions before constructing a per-system `BamSystemCtx`, so
/// `RunConditionContext` exposes world-scoped [`Host`] diagnostics, handle resolution, and
/// `"bam.v0.*"` custom symbol queries rather than entity iterators or deferred commands.
pub struct RunConditionContext<'a> {
    host: &'a Host,
}

impl<'a> RunConditionContext<'a> {
    #[inline]
    pub(crate) const fn new(host: &'a Host) -> Self {
        Self { host }
    }

    /// Returns the cached [`Host`] wrapper for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Returns the validated [`BamHostApi`] function table.
    #[inline]
    pub const fn host_api(&self) -> &'a BamHostApi {
        self.host.api()
    }

    /// Reads the most recent human-readable UTF-8 diagnostic recorded on this world's host context.
    #[inline]
    pub fn last_error(&self) -> String {
        self.host.last_error()
    }

    /// Resolves a component or resource handle by `name`, `uuid`, or both.
    #[inline]
    pub fn resolve_component(
        &self,
        name: Option<&CStr>,
        uuid: Option<ComponentUuid>,
    ) -> Result<BamComponent, FfiResult> {
        resolve_component(self.host, name, uuid)
    }

    /// Resolves `T: GuestComponent` (including manifest-qualified `C-10` names) on this world.
    #[inline]
    pub fn resolve_guest_component<T: GuestComponent>(&self) -> Result<BamComponent, FfiResult> {
        resolve_guest_component::<T>(self.host, self.host.manifest_name())
    }

    /// Resolves a custom `"bam.v0.<name>"` host symbol pointer.
    #[inline]
    pub fn resolve_custom_symbol(&self, symbol: &CStr) -> Result<*const c_void, FfiResult> {
        self.host.resolve_custom_symbol(symbol)
    }
}

/// Unified per-tick execution context passed to every guest system built with
/// [`SystemBuilder::build`](crate::SystemBuilder::build).
///
/// Supports zero-query, single-query (`iter`, `iter_and_commands`), multi-query (`query_mut`,
/// `queries_2_mut`, `queries_2_and_commands`), declaration-indexed singleton resources
/// (`resource`, `resource_mut`), and handle/batched [`GuestCommands`].
pub struct SystemContext<'a> {
    host: &'a Host,
    raw_sys_ctx: *mut BamSystemCtx,
    user_query_count: usize,
    queries: [Option<ChunkIter<'a>>; MAX_QUERIES_PER_SYSTEM],
    resource_query: Option<ChunkIter<'a>>,
    empty_iter: ChunkIter<'a>,
    commands: GuestCommands<'a>,
}

/// Backwards-compatible alias unifying multi-query systems onto [`SystemContext`].
pub type MultiQueryContext<'a> = SystemContext<'a>;

impl<'a> SystemContext<'a> {
    pub(crate) fn from_invocation(inv: &mut ValidatedSystemInvocation<'a>) -> Self {
        let host = inv.host();
        let raw_sys_ctx = inv.as_raw_sys_ctx();
        let user_query_count = inv.query_count();
        let mut queries: [Option<ChunkIter<'a>>; MAX_QUERIES_PER_SYSTEM] =
            [const { None }; MAX_QUERIES_PER_SYSTEM];
        for (idx, slot) in queries.iter_mut().enumerate().take(user_query_count) {
            *slot = inv.take_raw_query_slot(idx).map(ChunkIter::from_validated);
        }
        let resource_query = inv
            .resource_query_index()
            .and_then(|res_idx| inv.take_raw_query_slot(res_idx))
            .map(ChunkIter::from_validated);
        let commands = GuestCommands::from_validated(inv.take_commands());
        Self {
            host,
            raw_sys_ctx,
            user_query_count,
            queries,
            resource_query,
            empty_iter: ChunkIter::from_validated(ValidatedIter::empty(host)),
            commands,
        }
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Returns the validated [`BamHostApi`] function table.
    #[inline]
    pub const fn host_api(&self) -> &'a BamHostApi {
        self.host.api()
    }

    /// Returns the raw `*mut BamSystemCtx` pointer for low-level FFI helpers.
    #[inline]
    pub const fn as_raw_ptr(&self) -> *mut BamSystemCtx {
        self.raw_sys_ctx
    }

    /// Reads the most recent human-readable UTF-8 diagnostic recorded on this world's host context.
    #[inline]
    pub fn last_error(&self) -> String {
        self.host.last_error()
    }

    /// Returns the number of explicit `.query(...)` clauses declared on this system.
    #[inline]
    pub const fn query_count(&self) -> usize {
        self.user_query_count
    }

    /// Borrows primary query `0` mutably (yielding an exhausted empty iterator if no `.query(...)`
    /// clause was declared).
    #[inline]
    pub fn iter(&mut self) -> &mut ChunkIter<'a> {
        if self.user_query_count == 0 {
            return &mut self.empty_iter;
        }
        self.queries[0].as_mut().unwrap_or(&mut self.empty_iter)
    }

    /// Borrows declared query `index` (`0..query_count()`) mutably (`None` if out of range).
    #[inline]
    pub fn query(&mut self, index: usize) -> Option<&mut ChunkIter<'a>> {
        self.query_mut(index)
    }

    /// Borrows declared query `index` (`0..query_count()`) mutably (`None` if out of range).
    #[inline]
    pub fn query_mut(&mut self, index: usize) -> Option<&mut ChunkIter<'a>> {
        if index >= self.user_query_count {
            return None;
        }
        self.queries[index].as_mut()
    }

    /// Simultaneously borrows two distinct queries `idx_a` and `idx_b` (`None` if `idx_a == idx_b`
    /// or either index is out of range).
    pub fn queries_2_mut(
        &mut self,
        idx_a: usize,
        idx_b: usize,
    ) -> Option<(&mut ChunkIter<'a>, &mut ChunkIter<'a>)> {
        if idx_a == idx_b || idx_a >= self.user_query_count || idx_b >= self.user_query_count {
            return None;
        }
        if idx_a < idx_b {
            let (left, right) = self.queries.split_at_mut(idx_b);
            Some((left[idx_a].as_mut()?, right[0].as_mut()?))
        } else {
            let (left, right) = self.queries.split_at_mut(idx_a);
            Some((right[0].as_mut()?, left[idx_b].as_mut()?))
        }
    }

    /// Borrows the deferred [`GuestCommands`] buffer mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommands<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows primary query `0` and [`GuestCommands`].
    #[inline]
    pub fn iter_and_commands(&mut self) -> (&mut ChunkIter<'a>, &mut GuestCommands<'a>) {
        let cmds = &mut self.commands;
        let iter = if self.user_query_count == 0 {
            &mut self.empty_iter
        } else {
            self.queries[0].as_mut().unwrap_or(&mut self.empty_iter)
        };
        (iter, cmds)
    }

    /// Simultaneously borrows two distinct queries `idx_a` and `idx_b` alongside [`GuestCommands`].
    pub fn queries_2_and_commands(
        &mut self,
        idx_a: usize,
        idx_b: usize,
    ) -> Option<(
        &mut ChunkIter<'a>,
        &mut ChunkIter<'a>,
        &mut GuestCommands<'a>,
    )> {
        if idx_a == idx_b || idx_a >= self.user_query_count || idx_b >= self.user_query_count {
            return None;
        }
        let cmds = &mut self.commands;
        if idx_a < idx_b {
            let (left, right) = self.queries.split_at_mut(idx_b);
            Some((left[idx_a].as_mut()?, right[0].as_mut()?, cmds))
        } else {
            let (left, right) = self.queries.split_at_mut(idx_a);
            Some((right[0].as_mut()?, left[idx_b].as_mut()?, cmds))
        }
    }

    /// Borrows singleton resource `R` (`R: AbiPod`) by declaration index `decl_index` (`C-05`).
    ///
    /// Routes to the system-level resource bundle when `.resource_read` / `.resource_write` was
    /// called on [`SystemBuilder`](crate::SystemBuilder), or to query `0` when resources were
    /// declared inside `.query(|q| ...)`.
    #[inline]
    pub fn resource<R: AbiPod>(&self, decl_index: usize) -> Option<&R> {
        if let Some(ref res_q) = self.resource_query {
            res_q.resource::<R>(decl_index)
        } else {
            self.queries.first()?.as_ref()?.resource::<R>(decl_index)
        }
    }

    /// Borrows singleton resource `R` (`R: GuestComponent`) by declaration index `decl_index`,
    /// validating `R::UUID` (`C-05` + `C-13`).
    #[inline]
    pub fn component_resource<R: GuestComponent>(&self, decl_index: usize) -> Option<&R> {
        if let Some(ref res_q) = self.resource_query {
            res_q.component_resource::<R>(decl_index)
        } else {
            self.queries
                .first()?
                .as_ref()?
                .component_resource::<R>(decl_index)
        }
    }

    /// Mutably borrows singleton resource `R` (`R: AbiPod`) declared with `RESOURCE_WRITE` at
    /// `decl_index` (`C-05`).
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, decl_index: usize) -> Option<&mut R> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.resource_mut::<R>(decl_index)
        } else {
            self.queries
                .first_mut()?
                .as_mut()?
                .resource_mut::<R>(decl_index)
        }
    }

    /// Mutably borrows singleton resource `R` (`R: GuestComponent`) at `decl_index`, validating
    /// `R::UUID` (`C-05` + `C-13`).
    #[inline]
    pub fn component_resource_mut<R: GuestComponent>(
        &mut self,
        decl_index: usize,
    ) -> Option<&mut R> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.component_resource_mut::<R>(decl_index)
        } else {
            self.queries
                .first_mut()?
                .as_mut()?
                .component_resource_mut::<R>(decl_index)
        }
    }
}
