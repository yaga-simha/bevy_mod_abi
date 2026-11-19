//! Unified system execution context (`SystemContext`) and run-condition context (`RunConditionContext`).

use core::ffi::{CStr, c_void};

use bevy_abi_mod_guest_sys::{
    Host, ValidatedIter, ValidatedSystemInvocation, ValidatedSystemRef, resolve_component,
};
use bevy_abi_mod_sys::{
    AbiPod, BamChangeTicks, BamComponent, BamHostApi, BamSystemCtx, ComponentUuid, FfiEntity,
    FfiResult, MAX_QUERIES_PER_SYSTEM,
};

use crate::{
    app::builder::IntoSystemResult,
    commands::GuestCommands,
    component::{GuestComponent, guest_uuid_filter, resolve_guest_component},
    iter::{Chunk, ChunkIter, QueryTuple, TypedChunkIter},
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

    /// Resolves `T: GuestComponent` (including manifest-qualified names) on this world.
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
    pub(crate) sys_ref: ValidatedSystemRef<'a>,
    user_query_count: usize,
    queries: [Option<ChunkIter<'a>>; MAX_QUERIES_PER_SYSTEM],
    resource_query: Option<ChunkIter<'a>>,
    empty_iter: ChunkIter<'a>,
    commands: GuestCommands<'a>,
}

/// Safe read-only singleton resource guard carrying Bevy change-detection [`BamChangeTicks`].
pub struct GuestRes<'a, R> {
    value: &'a R,
    ticks: BamChangeTicks,
}

impl<'a, R> GuestRes<'a, R> {
    /// Wraps a validated resource reference and its Bevy change-detection ticks.
    #[inline]
    pub const fn new(value: &'a R, ticks: BamChangeTicks) -> Self {
        Self { value, ticks }
    }

    /// Returns raw [`BamChangeTicks`] (`added`, `changed`, `last_run`, `this_run`).
    #[inline]
    pub const fn ticks(&self) -> BamChangeTicks {
        self.ticks
    }

    /// Returns `true` if the resource was added since this system last ran.
    #[inline]
    pub const fn is_added(&self) -> bool {
        self.ticks.is_added()
    }

    /// Returns `true` if the resource was added or changed since this system last ran.
    #[inline]
    pub const fn is_changed(&self) -> bool {
        self.ticks.is_changed()
    }
}

impl<R> core::ops::Deref for GuestRes<'_, R> {
    type Target = R;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.value
    }
}

/// Safe exclusive mutable singleton resource guard carrying prior [`BamChangeTicks`].
pub struct GuestResMut<'a, R> {
    value: &'a mut R,
    ticks: BamChangeTicks,
}

impl<'a, R> GuestResMut<'a, R> {
    /// Wraps a validated mutable resource reference and its prior Bevy change-detection ticks.
    #[inline]
    pub const fn new(value: &'a mut R, ticks: BamChangeTicks) -> Self {
        Self { value, ticks }
    }

    /// Returns raw [`BamChangeTicks`] captured before this mutable borrow stamped `changed`.
    #[inline]
    pub const fn ticks(&self) -> BamChangeTicks {
        self.ticks
    }

    /// Returns `true` if the resource was added since this system last ran.
    #[inline]
    pub const fn is_added(&self) -> bool {
        self.ticks.is_added()
    }

    /// Returns `true` if the resource was changed by an earlier system since this system last ran.
    #[inline]
    pub const fn is_changed(&self) -> bool {
        self.ticks.is_changed()
    }
}

impl<R> core::ops::Deref for GuestResMut<'_, R> {
    type Target = R;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<R> core::ops::DerefMut for GuestResMut<'_, R> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
    }
}

impl<'a> SystemContext<'a> {
    pub(crate) fn from_invocation(inv: &mut ValidatedSystemInvocation<'a>) -> Self {
        let host = inv.host();
        let sys_ref = inv.system_ref();
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
            sys_ref,
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
        self.sys_ref.as_raw_sys_ctx()
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

    /// Borrows singleton resource `R` (`R: AbiPod`) by declaration index `decl_index`.
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
    /// validating `R::UUID`.
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
    /// `decl_index`.
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
    /// `R::UUID`.
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

    /// Looks up a shared singleton resource `R` wrapped in [`GuestRes`] by pre-resolved
    /// [`BamComponent`] handle (`sys_res_ptr`), returning `None` if undeclared or absent.
    #[inline]
    pub fn res_by_handle<R: AbiPod>(&self, handle: BamComponent) -> Option<GuestRes<'_, R>> {
        self.sys_ref
            .res_ptr_by_handle_checked::<R>(handle)
            .map(|(val, ticks)| GuestRes::new(val, ticks))
    }

    /// Looks up an exclusive mutable singleton resource `R` wrapped in [`GuestResMut`] by
    /// pre-resolved [`BamComponent`] handle (`sys_res_mut_ptr`), returning `None` if undeclared
    /// with `ResourceWrite`, immutable, or absent.
    #[inline]
    pub fn res_mut_by_handle<R: AbiPod>(
        &mut self,
        handle: BamComponent,
    ) -> Option<GuestResMut<'_, R>> {
        self.sys_ref
            .res_mut_ptr_by_handle_checked::<R>(handle)
            .map(|(val, ticks)| GuestResMut::new(val, ticks))
    }

    /// Reads Bevy change-detection ticks for the singleton resource declared at `decl_index`.
    #[inline]
    pub fn resource_ticks(&self, decl_index: usize) -> Result<BamChangeTicks, FfiResult> {
        if let Some(ref res_q) = self.resource_query {
            res_q.resource_ticks(decl_index)
        } else {
            self.queries
                .first()
                .and_then(Option::as_ref)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .resource_ticks(decl_index)
        }
    }

    /// Borrows singleton resource `R` (`R: GuestComponent`) with [`BamChangeTicks`] at
    /// `decl_index`.
    #[inline]
    pub fn res_with_ticks<R: GuestComponent>(
        &self,
        decl_index: usize,
    ) -> Result<GuestRes<'_, R>, FfiResult> {
        if let Some(ref res_q) = self.resource_query {
            res_q.component_resource_with_ticks::<R>(decl_index)
        } else {
            self.queries
                .first()
                .and_then(Option::as_ref)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .component_resource_with_ticks::<R>(decl_index)
        }
    }

    /// Mutably borrows singleton resource `R` (`R: GuestComponent`) with prior [`BamChangeTicks`]
    /// at `decl_index`.
    #[inline]
    pub fn res_mut_with_ticks<R: GuestComponent>(
        &mut self,
        decl_index: usize,
    ) -> Result<GuestResMut<'_, R>, FfiResult> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.component_resource_mut_with_ticks::<R>(decl_index)
        } else {
            self.queries
                .first_mut()
                .and_then(Option::as_mut)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .component_resource_mut_with_ticks::<R>(decl_index)
        }
    }

    /// Processes primary query `0` chunks in parallel across the host `ComputeTaskPool`.
    #[inline]
    pub fn par_chunks<F>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Chunk<'_, '_>) + Sync,
    {
        self.iter().par_for_each(f)
    }

    /// Fallible variant of [`Self::par_chunks`] propagating non-`OK` [`IntoSystemResult`] returns.
    #[inline]
    pub fn par_try_chunks<F, R>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Chunk<'_, '_>) -> R + Sync,
        R: IntoSystemResult,
    {
        self.iter().par_try_for_each(f)
    }

    /// Borrows primary query `0` as a [`TypedChunkIter`] for [`QueryTuple`] `Q` without magic
    /// column index literals.
    #[inline]
    pub fn single_query<Q: QueryTuple>(&mut self) -> TypedChunkIter<'_, 'a, Q> {
        self.iter().typed::<Q>()
    }

    /// Simultaneously borrows primary query `0` as a [`TypedChunkIter`] alongside [`GuestCommands`].
    #[inline]
    pub fn single_query_and_commands<Q: QueryTuple>(
        &mut self,
    ) -> (TypedChunkIter<'_, 'a, Q>, &mut GuestCommands<'a>) {
        let (iter, cmds) = self.iter_and_commands();
        (iter.typed::<Q>(), cmds)
    }

    /// Borrows declared query `index` (`0..query_count()`) as a [`TypedChunkIter`] for [`QueryTuple`] `Q`.
    #[inline]
    pub fn query_typed<Q: QueryTuple>(
        &mut self,
        index: usize,
    ) -> Option<TypedChunkIter<'_, 'a, Q>> {
        Some(self.query_mut(index)?.typed::<Q>())
    }

    /// Simultaneously borrows two distinct queries as [`TypedChunkIter`]s.
    #[inline]
    pub fn queries_2_typed<Q1: QueryTuple, Q2: QueryTuple>(
        &mut self,
        idx_a: usize,
        idx_b: usize,
    ) -> Option<(TypedChunkIter<'_, 'a, Q1>, TypedChunkIter<'_, 'a, Q2>)> {
        let (qa, qb) = self.queries_2_mut(idx_a, idx_b)?;
        Some((qa.typed::<Q1>(), qb.typed::<Q2>()))
    }

    /// Looks up a shared `&T` (`T: GuestComponent`) on `entity` using declared access on
    /// `(query_index, term_index)`, verifying `T`'s UUID.
    #[inline]
    pub fn get<T: GuestComponent>(
        &self,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&T> {
        if query_index >= self.user_query_count {
            return None;
        }
        let q = self.queries[query_index].as_ref()?;
        let filter = guest_uuid_filter::<T>(self.host.manifest_name());
        self.sys_ref.entity_get_checked::<T>(
            &q.inner,
            query_index,
            term_index,
            entity,
            Some(filter),
        )
    }

    /// Looks up an exclusive `&mut T` (`T: GuestComponent`) on `entity` using declared writable
    /// access on `(query_index, term_index)`, marking Bevy change ticks and verifying `T`'s UUID.
    #[inline]
    pub fn get_mut<T: GuestComponent>(
        &mut self,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&mut T> {
        if query_index >= self.user_query_count {
            return None;
        }
        let q = self.queries[query_index].as_mut()?;
        let filter = guest_uuid_filter::<T>(self.host.manifest_name());
        self.sys_ref.entity_get_mut_checked::<T>(
            &mut q.inner,
            query_index,
            term_index,
            entity,
            Some(filter),
        )
    }

    /// Looks up a shared `&T` (`T: AbiPod`) on `entity` using declared access on
    /// `(query_index, term_index)`.
    #[inline]
    pub fn get_pod<T: AbiPod>(
        &self,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&T> {
        if query_index >= self.user_query_count {
            return None;
        }
        let q = self.queries[query_index].as_ref()?;
        self.sys_ref
            .entity_get_checked::<T>(&q.inner, query_index, term_index, entity, None)
    }

    /// Looks up an exclusive `&mut T` (`T: AbiPod`) on `entity` using declared writable access on
    /// `(query_index, term_index)`.
    #[inline]
    pub fn get_pod_mut<T: AbiPod>(
        &mut self,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&mut T> {
        if query_index >= self.user_query_count {
            return None;
        }
        let q = self.queries[query_index].as_mut()?;
        self.sys_ref.entity_get_mut_checked::<T>(
            &mut q.inner,
            query_index,
            term_index,
            entity,
            None,
        )
    }

    /// Drains unread messages (`M: GuestComponent`) declared at `decl_index`, validating `M`'s UUID.
    #[inline]
    pub fn read_messages<M: GuestComponent>(
        &mut self,
        decl_index: usize,
    ) -> Result<Vec<M>, FfiResult> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.read_messages::<M>(decl_index)
        } else {
            self.queries
                .first_mut()
                .and_then(Option::as_mut)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .read_messages::<M>(decl_index)
        }
    }

    /// Drains unread POD messages (`M: AbiPod`) declared at `decl_index`.
    #[inline]
    pub fn read_pod_messages<M: AbiPod>(&mut self, decl_index: usize) -> Result<Vec<M>, FfiResult> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.read_pod_messages::<M>(decl_index)
        } else {
            self.queries
                .first_mut()
                .and_then(Option::as_mut)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .read_pod_messages::<M>(decl_index)
        }
    }

    /// Appends a message (`M: GuestComponent`) to the stream declared with `MESSAGE_WRITE` at
    /// `decl_index`, validating `M`'s UUID.
    #[inline]
    pub fn send_message<M: GuestComponent>(&mut self, decl_index: usize, msg: &M) -> FfiResult {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.send_message::<M>(decl_index, msg)
        } else {
            match self.queries.first_mut().and_then(Option::as_mut) {
                Some(q) => q.send_message::<M>(decl_index, msg),
                None => FfiResult::OUT_OF_RANGE,
            }
        }
    }

    /// Appends a POD message (`M: AbiPod`) to the stream declared with `MESSAGE_WRITE` at
    /// `decl_index`.
    #[inline]
    pub fn send_pod_message<M: AbiPod>(&mut self, decl_index: usize, msg: &M) -> FfiResult {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.send_pod_message::<M>(decl_index, msg)
        } else {
            match self.queries.first_mut().and_then(Option::as_mut) {
                Some(q) => q.send_pod_message::<M>(decl_index, msg),
                None => FfiResult::OUT_OF_RANGE,
            }
        }
    }

    /// Batch-appends `messages` (`M: GuestComponent`) into the stream declared with `MESSAGE_WRITE`
    /// at `decl_index`.
    #[inline]
    pub fn send_messages<M: GuestComponent>(
        &mut self,
        decl_index: usize,
        messages: &[M],
    ) -> Result<(), FfiResult> {
        if let Some(ref mut res_q) = self.resource_query {
            res_q.send_messages::<M>(decl_index, messages)
        } else {
            self.queries
                .first_mut()
                .and_then(Option::as_mut)
                .ok_or(FfiResult::OUT_OF_RANGE)?
                .send_messages::<M>(decl_index, messages)
        }
    }
}
