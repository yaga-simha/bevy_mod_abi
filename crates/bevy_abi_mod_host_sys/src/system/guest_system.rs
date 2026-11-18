//! `GuestSystem` implementation of Bevy's `System` trait and `HostSystemCtx` (`BamSystemCtx`)
//!.

use crate::ffi::commands::{HostCommands, HostCommandsStorage};
use crate::ffi::iter::{HostIter, QueryPlan};
use crate::handle_header::{
    HandleHeader, HostContext, SYS_CTX_MAGIC, validate_handle_mut, validate_handle_ref,
};
use crate::loader::{ModHost, ModId};
use crate::registry::{ModRegistry, RegistrySnapshot};
use crate::schedule::ModSystemSet;
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::initialize_query_groups_in_world;
use crate::system::error_policy::{
    ModErrorPolicy, ModErrorPolicyConfig, ModSystemError, ModSystemFaulted, SystemInitError,
};
use crate::system::query_group::{BamQuerySpecOwned, CachedQueryGroup, MAX_SYSTEM_QUERIES};
use bevy_abi_mod_sys::{
    AccessMode, BamChangeTicks, BamCommands, BamComponent, BamHostCtx, BamIter, BamRunIfFn,
    BamSystemCtx, BamSystemFn, FfiEntity, FfiResult, ScheduleId,
};
use bevy_ecs::archetype::ArchetypeId;
use bevy_ecs::change_detection::{CheckChangeTicks, Tick};
use bevy_ecs::entity::Entity;
use bevy_ecs::error::{BevyError, ContextExt};
use bevy_ecs::message::Messages;
#[cfg(feature = "diagnostics")]
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{InternedSystemSet, SystemSet};
use bevy_ecs::storage::TableId;
use bevy_ecs::system::{RunSystemError, System, SystemAccess, SystemIn, SystemStateFlags};
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_ecs::world::{CommandQueue, DeferredWorld, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(feature = "diagnostics")]
use std::sync::Mutex;

/// Snapshot of a single guest system execution recorded when the `diagnostics` feature is enabled.
#[cfg(feature = "diagnostics")]
#[derive(Clone, Debug)]
pub struct ModDiagnosticsSample {
    /// Owning [`ModId`], if associated with a loaded mod.
    pub mod_id: Option<ModId>,
    /// Canonical mod namespace (`BamManifest::name` or `"unscoped"`).
    pub mod_name: Arc<str>,
    /// Guest system name.
    pub system_name: Arc<str>,
    /// Wall-clock CPU duration of the foreign `extern "C"` callback in microseconds.
    pub cpu_time_us: f64,
    /// Number of deferred commands queued during this system invocation.
    pub commands_queued: usize,
    /// Whether this invocation faulted or caught a guest panic.
    pub faulted: bool,
}

/// World resource buffering [`ModDiagnosticsSample`]s from parallel [`GuestSystem`] executions
/// until flushed into Bevy's `DiagnosticsStore`.
#[cfg(feature = "diagnostics")]
#[derive(Resource, Default)]
pub struct ModDiagnosticsCollector {
    samples: Mutex<Vec<ModDiagnosticsSample>>,
}

#[cfg(feature = "diagnostics")]
impl ModDiagnosticsCollector {
    /// Creates an empty [`ModDiagnosticsCollector`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a single execution sample from a `GuestSystem` run.
    pub fn push(&self, sample: ModDiagnosticsSample) {
        if let Ok(mut guard) = self.samples.lock() {
            guard.push(sample);
        }
    }

    /// Drains all buffered samples accumulated since the last flush.
    pub fn drain(&self) -> Vec<ModDiagnosticsSample> {
        match self.samples.lock() {
            Ok(mut guard) => core::mem::take(&mut *guard),
            Err(poisoned) => core::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RandomQueryView<'w> {
    pub(crate) plan: &'w QueryPlan,
    pub(crate) is_dense: bool,
    pub(crate) matched_tables: &'w [TableId],
    pub(crate) matched_archetypes: &'w [ArchetypeId],
    pub(crate) has_query_state: bool,
}

/// Concrete `#[repr(C)]` system invocation context backing opaque [`BamSystemCtx`].
#[repr(C)]
pub struct HostSystemCtx<'w> {
    /// Leading handle validation header (`SYS_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    host_ctx: HostContext,
    iter_ptrs: [*mut BamIter; MAX_SYSTEM_QUERIES],
    query_views: [Option<RandomQueryView<'w>>; MAX_SYSTEM_QUERIES],
    query_count: usize,
    commands_ptr: *mut BamCommands,
    world: Option<UnsafeWorldCell<'w>>,
    last_run: Tick,
    this_run: Tick,
}

const _: () = assert!(core::mem::offset_of!(HostSystemCtx<'_>, header) == 0);

impl<'w> HostSystemCtx<'w> {
    /// Creates an active stack-scoped [`HostSystemCtx`] for one `GuestSystem` callback invocation.
    pub fn new(
        snapshot: Arc<RegistrySnapshot>,
        mod_namespace: Option<Arc<str>>,
        error_buf: String,
        iter_ptrs: [*mut BamIter; MAX_SYSTEM_QUERIES],
        query_count: usize,
        commands_ptr: *mut BamCommands,
    ) -> Self {
        Self {
            header: HandleHeader::new(SYS_CTX_MAGIC),
            host_ctx: HostContext::with_error_buffer(snapshot, error_buf)
                .with_namespace(mod_namespace),
            iter_ptrs,
            query_views: [None; MAX_SYSTEM_QUERIES],
            query_count,
            commands_ptr,
            world: None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
        }
    }

    #[inline]
    pub(crate) fn with_world_and_queries(
        mut self,
        world: UnsafeWorldCell<'w>,
        query_views: [Option<RandomQueryView<'w>>; MAX_SYSTEM_QUERIES],
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        self.world = Some(world);
        self.query_views = query_views;
        self.last_run = last_run;
        self.this_run = this_run;
        self
    }

    /// Reclaims the internal diagnostic `String` capacity for zero-allocation reuse across frames.
    #[inline]
    pub fn reclaim_error_buffer(&mut self) -> String {
        self.host_ctx.reclaim_error_buffer()
    }

    /// Casts `&mut self` to an opaque `*mut BamSystemCtx` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamSystemCtx {
        (self as *mut Self).cast::<BamSystemCtx>()
    }
}

impl Drop for HostSystemCtx<'_> {
    #[inline]
    fn drop(&mut self) {
        self.header.poison();
    }
}

/// Obtains the world-scoped [`BamHostCtx`] pointer from an active [`BamSystemCtx`].
///
/// # Safety
/// `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
pub unsafe extern "C" fn host_sys_host_ctx(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return core::ptr::null();
    };
    ctx.host_ctx.as_ffi_ptr()
}

/// Borrows the [`BamIter`] handle for declared query `query_index` (`0..query_count`).
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - `out_iter` must point to writable `*mut BamIter` storage, or null.
pub unsafe extern "C" fn host_sys_query(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult {
    if out_iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let ctx = match unsafe {
        validate_handle_mut::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    if query_index >= ctx.query_count {
        return FfiResult::OUT_OF_RANGE;
    }
    // SAFETY: `out_iter` is non-null and writable.
    unsafe {
        *out_iter = ctx.iter_ptrs[query_index];
    }
    FfiResult::OK
}

/// Borrows the per-system [`BamCommands`] deferred command buffer from `sys_ctx`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - `out_commands` must point to writable `*mut BamCommands` storage, or null.
pub unsafe extern "C" fn host_sys_commands(
    sys_ctx: *mut BamSystemCtx,
    out_commands: *mut *mut BamCommands,
) -> FfiResult {
    if out_commands.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let ctx = match unsafe {
        validate_handle_mut::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `out_commands` is non-null and writable.
    unsafe {
        *out_commands = ctx.commands_ptr;
    }
    FfiResult::OK
}

fn entity_matches_random_query_view<'w>(
    world: UnsafeWorldCell<'w>,
    view: &RandomQueryView<'_>,
    entity: Entity,
    last_run: Tick,
    this_run: Tick,
) -> Option<bevy_ecs::world::unsafe_world_cell::UnsafeEntityCell<'w>> {
    if !view.has_query_state {
        return None;
    }
    let entity_cell = world
        .get_entity_with_ticks(entity, last_run, this_run)
        .ok()?;
    let location = entity_cell.location();
    let archetype_matched = if view.is_dense {
        view.matched_tables.contains(&location.table_id)
    } else {
        view.matched_archetypes.contains(&location.archetype_id)
    };
    if !archetype_matched {
        return None;
    }
    if !view.plan.tick_filters.is_empty() {
        // SAFETY: `FilteredAccessSet` declared read/write access on every tick-filter component in `view.plan`.
        let storages = unsafe { world.storages() };
        for tf in &view.plan.tick_filters {
            let tick = if tf.is_sparse {
                let ss = storages.sparse_sets.get(tf.component_id)?;
                let cell = if tf.is_added {
                    ss.get_added_tick(entity)?
                } else {
                    ss.get_changed_tick(entity)?
                };
                // SAFETY: Shared tick access declared in `FilteredAccessSet`.
                unsafe { *cell.get() }
            } else {
                let table = storages.tables.get(location.table_id)?;
                let col = table.get_column(tf.component_id)?;
                let table_len = table.entity_count() as usize;
                let row = location.table_row.index();
                if row >= table_len {
                    return None;
                }
                let slice = if tf.is_added {
                    // SAFETY: `table_len == table.entity_count() as usize`.
                    unsafe { col.get_added_ticks_slice(table_len) }
                } else {
                    // SAFETY: `table_len == table.entity_count() as usize`.
                    unsafe { col.get_changed_ticks_slice(table_len) }
                };
                // SAFETY: `row < table_len == slice.len()`.
                unsafe { *slice.get_unchecked(row).get() }
            };
            if !tick.is_newer_than(last_run, this_run) {
                return None;
            }
        }
    }
    Some(entity_cell)
}

/// Random `O(1)` read-only component lookup on `entity` using the access declared by
/// `(query_index, term_index)` in `FilteredAccessSet`.
///
/// Strictly verifies both that `term_index` declared component data read/write access and that
/// `entity`'s archetype matches `query_index`'s `With`/`Without`/tick filters so Bevy's parallel
/// scheduler aliasing invariants remain sound.
///
/// # Safety
/// `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
pub unsafe extern "C" fn host_sys_entity_get(
    sys_ctx: *const BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *const u8 {
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return core::ptr::null();
    };
    if query_index >= ctx.query_count || entity.is_null() {
        return core::ptr::null();
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return core::ptr::null();
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return core::ptr::null();
    }
    let Some(ref view) = ctx.query_views[query_index] else {
        return core::ptr::null();
    };
    if term_index >= view.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &view.plan.terms[term_index];
    if !term.mode.has_entity_column_data() || term.is_resource() || term.is_message() {
        return core::ptr::null();
    }
    let Some(world) = ctx.world else {
        return core::ptr::null();
    };
    let Some(entity_cell) =
        entity_matches_random_query_view(world, view, bevy_entity, ctx.last_run, ctx.this_run)
    else {
        return core::ptr::null();
    };
    // SAFETY: `view` verified that `entity` belongs to an archetype matched by `query_index` and
    // `term_index` declared read or write component access in `FilteredAccessSet`.
    match unsafe { entity_cell.get_by_id(term.component_id) } {
        Some(ptr) => ptr.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Random `O(1)` mutable component lookup on `entity` using the `WRITE` / `OPTIONAL_WRITE` access
/// declared by `(query_index, term_index)` in `FilteredAccessSet`, marking `entity`'s
/// component change tick on success.
///
/// # Safety
/// `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
pub unsafe extern "C" fn host_sys_entity_get_mut(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *mut u8 {
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_mut::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return core::ptr::null_mut();
    };
    if query_index >= ctx.query_count || entity.is_null() {
        return core::ptr::null_mut();
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return core::ptr::null_mut();
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return core::ptr::null_mut();
    }
    let Some(ref view) = ctx.query_views[query_index] else {
        return core::ptr::null_mut();
    };
    if term_index >= view.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &view.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
        || term.is_resource()
        || term.is_message()
    {
        return core::ptr::null_mut();
    }
    let Some(world) = ctx.world else {
        return core::ptr::null_mut();
    };
    let Some(entity_cell) =
        entity_matches_random_query_view(world, view, bevy_entity, ctx.last_run, ctx.this_run)
    else {
        return core::ptr::null_mut();
    };
    // SAFETY: `view` verified that `entity` belongs to an archetype matched by `query_index` and
    // `term_index` declared exclusive `WRITE` or `OPTIONAL_WRITE` access in `FilteredAccessSet`.
    match unsafe { entity_cell.get_mut_by_id(term.component_id) } {
        Ok(mut_untyped) => mut_untyped.into_inner().as_ptr(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// Looks up a singleton read-only `Resource` by its pre-resolved [`BamComponent`] handle
/// (`host_sys_res_ptr`) across this system's declared queries, optionally writing its Bevy
/// change-detection ticks into `out_ticks` (nullable).
///
/// Returns null if `sys_ctx` is invalid, `component` was not declared with `RESOURCE_READ` or
/// `RESOURCE_WRITE` in this system, `out_ticks` is non-null and misaligned, or the resource is absent.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - If `out_ticks` is non-null, it must point to aligned writable [`BamChangeTicks`] storage.
pub unsafe extern "C" fn host_sys_res_ptr(
    sys_ctx: *const BamSystemCtx,
    component: BamComponent,
    out_ticks: *mut BamChangeTicks,
) -> *const u8 {
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return core::ptr::null();
    };
    if !out_ticks.is_null() && !out_ticks.is_aligned() {
        ctx.host_ctx
            .set_last_error("sys_res_ptr: misaligned out_ticks pointer");
        return core::ptr::null();
    }
    let Some(term) = ctx
        .query_views
        .iter()
        .take(ctx.query_count)
        .flatten()
        .flat_map(|v| v.plan.terms.iter())
        .find(|t| t.component == component && t.mode.is_resource() && t.is_resource())
    else {
        ctx.host_ctx.set_last_error(
            "sys_res_ptr: resource handle was not declared with ResourceRead or ResourceWrite in any query of this system",
        );
        return core::ptr::null();
    };
    let Some(world) = ctx.world else {
        return core::ptr::null();
    };
    // SAFETY: `RESOURCE_READ` or `RESOURCE_WRITE` on `term.component_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return core::ptr::null();
    };
    let Ok(entity_cell) = world.get_entity(entity) else {
        return core::ptr::null();
    };
    if !out_ticks.is_null() {
        // SAFETY: Shared read access to `term.component_id` was declared in `FilteredAccessSet`.
        if let Some(ticks) = unsafe { entity_cell.get_change_ticks_by_id(term.component_id) } {
            // SAFETY: `out_ticks` is non-null, aligned, and exclusively writable here.
            unsafe {
                *out_ticks = BamChangeTicks {
                    added: ticks.added.get(),
                    changed: ticks.changed.get(),
                    last_run: ctx.last_run.get(),
                    this_run: ctx.this_run.get(),
                };
            }
        }
    }
    // SAFETY: Shared read access to `term.component_id` was declared in `FilteredAccessSet`.
    match unsafe { entity_cell.get_by_id(term.component_id) } {
        Some(ptr) => ptr.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Looks up an exclusive mutable singleton `Resource` by its pre-resolved [`BamComponent`] handle
/// (`host_sys_res_mut_ptr`), optionally writing its prior Bevy change-detection ticks into
/// `out_ticks` (nullable) and stamping `changed = this_run`.
///
/// Returns null if `sys_ctx` is invalid, `component` was not declared with `RESOURCE_WRITE` in
/// this system (or is immutable), `out_ticks` is non-null and misaligned, or the resource is absent.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - If `out_ticks` is non-null, it must point to aligned writable [`BamChangeTicks`] storage.
pub unsafe extern "C" fn host_sys_res_mut_ptr(
    sys_ctx: *mut BamSystemCtx,
    component: BamComponent,
    out_ticks: *mut BamChangeTicks,
) -> *mut u8 {
    // SAFETY: Validating `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_mut::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return core::ptr::null_mut();
    };
    if !out_ticks.is_null() && !out_ticks.is_aligned() {
        ctx.host_ctx
            .set_last_error("sys_res_mut_ptr: misaligned out_ticks pointer");
        return core::ptr::null_mut();
    }
    let Some(term) = ctx
        .query_views
        .iter()
        .take(ctx.query_count)
        .flatten()
        .flat_map(|v| v.plan.terms.iter())
        .find(|t| {
            t.component == component && t.mode == AccessMode::RESOURCE_WRITE && t.is_resource()
        })
    else {
        ctx.host_ctx.set_last_error(
            "sys_res_mut_ptr: resource handle was not declared with ResourceWrite in this system",
        );
        return core::ptr::null_mut();
    };
    if term.is_immutable() {
        ctx.host_ctx
            .set_last_error("sys_res_mut_ptr: resource is immutable");
        return core::ptr::null_mut();
    }
    let Some(world) = ctx.world else {
        return core::ptr::null_mut();
    };
    // SAFETY: `RESOURCE_WRITE` on `term.component_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return core::ptr::null_mut();
    };
    if !out_ticks.is_null()
        && let Ok(read_cell) = world.get_entity(entity)
        && let Some(ticks) =
            // SAFETY: No alias exists yet; reading prior change ticks before mutable borrow.
            unsafe { read_cell.get_change_ticks_by_id(term.component_id) }
    {
        // SAFETY: `out_ticks` is non-null, aligned, and exclusively writable here.
        unsafe {
            *out_ticks = BamChangeTicks {
                added: ticks.added.get(),
                changed: ticks.changed.get(),
                last_run: ctx.last_run.get(),
                this_run: ctx.this_run.get(),
            };
        }
    }
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, ctx.last_run, ctx.this_run) else {
        return core::ptr::null_mut();
    };
    // SAFETY: Exclusive mutable access to this resource component is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(term.component_id) }) else {
        return core::ptr::null_mut();
    };
    mut_untyped.into_inner().as_ptr()
}

/// Host-side Bevy [`System`] wrapping a unified ABI v0.2 [`BamSystemFn`] callback.
pub struct GuestSystem {
    name: Arc<str>,
    debug_name: DebugName,
    query_groups: Vec<CachedQueryGroup>,

    access: SystemAccess,
    self_conflict: bool,
    init_error: Option<SystemInitError>,
    last_run: Tick,
    this_run: Tick,

    callback: GuestFnPtr<BamSystemFn>,
    user_data: GuestUserData,
    run_if: Option<(GuestFnPtr<BamRunIfFn>, GuestUserData)>,

    mod_id: Option<ModId>,
    mod_namespace: Option<Arc<str>>,
    schedule: ScheduleId,
    is_main_thread: bool,
    after_systems: Vec<String>,
    before_systems: Vec<String>,
    after_weak_systems: Vec<String>,
    before_weak_systems: Vec<String>,
    in_sets: Vec<String>,
    ambiguous_with_systems: Vec<String>,
    ambiguous_with_all: bool,
    error_policy: ModErrorPolicy,
    error_policy_explicit: bool,

    enabled_flag: Arc<AtomicBool>,
    is_active: Arc<AtomicBool>,
    commands_storage: HostCommandsStorage,
    fault_queue: CommandQueue,
    last_error_buf: String,

    registry: ModRegistry,
    cached_registry_gen: u64,
    snapshot: Arc<RegistrySnapshot>,
    #[cfg(feature = "diagnostics")]
    diagnostics_collector_id: Option<bevy_ecs::component::ComponentId>,
}

impl GuestSystem {
    /// Creates a unified ABI v0.2 [`GuestSystem`] over `0..=MAX_SYSTEM_QUERIES` query clauses.
    pub fn new(
        name: impl Into<Arc<str>>,
        query_specs: Vec<BamQuerySpecOwned>,
        callback: BamSystemFn,
        user_data: *mut c_void,
        registry: ModRegistry,
    ) -> Self {
        let name_arc: Arc<str> = name.into();
        let debug_name = DebugName::owned(name_arc.to_string());
        let query_groups = query_specs.into_iter().map(CachedQueryGroup::new).collect();
        let cached_registry_gen = registry.generation();
        let snapshot = registry.snapshot_arc();

        Self {
            name: name_arc,
            debug_name,
            query_groups,
            access: SystemAccess::None,
            self_conflict: false,
            init_error: None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: GuestFnPtr(callback),
            user_data: GuestUserData::new(user_data),
            run_if: None,
            mod_id: None,
            mod_namespace: None,
            schedule: ScheduleId::UPDATE,
            is_main_thread: false,
            after_systems: Vec::new(),
            before_systems: Vec::new(),
            after_weak_systems: Vec::new(),
            before_weak_systems: Vec::new(),
            in_sets: Vec::new(),
            ambiguous_with_systems: Vec::new(),
            ambiguous_with_all: false,
            error_policy: ModErrorPolicy::Quarantine,
            error_policy_explicit: false,
            enabled_flag: Arc::new(AtomicBool::new(true)),
            is_active: Arc::new(AtomicBool::new(true)),
            commands_storage: HostCommandsStorage::new(),
            fault_queue: CommandQueue::default(),
            last_error_buf: String::new(),
            registry,
            cached_registry_gen,
            snapshot,
            #[cfg(feature = "diagnostics")]
            diagnostics_collector_id: None,
        }
    }

    /// Returns the guest system's registered name.
    #[inline]
    pub fn system_name(&self) -> &str {
        &self.name
    }

    /// Returns an `Arc<str>` clone of the guest system's registered name.
    #[inline]
    pub fn system_name_arc(&self) -> Arc<str> {
        Arc::clone(&self.name)
    }

    /// Returns a clone of the shared atomic enablement flag used by [`crate::ModQuarantine`].
    #[inline]
    pub fn enabled_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.enabled_flag)
    }

    /// Returns `true` if the system is currently enabled (not quarantined).
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled_flag.load(Ordering::Acquire)
    }

    /// Enables or disables execution of this system.
    #[inline]
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled_flag.store(enabled, Ordering::Release);
    }

    /// Replaces the [`ModRegistry`] handle used by this system.
    pub(crate) fn set_registry(&mut self, registry: ModRegistry) {
        self.cached_registry_gen = registry.generation();
        self.snapshot = registry.snapshot_arc();
        self.registry = registry;
    }

    /// Returns `true` if the system was permanently disabled due to internal query borrow conflicts.
    #[inline]
    pub const fn has_self_conflict(&self) -> bool {
        self.self_conflict
    }

    /// Returns the structured [`SystemInitError`] recorded if `System::initialize` failed.
    #[inline]
    pub fn init_error(&self) -> Option<&SystemInitError> {
        self.init_error.as_ref()
    }

    /// Validates that every `BamComponent` handle referenced in `query_groups` exists in `registry`.
    pub fn validate_against_registry(&self, registry: &ModRegistry) -> Result<(), Vec<String>> {
        let snap = registry.snapshot();
        let mut missing = Vec::new();
        for group in &self.query_groups {
            for term in &group.spec.terms {
                if snap.get_by_handle(term.component).is_none() {
                    let label = format!("BamComponent({})", term.component.0);
                    if !missing.contains(&label) {
                        missing.push(label);
                    }
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }

    /// Returns the [`ModId`] associated with this system, if any.
    #[inline]
    pub const fn mod_id(&self) -> Option<ModId> {
        self.mod_id
    }

    /// Associates this system with a [`ModId`] so it joins [`ModSystemSet::Mod`].
    #[inline]
    pub fn set_mod_id(&mut self, mod_id: ModId) {
        self.mod_id = Some(mod_id);
    }

    /// Sets the canonical module namespace (`BamManifest::name`) attached to `BamHostCtx`.
    #[inline]
    pub fn set_mod_namespace(&mut self, namespace: Option<Arc<str>>) {
        self.mod_namespace = namespace;
    }

    /// Builder helper to attach the canonical module namespace (`BamManifest::name`).
    #[inline]
    pub fn with_mod_namespace(mut self, namespace: impl Into<Arc<str>>) -> Self {
        self.mod_namespace = Some(namespace.into());
        self
    }

    /// Builder helper to associate this system with a [`ModId`].
    #[inline]
    pub fn with_mod_id(mut self, mod_id: ModId) -> Self {
        self.mod_id = Some(mod_id);
        self
    }

    /// Returns the target [`ScheduleId`] requested for this system.
    #[inline]
    pub const fn schedule(&self) -> ScheduleId {
        self.schedule
    }

    /// Sets the target [`ScheduleId`] for this system.
    #[inline]
    pub fn set_schedule(&mut self, schedule: ScheduleId) {
        self.schedule = schedule;
    }

    /// Builder helper to set the target [`ScheduleId`] for this system.
    #[inline]
    pub const fn with_schedule(mut self, schedule: ScheduleId) -> Self {
        self.schedule = schedule;
        self
    }

    /// Returns `true` if `BAM_SYSTEM_FLAG_MAIN_THREAD` was set (`SystemStateFlags::NON_SEND`).
    #[inline]
    pub const fn is_main_thread(&self) -> bool {
        self.is_main_thread
    }

    /// Pins or unpins this system onto Bevy's main thread.
    #[inline]
    pub fn set_main_thread(&mut self, main_thread: bool) {
        self.is_main_thread = main_thread;
    }

    /// Builder helper to pin this system onto Bevy's main thread.
    #[inline]
    pub const fn with_main_thread(mut self, main_thread: bool) -> Self {
        self.is_main_thread = main_thread;
        self
    }

    /// Builder helper to configure explicit `after` and `before` ordering target names.
    pub fn with_ordering(mut self, after: Vec<String>, before: Vec<String>) -> Self {
        self.after_systems = after;
        self.before_systems = before;
        self
    }

    /// System-set target names declared in `BamSystemDesc::after`.
    #[inline]
    pub fn after_systems(&self) -> &[String] {
        &self.after_systems
    }

    /// System-set target names declared in `BamSystemDesc::before`.
    #[inline]
    pub fn before_systems(&self) -> &[String] {
        &self.before_systems
    }

    /// Builder helper to configure weak `after_weak` and `before_weak` ordering target names.
    pub fn with_weak_ordering(mut self, after_weak: Vec<String>, before_weak: Vec<String>) -> Self {
        self.after_weak_systems = after_weak;
        self.before_weak_systems = before_weak;
        self
    }

    /// System-set target names declared in `BamSystemDesc::after_weak`.
    #[inline]
    pub fn after_weak_systems(&self) -> &[String] {
        &self.after_weak_systems
    }

    /// System-set target names declared in `BamSystemDesc::before_weak`.
    #[inline]
    pub fn before_weak_systems(&self) -> &[String] {
        &self.before_weak_systems
    }

    /// Configures explicit `in_sets` membership target names (`"host::<set>"` or named sets).
    pub fn with_in_sets(mut self, in_sets: Vec<String>) -> Self {
        self.in_sets = in_sets;
        self
    }

    /// System-set membership target names declared in `BamSystemDesc::in_sets`.
    #[inline]
    pub fn in_sets(&self) -> &[String] {
        &self.in_sets
    }

    /// Configures explicit `ambiguous_with` target names (`BamSystemDesc::ambiguous_with`).
    pub fn with_ambiguous_with(mut self, ambiguous_with: Vec<String>) -> Self {
        self.ambiguous_with_systems = ambiguous_with;
        self
    }

    /// System-set target names declared in `BamSystemDesc::ambiguous_with`.
    #[inline]
    pub fn ambiguous_with_systems(&self) -> &[String] {
        &self.ambiguous_with_systems
    }

    /// Configures `BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL` (`.ambiguous_with_all()`).
    #[inline]
    pub const fn with_ambiguous_with_all(mut self, ambiguous_with_all: bool) -> Self {
        self.ambiguous_with_all = ambiguous_with_all;
        self
    }

    /// Returns `true` if `BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL` was set on this system.
    #[inline]
    pub const fn is_ambiguous_with_all(&self) -> bool {
        self.ambiguous_with_all
    }

    /// Returns the [`ModErrorPolicy`] governing faults in this system.
    #[inline]
    pub const fn error_policy(&self) -> ModErrorPolicy {
        self.error_policy
    }

    /// Overrides the [`ModErrorPolicy`] for this system.
    #[inline]
    pub fn set_error_policy(&mut self, policy: ModErrorPolicy) {
        self.error_policy = policy;
        self.error_policy_explicit = true;
    }

    /// Builder helper to override the [`ModErrorPolicy`] for this system.
    #[inline]
    pub const fn with_error_policy(mut self, policy: ModErrorPolicy) -> Self {
        self.error_policy = policy;
        self.error_policy_explicit = true;
        self
    }

    /// Obtains a clone of the atomic module liveness tombstone flag.
    #[inline]
    pub fn is_active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_active)
    }

    /// Sets the atomic module liveness tombstone flag.
    #[inline]
    pub fn set_is_active(&mut self, is_active: Arc<AtomicBool>) {
        self.is_active = is_active;
    }

    /// Fluent builder method to attach an atomic module liveness tombstone flag.
    #[inline]
    pub fn with_is_active(mut self, is_active: Arc<AtomicBool>) -> Self {
        self.is_active = is_active;
        self
    }

    /// Configures an optional C-ABI `run_if` predicate evaluated before each system run.
    #[inline]
    pub fn set_run_if(&mut self, run_if: Option<(BamRunIfFn, *mut c_void)>) {
        self.run_if = run_if.map(|(cb, ud)| (GuestFnPtr(cb), GuestUserData::new(ud)));
    }

    /// Fluent builder method to attach an optional C-ABI `run_if` predicate.
    #[inline]
    pub fn with_run_if(mut self, run_if: Option<(BamRunIfFn, *mut c_void)>) -> Self {
        self.set_run_if(run_if);
        self
    }

    /// Returns the configured `run_if` callback and context pointer, if any.
    #[inline]
    pub fn run_if(&self) -> Option<(BamRunIfFn, *mut c_void)> {
        self.run_if.map(|(GuestFnPtr(cb), ud)| (cb, ud.as_ptr()))
    }

    fn record_fault_and_apply_policy(
        &mut self,
        status: FfiResult,
        panicked: bool,
    ) -> Result<(), RunSystemError> {
        if self.error_policy != ModErrorPolicy::DelegateToBevyFallback {
            self.enabled_flag.store(false, Ordering::Release);
        }
        // Discard any partial deferred commands recorded by the faulting callback invocation.
        self.commands_storage.clear();

        let mod_id = self.mod_id;
        let sys_name = self.name.to_string();

        match self.error_policy {
            ModErrorPolicy::Quarantine => {
                tracing::warn!(
                    system = %sys_name,
                    mod_id = ?mod_id,
                    %status,
                    panicked,
                    "quarantined faulting guest mod system"
                );
            }
            ModErrorPolicy::Log | ModErrorPolicy::ReportToBevy => {
                tracing::error!(
                    system = %sys_name,
                    mod_id = ?mod_id,
                    %status,
                    panicked,
                    "guest mod system faulted"
                );
            }
            ModErrorPolicy::DelegateToBevyFallback => {
                tracing::debug!(
                    system = %sys_name,
                    mod_id = ?mod_id,
                    %status,
                    panicked,
                    "delegating guest mod system fault to Bevy FallbackErrorHandler"
                );
            }
        }

        let fault_msg = ModSystemFaulted {
            mod_id,
            system_name: sys_name.clone(),
            status,
            panicked,
            init_error: None,
        };
        self.fault_queue.push(move |world: &mut World| {
            if let Some(id) = mod_id {
                ModHost::record_fault_in_world(world, id);
            }
            if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                msgs.write(fault_msg);
            }
        });

        match self.error_policy {
            ModErrorPolicy::Quarantine | ModErrorPolicy::Log => Ok(()),
            ModErrorPolicy::ReportToBevy => Err(RunSystemError::from(ModSystemError {
                mod_id,
                system_name: sys_name,
                status,
                panicked,
            })),
            ModErrorPolicy::DelegateToBevyFallback => {
                let err_res: Result<(), ModSystemError> = Err(ModSystemError {
                    mod_id,
                    system_name: sys_name.clone(),
                    status,
                    panicked,
                });
                let bevy_err: BevyError = err_res
                    .with_context(|| {
                        format!(
                            "guest mod {:?} system {:?} faulted with {}",
                            mod_id, sys_name, status
                        )
                    })
                    .unwrap_err();
                Err(RunSystemError::Failed(bevy_err))
            }
        }
    }
}

impl System for GuestSystem {
    type In = ();
    type Out = ();

    fn name(&self) -> DebugName {
        self.debug_name.clone()
    }

    fn flags(&self) -> SystemStateFlags {
        if self.is_main_thread {
            SystemStateFlags::DEFERRED | SystemStateFlags::NON_SEND
        } else {
            SystemStateFlags::DEFERRED
        }
    }

    fn default_system_sets(&self) -> Vec<InternedSystemSet> {
        let mut sets = vec![SystemSet::intern(&ModSystemSet::All)];
        if let Some(id) = self.mod_id {
            sets.push(SystemSet::intern(&ModSystemSet::Mod(id)));
        }
        sets
    }

    fn initialize(&mut self, world: &mut World) -> SystemAccess {
        self.this_run = world.change_tick();
        self.last_run = Tick::new(world.change_tick().get().wrapping_sub(Tick::MAX.get()));
        self.self_conflict = false;
        self.init_error = None;

        if !self.error_policy_explicit
            && let Some(cfg) = world.get_resource::<ModErrorPolicyConfig>()
        {
            self.error_policy = cfg.0;
        }

        if let Some(world_reg) = world.get_resource::<ModRegistry>() {
            let world_gen = world_reg.generation();
            if world_gen != self.cached_registry_gen {
                self.snapshot = world_reg.snapshot_arc();
                self.cached_registry_gen = world_gen;
            }
            self.registry = world_reg.clone();
        }

        match initialize_query_groups_in_world(world, &self.snapshot, &mut self.query_groups) {
            Ok(access_set) => {
                #[cfg(feature = "diagnostics")]
                let access_set = {
                    let mut access_set = access_set;
                    if let Some(col_id) =
                        world.components().component_id::<ModDiagnosticsCollector>()
                    {
                        access_set.add_resource_read(col_id);
                        self.diagnostics_collector_id = Some(col_id);
                    } else {
                        self.diagnostics_collector_id = None;
                    }
                    access_set
                };
                self.access = SystemAccess::Shared(access_set);
                let cell = world.as_unsafe_world_cell();
                for group in &mut self.query_groups {
                    group.update_archetypes(cell);
                }
                self.access.clone()
            }
            Err(err) => {
                self.enabled_flag.store(false, Ordering::Release);
                self.access = SystemAccess::None;
                if matches!(err, SystemInitError::SelfConflict { .. }) {
                    self.self_conflict = true;
                }
                let status = err.status_code();
                self.init_error = Some(err.clone());

                tracing::error!(
                    system = %self.name,
                    mod_id = ?self.mod_id,
                    error = %err,
                    "guest mod system failed initialization and was quarantined"
                );

                if let Some(id) = self.mod_id {
                    ModHost::record_fault_in_world(world, id);
                }

                let fault_msg = ModSystemFaulted {
                    mod_id: self.mod_id,
                    system_name: self.name.to_string(),
                    status,
                    panicked: false,
                    init_error: Some(err),
                };
                if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                    msgs.write(fault_msg);
                }

                self.access.clone()
            }
        }
    }

    unsafe fn run_unsafe(
        &mut self,
        _input: SystemIn<'_, Self>,
        world: UnsafeWorldCell,
    ) -> Result<Self::Out, RunSystemError> {
        if self.self_conflict
            || self.init_error.is_some()
            || !self.enabled_flag.load(Ordering::Acquire)
            || !self.is_active.load(Ordering::Acquire)
        {
            return Ok(());
        }

        // Refresh cached `Arc<RegistrySnapshot>` in O(1) only if `ModRegistry` generation advanced.
        let latest_gen = self.registry.latest_generation();
        if latest_gen != self.cached_registry_gen {
            self.snapshot = self.registry.latest_snapshot_arc();
            self.cached_registry_gen = latest_gen;
        }

        if let Some((GuestFnPtr(cond_fn), cond_ud)) = self.run_if {
            // SAFETY: `cond_fn` is a foreign `extern "C"` predicate belonging to an active loaded mod.
            let flag = unsafe { cond_fn(cond_ud.as_ptr()) };
            if !flag.get() {
                return Ok(());
            }
        }

        let change_tick = world.increment_change_tick();
        self.this_run = change_tick;
        let last_run = self.last_run;

        for group in &mut self.query_groups {
            group.update_archetypes(world);
        }

        // SAFETY: Scheduler verified access via `FilteredAccessSet`; no intra-system conflict exists.
        let storages = unsafe { world.storages() };
        let archetypes = world.archetypes();

        let count = self.query_groups.len().min(MAX_SYSTEM_QUERIES);
        let mut host_iters: [Option<HostIter<'_>>; MAX_SYSTEM_QUERIES] =
            [const { None }; MAX_SYSTEM_QUERIES];
        let mut query_views: [Option<RandomQueryView<'_>>; MAX_SYSTEM_QUERIES] =
            [None; MAX_SYSTEM_QUERIES];

        for (i, group) in self.query_groups.iter_mut().take(count).enumerate() {
            query_views[i] = Some(RandomQueryView {
                plan: &group.plan,
                is_dense: group.is_dense,
                matched_tables: &group.matched_tables,
                matched_archetypes: &group.matched_archetypes,
                has_query_state: group.query_state.is_some(),
            });
            let iter = if group.is_dense {
                HostIter::new_dense(
                    &storages.tables,
                    &group.matched_tables,
                    &group.plan,
                    last_run,
                    change_tick,
                )
            } else {
                HostIter::new_archetypal(
                    &storages.tables,
                    archetypes,
                    &storages.sparse_sets,
                    &group.matched_archetypes,
                    &group.plan,
                    &mut group.sparse_scratch,
                    last_run,
                    change_tick,
                )
            }
            .with_world(world)
            .with_msg_cursors(&mut group.msg_cursors);
            host_iters[i] = Some(iter);
        }

        let mut iter_ptrs: [*mut BamIter; MAX_SYSTEM_QUERIES] =
            [core::ptr::null_mut(); MAX_SYSTEM_QUERIES];
        for i in 0..count {
            if let Some(ref mut it) = host_iters[i] {
                iter_ptrs[i] = it.as_ffi_ptr();
            }
        }

        let mod_label = self.mod_namespace.as_deref().unwrap_or("unscoped");
        let sys_span = tracing::debug_span!(
            "bam_guest_system",
            mod_name = %mod_label,
            system = %self.name,
            chunks = tracing::field::Empty,
            entities = tracing::field::Empty,
            commands = tracing::field::Empty,
        );
        let _span_guard = sys_span.enter();

        #[cfg(feature = "diagnostics")]
        let start_time = std::time::Instant::now();

        let status = {
            let mut host_commands = HostCommands::new(
                &mut self.commands_storage,
                world.entity_allocator(),
                &self.snapshot,
            )
            .with_mod_context(self.mod_id, Some(Arc::clone(&self.is_active)))
            .with_registry(&self.registry);
            let commands_ptr = host_commands.as_ffi_ptr();
            let err_buf = core::mem::take(&mut self.last_error_buf);
            let mut sys_ctx = HostSystemCtx::new(
                Arc::clone(&self.snapshot),
                self.mod_namespace.clone(),
                err_buf,
                iter_ptrs,
                count,
                commands_ptr,
            )
            .with_world_and_queries(world, query_views, last_run, change_tick);
            let sys_ctx_ptr = sys_ctx.as_ffi_ptr();
            let GuestFnPtr(callback) = self.callback;
            // SAFETY: Calling foreign `extern "C"` callback with stack-scoped `HostSystemCtx`,
            // `HostIter`s, and `HostCommands` whose headers are poisoned when this block exits.
            let res = unsafe { callback(sys_ctx_ptr, self.user_data.as_ptr()) };
            self.last_error_buf = sys_ctx.reclaim_error_buffer();
            res
        };

        let mut total_chunks = 0usize;
        let mut total_entities = 0usize;
        for it in host_iters.iter().take(count).flatten() {
            let (c, e) = it.stats();
            total_chunks += c;
            total_entities += e;
        }
        drop(host_iters);

        let commands_queued = self.commands_storage.len();
        if !sys_span.is_disabled() {
            sys_span.record("chunks", total_chunks);
            sys_span.record("entities", total_entities);
            sys_span.record("commands", commands_queued);
        }

        let faulted = status == FfiResult::PANIC_CAUGHT || status.is_err();

        #[cfg(feature = "diagnostics")]
        if let Some(col_id) = self.diagnostics_collector_id {
            let cpu_time_us = start_time.elapsed().as_secs_f64() * 1_000_000.0;
            // SAFETY: Shared read access to `ModDiagnosticsCollector` (`col_id`) was declared in `FilteredAccessSet`.
            if let Some(ptr) = unsafe { world.get_resource_by_id(col_id) } {
                // SAFETY: `col_id` is `ModDiagnosticsCollector`'s `ComponentId`.
                let collector = unsafe { ptr.deref::<ModDiagnosticsCollector>() };
                collector.push(ModDiagnosticsSample {
                    mod_id: self.mod_id,
                    mod_name: self
                        .mod_namespace
                        .clone()
                        .unwrap_or_else(|| Arc::from("unscoped")),
                    system_name: Arc::clone(&self.name),
                    cpu_time_us,
                    commands_queued: if faulted { 0 } else { commands_queued },
                    faulted,
                });
            }
        }

        drop(_span_guard);
        self.last_run = change_tick;

        if faulted {
            return self.record_fault_and_apply_policy(status, status == FfiResult::PANIC_CAUGHT);
        }

        Ok(())
    }

    fn apply_deferred(&mut self, world: &mut World) {
        self.commands_storage.apply_to_world(world, &self.snapshot);
        if !self.fault_queue.is_empty() {
            self.fault_queue.apply(world);
        }
    }

    fn run(
        &mut self,
        input: SystemIn<'_, Self>,
        world: &mut World,
    ) -> Result<Self::Out, RunSystemError> {
        let res = self.run_without_applying_deferred(input, world);
        self.apply_deferred(world);
        res
    }

    fn queue_deferred(&mut self, mut world: DeferredWorld) {
        if !self.commands_storage.is_empty() {
            let mut taken = self.commands_storage.take_for_deferred_queue();
            let snap = Arc::clone(&self.snapshot);
            world.commands().queue(move |w: &mut World| {
                taken.apply_to_world(w, &snap);
            });
        }
        if !self.fault_queue.is_empty() {
            world.commands().append(&mut self.fault_queue);
        }
    }

    fn check_change_tick(&mut self, check: CheckChangeTicks) {
        self.last_run.check_tick(check);
        self.this_run.check_tick(check);
    }

    fn get_last_run(&self) -> Tick {
        self.last_run
    }

    fn set_last_run(&mut self, last_run: Tick) {
        self.last_run = last_run;
    }
}
