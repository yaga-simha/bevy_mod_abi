//! Lock-free re-entrancy-guarded guest closure trampolines (`BamSystemFn` and `BamRunIfFn`) and
//! unified [`BamSystemDesc`] registration.

use core::{
    cell::UnsafeCell,
    ffi::{CStr, c_char, c_void},
    mem::ManuallyDrop,
    sync::atomic::{AtomicBool, Ordering},
};
use std::ffi::CString;

use bevy_abi_mod_sys::{
    AbiPod, BAM_SYSTEM_KNOWN_FLAGS, BamChangeTicks, BamCommands, BamComponent,
    BamConfigureScheduleSetFn, BamEventKind, BamHostApi, BamIter, BamLoadCtx,
    BamMountSystemInScheduleFn, BamObserverDesc, BamObserverFn, BamObserverMutFn, BamQueryDesc,
    BamRegisterObserverFn, BamRegisterSystemHandleFn, BamRunIfFn, BamScheduleSetDesc,
    BamSysEntityChildrenFn, BamSysEntityGetFn, BamSysEntityGetMutFn, BamSysHostCtxFn,
    BamSysResMutPtrFn, BamSysResPtrFn, BamSystem, BamSystemCtx, BamSystemDesc, BamSystemFn,
    BamSystemSet, BamTermDesc, ComponentUuid, FfiBool, FfiEntity, FfiResult,
    MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent, ScheduleId,
};

use crate::{
    commands::{SpawnEntry, ValidatedCommands},
    host::{Host, ValidatedWireVTable},
    instance::{GuestInstance, qualify_mod_component_identity},
    iter::{UuidFilter, ValidatedIter},
    panic::{catch_guest_bool, catch_guest_result},
};

struct ReleaseGuard<'a>(&'a AtomicBool);

impl Drop for ReleaseGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Lock-free single-entry cell guarding `FnMut` system and `run_if` closures across FFI calls.
///
/// Uses an `Acquire`/`Release` [`AtomicBool`] re-entrancy flag instead of `std::sync::Mutex` so
/// hot per-frame system dispatch avoids OS mutex operations while deterministically rejecting
/// accidental re-entrant or concurrent entry without undefined behavior.
pub struct ReentrantCell<T> {
    in_use: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `T: Send` permits moving the cell across threads.
unsafe impl<T: Send> Send for ReentrantCell<T> {}

// SAFETY: Bevy's scheduler never runs the same system instance concurrently with itself across
// worker threads; sequential ticks on different threads synchronize-with each other via both the
// scheduler's thread-pool handoff and our `Acquire`/`Release` pair on `in_use`. Any buggy host or
// re-entrant callback attempting overlapping entry observes `in_use == true` via atomic
// `compare_exchange` and returns immediately without touching `UnsafeCell<T>`.
unsafe impl<T: Send> Sync for ReentrantCell<T> {}

impl<T> ReentrantCell<T> {
    /// Wraps `value` in an unlocked [`ReentrantCell`].
    #[inline]
    pub const fn new(value: T) -> Self {
        Self {
            in_use: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires exclusive mutable access to `T` for the duration of `f`, returning `None` if this
    /// cell is already executing on the current or another thread.
    ///
    /// An RAII guard resets `in_use` to `false` with `Ordering::Release` even if `f` panics and
    /// unwinds into [`catch_guest_result`] / [`catch_guest_bool`].
    #[inline]
    pub fn try_with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self
            .in_use
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let _guard = ReleaseGuard(&self.in_use);
        // SAFETY: `compare_exchange` transitioned `in_use` from `false` to `true` with `Acquire`
        // ordering, granting exclusive access to `self.value` until `_guard` drops (`Release`).
        // Bevy's scheduler invariant ensures a single system instance is never scheduled
        // concurrently with itself, and re-entrant entries fail `compare_exchange` above.
        Some(f(unsafe { &mut *self.value.get() }))
    }

    /// Returns `true` if `try_with_mut` is currently active on this cell.
    #[inline]
    pub fn is_in_use(&self) -> bool {
        self.in_use.load(Ordering::Acquire)
    }
}

/// Validated per-tick system execution frame wrapping `*mut BamSystemCtx`, up to
/// [`MAX_QUERIES_PER_SYSTEM`] disjoint [`ValidatedIter`] slots, and a lazy [`ValidatedCommands`] slot.
pub struct ValidatedSystemInvocation<'a> {
    sys_ctx: *mut BamSystemCtx,
    host: &'a Host,
    total_query_count: usize,
    user_query_count: usize,
    resource_query_index: Option<usize>,
    queries: [Option<ValidatedIter<'a>>; MAX_QUERIES_PER_SYSTEM],
    empty_iter: ValidatedIter<'a>,
    commands: Option<ValidatedCommands<'a>>,
}

impl<'a> ValidatedSystemInvocation<'a> {
    /// Creates a new system execution frame over `sys_ctx` and `host`.
    ///
    /// # Safety
    /// `sys_ctx` must be a non-null, active [`BamSystemCtx`] supplied by the host for the duration
    /// of `'a` with `total_query_count <= MAX_QUERIES_PER_SYSTEM`.
    pub unsafe fn from_raw(
        sys_ctx: *mut BamSystemCtx,
        host: &'a Host,
        total_query_count: usize,
        user_query_count: usize,
        resource_query_index: Option<usize>,
    ) -> Self {
        let clamped_total = total_query_count.min(MAX_QUERIES_PER_SYSTEM);
        let clamped_user = user_query_count.min(clamped_total);
        Self {
            sys_ctx,
            host,
            total_query_count: clamped_total,
            user_query_count: clamped_user,
            resource_query_index: resource_query_index.filter(|&idx| idx < clamped_total),
            queries: [const { None }; MAX_QUERIES_PER_SYSTEM],
            empty_iter: ValidatedIter::empty(host),
            commands: None,
        }
    }

    /// Returns the raw `*mut BamSystemCtx` pointer.
    #[inline]
    pub const fn as_raw_sys_ctx(&self) -> *mut BamSystemCtx {
        self.sys_ctx
    }

    /// Returns a validated [`ValidatedSystemRef`] token bound to this active system invocation.
    #[inline]
    pub const fn system_ref(&self) -> ValidatedSystemRef<'a> {
        ValidatedSystemRef {
            sys_ctx: self.sys_ctx,
            host: self.host,
        }
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Returns the number of entity/explicit queries declared on this system (`0..=MAX_QUERIES_PER_SYSTEM`).
    #[inline]
    pub const fn query_count(&self) -> usize {
        self.user_query_count
    }

    /// Returns the dedicated resource query index if system-level resources were declared.
    #[inline]
    pub const fn resource_query_index(&self) -> Option<usize> {
        self.resource_query_index
    }

    /// Extracts validated query slot `index` (`0..total_query_count`) by value.
    #[inline]
    pub fn take_raw_query_slot(&mut self, index: usize) -> Option<ValidatedIter<'a>> {
        if index >= self.total_query_count {
            return None;
        }
        self.ensure_query_slot(index);
        self.queries[index].take()
    }

    /// Fetches a [`ValidatedCommands`] buffer for this system invocation by value.
    #[inline]
    pub fn take_commands(&mut self) -> ValidatedCommands<'a> {
        self.ensure_commands_slot();
        self.commands
            .take()
            .unwrap_or_else(|| ValidatedCommands::empty(self.host))
    }

    #[inline]
    fn ensure_query_slot(&mut self, index: usize) {
        if index >= self.total_query_count || self.queries[index].is_some() {
            return;
        }
        let iter = if self.sys_ctx.is_null() {
            ValidatedIter::empty(self.host)
        } else if let Some(sys_query_fn) = self.host.api().sys_query {
            let mut raw_iter: *mut BamIter = core::ptr::null_mut();
            // SAFETY: `self.sys_ctx` is active for `'a`, `index < total_query_count`, and
            // `raw_iter` is writable stack storage.
            let status = unsafe { sys_query_fn(self.sys_ctx, index, &mut raw_iter) };
            if status.is_ok() && !raw_iter.is_null() {
                // SAFETY: `raw_iter` was returned by `sys_query` for query `index` on `self.sys_ctx`.
                unsafe { ValidatedIter::from_raw(raw_iter, self.host) }
            } else {
                ValidatedIter::empty(self.host)
            }
        } else {
            ValidatedIter::empty(self.host)
        };
        self.queries[index] = Some(iter);
    }

    #[inline]
    fn ensure_commands_slot(&mut self) {
        if self.commands.is_some() {
            return;
        }
        let cmds = if self.sys_ctx.is_null() {
            ValidatedCommands::empty(self.host)
        } else if let Some(sys_cmds_fn) = self.host.api().sys_commands {
            let mut raw_cmds: *mut BamCommands = core::ptr::null_mut();
            // SAFETY: `self.sys_ctx` is active for `'a` and `raw_cmds` is writable stack storage.
            let status = unsafe { sys_cmds_fn(self.sys_ctx, &mut raw_cmds) };
            if status.is_ok() && !raw_cmds.is_null() {
                // SAFETY: `raw_cmds` was returned by `sys_commands` for `self.sys_ctx`.
                unsafe { ValidatedCommands::from_raw(raw_cmds, self.host) }
            } else {
                ValidatedCommands::empty(self.host)
            }
        } else {
            ValidatedCommands::empty(self.host)
        };
        self.commands = Some(cmds);
    }

    /// Borrows primary query `0` mutably (yielding an empty exhausted iterator if no entity query
    /// was declared).
    #[inline]
    pub fn iter_mut(&mut self) -> &mut ValidatedIter<'a> {
        if self.user_query_count == 0 {
            return &mut self.empty_iter;
        }
        self.ensure_query_slot(0);
        self.queries[0].as_mut().unwrap_or(&mut self.empty_iter)
    }

    /// Borrows declared query `index` (`0..query_count()`) mutably, returning `None` if out of range.
    #[inline]
    pub fn query_mut(&mut self, index: usize) -> Option<&mut ValidatedIter<'a>> {
        if index >= self.user_query_count {
            return None;
        }
        self.ensure_query_slot(index);
        self.queries[index].as_mut()
    }

    /// Simultaneously borrows two distinct queries `idx_a` and `idx_b` (`None` if `idx_a == idx_b`
    /// or either index is out of range).
    pub fn queries_2_mut(
        &mut self,
        idx_a: usize,
        idx_b: usize,
    ) -> Option<(&mut ValidatedIter<'a>, &mut ValidatedIter<'a>)> {
        if idx_a == idx_b || idx_a >= self.user_query_count || idx_b >= self.user_query_count {
            return None;
        }
        self.ensure_query_slot(idx_a);
        self.ensure_query_slot(idx_b);
        if idx_a < idx_b {
            let (left, right) = self.queries.split_at_mut(idx_b);
            Some((left[idx_a].as_mut()?, right[0].as_mut()?))
        } else {
            let (left, right) = self.queries.split_at_mut(idx_a);
            Some((right[0].as_mut()?, left[idx_b].as_mut()?))
        }
    }

    /// Borrows the per-system [`ValidatedCommands`] buffer mutably.
    #[inline]
    pub fn commands_mut(&mut self) -> &mut ValidatedCommands<'a> {
        self.ensure_commands_slot();
        self.commands
            .as_mut()
            .expect("commands slot initialized by ensure_commands_slot")
    }

    /// Simultaneously borrows primary query `0` and the deferred [`ValidatedCommands`] buffer.
    #[inline]
    pub fn iter_and_commands_mut(
        &mut self,
    ) -> (&mut ValidatedIter<'a>, &mut ValidatedCommands<'a>) {
        if self.user_query_count > 0 {
            self.ensure_query_slot(0);
        }
        self.ensure_commands_slot();
        let cmds = self
            .commands
            .as_mut()
            .expect("commands slot initialized by ensure_commands_slot");
        let iter = if self.user_query_count == 0 {
            &mut self.empty_iter
        } else {
            self.queries[0].as_mut().unwrap_or(&mut self.empty_iter)
        };
        (iter, cmds)
    }

    /// Simultaneously borrows two distinct queries `idx_a` and `idx_b` alongside [`ValidatedCommands`].
    pub fn queries_2_and_commands_mut(
        &mut self,
        idx_a: usize,
        idx_b: usize,
    ) -> Option<(
        &mut ValidatedIter<'a>,
        &mut ValidatedIter<'a>,
        &mut ValidatedCommands<'a>,
    )> {
        if idx_a == idx_b || idx_a >= self.user_query_count || idx_b >= self.user_query_count {
            return None;
        }
        self.ensure_query_slot(idx_a);
        self.ensure_query_slot(idx_b);
        self.ensure_commands_slot();
        let cmds = self.commands.as_mut()?;
        if idx_a < idx_b {
            let (left, right) = self.queries.split_at_mut(idx_b);
            Some((left[idx_a].as_mut()?, right[0].as_mut()?, cmds))
        } else {
            let (left, right) = self.queries.split_at_mut(idx_a);
            Some((right[0].as_mut()?, left[idx_b].as_mut()?, cmds))
        }
    }

    /// Borrows singleton resource `R` at declaration index `decl_index` (routing to the dedicated
    /// system-level resource query if declared, or query `0` otherwise).
    #[inline]
    pub fn resource_checked<R: AbiPod>(
        &mut self,
        decl_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&R> {
        let q_idx = self.resource_query_index.unwrap_or(0);
        if q_idx >= self.total_query_count {
            return None;
        }
        self.ensure_query_slot(q_idx);
        self.queries[q_idx]
            .as_ref()?
            .resource_checked::<R>(decl_index, expected_uuid)
    }

    /// Mutably borrows singleton resource `R` at declaration index `decl_index` (routing to the
    /// dedicated system-level resource query if declared, or query `0` otherwise).
    #[inline]
    pub fn resource_mut_checked<R: AbiPod>(
        &mut self,
        decl_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&mut R> {
        let q_idx = self.resource_query_index.unwrap_or(0);
        if q_idx >= self.total_query_count {
            return None;
        }
        self.ensure_query_slot(q_idx);
        self.queries[q_idx]
            .as_mut()?
            .resource_mut_checked::<R>(decl_index, expected_uuid)
    }
}

/// Validated per-tick `*mut BamSystemCtx` token supporting borrow-checked random per-entity
/// component lookups.
#[derive(Clone, Copy)]
pub struct ValidatedSystemRef<'a> {
    sys_ctx: *mut BamSystemCtx,
    host: &'a Host,
}

impl<'a> ValidatedSystemRef<'a> {
    /// Returns the raw `*mut BamSystemCtx` pointer.
    #[inline]
    pub const fn as_raw_sys_ctx(&self) -> *mut BamSystemCtx {
        self.sys_ctx
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Looks up a shared `&'b T` component reference for `entity` using declared access on
    /// `(query_index, term_index)`.
    pub fn entity_get_checked<'b, T: AbiPod>(
        &'b self,
        iter: &'b ValidatedIter<'a>,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&'b T> {
        if self.sys_ctx.is_null() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_entity_get),
            core::mem::size_of::<Option<BamSysEntityGetFn>>(),
        ) {
            return None;
        }
        let get_fn = self.host.api().sys_entity_get?;
        let info = iter.column_info(term_index).ok()?;
        if !info.mode.has_entity_column_data() || info.is_resource() || info.is_message() {
            return None;
        }
        if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
            return None;
        }
        if let Some(filter) = expected_uuid
            && !filter.matches(info.uuid)
        {
            return None;
        }
        // SAFETY: `self.sys_ctx` is an active `BamSystemCtx` for `'a`, and `&'b self` + `&'b iter`
        // prevent concurrent mutable access through this system context for `'b`.
        let ptr = unsafe { get_fn(self.sys_ctx.cast_const(), query_index, entity, term_index) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: AbiPod` may be referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_ref() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        // SAFETY: Host verified query membership and declared read/write access; `info` confirmed
        // `T`'s layout and optional UUID, and `ptr` is non-null and aligned for `'b`.
        Some(unsafe { &*ptr.cast::<T>() })
    }

    /// Looks up an exclusive `&'b mut T` component reference for `entity` using declared writable
    /// access on `(query_index, term_index)`, marking the component changed.
    pub fn entity_get_mut_checked<'b, T: AbiPod>(
        &'b mut self,
        iter: &'b mut ValidatedIter<'a>,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&'b mut T> {
        if self.sys_ctx.is_null() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_entity_get_mut),
            core::mem::size_of::<Option<BamSysEntityGetMutFn>>(),
        ) {
            return None;
        }
        let get_mut_fn = self.host.api().sys_entity_get_mut?;
        let info = iter.column_info(term_index).ok()?;
        if !info.mode.is_mutable()
            || !info.mode.has_entity_column_data()
            || info.is_immutable()
            || info.is_resource()
            || info.is_message()
        {
            return None;
        }
        if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
            return None;
        }
        if let Some(filter) = expected_uuid
            && !filter.matches(info.uuid)
        {
            return None;
        }
        // SAFETY: `self.sys_ctx` is an active `BamSystemCtx` for `'a`, and `&'b mut self` +
        // `&'b mut iter` prevent any aliasing borrow on this system context for `'b`.
        let ptr = unsafe { get_mut_fn(self.sys_ctx, query_index, entity, term_index) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: AbiPod` may be mutably referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_mut() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        // SAFETY: Host verified query membership, writable `FilteredAccessSet` declaration, and
        // marked change ticks; `&'b mut self` guarantees exclusive access for `'b`.
        Some(unsafe { &mut *ptr.cast::<T>() })
    }

    /// Looks up a shared singleton resource `&'b R` by its pre-resolved `BamComponent` handle,
    /// returning `(ref, BamChangeTicks)` if declared with `ResourceRead` or `ResourceWrite`.
    pub fn res_ptr_by_handle_checked<R: AbiPod>(
        &self,
        component: BamComponent,
    ) -> Option<(&R, BamChangeTicks)> {
        if self.sys_ctx.is_null() || !component.is_valid() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_res_ptr),
            core::mem::size_of::<Option<BamSysResPtrFn>>(),
        ) {
            return None;
        }
        let get_fn = self.host.api().sys_res_ptr?;
        let mut ticks = BamChangeTicks::default();
        // SAFETY: `self.sys_ctx` is an active `BamSystemCtx` and `&mut ticks` is aligned writable storage.
        let ptr = unsafe { get_fn(self.sys_ctx.cast_const(), component, &mut ticks) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<R>() == 0 {
            // SAFETY: Zero-sized `R: AbiPod` may be referenced via `NonNull::dangling()`.
            return Some((
                unsafe { core::ptr::NonNull::<R>::dangling().as_ref() },
                ticks,
            ));
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
            return None;
        }
        // SAFETY: Host verified resource borrow permissions in `FilteredAccessSet` and alignment holds.
        Some((unsafe { &*ptr.cast::<R>() }, ticks))
    }

    /// Looks up an exclusive mutable singleton resource `&'b mut R` by its pre-resolved
    /// `BamComponent` handle, returning `(mut_ref, BamChangeTicks)` if declared with `ResourceWrite`.
    pub fn res_mut_ptr_by_handle_checked<R: AbiPod>(
        &mut self,
        component: BamComponent,
    ) -> Option<(&mut R, BamChangeTicks)> {
        if self.sys_ctx.is_null() || !component.is_valid() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_res_mut_ptr),
            core::mem::size_of::<Option<BamSysResMutPtrFn>>(),
        ) {
            return None;
        }
        let get_mut_fn = self.host.api().sys_res_mut_ptr?;
        let mut ticks = BamChangeTicks::default();
        // SAFETY: `self.sys_ctx` is active, `&'b mut self` prevents aliasing, and `&mut ticks` is aligned.
        let ptr = unsafe { get_mut_fn(self.sys_ctx, component, &mut ticks) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<R>() == 0 {
            // SAFETY: Zero-sized `R: AbiPod` may be mutably referenced via `NonNull::dangling()`.
            return Some((
                unsafe { core::ptr::NonNull::<R>::dangling().as_mut() },
                ticks,
            ));
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
            return None;
        }
        // SAFETY: Host verified exclusive `ResourceWrite` permission and stamped change ticks.
        Some((unsafe { &mut *ptr.cast::<R>() }, ticks))
    }

    /// Borrows the contiguous zero-copy `&[FfiEntity]` slice of child/target entities for `entity`
    /// under `relationship_target` (`BamComponent::INVALID` defaults to Bevy's built-in `Children`)
    /// via `BamHostApi::sys_entity_children`.
    pub fn entity_children(
        &self,
        entity: FfiEntity,
        relationship_target: BamComponent,
    ) -> Result<&[FfiEntity], FfiResult> {
        if self.sys_ctx.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if entity.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_entity_children),
            core::mem::size_of::<Option<BamSysEntityChildrenFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(children_fn) = self.host.api().sys_entity_children else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut ptr: *const FfiEntity = core::ptr::null();
        let mut len: usize = 0;
        // SAFETY: `self.sys_ctx` is an active `BamSystemCtx` and `ptr`/`len` are aligned stack slots.
        let status = unsafe {
            children_fn(
                self.sys_ctx.cast_const(),
                entity,
                relationship_target,
                &mut ptr,
                &mut len,
            )
        };
        if !status.is_ok() {
            return Err(status);
        }
        if len == 0 {
            return Ok(&[]);
        }
        if ptr.is_null() || !ptr.is_aligned() {
            return Err(FfiResult::NULL_POINTER);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<FfiEntity>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        if byte_len > isize::MAX as usize {
            return Err(FfiResult::OUT_OF_RANGE);
        }
        // SAFETY: Host verified declared read access to `relationship_target`; `ptr` is non-null,
        // aligned for `FfiEntity`, and valid for shared reads throughout `&self`.
        Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
    }

    /// Reads an opaque host-bridged singleton resource by `component` handle using `vtable`
    /// (`BamHostApi::sys_res_ptr`).
    pub fn read_mapped_wire_resource_by_handle<W: AbiPod>(
        &self,
        component: BamComponent,
        vtable: &ValidatedWireVTable<W>,
    ) -> Result<(W, BamChangeTicks), FfiResult> {
        if self.sys_ctx.is_null() || !component.is_valid() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_res_ptr),
            core::mem::size_of::<Option<BamSysResPtrFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(get_ptr_fn) = self.host.api().sys_res_ptr else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let Some(vt_get) = vtable.raw.get else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut ticks = BamChangeTicks::default();
        // SAFETY: `self.sys_ctx` is active and `&mut ticks` is aligned writable storage.
        let ptr = unsafe { get_ptr_fn(self.sys_ctx.cast_const(), component, &mut ticks) };
        if ptr.is_null() {
            return Err(FfiResult::NOT_FOUND);
        }
        if vtable.host_size() > 0 && !(ptr as usize).is_multiple_of(vtable.host_align()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let mut out = core::mem::MaybeUninit::<W>::zeroed();
        // SAFETY: Host verified resource read access on `component`; `vt_get` initializes `out`.
        let status = unsafe { vt_get(ptr.cast::<c_void>(), out.as_mut_ptr()) };
        if status.is_ok() {
            // SAFETY: `vt_get` reported `OK`, confirming `out` is initialized `W`.
            Ok((unsafe { out.assume_init() }, ticks))
        } else {
            Err(status)
        }
    }

    /// Writes `value` (`W: AbiPod`) into an opaque host-bridged singleton resource by `component`
    /// handle using `vtable` (`BamHostApi::sys_res_mut_ptr`).
    pub fn write_mapped_wire_resource_by_handle<W: AbiPod>(
        &mut self,
        component: BamComponent,
        vtable: &ValidatedWireVTable<W>,
        value: &W,
    ) -> Result<BamChangeTicks, FfiResult> {
        if self.sys_ctx.is_null() || !component.is_valid() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_res_mut_ptr),
            core::mem::size_of::<Option<BamSysResMutPtrFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(get_mut_fn) = self.host.api().sys_res_mut_ptr else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let Some(vt_set) = vtable.raw.set else {
            return Err(FfiResult::PERMISSION_DENIED);
        };
        let mut ticks = BamChangeTicks::default();
        // SAFETY: `self.sys_ctx` is active under `&mut self` and `&mut ticks` is writable storage.
        let ptr = unsafe { get_mut_fn(self.sys_ctx, component, &mut ticks) };
        if ptr.is_null() {
            return Err(FfiResult::NOT_FOUND);
        }
        if vtable.host_size() > 0 && !(ptr as usize).is_multiple_of(vtable.host_align()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        // SAFETY: Host verified exclusive `ResourceWrite` permission; `value` is aligned readable `&W`.
        let status = unsafe { vt_set(ptr.cast::<c_void>(), value) };
        if status.is_ok() {
            Ok(ticks)
        } else {
            Err(status)
        }
    }
}

struct SystemClosureState<F> {
    host: Host,
    total_query_count: usize,
    user_query_count: usize,
    resource_query_index: Option<usize>,
    closure: ReentrantCell<F>,
}

struct RunIfClosureState<R> {
    host: Host,
    closure: ReentrantCell<R>,
}

unsafe extern "C" fn system_closure_trampoline<F>(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult
where
    F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
{
    catch_guest_result(|| {
        if sys_ctx.is_null() || user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` was allocated via `GuestInstance::track_allocation` for
        // `SystemClosureState<F>` and remains valid until `bevy_abi_mod_shutdown`.
        let state = unsafe { &*user_data.cast::<SystemClosureState<F>>() };
        let inv_host = if state.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_host_ctx),
            core::mem::size_of::<Option<BamSysHostCtxFn>>(),
        ) && let Some(sys_host_ctx_fn) = state.host.api().sys_host_ctx
        {
            // SAFETY: `sys_ctx` is non-null and active for the duration of this `BamSystemFn` call.
            let raw_ctx = unsafe { sys_host_ctx_fn(sys_ctx.cast_const()) };
            if raw_ctx.is_null() {
                state.host
            } else {
                // SAFETY: `raw_ctx` was returned by `BamHostApi::sys_host_ctx(sys_ctx)` and
                // remains valid for the duration of this system invocation.
                unsafe { state.host.with_host_ctx_ptr(raw_ctx) }
            }
        } else {
            state.host
        };
        let Some(result) = state.closure.try_with_mut(|func| {
            // SAFETY: `sys_ctx` is non-null and active for the duration of this `BamSystemFn` call.
            let mut invocation = unsafe {
                ValidatedSystemInvocation::from_raw(
                    sys_ctx,
                    &inv_host,
                    state.total_query_count,
                    state.user_query_count,
                    state.resource_query_index,
                )
            };
            func(&mut invocation)
        }) else {
            return FfiResult::ACCESS_CONFLICT;
        };
        result
    })
}

unsafe extern "C" fn run_if_closure_trampoline<R>(user_data: *mut c_void) -> FfiBool
where
    R: FnMut(&Host) -> bool + Send + 'static,
{
    catch_guest_bool(|| {
        if user_data.is_null() {
            return FfiBool::FALSE;
        }
        // SAFETY: `user_data` was allocated via `GuestInstance::track_allocation` for
        // `RunIfClosureState<R>` and remains valid until `bevy_abi_mod_shutdown`.
        let state = unsafe { &*user_data.cast::<RunIfClosureState<R>>() };
        match state
            .closure
            .try_with_mut(|pred| FfiBool::from_bool(pred(&state.host)))
        {
            Some(flag) => flag,
            None => FfiBool::FALSE,
        }
    })
}

/// Allocates a standalone `(BamSystemFn, *mut c_void)` closure pair inside `instance` (used by
/// [`register_system_closure`] and direct re-entrancy / panic unit tests).
pub fn allocate_system_trampoline<F>(
    instance: &mut GuestInstance,
    total_query_count: usize,
    user_query_count: usize,
    resource_query_index: Option<usize>,
    closure: F,
) -> (BamSystemFn, *mut c_void)
where
    F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
{
    let host = *instance.host();
    let raw = instance.track_allocation(Box::new(SystemClosureState {
        host,
        total_query_count,
        user_query_count,
        resource_query_index,
        closure: ReentrantCell::new(closure),
    }));
    (system_closure_trampoline::<F>, raw.cast::<c_void>())
}

/// Validated `(BamRunIfFn, *mut c_void)` pair allocated inside a [`GuestInstance`].
pub struct PreparedRunIf {
    pub(crate) func: BamRunIfFn,
    pub(crate) user_data: *mut c_void,
}

impl PreparedRunIf {
    /// Allocates a panic-safe, re-entrancy-guarded `run_if` predicate inside `instance`.
    #[inline]
    pub fn new<R>(instance: &mut GuestInstance, predicate: R) -> Self
    where
        R: FnMut(&Host) -> bool + Send + 'static,
    {
        let (func, user_data) = allocate_run_if_trampoline(instance, predicate);
        Self { func, user_data }
    }

    /// Wraps a caller-managed raw `(BamRunIfFn, user_data)` pair.
    ///
    /// # Safety
    /// `func(user_data)` must be valid to invoke for the lifetime of the target `World` without
    /// unwinding across `extern "C"`.
    #[inline]
    pub const unsafe fn from_raw(func: BamRunIfFn, user_data: *mut c_void) -> Self {
        Self { func, user_data }
    }

    /// Returns the underlying `(BamRunIfFn, *mut c_void)` tuple.
    #[inline]
    pub const fn into_raw(self) -> (BamRunIfFn, *mut c_void) {
        (self.func, self.user_data)
    }
}

/// Allocates a standalone `(BamRunIfFn, *mut c_void)` closure pair inside `instance`.
pub fn allocate_run_if_trampoline<R>(
    instance: &mut GuestInstance,
    predicate: R,
) -> (BamRunIfFn, *mut c_void)
where
    R: FnMut(&Host) -> bool + Send + 'static,
{
    let host = *instance.host();
    let raw = instance.track_allocation(Box::new(RunIfClosureState {
        host,
        closure: ReentrantCell::new(predicate),
    }));
    (run_if_closure_trampoline::<R>, raw.cast::<c_void>())
}

/// Descriptor assembly configuration passed from safe `SystemBuilder` to [`register_system_closure`].
pub struct SystemRegistrationConfig<'a> {
    /// NUL-terminated UTF-8 system name.
    pub name: &'a CStr,
    /// User-declared entity/resource query clauses (`<= MAX_QUERIES_PER_SYSTEM`).
    pub user_queries: &'a [&'a [BamTermDesc]],
    /// Optional system-level singleton resource terms (`resource_read` / `resource_write` on `SystemBuilder`).
    pub system_resources: &'a [BamTermDesc],
    /// Target Bevy schedule.
    pub schedule: ScheduleId,
    /// `BAM_SYSTEM_FLAG_*` bitmask.
    pub flags: u32,
    /// Named system sets that this system must execute after.
    pub after: &'a [&'a CStr],
    /// Named system sets that this system must execute before.
    pub before: &'a [&'a CStr],
    /// Named system sets (`"host::<set>"` or custom sets) that this system belongs to.
    pub in_sets: &'a [&'a CStr],
    /// Named system sets that this system executes weakly after (`after_ignore_deferred`).
    pub after_weak: &'a [&'a CStr],
    /// Named system sets that this system executes weakly before (`before_ignore_deferred`).
    pub before_weak: &'a [&'a CStr],
    /// Named system sets whose access ambiguities with this system are explicitly suppressed.
    pub ambiguous_with: &'a [&'a CStr],
}

struct OwnedDescriptorStorage {
    name: CString,
    _term_vecs: Vec<Box<[BamTermDesc]>>,
    queries: Box<[BamQueryDesc]>,
    _after_names: Vec<CString>,
    after_ptrs: Box<[*const c_char]>,
    _before_names: Vec<CString>,
    before_ptrs: Box<[*const c_char]>,
    _in_set_names: Vec<CString>,
    in_set_ptrs: Box<[*const c_char]>,
    _after_weak_names: Vec<CString>,
    after_weak_ptrs: Box<[*const c_char]>,
    _before_weak_names: Vec<CString>,
    before_weak_ptrs: Box<[*const c_char]>,
    _ambiguous_with_names: Vec<CString>,
    ambiguous_with_ptrs: Box<[*const c_char]>,
}

// SAFETY: Every raw pointer inside `queries`, `after_ptrs`, and `before_ptrs` points exclusively
// into heap-allocated `Box<[BamTermDesc]>` and `CString` buffers owned by the same struct and
// never mutated after construction.
unsafe impl Send for OwnedDescriptorStorage {}

fn prepare_system_desc<F>(
    instance: &mut GuestInstance,
    config: &SystemRegistrationConfig<'_>,
    run_if: Option<(BamRunIfFn, *mut c_void)>,
    closure: F,
) -> Result<BamSystemDesc, FfiResult>
where
    F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
{
    if config.name.to_bytes().is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if config.name.to_str().is_err() {
        return Err(FfiResult::INVALID_UTF8);
    }
    if !config.schedule.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if (config.flags & !BAM_SYSTEM_KNOWN_FLAGS) != 0 {
        return Err(FfiResult::UNSUPPORTED);
    }

    let has_sys_resources = !config.system_resources.is_empty();
    let total_query_count = config.user_queries.len() + usize::from(has_sys_resources);
    if total_query_count > MAX_QUERIES_PER_SYSTEM {
        return Err(FfiResult::CAPACITY_EXCEEDED);
    }
    if config.system_resources.len() > MAX_TERMS_PER_QUERY {
        return Err(FfiResult::CAPACITY_EXCEEDED);
    }

    for terms in config.user_queries {
        if terms.len() > MAX_TERMS_PER_QUERY {
            return Err(FfiResult::CAPACITY_EXCEEDED);
        }
        for term in *terms {
            let status = term.validate();
            if !status.is_ok() {
                return Err(status);
            }
        }
    }
    for term in config.system_resources {
        let status = term.validate();
        if !status.is_ok() {
            return Err(status);
        }
    }
    for set_name in config
        .after
        .iter()
        .chain(config.before.iter())
        .chain(config.in_sets.iter())
        .chain(config.after_weak.iter())
        .chain(config.before_weak.iter())
        .chain(config.ambiguous_with.iter())
    {
        if set_name.to_bytes().is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if set_name.to_str().is_err() {
            return Err(FfiResult::INVALID_UTF8);
        }
    }

    let mut term_vecs: Vec<Box<[BamTermDesc]>> = Vec::with_capacity(total_query_count);
    let mut query_descs: Vec<BamQueryDesc> = Vec::with_capacity(total_query_count);
    for terms in config.user_queries {
        if terms.is_empty() {
            query_descs.push(BamQueryDesc::empty());
        } else {
            let boxed_terms: Box<[BamTermDesc]> = (*terms).into();
            let ptr = boxed_terms.as_ptr();
            let len = boxed_terms.len();
            term_vecs.push(boxed_terms);
            query_descs.push(BamQueryDesc::new(ptr, len));
        }
    }

    let resource_query_index = if has_sys_resources {
        let idx = query_descs.len();
        let boxed_res: Box<[BamTermDesc]> = config.system_resources.into();
        let ptr = boxed_res.as_ptr();
        let len = boxed_res.len();
        term_vecs.push(boxed_res);
        query_descs.push(BamQueryDesc::new(ptr, len));
        Some(idx)
    } else {
        None
    };

    fn collect_cstrings(names: &[&CStr]) -> (Vec<CString>, Box<[*const c_char]>) {
        let mut owned: Vec<CString> = Vec::with_capacity(names.len());
        let mut ptrs: Vec<*const c_char> = Vec::with_capacity(names.len());
        for set_name in names {
            let cstr = CString::from(*set_name);
            ptrs.push(cstr.as_ptr());
            owned.push(cstr);
        }
        (owned, ptrs.into_boxed_slice())
    }

    let (after_names, after_ptrs) = collect_cstrings(config.after);
    let (before_names, before_ptrs) = collect_cstrings(config.before);
    let (in_set_names, in_set_ptrs) = collect_cstrings(config.in_sets);
    let (after_weak_names, after_weak_ptrs) = collect_cstrings(config.after_weak);
    let (before_weak_names, before_weak_ptrs) = collect_cstrings(config.before_weak);
    let (ambiguous_with_names, ambiguous_with_ptrs) = collect_cstrings(config.ambiguous_with);

    let storage = OwnedDescriptorStorage {
        name: CString::from(config.name),
        _term_vecs: term_vecs,
        queries: query_descs.into_boxed_slice(),
        _after_names: after_names,
        after_ptrs,
        _before_names: before_names,
        before_ptrs,
        _in_set_names: in_set_names,
        in_set_ptrs,
        _after_weak_names: after_weak_names,
        after_weak_ptrs,
        _before_weak_names: before_weak_names,
        before_weak_ptrs,
        _ambiguous_with_names: ambiguous_with_names,
        ambiguous_with_ptrs,
    };
    let raw_storage = instance.track_allocation(Box::new(storage));
    // SAFETY: `raw_storage` was just allocated by `instance.track_allocation` and remains pinned
    // on the heap until `instance` drops.
    let storage_ref = unsafe { &*raw_storage };
    let name_ptr = storage_ref.name.as_ptr();
    let queries_ptr = if storage_ref.queries.is_empty() {
        core::ptr::null()
    } else {
        storage_ref.queries.as_ptr()
    };
    let slice_ptr_or_null = |s: &[*const c_char]| {
        if s.is_empty() {
            core::ptr::null()
        } else {
            s.as_ptr()
        }
    };
    let after_ptr = slice_ptr_or_null(&storage_ref.after_ptrs);
    let before_ptr = slice_ptr_or_null(&storage_ref.before_ptrs);
    let in_sets_ptr = slice_ptr_or_null(&storage_ref.in_set_ptrs);
    let after_weak_ptr = slice_ptr_or_null(&storage_ref.after_weak_ptrs);
    let before_weak_ptr = slice_ptr_or_null(&storage_ref.before_weak_ptrs);
    let ambiguous_with_ptr = slice_ptr_or_null(&storage_ref.ambiguous_with_ptrs);

    let (callback_fn, user_data) = allocate_system_trampoline(
        instance,
        total_query_count,
        config.user_queries.len(),
        resource_query_index,
        closure,
    );

    let mut desc = BamSystemDesc::new(
        name_ptr,
        queries_ptr,
        total_query_count,
        callback_fn,
        user_data,
    )
    .with_flags(config.flags)
    .with_schedule(config.schedule)
    .with_after(after_ptr, config.after.len())
    .with_before(before_ptr, config.before.len())
    .with_in_sets(in_sets_ptr, config.in_sets.len())
    .with_after_weak(after_weak_ptr, config.after_weak.len())
    .with_before_weak(before_weak_ptr, config.before_weak.len())
    .with_ambiguous_with(ambiguous_with_ptr, config.ambiguous_with.len());

    if let Some((run_if_fn, run_if_data)) = run_if {
        desc = desc.with_run_if(run_if_fn, run_if_data);
    }

    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    Ok(desc)
}

/// Validates and registers a unified [`BamSystemDesc`] on `load_ctx`, tracking all closure state
/// and owned descriptor buffers inside `instance`.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
pub unsafe fn register_system_closure<F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    config: &SystemRegistrationConfig<'_>,
    run_if: Option<(BamRunIfFn, *mut c_void)>,
    closure: F,
) -> FfiResult
where
    F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
{
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(register_fn) = instance.host().api().register_system else {
        return FfiResult::NULL_POINTER;
    };
    let desc = match prepare_system_desc(instance, config, run_if, closure) {
        Ok(d) => d,
        Err(err) => return err,
    };
    // SAFETY: `load_ctx` is active per caller contract and `desc` passed full validation with all
    // pointed-to strings, term slices, and closures owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
}

/// Validates and registers a unified [`BamSystemDesc`] via `BamHostApi::register_system_handle`
/// (slot 60), returning the assigned [`BamSystem`] handle.
///
/// Returns [`FfiResult::UNSUPPORTED`] on ABI `0.2` hosts without allocating closure state.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
pub unsafe fn register_system_handle_closure<F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    config: &SystemRegistrationConfig<'_>,
    run_if: Option<(BamRunIfFn, *mut c_void)>,
    closure: F,
) -> Result<BamSystem, FfiResult>
where
    F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
{
    if load_ctx.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if !instance.host().api().covers_slot(
        core::mem::offset_of!(BamHostApi, register_system_handle),
        core::mem::size_of::<Option<BamRegisterSystemHandleFn>>(),
    ) {
        return Err(FfiResult::UNSUPPORTED);
    }
    let Some(register_handle_fn) = instance.host().api().register_system_handle else {
        return Err(FfiResult::UNSUPPORTED);
    };
    let desc = prepare_system_desc(instance, config, run_if, closure)?;
    let mut out = BamSystem::INVALID;
    // SAFETY: `load_ctx` is active per caller contract, `desc` passed validation with all pointed-to
    // buffers owned by `instance`, and `&mut out` is aligned writable stack storage.
    let status = unsafe { register_handle_fn(load_ctx, &desc, &mut out) };
    if !status.is_ok() {
        return Err(status);
    }
    if !out.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(out)
}

/// Raw wrapper invoking `BamHostApi::configure_schedule_set` (slot 61) after checking
/// `covers_slot`, `load_ctx`, and `desc.validate()`.
///
/// # Safety
/// - `load_ctx` must be a non-null, active [`BamLoadCtx`] during `bevy_abi_mod_init`.
/// - `desc` must satisfy [`BamScheduleSetDesc::validate`] with valid `name`, `before_sets`, and
///   `after_sets` pointers for the duration of this call.
pub unsafe fn configure_schedule_set_raw(
    load_ctx: *mut BamLoadCtx,
    host: &Host,
    desc: &BamScheduleSetDesc,
) -> Result<BamSystemSet, FfiResult> {
    if load_ctx.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    if !host.api().covers_slot(
        core::mem::offset_of!(BamHostApi, configure_schedule_set),
        core::mem::size_of::<Option<BamConfigureScheduleSetFn>>(),
    ) {
        return Err(FfiResult::UNSUPPORTED);
    }
    let Some(configure_fn) = host.api().configure_schedule_set else {
        return Err(FfiResult::UNSUPPORTED);
    };
    let mut out = BamSystemSet::INVALID;
    // SAFETY: `load_ctx` is active per caller contract, `desc` passed validation, and `&mut out`
    // is aligned writable stack storage.
    let status = unsafe { configure_fn(load_ctx, desc, &mut out) };
    if !status.is_ok() {
        return Err(status);
    }
    if !out.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(out)
}

/// Configures a named schedule system set via `BamHostApi::configure_schedule_set` (slot 61),
/// storing an owned copy of `name` inside `instance` and returning its assigned [`BamSystemSet`].
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
pub unsafe fn configure_schedule_set(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &CStr,
    schedule: ScheduleId,
    flags: u32,
    before_sets: &[BamSystemSet],
    after_sets: &[BamSystemSet],
) -> Result<BamSystemSet, FfiResult> {
    if load_ctx.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if name.to_bytes().is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if name.to_str().is_err() {
        return Err(FfiResult::INVALID_UTF8);
    }
    if before_sets.iter().any(|s| !s.is_valid()) || after_sets.iter().any(|s| !s.is_valid()) {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let before_count =
        u32::try_from(before_sets.len()).map_err(|_| FfiResult::CAPACITY_EXCEEDED)?;
    let after_count = u32::try_from(after_sets.len()).map_err(|_| FfiResult::CAPACITY_EXCEEDED)?;
    let host = *instance.host();
    if !host.api().covers_slot(
        core::mem::offset_of!(BamHostApi, configure_schedule_set),
        core::mem::size_of::<Option<BamConfigureScheduleSetFn>>(),
    ) || host.api().configure_schedule_set.is_none()
    {
        return Err(FfiResult::UNSUPPORTED);
    }
    let before_ptr = if before_sets.is_empty() {
        core::ptr::null()
    } else {
        before_sets.as_ptr()
    };
    let after_ptr = if after_sets.is_empty() {
        core::ptr::null()
    } else {
        after_sets.as_ptr()
    };
    let mut probe_desc = BamScheduleSetDesc::new(name.as_ptr()).with_flags(flags);
    probe_desc.schedule = schedule;
    probe_desc.before_count = before_count;
    probe_desc.after_count = after_count;
    probe_desc.before_sets = before_ptr;
    probe_desc.after_sets = after_ptr;
    let validation = probe_desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }

    let owned_name = CString::from(name);
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` stays pinned inside `instance` until `bevy_abi_mod_shutdown` (or rollback).
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let mut desc = BamScheduleSetDesc::new(name_ptr).with_flags(flags);
    desc.schedule = schedule;
    desc.before_count = before_count;
    desc.after_count = after_count;
    desc.before_sets = before_ptr;
    desc.after_sets = after_ptr;
    // SAFETY: `load_ctx` is active per caller contract, `name_ptr` is owned by `instance`, and
    // `before_sets`/`after_sets` slices stay live across the synchronous host FFI call.
    unsafe { configure_schedule_set_raw(load_ctx, &host, &desc) }
}

/// Mounts a previously registered [`BamSystem`] handle into `schedule` and optional `set`
/// (`BamSystemSet::INVALID` for ungrouped mounting) via `BamHostApi::mount_system_in_schedule`
/// (slot 62).
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] during `bevy_abi_mod_init`.
pub unsafe fn mount_system_in_schedule(
    load_ctx: *mut BamLoadCtx,
    host: &Host,
    system: BamSystem,
    schedule: ScheduleId,
    set: BamSystemSet,
) -> FfiResult {
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !system.is_valid() || !schedule.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if !host.api().covers_slot(
        core::mem::offset_of!(BamHostApi, mount_system_in_schedule),
        core::mem::size_of::<Option<BamMountSystemInScheduleFn>>(),
    ) {
        return FfiResult::UNSUPPORTED;
    }
    let Some(mount_fn) = host.api().mount_system_in_schedule else {
        return FfiResult::UNSUPPORTED;
    };
    // SAFETY: `load_ctx` is active per caller contract and `system`/`schedule` passed validation.
    unsafe { mount_fn(load_ctx, system, schedule, set) }
}

struct ObserverTrampolineState<F> {
    host: Host,
    callback: ReentrantCell<F>,
}

unsafe extern "C" fn observer_closure_trampoline<E, F>(
    target: FfiEntity,
    event_data: *const c_void,
    commands: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult
where
    E: AbiPod,
    F: FnMut(FfiEntity, &E, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    catch_guest_result(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to `ObserverTrampolineState<F>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<ObserverTrampolineState<F>>() };
        let event_ref: &E = if core::mem::size_of::<E>() == 0 {
            // SAFETY: Zero-sized `E: AbiPod` may be referenced via `NonNull::dangling()`.
            unsafe { core::ptr::NonNull::<E>::dangling().as_ref() }
        } else {
            if event_data.is_null()
                || !(event_data as usize).is_multiple_of(core::mem::align_of::<E>())
            {
                return FfiResult::INVALID_ARGUMENT;
            }
            // SAFETY: Host guarantees `event_data` points to initialized `E` matching `event_key`.
            unsafe { &*event_data.cast::<E>() }
        };
        // SAFETY: `commands` is stack-scoped for this observer invocation.
        let mut cmds = unsafe { ValidatedCommands::from_raw(commands, &state.host) };
        state
            .callback
            .try_with_mut(|cb| cb(target, event_ref, &mut cmds))
            .unwrap_or(FfiResult::ACCESS_CONFLICT)
    })
}

/// Validates and registers a dynamic [`BamObserverDesc`] closure on `load_ctx`, tracking its
/// heap-owned name and state inside `instance`.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
pub unsafe fn register_observer_closure<E, F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &str,
    event: bevy_abi_mod_sys::BamComponent,
    target: FfiEntity,
    callback: F,
) -> FfiResult
where
    E: AbiPod,
    F: FnMut(FfiEntity, &E, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if name.is_empty() || !event.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(owned_name) = CString::new(name) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    if !instance.host().api().covers_slot(
        core::mem::offset_of!(BamHostApi, register_observer),
        core::mem::size_of::<Option<BamRegisterObserverFn>>(),
    ) {
        return FfiResult::UNSUPPORTED;
    }
    let Some(register_fn) = instance.host().api().register_observer else {
        return FfiResult::UNSUPPORTED;
    };

    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` stays pinned inside `instance` until `shutdown_guest_instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };

    let host = *instance.host();
    let raw_state = instance.track_allocation(Box::new(ObserverTrampolineState {
        host,
        callback: ReentrantCell::new(callback),
    }));

    let obs_cb: BamObserverFn = observer_closure_trampoline::<E, F>;
    let desc = BamObserverDesc::new(event, obs_cb, raw_state.cast::<c_void>())
        .with_name(name_ptr)
        .with_entity(target);
    let validation = desc.validate();
    if !validation.is_ok() {
        return validation;
    }
    // SAFETY: `load_ctx` is active per caller contract and `desc` passed validation with `name_ptr`
    // and `raw_state` owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
}

unsafe extern "C" fn observer_mut_closure_trampoline<E, F>(
    target: FfiEntity,
    event_data: *mut u8,
    propagate: *mut FfiBool,
    commands: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult
where
    E: AbiPod,
    F: FnMut(FfiEntity, &mut E, &mut bool, &mut ValidatedCommands<'_>) -> FfiResult
        + Send
        + 'static,
{
    catch_guest_result(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to `ObserverTrampolineState<F>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<ObserverTrampolineState<F>>() };
        let event_ref: &mut E = if core::mem::size_of::<E>() == 0 {
            // SAFETY: Zero-sized `E: AbiPod` may be mutably referenced via `NonNull::dangling()`.
            unsafe { core::ptr::NonNull::<E>::dangling().as_mut() }
        } else {
            if event_data.is_null()
                || !(event_data as usize).is_multiple_of(core::mem::align_of::<E>())
            {
                return FfiResult::INVALID_ARGUMENT;
            }
            // SAFETY: Host guarantees exclusive mutable access to `E` at `event_data` during
            // `BamObserverMutFn` execution.
            unsafe { &mut *event_data.cast::<E>() }
        };
        let mut prop_val = if propagate.is_null() {
            false
        } else {
            // SAFETY: When non-null, `propagate` points to an initialized `FfiBool` on the host stack.
            unsafe { (*propagate).get() }
        };
        // SAFETY: `commands` is stack-scoped for this observer invocation.
        let mut cmds = unsafe { ValidatedCommands::from_raw(commands, &state.host) };
        let status = state
            .callback
            .try_with_mut(|cb| cb(target, event_ref, &mut prop_val, &mut cmds))
            .unwrap_or(FfiResult::ACCESS_CONFLICT);
        if !propagate.is_null() {
            // SAFETY: `propagate` is non-null and writable for the duration of this callback.
            unsafe {
                *propagate = FfiBool::from_bool(prop_val);
            }
        }
        status
    })
}

unsafe extern "C" fn lifecycle_observer_closure_trampoline<F>(
    target: FfiEntity,
    _event_data: *const c_void,
    commands: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult
where
    F: FnMut(FfiEntity, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    catch_guest_result(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to `ObserverTrampolineState<F>` owned by `GuestInstance`.
        let state = unsafe { &*user_data.cast::<ObserverTrampolineState<F>>() };
        // SAFETY: `commands` is stack-scoped for this observer invocation.
        let mut cmds = unsafe { ValidatedCommands::from_raw(commands, &state.host) };
        state
            .callback
            .try_with_mut(|cb| cb(target, &mut cmds))
            .unwrap_or(FfiResult::ACCESS_CONFLICT)
    })
}

/// Validates and registers a custom [`BamObserverDesc`] closure with explicit propagation flags
/// and optional `propagate_relationship` on `load_ctx`.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn register_observer_closure_with_options<E, F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &str,
    event: BamComponent,
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> FfiResult
where
    E: AbiPod,
    F: FnMut(FfiEntity, &E, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if name.is_empty() || !event.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(owned_name) = CString::new(name) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    if !instance.host().api().covers_slot(
        core::mem::offset_of!(BamHostApi, register_observer),
        core::mem::size_of::<Option<BamRegisterObserverFn>>(),
    ) {
        return FfiResult::UNSUPPORTED;
    }
    let Some(register_fn) = instance.host().api().register_observer else {
        return FfiResult::UNSUPPORTED;
    };
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` stays pinned inside `instance` until `shutdown_guest_instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let host = *instance.host();
    let raw_state = instance.track_allocation(Box::new(ObserverTrampolineState {
        host,
        callback: ReentrantCell::new(callback),
    }));
    let obs_cb: BamObserverFn = observer_closure_trampoline::<E, F>;
    let mut desc = BamObserverDesc::new(event, obs_cb, raw_state.cast::<c_void>())
        .with_name(name_ptr)
        .with_entity(target)
        .with_flags(flags);
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return validation;
    }
    // SAFETY: `load_ctx` is active and `desc` passed validation with buffers owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
}

/// Validates and registers a mutable [`BamObserverDesc`] closure (`&mut E` + `&mut bool` propagate)
/// on `load_ctx`.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn register_observer_mut_closure<E, F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &str,
    event: BamComponent,
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> FfiResult
where
    E: AbiPod,
    F: FnMut(FfiEntity, &mut E, &mut bool, &mut ValidatedCommands<'_>) -> FfiResult
        + Send
        + 'static,
{
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if name.is_empty() || !event.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(owned_name) = CString::new(name) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    if !instance.host().api().covers_slot(
        core::mem::offset_of!(BamHostApi, register_observer),
        core::mem::size_of::<Option<BamRegisterObserverFn>>(),
    ) {
        return FfiResult::UNSUPPORTED;
    }
    let Some(register_fn) = instance.host().api().register_observer else {
        return FfiResult::UNSUPPORTED;
    };
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` stays pinned inside `instance` until `shutdown_guest_instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let host = *instance.host();
    let raw_state = instance.track_allocation(Box::new(ObserverTrampolineState {
        host,
        callback: ReentrantCell::new(callback),
    }));
    let obs_cb: BamObserverMutFn = observer_mut_closure_trampoline::<E, F>;
    let mut desc = BamObserverDesc::new_mut(event, obs_cb, raw_state.cast::<c_void>())
        .with_name(name_ptr)
        .with_entity(target)
        .with_flags(flags);
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return validation;
    }
    // SAFETY: `load_ctx` is active and `desc` passed validation with buffers owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
}

/// Validates and registers a component lifecycle or `Ready` [`BamObserverDesc`] closure on
/// `load_ctx`.
///
/// # Safety
/// `load_ctx` must be a non-null, active [`BamLoadCtx`] belonging to `instance` during
/// `bevy_abi_mod_init`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn register_lifecycle_observer_closure<F>(
    instance: &mut GuestInstance,
    load_ctx: *mut BamLoadCtx,
    name: &str,
    event_kind: BamEventKind,
    watched_components: &[BamComponent],
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> FfiResult
where
    F: FnMut(FfiEntity, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if name.is_empty() || event_kind == BamEventKind::CUSTOM || !event_kind.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(owned_name) = CString::new(name) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    if !instance.host().api().covers_slot(
        core::mem::offset_of!(BamHostApi, register_observer),
        core::mem::size_of::<Option<BamRegisterObserverFn>>(),
    ) {
        return FfiResult::UNSUPPORTED;
    }
    let Some(register_fn) = instance.host().api().register_observer else {
        return FfiResult::UNSUPPORTED;
    };
    let raw_name = instance.track_allocation(Box::new(owned_name));
    // SAFETY: `raw_name` stays pinned inside `instance` until `shutdown_guest_instance`.
    let name_ptr = unsafe { (*raw_name).as_ptr() };
    let (watched_ptr, watched_len) = if watched_components.is_empty() {
        (core::ptr::null(), 0)
    } else {
        let boxed: Box<[BamComponent]> = watched_components.into();
        let len = boxed.len();
        let raw_watched = instance.track_allocation(Box::new(boxed));
        // SAFETY: `raw_watched` stays pinned inside `instance`.
        (unsafe { (*raw_watched).as_ptr() }, len)
    };
    let host = *instance.host();
    let raw_state = instance.track_allocation(Box::new(ObserverTrampolineState {
        host,
        callback: ReentrantCell::new(callback),
    }));
    let obs_cb: BamObserverFn = lifecycle_observer_closure_trampoline::<F>;
    let mut desc = BamObserverDesc::new_lifecycle(
        event_kind,
        watched_ptr,
        watched_len,
        obs_cb,
        raw_state.cast::<c_void>(),
    )
    .with_name(name_ptr)
    .with_entity(target)
    .with_flags(flags);
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return validation;
    }
    // SAFETY: `load_ctx` is active and `desc` passed validation with buffers owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
}

pub(crate) fn allocate_runtime_observer_desc<E, F>(
    host: &Host,
    name: Option<&str>,
    event: BamComponent,
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> Result<BamObserverDesc, FfiResult>
where
    E: AbiPod,
    F: FnMut(FfiEntity, &E, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    if !event.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let name_ptr = match name {
        Some(s) => {
            if s.is_empty() {
                return Err(FfiResult::INVALID_ARGUMENT);
            }
            let cstr = CString::new(s).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
            let raw_name = host
                .track_runtime_allocation(Box::new(cstr))
                .ok_or(FfiResult::INVALID_STATE)?;
            // SAFETY: `raw_name` is pinned inside `GuestInstance`'s `RuntimeAllocTracker`.
            unsafe { (*raw_name).as_ptr() }
        }
        None => core::ptr::null(),
    };
    let raw_state = host
        .track_runtime_allocation(Box::new(ObserverTrampolineState {
            host: *host,
            callback: ReentrantCell::new(callback),
        }))
        .ok_or(FfiResult::INVALID_STATE)?;
    let obs_cb: BamObserverFn = observer_closure_trampoline::<E, F>;
    let mut desc = BamObserverDesc::new(event, obs_cb, raw_state.cast::<c_void>())
        .with_entity(target)
        .with_flags(flags);
    if !name_ptr.is_null() {
        desc = desc.with_name(name_ptr);
    }
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    Ok(desc)
}

pub(crate) fn allocate_runtime_observer_mut_desc<E, F>(
    host: &Host,
    name: Option<&str>,
    event: BamComponent,
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> Result<BamObserverDesc, FfiResult>
where
    E: AbiPod,
    F: FnMut(FfiEntity, &mut E, &mut bool, &mut ValidatedCommands<'_>) -> FfiResult
        + Send
        + 'static,
{
    if !event.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let name_ptr = match name {
        Some(s) => {
            if s.is_empty() {
                return Err(FfiResult::INVALID_ARGUMENT);
            }
            let cstr = CString::new(s).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
            let raw_name = host
                .track_runtime_allocation(Box::new(cstr))
                .ok_or(FfiResult::INVALID_STATE)?;
            // SAFETY: `raw_name` is pinned inside `GuestInstance`'s `RuntimeAllocTracker`.
            unsafe { (*raw_name).as_ptr() }
        }
        None => core::ptr::null(),
    };
    let raw_state = host
        .track_runtime_allocation(Box::new(ObserverTrampolineState {
            host: *host,
            callback: ReentrantCell::new(callback),
        }))
        .ok_or(FfiResult::INVALID_STATE)?;
    let obs_cb: BamObserverMutFn = observer_mut_closure_trampoline::<E, F>;
    let mut desc = BamObserverDesc::new_mut(event, obs_cb, raw_state.cast::<c_void>())
        .with_entity(target)
        .with_flags(flags);
    if !name_ptr.is_null() {
        desc = desc.with_name(name_ptr);
    }
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    Ok(desc)
}

pub(crate) fn allocate_runtime_lifecycle_observer_desc<F>(
    host: &Host,
    name: Option<&str>,
    event_kind: BamEventKind,
    watched_components: &[BamComponent],
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    callback: F,
) -> Result<BamObserverDesc, FfiResult>
where
    F: FnMut(FfiEntity, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
{
    if event_kind == BamEventKind::CUSTOM || !event_kind.is_valid() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let name_ptr = match name {
        Some(s) => {
            if s.is_empty() {
                return Err(FfiResult::INVALID_ARGUMENT);
            }
            let cstr = CString::new(s).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
            let raw_name = host
                .track_runtime_allocation(Box::new(cstr))
                .ok_or(FfiResult::INVALID_STATE)?;
            // SAFETY: `raw_name` is pinned inside `GuestInstance`'s `RuntimeAllocTracker`.
            unsafe { (*raw_name).as_ptr() }
        }
        None => core::ptr::null(),
    };
    let (watched_ptr, watched_len) = if watched_components.is_empty() {
        (core::ptr::null(), 0)
    } else {
        let boxed: Box<[BamComponent]> = watched_components.into();
        let len = boxed.len();
        let raw_watched = host
            .track_runtime_allocation(Box::new(boxed))
            .ok_or(FfiResult::INVALID_STATE)?;
        // SAFETY: `raw_watched` is pinned inside `GuestInstance`'s `RuntimeAllocTracker`.
        (unsafe { (*raw_watched).as_ptr() }, len)
    };
    let raw_state = host
        .track_runtime_allocation(Box::new(ObserverTrampolineState {
            host: *host,
            callback: ReentrantCell::new(callback),
        }))
        .ok_or(FfiResult::INVALID_STATE)?;
    let obs_cb: BamObserverFn = lifecycle_observer_closure_trampoline::<F>;
    let mut desc = BamObserverDesc::new_lifecycle(
        event_kind,
        watched_ptr,
        watched_len,
        obs_cb,
        raw_state.cast::<c_void>(),
    )
    .with_entity(target)
    .with_flags(flags);
    if !name_ptr.is_null() {
        desc = desc.with_name(name_ptr);
    }
    if propagate_relationship.is_valid() {
        desc = desc.with_propagate(propagate_relationship);
    }
    let validation = desc.validate();
    if !validation.is_ok() {
        return Err(validation);
    }
    Ok(desc)
}

/// Generates a panic-safe [`BamSystemFn`] `extern "C"` wrapper around a Rust system function
/// `fn(*mut BamSystemCtx, *mut c_void) -> FfiResult`.
#[macro_export]
macro_rules! wrap_system_callback {
    ($wrapper_name:ident, $inner_fn:path) => {
        /// Panic-safe `extern "C"` [`bevy_abi_mod_sys::BamSystemFn`] trampoline.
        ///
        /// # Safety
        /// `sys_ctx` must be a valid active [`bevy_abi_mod_sys::BamSystemCtx`] pointer for the
        /// duration of the invocation.
        pub unsafe extern "C" fn $wrapper_name(
            sys_ctx: *mut $crate::BamSystemCtx,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_result(|| {
                if sys_ctx.is_null() {
                    return $crate::FfiResult::NULL_POINTER;
                }
                // SAFETY: Caller guarantees `sys_ctx` and `user_data` satisfy `$inner_fn` contract.
                unsafe { $inner_fn(sys_ctx, user_data) }
            })
        }
    };
}

/// Generates a panic-safe [`BamRunIfFn`] `extern "C"` wrapper around a Rust predicate function
/// `fn(*mut c_void) -> FfiBool`.
#[macro_export]
macro_rules! wrap_run_condition {
    ($wrapper_name:ident, $inner_fn:path) => {
        /// Panic-safe `extern "C"` [`bevy_abi_mod_sys::BamRunIfFn`] trampoline.
        ///
        /// # Safety
        /// `user_data` must satisfy the safety contract of `$inner_fn`.
        pub unsafe extern "C" fn $wrapper_name(
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiBool {
            $crate::catch_guest_bool(|| {
                // SAFETY: Caller guarantees `user_data` satisfies `$inner_fn` contract.
                unsafe { $inner_fn(user_data) }
            })
        }
    };
}

// ============================================================================
// Same-Mod Dynamic `ModComponent` Verification, Borrowing & Move Commands
// ============================================================================

#[inline]
fn resolve_vtable_symbol_for_name(
    host: &Host,
    comp_name: &str,
) -> Result<*const c_void, FfiResult> {
    const PREFIX: &[u8] = b"bam.v0.";
    const SUFFIX: &[u8] = b".vtable\0";
    let total_len = PREFIX.len() + comp_name.len() + SUFFIX.len();
    if total_len <= 256 {
        let mut buf = [0u8; 256];
        buf[..PREFIX.len()].copy_from_slice(PREFIX);
        buf[PREFIX.len()..PREFIX.len() + comp_name.len()].copy_from_slice(comp_name.as_bytes());
        buf[PREFIX.len() + comp_name.len()..total_len].copy_from_slice(SUFFIX);
        let Ok(cstr) = CStr::from_bytes_with_nul(&buf[..total_len]) else {
            return Err(FfiResult::INVALID_ARGUMENT);
        };
        return host.resolve_custom_symbol(cstr);
    }
    let sym = format!("bam.v0.{comp_name}.vtable");
    let Ok(cstr) = CString::new(sym.as_bytes()) else {
        return Err(FfiResult::INVALID_ARGUMENT);
    };
    host.resolve_custom_symbol(cstr.as_c_str())
}

/// Verifies that a host column/component with `(actual_uuid, actual_size, actual_align)` matches
/// `T: ModComponent` registered by this same active mod instance (`vtable_ptr` address equality
/// proves identical Rust ABI + active `ModRegistry` ownership).
pub fn verify_same_mod_dynamic_component<T: ModComponent>(
    host: &Host,
    actual_uuid: ComponentUuid,
    actual_size: usize,
    actual_align: usize,
) -> bool {
    if actual_size != core::mem::size_of::<T>() || actual_align != core::mem::align_of::<T>() {
        return false;
    }
    let Ok((qualified_name, qualified_uuid)) =
        qualify_mod_component_identity::<T>(host.manifest_name())
    else {
        return false;
    };
    let matches_qualified = actual_uuid == qualified_uuid;
    let matches_canonical = actual_uuid == T::UUID;
    if !matches_qualified && !matches_canonical {
        return false;
    }
    let expected_vtable = T::erased_vtable_ptr();
    if matches_qualified
        && let Ok(ptr) = resolve_vtable_symbol_for_name(host, &qualified_name)
        && core::ptr::eq(ptr, expected_vtable)
    {
        return true;
    }
    if matches_canonical
        && qualified_name != T::CANONICAL_NAME
        && let Ok(ptr) = resolve_vtable_symbol_for_name(host, T::CANONICAL_NAME)
        && core::ptr::eq(ptr, expected_vtable)
    {
        return true;
    }
    false
}

/// Borrows dense column `col` as `&[T]` for a same-mod dynamic [`ModComponent`] (`T`), verifying
/// layout, [`ComponentUuid`], and active same-binary VTable provenance.
pub fn iter_dynamic_slice<'b, 'a, T: ModComponent>(
    iter: &'b ValidatedIter<'a>,
    col: usize,
) -> Option<&'b [T]> {
    let info = iter.column_info(col).ok()?;
    if !info.mode.has_entity_column_data() || info.is_resource() || info.is_sparse_set() {
        return None;
    }
    if !verify_same_mod_dynamic_component::<T>(iter.host(), info.uuid, info.size, info.align) {
        return None;
    }
    let len = iter.entities().len();
    if len == 0 {
        if info.mode.is_optional() {
            return None;
        }
        return Some(&[]);
    }
    let elem_size = core::mem::size_of::<T>();
    let ptr = iter.column_readonly_ptr(col);
    if elem_size == 0 {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `T` is zero-sized, `ptr` is non-null (present), and `NonNull::dangling()` is
        // aligned for `T`.
        return Some(unsafe {
            core::slice::from_raw_parts(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
        });
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    let byte_len = len.checked_mul(elem_size)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    // SAFETY: Verified same-mod `T` UUID + active VTable + layout, `ptr` non-null/aligned, and
    // host guarantees `ptr` points to `len` initialized `T` elements for `'b`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<T>(), len) })
}

/// Mutably borrows dense column `col` as `&mut [T]` for a same-mod dynamic [`ModComponent`] (`T`).
pub fn iter_dynamic_slice_mut<'b, 'a, T: ModComponent>(
    iter: &'b mut ValidatedIter<'a>,
    col: usize,
    mark_changed: bool,
) -> Option<&'b mut [T]> {
    let info = iter.column_info(col).ok()?;
    if !info.mode.is_mutable()
        || !info.mode.has_entity_column_data()
        || info.is_immutable()
        || info.is_resource()
        || info.is_sparse_set()
    {
        return None;
    }
    if !verify_same_mod_dynamic_component::<T>(iter.host(), info.uuid, info.size, info.align) {
        return None;
    }
    let len = iter.entities().len();
    if len == 0 {
        if info.mode.is_optional() {
            return None;
        }
        return Some(&mut []);
    }
    let elem_size = core::mem::size_of::<T>();
    let ptr = if mark_changed {
        iter.column_mutable_ptr(col)
    } else {
        iter.column_mutable_untracked_ptr(col)
    };
    if elem_size == 0 {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `T` is zero-sized and `NonNull::dangling()` forms a sound mutable ZST slice.
        return Some(unsafe {
            core::slice::from_raw_parts_mut(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
        });
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    let byte_len = len.checked_mul(elem_size)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    // SAFETY: Verified writable dense column, same-mod `T` UUID + VTable + layout, and `&mut iter`
    // enforces exclusive mutable access for `'b`.
    Some(unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), len) })
}

/// Simultaneously borrows `[FfiEntity; len]` and writable dense dynamic component slice `&mut [T]`
/// (`T: ModComponent`) for the current chunk.
pub fn iter_entities_and_dynamic_slice_mut<'b, 'a, T: ModComponent>(
    iter: &'b mut ValidatedIter<'a>,
    write_col: usize,
) -> Option<(&'b [FfiEntity], &'b mut [T])> {
    let info = iter.column_info(write_col).ok()?;
    if !info.mode.is_mutable()
        || !info.mode.has_entity_column_data()
        || info.is_immutable()
        || info.is_resource()
        || info.is_sparse_set()
    {
        return None;
    }
    if !verify_same_mod_dynamic_component::<T>(iter.host(), info.uuid, info.size, info.align) {
        return None;
    }
    let len = iter.entities().len();
    if len == 0 {
        if info.mode.is_optional() {
            return None;
        }
        return Some((&[], &mut []));
    }
    let elem_size = core::mem::size_of::<T>();
    let ptr = iter.column_mutable_ptr(write_col);
    let entities = iter.entities();
    if elem_size == 0 {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `T` is zero-sized and disjoint from `entities`.
        let slice_mut = unsafe {
            core::slice::from_raw_parts_mut(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
        };
        return Some((entities, slice_mut));
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    let byte_len = len.checked_mul(elem_size)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    // SAFETY: `entities` and component column `ptr` occupy disjoint Bevy archetype storage and
    // `&mut iter` prevents aliasing borrows for `'b`.
    let slice_mut = unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), len) };
    Some((entities, slice_mut))
}

impl<'a> ValidatedSystemRef<'a> {
    /// Looks up a shared `&'b T` reference for a same-mod dynamic [`ModComponent`] on `entity`
    /// using declared access on `(query_index, term_index)`.
    pub fn entity_get_dynamic_checked<'b, T: ModComponent>(
        &'b self,
        iter: &'b ValidatedIter<'a>,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&'b T> {
        if self.sys_ctx.is_null() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_entity_get),
            core::mem::size_of::<Option<BamSysEntityGetFn>>(),
        ) {
            return None;
        }
        let get_fn = self.host.api().sys_entity_get?;
        let info = iter.column_info(term_index).ok()?;
        if !info.mode.has_entity_column_data() || info.is_resource() || info.is_message() {
            return None;
        }
        if !verify_same_mod_dynamic_component::<T>(self.host, info.uuid, info.size, info.align) {
            return None;
        }
        // SAFETY: `self.sys_ctx` is active for `'a` and `&'b self` + `&'b iter` prevent mutable
        // aliasing for `'b`.
        let ptr = unsafe { get_fn(self.sys_ctx.cast_const(), query_index, entity, term_index) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T` may be referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_ref() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        // SAFETY: Host verified query membership & declared access; `verify_same_mod_dynamic_component`
        // confirmed `T`'s layout, UUID, and active same-binary VTable.
        Some(unsafe { &*ptr.cast::<T>() })
    }

    /// Looks up an exclusive `&'b mut T` reference for a same-mod dynamic [`ModComponent`] on
    /// `entity` using declared writable access on `(query_index, term_index)`.
    pub fn entity_get_dynamic_mut_checked<'b, T: ModComponent>(
        &'b mut self,
        iter: &'b mut ValidatedIter<'a>,
        query_index: usize,
        term_index: usize,
        entity: FfiEntity,
    ) -> Option<&'b mut T> {
        if self.sys_ctx.is_null() {
            return None;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, sys_entity_get_mut),
            core::mem::size_of::<Option<BamSysEntityGetMutFn>>(),
        ) {
            return None;
        }
        let get_mut_fn = self.host.api().sys_entity_get_mut?;
        let info = iter.column_info(term_index).ok()?;
        if !info.mode.is_mutable()
            || !info.mode.has_entity_column_data()
            || info.is_immutable()
            || info.is_resource()
            || info.is_message()
        {
            return None;
        }
        if !verify_same_mod_dynamic_component::<T>(self.host, info.uuid, info.size, info.align) {
            return None;
        }
        // SAFETY: `self.sys_ctx` is active for `'a` and `&'b mut self` + `&'b mut iter` prevent
        // aliasing borrows for `'b`.
        let ptr = unsafe { get_mut_fn(self.sys_ctx, query_index, entity, term_index) };
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T` may be mutably referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_mut() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        // SAFETY: Host verified writable access & stamped change ticks; `T`'s layout, UUID, and
        // active same-binary VTable were verified.
        Some(unsafe { &mut *ptr.cast::<T>() })
    }
}

impl<'a> ValidatedCommands<'a> {
    /// Moves an owned non-POD (or POD) [`ModComponent`] `value` into `BamHostApi::cmd_insert` via
    /// [`ManuallyDrop`], dropping `value` on the guest heap if queueing fails before ownership
    /// transfers to the host `CommandArena`.
    pub fn insert_moved<T: ModComponent>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: T,
    ) -> FfiResult {
        let mut slot = ManuallyDrop::new(value);
        let data_ptr = if core::mem::size_of::<T>() == 0 {
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const()
        } else {
            (&raw const *slot).cast::<u8>()
        };
        // SAFETY: `slot` remains alive on the stack during `insert_raw`. On `FfiResult::OK`, the
        // host `CommandArena` copied `size_of::<T>()` bytes with ownership and `drop_fn`; on error,
        // `slot` was not taken and is dropped cleanly below on the guest heap.
        let status =
            unsafe { self.insert_raw(entity, component, data_ptr, core::mem::size_of::<T>()) };
        if !status.is_ok() {
            // SAFETY: `insert_raw` failed before transferring ownership to the host.
            unsafe {
                ManuallyDrop::drop(&mut slot);
            }
        }
        status
    }

    /// Spawns a fresh entity (mod-scoped or persistent) with an owned non-POD (or POD)
    /// [`ModComponent`] `value` plus optional `extra` [`SpawnEntry`] items, dropping `value` on
    /// the guest heap if spawning fails.
    pub fn spawn_moved<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
        extra: &[SpawnEntry<'_>],
        persistent: bool,
    ) -> Result<FfiEntity, FfiResult> {
        let mut slot = ManuallyDrop::new(value);
        if !component.is_valid() || extra.iter().any(|e| !e.component().is_valid()) {
            // SAFETY: Ownership never transferred to the host.
            unsafe {
                ManuallyDrop::drop(&mut slot);
            }
            return Err(FfiResult::INVALID_COMPONENT);
        }
        let val_ptr = if core::mem::size_of::<T>() == 0 {
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const()
        } else {
            (&raw const *slot).cast::<u8>()
        };
        let mut comps = Vec::with_capacity(1 + extra.len());
        let mut ptrs = Vec::with_capacity(1 + extra.len());
        comps.push(component);
        ptrs.push(val_ptr);
        for entry in extra {
            comps.push(entry.component());
            ptrs.push(entry.data_ptr());
        }
        // SAFETY: `comps` and `ptrs` have matching length, `slot` remains live across the call,
        // and each `extra` entry carries a valid `T: AbiPod` reference for `'a`.
        let res = unsafe {
            if persistent {
                self.spawn_persistent_raw(&comps, &ptrs)
            } else {
                self.spawn_raw(&comps, &ptrs)
            }
        };
        if res.is_err() {
            // SAFETY: Spawn failed before transferring ownership into `CommandArena`.
            unsafe {
                ManuallyDrop::drop(&mut slot);
            }
        }
        res
    }
}
