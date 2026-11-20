//! ABI `0.3.0` (`0x0003_0000`) host FFI extension entrypoints: structured diagnostics, schema and
//! layout introspection, iterator counting, system-handle registration, command-buffer checkpoints
//! and rollback, entity relationships, symbol interning, named object binding, ancestor cascades,
//! read-only parallel chunk iteration, schedule sets, and live component column migration.

use crate::ffi::register::{register_system_inner, validate_active_load_ctx};
use crate::handle_header::{
    decode_live_entity, ffi_try, validate_commands_mut, validate_host_ctx, validate_iter_mut,
    validate_non_empty_cstr,
};
use crate::loader::ModHost;
use crate::schedule::{
    ModNamedSystemSet, ModScheduleSetRegistry, ScheduleSetConfig, ScheduleSetMode,
    ensure_mod_schedule_set_pass_if_schedules_present, resolve_schedule_id_in_world,
};
use crate::system::error_policy::record_active_guest_fault;
use bevy_abi_mod_sys::{
    BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN,
    BAM_SCHEDULE_SET_FLAG_AFTER_WEAK, BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK, BamChunkTaskFn,
    BamCommands, BamComponent, BamHostCtx, BamIter, BamLayoutInfo, BamLoadCtx, BamRelationDesc,
    BamRowMigrateFn, BamScheduleSetDesc, BamSymbol, BamSystem, BamSystemDesc, BamSystemSet,
    ComponentDesc, ComponentUuid, FfiEntity, FfiResult, ScheduleId, StrView, read_descriptor,
};
use core::ffi::c_void;
use core::ptr::null;
use std::sync::Arc;

#[inline]
unsafe fn validate_str_view<'a>(view: StrView) -> Result<&'a str, FfiResult> {
    // SAFETY: Caller guarantees `view.ptr` references `view.len` readable bytes when non-null.
    unsafe { view.try_as_str() }
}

#[inline]
unsafe fn validate_non_empty_str_view<'a>(view: StrView) -> Result<&'a str, FfiResult> {
    // SAFETY: Forwarding under caller's initialized-bytes contract.
    let s = unsafe { validate_str_view(view) }?;
    if s.is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(s)
}

#[inline]
unsafe fn read_set_handles(ptr: *const BamSystemSet, count: u32) -> Vec<BamSystemSet> {
    let n = count as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: Caller verified `ptr` is non-null when `count > 0` and holds `count` readable handles.
        out.push(unsafe { core::ptr::read_unaligned(ptr.add(i)) });
    }
    out
}

/// Emits a structured guest diagnostic message via the host's `tracing` subscriber (`BAM_LOG_*`)
/// without clobbering `last_error`.
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - When `message.len > 0`, `message.ptr` must point to `message.len` initialized readable bytes.
pub unsafe extern "C" fn host_log_diagnostic(
    ctx: *const BamHostCtx,
    level: u32,
    message: StrView,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });
    // SAFETY: Caller guarantees `message.ptr` references `message.len` readable bytes when `len > 0`.
    let msg = ffi_try!(unsafe { validate_str_view(message) });

    let ns = host_ctx.default_namespace().unwrap_or("<host>");
    match level {
        BAM_LOG_TRACE => {
            tracing::trace!(target: "bevy_abi_mod::guest", namespace = ns, "{msg}");
        }
        BAM_LOG_DEBUG => {
            tracing::debug!(target: "bevy_abi_mod::guest", namespace = ns, "{msg}");
        }
        BAM_LOG_INFO => {
            tracing::info!(target: "bevy_abi_mod::guest", namespace = ns, "{msg}");
        }
        BAM_LOG_WARN => {
            tracing::warn!(target: "bevy_abi_mod::guest", namespace = ns, "{msg}");
        }
        BAM_LOG_ERROR => {
            tracing::error!(target: "bevy_abi_mod::guest", namespace = ns, "{msg}");
        }
        _ => return FfiResult::INVALID_ARGUMENT,
    }
    FfiResult::OK
}

/// Records a guest-reported fault message onto `ctx`'s per-context `last_error` buffer, attaches
/// structured fault context to any active [`crate::GuestSystem`] invocation on the current thread
/// (so it surfaces in `ModSystemFaulted`, `ModSystemError`, and Bevy `0.20`'s
/// `FallbackErrorHandler`), and emits a structured `tracing::error!` event.
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - When `message.len > 0`, `message.ptr` must point to `message.len` initialized readable bytes.
pub unsafe extern "C" fn host_report_guest_fault(
    ctx: *const BamHostCtx,
    message: StrView,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });
    // SAFETY: Caller guarantees `message.ptr` references `message.len` readable bytes when `len > 0`.
    let msg = ffi_try!(unsafe { validate_str_view(message) });

    host_ctx.set_last_error(msg);
    let ns = host_ctx.default_namespace().unwrap_or("<host>");
    if let Some(active) = record_active_guest_fault(ctx, msg) {
        let active_ns = active.mod_namespace.as_deref().unwrap_or(ns);
        tracing::error!(
            target: "bevy_abi_mod::guest_fault",
            namespace = active_ns,
            mod_id = ?active.mod_id,
            generation = active.generation,
            system = %active.system_name,
            "{msg}"
        );
    } else {
        tracing::error!(target: "bevy_abi_mod::guest_fault", namespace = ns, "{msg}");
    }
    FfiResult::OK
}

/// Resolves an existing registered component or resource handle by its deterministic 128-bit UUID.
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - `uuid` must point to a readable [`ComponentUuid`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_resolve_component_by_uuid(
    ctx: *const BamHostCtx,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });
    if uuid.is_null() {
        host_ctx.set_last_error("resolve_component_by_uuid requires a non-null `uuid` pointer");
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: `uuid` was checked non-null and caller guarantees readable `ComponentUuid` bytes.
    let parsed_uuid = unsafe { core::ptr::read_unaligned(uuid) };
    let Some(entry) = host_ctx.snapshot().get_by_uuid(&parsed_uuid) else {
        host_ctx.set_last_error(format!(
            "component or resource with UUID {parsed_uuid:?} not found"
        ));
        return FfiResult::NOT_FOUND;
    };

    // SAFETY: `out_component` was checked non-null and caller guarantees aligned writable storage.
    unsafe {
        *out_component = entry.handle;
    }
    FfiResult::OK
}

/// Retrieves ABI `0.3` layout metadata (`size`, `align`, `schema_fingerprint`, and host-retained
/// `BamFieldDesc` slice) for a registered [`BamComponent`] handle.
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - `out_info` must point to writable [`BamLayoutInfo`] storage, or null.
pub unsafe extern "C" fn host_component_layout_info(
    ctx: *const BamHostCtx,
    component: BamComponent,
    out_info: *mut BamLayoutInfo,
) -> FfiResult {
    if out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });

    let Some(entry) = host_ctx.snapshot().get_by_handle(component) else {
        host_ctx.set_last_error(format!(
            "component handle {} is not registered",
            component.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };

    let info = BamLayoutInfo {
        struct_size: core::mem::size_of::<BamLayoutInfo>() as u32,
        flags: entry.storage_flags,
        uuid: entry.uuid,
        schema_fingerprint: entry.schema_fingerprint,
        size: entry.size,
        align: entry.align,
        fields: if entry.fields.is_empty() {
            null()
        } else {
            entry.fields.as_ptr()
        },
        field_count: entry.fields.len(),
    };
    // SAFETY: `out_info` was checked non-null and caller guarantees writable `BamLayoutInfo` storage.
    unsafe {
        *out_info = info;
    }
    FfiResult::OK
}

/// Counts all entities matching an active [`crate::ffi::iter::HostIter`] without mutating or advancing cursor state.
///
/// # Safety
/// - `iter` must point to an active [`crate::ffi::iter::HostIter`] cast to [`BamIter`], or null.
/// - `out_count` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_iter_count(iter: *mut BamIter, out_count: *mut usize) -> FfiResult {
    if out_count.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });

    let count = host_iter.count_matching();
    // SAFETY: `out_count` was checked non-null and caller guarantees writable `usize` storage.
    unsafe {
        *out_count = count;
    }
    FfiResult::OK
}

/// Registers a unified [`BamSystemDesc`] and outputs its assigned [`BamSystem`] handle.
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
/// - `out_system` must point to writable [`BamSystem`] storage, or null.
pub unsafe extern "C" fn host_register_system_handle(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
    out_system: *mut BamSystem,
) -> FfiResult {
    if out_system.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller upholds `load_ctx` and `desc_ptr` FFI contract.
    let handle = ffi_try!(unsafe { register_system_inner(load_ctx, desc_ptr) });
    // SAFETY: `out_system` was checked non-null and caller guarantees writable `BamSystem` storage.
    unsafe {
        *out_system = handle;
    }
    FfiResult::OK
}

/// Captures an opaque checkpoint token from the active [`crate::ffi::commands::HostCommands`] queue.
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - `out_checkpoint` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_cmd_checkpoint(
    commands: *mut BamCommands,
    out_checkpoint: *mut usize,
) -> FfiResult {
    if out_checkpoint.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `out_checkpoint` was checked non-null and caller guarantees writable `usize` storage.
    unsafe {
        *out_checkpoint = cmds.checkpoint();
    }
    FfiResult::OK
}

/// Rolls back deferred commands queued on `commands` after `checkpoint`, running foreign
/// destructors for uncommitted dynamic component payloads.
///
/// # Safety
/// `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_rollback(
    commands: *mut BamCommands,
    checkpoint: usize,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    cmds.rollback_to_checkpoint(checkpoint)
}

/// Registers a typed entity relationship edge descriptor ([`BamRelationDesc`]) in `ModRegistry`
/// and the world's `ModRelationGraph`, returning its 1-based [`BamComponent`] handle.
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamRelationDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_relation(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamRelationDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    // SAFETY: Forwarding under caller's active `LoadContext`, readable descriptor, and writable output contract.
    unsafe { crate::ffi::relations::register_relation_impl(load_ctx, desc_ptr, out_component) }
}

/// Interns a non-empty UTF-8 symbol string into the world's monotonic [`crate::loader::BamSymbolTable`],
/// writing its stable [`BamSymbol`] handle (`BamSymbol(0)`, `BamSymbol(1)`, ...) into `*out_symbol`.
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - When `symbol.len > 0`, `symbol.ptr` must point to `symbol.len` readable bytes.
/// - `out_symbol` must point to writable [`BamSymbol`] storage, or null.
pub unsafe extern "C" fn host_intern_symbol(
    ctx: *const BamHostCtx,
    symbol: StrView,
    out_symbol: *mut BamSymbol,
) -> FfiResult {
    if out_symbol.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });
    // SAFETY: Caller guarantees `symbol.ptr` references `symbol.len` readable bytes when `len > 0`.
    let sym_str = match unsafe { validate_non_empty_str_view(symbol) } {
        Ok(s) => s,
        Err(err) => {
            host_ctx.set_last_error(format!("intern_symbol `symbol` validation failed ({err})"));
            return err;
        }
    };
    let handle = ffi_try!(crate::loader::symbols::intern_symbol_for_host_ctx(
        host_ctx, sym_str
    ));
    // SAFETY: `out_symbol` was checked non-null and caller guarantees writable `BamSymbol`.
    unsafe {
        *out_symbol = handle;
    }
    FfiResult::OK
}

/// Binds or rebinds a canonical UTF-8 object/prefab `name` (and its deterministic 128-bit UUID
/// `ComponentUuid::from_parts(name, 0, 0, 0)`) to `entity` in the world's persistent
/// [`crate::loader::ModNamedObjects`] registry.
///
/// Rejects [`FfiEntity::NULL`], `Entity::PLACEHOLDER`, or invalid entity bit patterns with
/// [`FfiResult::INVALID_ENTITY`].
///
/// # Safety
/// - `ctx` must point to an active [`crate::HostContext`] cast to [`BamHostCtx`], or null.
/// - When `name.len > 0`, `name.ptr` must point to `name.len` readable bytes.
pub unsafe extern "C" fn host_bind_named_object(
    ctx: *const BamHostCtx,
    name: StrView,
    entity: FfiEntity,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });
    // SAFETY: Caller guarantees `name.ptr` references `name.len` readable bytes when `len > 0`.
    let name_str = match unsafe { validate_non_empty_str_view(name) } {
        Ok(s) => s,
        Err(err) => {
            host_ctx.set_last_error(format!(
                "bind_named_object `name` validation failed ({err})"
            ));
            return err;
        }
    };
    let bevy_entity = match decode_live_entity(entity) {
        Ok(e) => e,
        Err(err) => {
            host_ctx.set_last_error("bind_named_object requires a valid non-null entity");
            return err;
        }
    };
    match crate::loader::symbols::bind_named_object_for_host_ctx(host_ctx, name_str, bevy_entity) {
        Ok(()) => FfiResult::OK,
        Err(err) => err,
    }
}

/// Walks upward from `entity`'s parent along `relation` (`BamComponent::INVALID` defaults to
/// Bevy's built-in `ChildOf`) with cycle guarding to locate the nearest ancestor owning the
/// component declared at `term_index`.
///
/// On match, writes the component pointer into `*out_ptr` and owning ancestor into `*out_owner`
/// and returns [`FfiResult::OK`]. When no ancestor carries the component, zeroes `*out_ptr` and
/// `*out_owner` and returns [`FfiResult::NOT_FOUND`].
///
/// # Safety
/// - `iter` must point to an active [`crate::ffi::iter::HostIter`] cast to [`BamIter`], or null.
/// - `out_ptr` must point to aligned writable `*const c_void` storage, or null.
/// - `out_owner` must point to aligned writable [`FfiEntity`] storage, or null.
pub unsafe extern "C" fn host_iter_up_cascade(
    iter: *mut BamIter,
    relation: BamComponent,
    entity: FfiEntity,
    term_index: usize,
    out_ptr: *mut *const c_void,
    out_owner: *mut FfiEntity,
) -> FfiResult {
    // SAFETY: Caller upholds `iter`, `out_ptr`, and `out_owner` FFI contract.
    unsafe {
        crate::ffi::hierarchy_ext::iter_up_cascade_impl(
            iter, relation, entity, term_index, out_ptr, out_owner,
        )
    }
}

/// Host-driven zero-arena read-only parallel chunk iteration across Bevy's `ComputeTaskPool`.
///
/// Rejects any query declaring a writable term (`WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`, or
/// `MESSAGE_WRITE`) with [`FfiResult::PERMISSION_DENIED`] and dispatches disjoint table/archetype
/// sub-ranges with zero per-worker command-arena allocations.
///
/// # Safety
/// - `iter` must point to an active [`crate::ffi::iter::HostIter`] cast to [`BamIter`], or null.
/// - If `chunk_fn` is `Some`, it must uphold [`BamChunkTaskFn`]'s thread-safety and nounwind
///   safety contract.
pub unsafe extern "C" fn host_par_for_each_chunk_readonly(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Caller upholds `iter`, `chunk_fn`, and `user_data` FFI contract.
    unsafe { crate::ffi::iter_ext::par_for_each_chunk_readonly_impl(iter, chunk_fn, user_data) }
}

/// Configures a decoupled ABI `0.3.0` schedule system-set container ([`BamSystemSet`]) mapped to
/// a mod- and generation-scoped [`ModNamedSystemSet`], recording its execution mode (`SEQUENCE`,
/// `SEQUENCE_WEAK`, or `PARALLEL`) and `before_sets` / `after_sets` ordering edges.
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamScheduleSetDesc`], or null.
/// - When `desc.before_count > 0` / `desc.after_count > 0`, `desc.before_sets` / `desc.after_sets`
///   must point to that many readable [`BamSystemSet`] handles.
/// - `out_set` must point to writable [`BamSystemSet`] storage, or null.
pub unsafe extern "C" fn host_configure_schedule_set(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamScheduleSetDesc,
    out_set: *mut BamSystemSet,
) -> FfiResult {
    if out_set.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = ffi_try!(unsafe { validate_active_load_ctx(load_ctx) });
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamScheduleSetDesc::MIN_V0_3_SIZE`.
    let desc = match unsafe { read_descriptor::<BamScheduleSetDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamScheduleSetDesc header ({err})"));
            return err;
        }
    };
    let status = desc.validate();
    if status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamScheduleSetDesc validation failed ({status})"));
        return status;
    }
    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let raw_name = match unsafe { validate_non_empty_cstr(desc.name) } {
        Ok(s) => s,
        Err(err) => {
            ctx.host_ctx.set_last_error(format!(
                "BamScheduleSetDesc `name` validation failed ({err})"
            ));
            return err;
        }
    };

    let default_mod_name = ctx
        .manifest_name
        .as_deref()
        .or_else(|| ctx.host_ctx.default_namespace())
        .unwrap_or("unscoped");
    let Some(parsed) = ModNamedSystemSet::parse_target(raw_name, default_mod_name) else {
        ctx.host_ctx.set_last_error(format!(
            "BamScheduleSetDesc `name` '{raw_name}' has invalid set target syntax"
        ));
        return FfiResult::INVALID_ARGUMENT;
    };

    if ctx.world_ptr.is_null() {
        ctx.host_ctx.set_last_error(
            "configure_schedule_set requires a LoadContext bound to an active World",
        );
        return FfiResult::INVALID_STATE;
    }

    // SAFETY: `desc.validate()` verified non-null pointers when counts > 0, and caller guarantees
    // `before_count` / `after_count` readable `BamSystemSet` handles.
    let before_sets = unsafe { read_set_handles(desc.before_sets, desc.before_count) };
    // SAFETY: Same as above for `after_sets`.
    let after_sets = unsafe { read_set_handles(desc.after_sets, desc.after_count) };

    // SAFETY: `ctx.world_ptr` was checked non-null and references the exclusively borrowed
    // `&mut World` for the duration of `bevy_abi_mod_init`.
    let world = unsafe { &mut *ctx.world_ptr };

    if resolve_schedule_id_in_world(world, desc.schedule).is_none() {
        ctx.host_ctx.set_last_error(format!(
            "configure_schedule_set schedule {} is not registered in ModScheduleMap",
            desc.schedule.0
        ));
        return FfiResult::NOT_FOUND;
    }

    let generation = world
        .get_resource::<ModHost>()
        .map(|h| h.mod_generation(parsed.mod_name.as_ref()))
        .unwrap_or(0);
    world.init_resource::<ModScheduleSetRegistry>();
    let handle = match world
        .resource_mut::<ModScheduleSetRegistry>()
        .configure_set(ScheduleSetConfig {
            mod_id: ctx.mod_id,
            mod_name: parsed.mod_name,
            set_name: parsed.system_name,
            generation,
            schedule: desc.schedule,
            mode: ScheduleSetMode::from_abi_flags(desc.flags),
            before_sets,
            after_sets,
            before_weak: (desc.flags & BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK) != 0,
            after_weak: (desc.flags & BAM_SCHEDULE_SET_FLAG_AFTER_WEAK) != 0,
        }) {
        Ok(h) => h,
        Err((err, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return err;
        }
    };

    ensure_mod_schedule_set_pass_if_schedules_present(world, desc.schedule);

    // SAFETY: `out_set` was checked non-null and caller guarantees aligned writable `BamSystemSet`.
    unsafe {
        *out_set = handle;
    }
    FfiResult::OK
}

/// Mounts a registered [`BamSystem`] handle (previously returned by `host_register_system_handle`)
/// into `schedule` and, when `set != BamSystemSet::INVALID`, into the configured [`BamSystemSet`]
/// container in mount order (enabling `.chain()` / `.chain_weak()` ordering for `SEQUENCE` /
/// `SEQUENCE_WEAK` containers).
///
/// # Safety
/// `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
pub unsafe extern "C" fn host_mount_system_in_schedule(
    load_ctx: *mut BamLoadCtx,
    system: BamSystem,
    schedule: ScheduleId,
    set: BamSystemSet,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = ffi_try!(unsafe { validate_active_load_ctx(load_ctx) });
    if !system.is_valid()
        || system.to_raw() == 0
        || (system.to_raw() as usize) > ctx.registered_systems.len()
    {
        ctx.host_ctx
            .set_last_error("mount_system_in_schedule requires a registered BamSystem handle");
        return FfiResult::INVALID_ARGUMENT;
    }
    if !schedule.is_valid() {
        ctx.host_ctx
            .set_last_error("mount_system_in_schedule received an invalid ScheduleId");
        return FfiResult::INVALID_ARGUMENT;
    }

    let sys_idx = (system.to_raw() - 1) as usize;
    let needs_world = set.is_valid() || schedule.is_custom();
    if needs_world && ctx.world_ptr.is_null() {
        ctx.host_ctx.set_last_error(
            "mount_system_in_schedule requires a LoadContext bound to an active World",
        );
        return FfiResult::INVALID_STATE;
    }

    let apply_schedule = |sys: &mut crate::GuestSystem, manifest_name: &Option<Arc<str>>| {
        sys.set_schedule(schedule);
        if let Some(ns) = manifest_name.as_ref()
            && sys.mod_namespace().is_none()
        {
            sys.set_mod_namespace(Some(Arc::clone(ns)));
        }
    };

    if !ctx.world_ptr.is_null() {
        // SAFETY: `ctx.world_ptr` was checked non-null and references the exclusively borrowed
        // `&mut World` for the duration of `bevy_abi_mod_init`.
        let world = unsafe { &mut *ctx.world_ptr };

        if resolve_schedule_id_in_world(world, schedule).is_none() {
            ctx.host_ctx.set_last_error(format!(
                "mount_system_in_schedule custom schedule {} is not registered in ModScheduleMap",
                schedule.0
            ));
            return FfiResult::NOT_FOUND;
        }

        let sys_mod_name: Arc<str> = ctx
            .manifest_name
            .clone()
            .or_else(|| {
                ctx.registered_systems[sys_idx]
                    .mod_namespace()
                    .map(Arc::from)
            })
            .unwrap_or_else(|| Arc::from("unscoped"));
        let sys_gen = world
            .get_resource::<ModHost>()
            .map(|h| h.mod_generation(sys_mod_name.as_ref()))
            .unwrap_or(0);
        let sys_name_arc = ctx.registered_systems[sys_idx].system_name_arc();
        let sys_own_set =
            ModNamedSystemSet::with_generation(Arc::clone(&sys_mod_name), sys_name_arc, sys_gen);

        let prev_schedule = ctx.registered_systems[sys_idx].schedule();
        let schedule_changed = prev_schedule != schedule;

        // Pre-validate `set` before mutating `sys` or prior mounts so failures leave state intact.
        if set.is_valid() {
            let Some(entry) = world
                .get_resource::<ModScheduleSetRegistry>()
                .and_then(|reg| reg.get(set))
            else {
                ctx.host_ctx.set_last_error(format!(
                    "mount_system_in_schedule set handle {} is not registered",
                    set.0
                ));
                return FfiResult::NOT_FOUND;
            };
            if entry.schedule != schedule {
                ctx.host_ctx.set_last_error(format!(
                    "mount_system_in_schedule set handle {} belongs to schedule {}, not schedule {}",
                    set.0, entry.schedule.0, schedule.0
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
        }

        if schedule_changed {
            let old_sets: Vec<BamSystemSet> = ctx.registered_systems[sys_idx]
                .mounted_schedule_sets()
                .to_vec();
            if let Some(mut reg) = world.get_resource_mut::<ModScheduleSetRegistry>() {
                for old_h in &old_sets {
                    if let Some(old_entry) = reg.get(*old_h) {
                        let old_canonical = old_entry.canonical_name();
                        ctx.registered_systems[sys_idx].remove_in_set(&old_canonical);
                    }
                }
                reg.unmount_system_from_schedule(prev_schedule, &sys_own_set);
            }
            ctx.registered_systems[sys_idx].clear_mounted_schedule_sets();
        }

        let sys = &mut ctx.registered_systems[sys_idx];
        apply_schedule(sys, &ctx.manifest_name);

        if set.is_valid() {
            let canonical_set_name = match world
                .resource_mut::<ModScheduleSetRegistry>()
                .mount_system(set, schedule, sys_own_set)
            {
                Ok(entry) => entry.canonical_name(),
                Err((err, msg)) => {
                    ctx.host_ctx.set_last_error(msg);
                    return err;
                }
            };
            sys.add_in_set(canonical_set_name);
            sys.add_mounted_schedule_set(set);
            ensure_mod_schedule_set_pass_if_schedules_present(world, schedule);
        }
        return FfiResult::OK;
    }

    apply_schedule(&mut ctx.registered_systems[sys_idx], &ctx.manifest_name);
    FfiResult::OK
}

/// Enqueues deferred insertion of a typed entity relationship edge `(entity --relation--> target)`.
///
/// # Safety
/// - `commands` must point to an active [`crate::HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_size > 0`, `data` must point to at least `data_size` initialized readable bytes.
pub unsafe extern "C" fn host_cmd_insert_relation(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
    data: *const c_void,
    data_size: usize,
) -> FfiResult {
    // SAFETY: Forwarding under caller's active `HostCommands` and readable `data` buffer contract.
    unsafe {
        crate::ffi::relations::cmd_insert_relation_impl(
            commands, entity, relation, target, data, data_size,
        )
    }
}

/// Enqueues deferred removal of a typed entity relationship edge `(entity --relation--> target)`.
///
/// # Safety
/// `commands` must point to an active [`crate::HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_remove_relation(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
) -> FfiResult {
    // SAFETY: Forwarding under caller's active `HostCommands` contract.
    unsafe { crate::ffi::relations::cmd_remove_relation_impl(commands, entity, relation, target) }
}

/// Migrates live ECS rows of dynamic `AbiPod` component `component` from `old_fingerprint` to
/// `new_desc_ptr` using contiguous per-table scratch buffers and atomically remaps `component`
/// in [`crate::ModRegistry`] (zero-copy fast path when `schema_fingerprint` and layout are unchanged).
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `new_desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - If `migrate_fn` is `Some`, it must uphold [`BamRowMigrateFn`]'s nounwind safety contract.
pub unsafe extern "C" fn host_migrate_component_column(
    load_ctx: *mut BamLoadCtx,
    component: BamComponent,
    old_fingerprint: u64,
    new_desc_ptr: *const ComponentDesc,
    migrate_fn: Option<BamRowMigrateFn>,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = ffi_try!(unsafe { validate_active_load_ctx(load_ctx) });
    if !component.is_valid() || ctx.registry.snapshot().get_by_handle(component).is_none() {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column component handle {} is not registered",
            component.0
        ));
        return FfiResult::INVALID_COMPONENT;
    }
    // SAFETY: `read_descriptor` checks null and `struct_size >= ComponentDesc::MIN_V0_2_SIZE`.
    let new_desc = match unsafe { read_descriptor::<ComponentDesc>(new_desc_ptr.cast::<c_void>()) }
    {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid migration ComponentDesc header ({err})"));
            return err;
        }
    };
    let status = new_desc.validate();
    if status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("migration ComponentDesc invalid ({status})"));
        return status;
    }
    let Some(migrate_cb) = migrate_fn else {
        ctx.host_ctx
            .set_last_error("migrate_component_column requires a non-null `migrate_fn`");
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: `new_desc` passed `ComponentDesc::validate()` and `migrate_cb` satisfies caller contract.
    unsafe {
        crate::loader::schema_migration::migrate_component_column_ffi(
            ctx,
            component,
            old_fingerprint,
            &new_desc,
            migrate_cb,
            user_data,
        )
    }
}

/// Retains a single place checking the `host_cmd_*` imports used in unit tests cleanly while
/// enforcing `host_cmd_insert` and `host_cmd_spawn_empty` integration paths.
#[cfg(test)]
#[path = "../../tests/unit/v03_ext_tests.rs"]
mod tests;
