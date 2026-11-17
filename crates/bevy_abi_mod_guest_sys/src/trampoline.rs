//! Lock-free re-entrancy-guarded guest closure trampolines (`BamSystemFn` and `BamRunIfFn`) and
//! unified [`BamSystemDesc`] registration.

use core::{
    cell::UnsafeCell,
    ffi::{CStr, c_char, c_void},
    sync::atomic::{AtomicBool, Ordering},
};
use std::ffi::CString;

use bevy_abi_mod_sys::{
    AbiPod, BAM_SYSTEM_KNOWN_FLAGS, BamCommands, BamHostApi, BamIter, BamLoadCtx, BamQueryDesc,
    BamRunIfFn, BamSysEntityGetFn, BamSysEntityGetMutFn, BamSystemCtx, BamSystemDesc, BamSystemFn,
    BamTermDesc, FfiBool, FfiEntity, FfiResult, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY,
    ScheduleId,
};

use crate::{
    commands::ValidatedCommands,
    host::Host,
    instance::GuestInstance,
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

    /// Returns a validated [`ValidatedSystemRef`] token bound to this active system invocation (`F-03`).
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
/// component lookups (`F-03`).
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
    /// `(query_index, term_index)` (`F-03`).
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
    /// access on `(query_index, term_index)`, marking the component changed (`F-03`).
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
        let Some(result) = state.closure.try_with_mut(|func| {
            // SAFETY: `sys_ctx` is non-null and active for the duration of this `BamSystemFn` call.
            let mut invocation = unsafe {
                ValidatedSystemInvocation::from_raw(
                    sys_ctx,
                    &state.host,
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
}

struct OwnedDescriptorStorage {
    name: CString,
    _term_vecs: Vec<Box<[BamTermDesc]>>,
    queries: Box<[BamQueryDesc]>,
    _after_names: Vec<CString>,
    after_ptrs: Box<[*const c_char]>,
    _before_names: Vec<CString>,
    before_ptrs: Box<[*const c_char]>,
}

// SAFETY: Every raw pointer inside `queries`, `after_ptrs`, and `before_ptrs` points exclusively
// into heap-allocated `Box<[BamTermDesc]>` and `CString` buffers owned by the same struct and
// never mutated after construction.
unsafe impl Send for OwnedDescriptorStorage {}

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
    if config.name.to_bytes().is_empty() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if config.name.to_str().is_err() {
        return FfiResult::INVALID_UTF8;
    }
    if !config.schedule.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if (config.flags & !BAM_SYSTEM_KNOWN_FLAGS) != 0 {
        return FfiResult::UNSUPPORTED;
    }

    let has_sys_resources = !config.system_resources.is_empty();
    let total_query_count = config.user_queries.len() + usize::from(has_sys_resources);
    if total_query_count > MAX_QUERIES_PER_SYSTEM {
        return FfiResult::CAPACITY_EXCEEDED;
    }
    if config.system_resources.len() > MAX_TERMS_PER_QUERY {
        return FfiResult::CAPACITY_EXCEEDED;
    }

    for terms in config.user_queries {
        if terms.len() > MAX_TERMS_PER_QUERY {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        for term in *terms {
            let status = term.validate();
            if !status.is_ok() {
                return status;
            }
        }
    }
    for term in config.system_resources {
        let status = term.validate();
        if !status.is_ok() {
            return status;
        }
    }
    for set_name in config.after.iter().chain(config.before.iter()) {
        if set_name.to_bytes().is_empty() {
            return FfiResult::INVALID_ARGUMENT;
        }
        if set_name.to_str().is_err() {
            return FfiResult::INVALID_UTF8;
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

    let mut after_names: Vec<CString> = Vec::with_capacity(config.after.len());
    let mut after_ptrs: Vec<*const c_char> = Vec::with_capacity(config.after.len());
    for set_name in config.after {
        let cstr = CString::from(*set_name);
        after_ptrs.push(cstr.as_ptr());
        after_names.push(cstr);
    }

    let mut before_names: Vec<CString> = Vec::with_capacity(config.before.len());
    let mut before_ptrs: Vec<*const c_char> = Vec::with_capacity(config.before.len());
    for set_name in config.before {
        let cstr = CString::from(*set_name);
        before_ptrs.push(cstr.as_ptr());
        before_names.push(cstr);
    }

    let storage = OwnedDescriptorStorage {
        name: CString::from(config.name),
        _term_vecs: term_vecs,
        queries: query_descs.into_boxed_slice(),
        _after_names: after_names,
        after_ptrs: after_ptrs.into_boxed_slice(),
        _before_names: before_names,
        before_ptrs: before_ptrs.into_boxed_slice(),
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
    let after_ptr = if storage_ref.after_ptrs.is_empty() {
        core::ptr::null()
    } else {
        storage_ref.after_ptrs.as_ptr()
    };
    let before_ptr = if storage_ref.before_ptrs.is_empty() {
        core::ptr::null()
    } else {
        storage_ref.before_ptrs.as_ptr()
    };

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
    .with_before(before_ptr, config.before.len());

    if let Some((run_if_fn, run_if_data)) = run_if {
        desc = desc.with_run_if(run_if_fn, run_if_data);
    }

    let validation = desc.validate();
    if !validation.is_ok() {
        return validation;
    }

    let Some(register_fn) = instance.host().api().register_system else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: `load_ctx` is active per caller contract and `desc` passed full validation with all
    // pointed-to strings, term slices, and closures owned by `instance`.
    unsafe { register_fn(load_ctx, &desc) }
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
