//! ABI `0.3.0` (`0x0003_0000`) host FFI extension entrypoints: structured diagnostics, schema and
//! layout introspection, iterator counting, system-handle registration, command-buffer checkpoints
//! and rollback, plus reserved ABI `0.3.0` relation and scheduling slots.

use crate::ffi::commands::HostCommands;
use crate::ffi::iter::HostIter;
use crate::ffi::register::{register_system_inner, validate_active_load_ctx};
use crate::handle_header::{
    COMMANDS_MAGIC, HOST_CTX_MAGIC, HostContext, ITER_MAGIC, validate_handle_mut,
    validate_handle_ref,
};
use bevy_abi_mod_sys::{
    BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN, BamChunkTaskFn,
    BamCommands, BamComponent, BamHostCtx, BamIter, BamLayoutInfo, BamLoadCtx, BamRelationDesc,
    BamRowMigrateFn, BamScheduleSetDesc, BamSymbol, BamSystem, BamSystemDesc, BamSystemSet,
    ComponentDesc, ComponentUuid, FfiEntity, FfiResult, ScheduleId, StrView, read_descriptor,
};
use bevy_ecs::entity::Entity;
use core::ffi::{CStr, c_char, c_void};
use core::ptr::null;

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
unsafe fn validate_non_empty_cstr<'a>(ptr: *const c_char) -> Result<&'a str, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: Caller guarantees `ptr` points to a valid NUL-terminated C string.
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| FfiResult::INVALID_UTF8)?;
    if s.is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(s)
}

#[inline]
fn validate_non_null_entity(entity: FfiEntity) -> Result<Entity, FfiResult> {
    if entity.is_null() {
        return Err(FfiResult::INVALID_ENTITY);
    }
    let Some(e) = Entity::try_from_bits(entity.to_bits()) else {
        return Err(FfiResult::INVALID_ENTITY);
    };
    if e == Entity::PLACEHOLDER {
        return Err(FfiResult::INVALID_ENTITY);
    }
    Ok(e)
}

/// Emits a structured guest diagnostic message via the host's `tracing` subscriber (`BAM_LOG_*`)
/// without clobbering `last_error`.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - When `message.len > 0`, `message.ptr` must point to `message.len` initialized readable bytes.
pub unsafe extern "C" fn host_log_diagnostic(
    ctx: *const BamHostCtx,
    level: u32,
    message: StrView,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: Caller guarantees `message.ptr` references `message.len` readable bytes when `len > 0`.
    let Ok(msg) = (unsafe { validate_str_view(message) }) else {
        // SAFETY: Re-evaluate to return exact error code (`NULL_POINTER`, `INVALID_ARGUMENT`, `INVALID_UTF8`).
        return unsafe { validate_str_view(message) }.unwrap_err();
    };

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

/// Records a guest-reported fault message onto `ctx`'s per-context `last_error` buffer and emits
/// a structured `tracing::error!` event.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - When `message.len > 0`, `message.ptr` must point to `message.len` initialized readable bytes.
pub unsafe extern "C" fn host_report_guest_fault(
    ctx: *const BamHostCtx,
    message: StrView,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: Caller guarantees `message.ptr` references `message.len` readable bytes when `len > 0`.
    let msg = match unsafe { validate_str_view(message) } {
        Ok(s) => s,
        Err(err) => return err,
    };

    host_ctx.set_last_error(msg);
    let ns = host_ctx.default_namespace().unwrap_or("<host>");
    tracing::error!(target: "bevy_abi_mod::guest_fault", namespace = ns, "{msg}");
    FfiResult::OK
}

/// Resolves an existing registered component or resource handle by its deterministic 128-bit UUID.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
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
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
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
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
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
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

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

/// Counts all entities matching an active [`HostIter`] without mutating or advancing cursor state.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_count` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_iter_count(iter: *mut BamIter, out_count: *mut usize) -> FfiResult {
    if out_count.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };

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
    match unsafe { register_system_inner(load_ctx, desc_ptr) } {
        Ok(handle) => {
            // SAFETY: `out_system` was checked non-null and caller guarantees writable `BamSystem` storage.
            unsafe {
                *out_system = handle;
            }
            FfiResult::OK
        }
        Err(err) => err,
    }
}

/// Captures an opaque checkpoint token from the active [`HostCommands`] queue.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `out_checkpoint` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_cmd_checkpoint(
    commands: *mut BamCommands,
    out_checkpoint: *mut usize,
) -> FfiResult {
    if out_checkpoint.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let token = cmds.checkpoint();
    // SAFETY: `out_checkpoint` was checked non-null and caller guarantees writable `usize` storage.
    unsafe {
        *out_checkpoint = token;
    }
    FfiResult::OK
}

/// Rolls back deferred commands queued on `commands` after `checkpoint`, running foreign
/// destructors for uncommitted dynamic component payloads.
///
/// # Safety
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_rollback(
    commands: *mut BamCommands,
    checkpoint: usize,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
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

/// Reserved ABI `0.3.0` slot for symbol table string interning (`BamSymbol`)
/// (validates inputs and currently returns [`FfiResult::UNSUPPORTED`]).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
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
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: Caller guarantees `symbol.ptr` references `symbol.len` readable bytes when `len > 0`.
    if let Err(err) = unsafe { validate_non_empty_str_view(symbol) } {
        host_ctx.set_last_error(format!("intern_symbol `symbol` validation failed ({err})"));
        return err;
    }
    host_ctx.set_last_error("intern_symbol is reserved in ABI 0.3.0 and not yet implemented");
    FfiResult::UNSUPPORTED
}

/// Reserved ABI `0.3.0` slot for canonical entity name binding
/// (validates inputs and currently returns [`FfiResult::UNSUPPORTED`]).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - When `name.len > 0`, `name.ptr` must point to `name.len` readable bytes.
pub unsafe extern "C" fn host_bind_named_object(
    ctx: *const BamHostCtx,
    name: StrView,
    entity: FfiEntity,
) -> FfiResult {
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: Caller guarantees `name.ptr` references `name.len` readable bytes when `len > 0`.
    if let Err(err) = unsafe { validate_non_empty_str_view(name) } {
        host_ctx.set_last_error(format!(
            "bind_named_object `name` validation failed ({err})"
        ));
        return err;
    }
    if let Err(err) = validate_non_null_entity(entity) {
        host_ctx.set_last_error("bind_named_object requires a valid non-null entity");
        return err;
    }
    host_ctx.set_last_error("bind_named_object is reserved in ABI 0.3.0 and not yet implemented");
    FfiResult::UNSUPPORTED
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
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
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
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
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

/// Reserved ABI `0.3.0` slot for schedule system-set configuration (`BamSystemSet`)
/// (validates inputs and currently returns [`FfiResult::UNSUPPORTED`]).
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamScheduleSetDesc`], or null.
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
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
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
    if let Err(err) = unsafe { validate_non_empty_cstr(desc.name) } {
        ctx.host_ctx.set_last_error(format!(
            "BamScheduleSetDesc `name` validation failed ({err})"
        ));
        return err;
    }
    ctx.host_ctx
        .set_last_error("configure_schedule_set is reserved in ABI 0.3.0 and not yet implemented");
    FfiResult::UNSUPPORTED
}

/// Reserved ABI `0.3.0` slot for mounting a [`BamSystem`] into a schedule and [`BamSystemSet`]
/// (validates inputs and currently returns [`FfiResult::UNSUPPORTED`]).
///
/// # Safety
/// `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
pub unsafe extern "C" fn host_mount_system_in_schedule(
    load_ctx: *mut BamLoadCtx,
    system: BamSystem,
    schedule: ScheduleId,
    _set: BamSystemSet,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
    if !system.is_valid() || (system.to_raw() as usize) > ctx.registered_systems.len() {
        ctx.host_ctx
            .set_last_error("mount_system_in_schedule requires a registered BamSystem handle");
        return FfiResult::INVALID_ARGUMENT;
    }
    if !schedule.is_valid() {
        ctx.host_ctx
            .set_last_error("mount_system_in_schedule received an invalid ScheduleId");
        return FfiResult::INVALID_ARGUMENT;
    }
    ctx.host_ctx.set_last_error(
        "mount_system_in_schedule is reserved in ABI 0.3.0 and not yet implemented",
    );
    FfiResult::UNSUPPORTED
}

/// Enqueues deferred insertion of a typed entity relationship edge `(entity --relation--> target)`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
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
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_remove_relation(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
) -> FfiResult {
    // SAFETY: Forwarding under caller's active `HostCommands` contract.
    unsafe { crate::ffi::relations::cmd_remove_relation_impl(commands, entity, relation, target) }
}

/// Reserved ABI `0.3.0` slot for live schema-fingerprint component column migration
/// (validates inputs and currently returns [`FfiResult::UNSUPPORTED`]).
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `new_desc_ptr` must point to a readable [`ComponentDesc`], or null.
pub unsafe extern "C" fn host_migrate_component_column(
    load_ctx: *mut BamLoadCtx,
    component: BamComponent,
    _old_fingerprint: u64,
    new_desc_ptr: *const ComponentDesc,
    migrate_fn: Option<BamRowMigrateFn>,
    _user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
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
    if migrate_fn.is_none() {
        ctx.host_ctx
            .set_last_error("migrate_component_column requires a non-null `migrate_fn`");
        return FfiResult::NULL_POINTER;
    }
    ctx.host_ctx.set_last_error(
        "migrate_component_column is reserved in ABI 0.3.0 and not yet implemented",
    );
    FfiResult::UNSUPPORTED
}

/// Retains a single place checking the `host_cmd_*` imports used in unit tests cleanly while
/// enforcing `host_cmd_insert` and `host_cmd_spawn_empty` integration paths.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::commands::{
        HostCommandsStorage, host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty,
    };
    use crate::ffi::iter::{QueryPlan, QueryTermMeta, host_iter_next_chunk};
    use crate::ffi::register::{LoadContext, host_register_component};
    use crate::loader::ModId;
    use crate::registry::{ComponentOptions, ModRegistry};
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{AccessMode, BamFieldDesc, BamFieldKind, FfiBool, ModComponent as _};
    use bevy_ecs::change_detection::Tick;
    use bevy_ecs::prelude::Component;
    use bevy_ecs::world::World;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[repr(C)]
    #[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
    #[mod_component(name = "v03_test::Stats")]
    struct TestStats {
        hp: i32,
        speed: f32,
    }

    #[test]
    fn diagnostics_uuid_and_layout_introspection_work() {
        let mut world = World::new();
        let comp_id = world.register_component::<TestStats>();
        let mut registry = ModRegistry::new();
        let stats_handle = registry
            .register_component::<TestStats>(comp_id, ComponentOptions::default())
            .expect("register TestStats");
        let snap = registry.snapshot_arc();
        let host_ctx = HostContext::new(snap).with_namespace(Some(Arc::from("v03_test")));

        // SAFETY: `host_ctx` and local output buffers are valid and aligned for every FFI call below.
        unsafe {
            // 1. `log_diagnostic` must emit without overwriting `last_error`.
            host_ctx.set_last_error("prior error");
            let ok_res = host_log_diagnostic(
                host_ctx.as_ffi_ptr(),
                BAM_LOG_INFO,
                StrView::from_str("hello from guest"),
            );
            assert_eq!(ok_res, FfiResult::OK);
            assert_eq!(host_ctx.last_error_message(), "prior error");
            assert_eq!(
                host_log_diagnostic(host_ctx.as_ffi_ptr(), 99, StrView::from_str("bad level")),
                FfiResult::INVALID_ARGUMENT
            );

            // 2. `report_guest_fault` records into `last_error`.
            assert_eq!(
                host_report_guest_fault(host_ctx.as_ffi_ptr(), StrView::from_str("boom fault")),
                FfiResult::OK
            );
            assert_eq!(host_ctx.last_error_message(), "boom fault");

            // 3. `resolve_component_by_uuid`
            let mut resolved = BamComponent::INVALID;
            let uuid = TestStats::UUID;
            assert_eq!(
                host_resolve_component_by_uuid(host_ctx.as_ffi_ptr(), &uuid, &raw mut resolved),
                FfiResult::OK
            );
            assert_eq!(resolved, stats_handle);

            let missing_uuid = ComponentUuid::from_canonical_name("v03_test::DoesNotExist");
            assert_eq!(
                host_resolve_component_by_uuid(
                    host_ctx.as_ffi_ptr(),
                    &missing_uuid,
                    &raw mut resolved,
                ),
                FfiResult::NOT_FOUND
            );

            // 4. `component_layout_info` for static host `#[derive(ModComponent)]`
            let mut layout = BamLayoutInfo::empty();
            assert_eq!(
                host_component_layout_info(host_ctx.as_ffi_ptr(), stats_handle, &raw mut layout),
                FfiResult::OK
            );
            assert_eq!(layout.uuid, TestStats::UUID);
            assert_eq!(layout.schema_fingerprint, TestStats::SCHEMA_FINGERPRINT);
            assert_eq!(layout.size, core::mem::size_of::<TestStats>());
            assert_eq!(layout.align, core::mem::align_of::<TestStats>());
            assert_eq!(layout.field_count, 2);
            let fields_slice = core::slice::from_raw_parts(layout.fields, layout.field_count);
            assert_eq!(fields_slice[0].name.as_str().unwrap(), "hp");
            assert_eq!(fields_slice[0].kind, BamFieldKind::I32);
            assert_eq!(fields_slice[1].name.as_str().unwrap(), "speed");
            assert_eq!(fields_slice[1].kind, BamFieldKind::F32);

            // 5. Dynamic component with owned copied `BamFieldDesc` survives source buffer drop
            let mut load_ctx = LoadContext::with_manifest_name(
                registry,
                Arc::new(AtomicBool::new(true)),
                (&raw mut world).cast::<World>(),
                Some(ModId(3)),
                Some(Arc::from("dyn_mod")),
            );
            let dyn_handle = {
                let local_name = String::from("mana");
                let local_tag = String::from("f32");
                let local_fields = [BamFieldDesc::new(
                    BamFieldKind::F32,
                    StrView::from_str(&local_name),
                    StrView::from_str(&local_tag),
                    0,
                    4,
                    4,
                )];
                let desc = ComponentDesc::new(
                    c"DynMana".as_ptr(),
                    ComponentUuid::from_parts("dyn_mod::DynMana", 4, 4, 0),
                    4,
                    4,
                )
                .with_schema_fingerprint(0xDEAD_BEEF_CAFE_BABEu64)
                .with_fields(local_fields.as_ptr(), local_fields.len());
                let mut out = BamComponent::INVALID;
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc, &raw mut out),
                    FfiResult::OK
                );
                out
            };
            let mut dyn_layout = BamLayoutInfo::empty();
            assert_eq!(
                host_component_layout_info(
                    load_ctx.host_ctx.as_ffi_ptr(),
                    dyn_handle,
                    &raw mut dyn_layout,
                ),
                FfiResult::OK
            );
            assert_eq!(dyn_layout.schema_fingerprint, 0xDEAD_BEEF_CAFE_BABEu64);
            assert_eq!(dyn_layout.field_count, 1);
            let dyn_fields = core::slice::from_raw_parts(dyn_layout.fields, dyn_layout.field_count);
            assert_eq!(dyn_fields[0].name.as_str().unwrap(), "mana");
            assert_eq!(dyn_fields[0].type_tag.as_str().unwrap(), "f32");
            load_ctx.finish_init();
        }
    }

    #[test]
    fn iter_count_and_register_system_handle_work() {
        let mut world = World::new();
        let comp_id = world.register_component::<TestStats>();
        world.spawn(TestStats { hp: 10, speed: 1.0 });
        world.spawn(TestStats { hp: 20, speed: 2.0 });
        world.spawn(TestStats { hp: 30, speed: 3.0 });

        let mut registry = ModRegistry::new();
        let stats_handle = registry
            .register_component::<TestStats>(comp_id, ComponentOptions::default())
            .expect("register TestStats");

        let plan = QueryPlan::from_terms(vec![QueryTermMeta {
            component: stats_handle,
            component_id: comp_id,
            mode: AccessMode::READ,
            uuid: TestStats::UUID,
            size: core::mem::size_of::<TestStats>(),
            align: core::mem::align_of::<TestStats>(),
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        }]);
        let table_id = world
            .archetypes()
            .iter()
            .find(|a| a.contains(comp_id))
            .expect("archetype")
            .table_id();
        let matched = [table_id];
        let mut iter = HostIter::new_dense(
            &world.storages().tables,
            &matched,
            &plan,
            Tick::new(0),
            Tick::new(1),
        );
        let mut count = 0usize;
        let mut chunk_len = 0usize;
        // SAFETY: `iter` and local stack outputs are valid and aligned.
        unsafe {
            assert_eq!(
                host_iter_count(iter.as_ffi_ptr(), &raw mut count),
                FfiResult::OK
            );
            assert_eq!(count, 3);
            // Non-destructive: `next_chunk` still yields all 3 entities.
            assert_eq!(
                host_iter_next_chunk(iter.as_ffi_ptr(), &raw mut chunk_len),
                FfiBool::TRUE
            );
            assert_eq!(chunk_len, 3);
            assert_eq!(
                host_iter_count(iter.as_ffi_ptr(), &raw mut count),
                FfiResult::OK
            );
            assert_eq!(count, 3);
        }
        drop(iter);

        // `register_system_handle` returns a valid 1-based handle.
        unsafe extern "C" fn noop_sys(
            _ctx: *mut bevy_abi_mod_sys::BamSystemCtx,
            _ud: *mut c_void,
        ) -> FfiResult {
            FfiResult::OK
        }
        let mut load_ctx = LoadContext::with_manifest_name(
            registry,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let sys_desc = BamSystemDesc::new(
            c"v03_test::count_sys".as_ptr(),
            null(),
            0,
            noop_sys,
            core::ptr::null_mut(),
        );
        let mut sys_handle = BamSystem::INVALID;
        // SAFETY: `load_ctx`, `sys_desc`, and `sys_handle` are valid aligned pointers.
        unsafe {
            assert_eq!(
                host_register_system_handle(load_ctx.as_ffi_ptr(), &sys_desc, &raw mut sys_handle),
                FfiResult::OK
            );
        }
        assert_eq!(sys_handle, BamSystem::from_raw(1));
        load_ctx.finish_init();
    }

    static DROP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn test_dyn_drop(_ptr: *mut c_void) {
        DROP_COUNTER.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn cmd_checkpoint_and_rollback_drop_uncommitted_payloads_once() {
        let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        DROP_COUNTER.store(0, Ordering::SeqCst);
        let mut world = World::new();
        let mut registry = ModRegistry::new();
        let dyn_uuid = ComponentUuid::from_parts("rollback_test::DynBox", 8, 8, 0);
        let desc = ComponentDesc::new(c"rollback_test::DynBox".as_ptr(), dyn_uuid, 8, 8)
            .with_drop(test_dyn_drop);

        let mut dyn_handle = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_manifest_name(
                registry,
                Arc::new(AtomicBool::new(true)),
                (&raw mut world).cast::<World>(),
                None,
                None,
            );
            // SAFETY: `load_ctx`, `desc`, and `dyn_handle` are valid aligned pointers.
            unsafe {
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc, &raw mut dyn_handle),
                    FfiResult::OK
                );
            }
            registry = load_ctx.registry.clone();
            load_ctx.finish_init();
        }

        let e_keep = world.spawn_empty().id();
        let snap = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let cmd_ptr = cmds.as_ffi_ptr();

        let keep_val = 111u64;
        let roll_val = 222u64;
        let batch_val = 333u64;
        let batch_comps = [dyn_handle];
        let batch_ptrs = [(&raw const batch_val).cast::<u8>()];
        let mut cp = 0usize;
        let mut temp_ent = FfiEntity::NULL;
        let mut batch_ent = FfiEntity::NULL;

        // SAFETY: `cmd_ptr` and all stack component buffers and out-pointers are valid for the active `HostCommands`.
        unsafe {
            assert_eq!(
                host_cmd_insert(
                    cmd_ptr,
                    FfiEntity::from_bits(e_keep.to_bits()),
                    dyn_handle,
                    (&raw const keep_val).cast::<u8>(),
                    8,
                ),
                FfiResult::OK
            );
            assert_eq!(host_cmd_checkpoint(cmd_ptr, &raw mut cp), FfiResult::OK);
            assert_eq!(cp, 1);

            assert_eq!(
                host_cmd_spawn_empty(cmd_ptr, &raw mut temp_ent),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert(
                    cmd_ptr,
                    temp_ent,
                    dyn_handle,
                    (&raw const roll_val).cast::<u8>(),
                    8,
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn(
                    cmd_ptr,
                    batch_comps.as_ptr(),
                    batch_ptrs.as_ptr(),
                    1,
                    &raw mut batch_ent,
                ),
                FfiResult::OK
            );

            // Rolling back out-of-range checkpoint fails without mutating queue.
            assert_eq!(host_cmd_rollback(cmd_ptr, cp + 99), FfiResult::OUT_OF_RANGE);

            // Roll back to `cp`: drops `roll_val` and `batch_val` (2 payloads), leaves `keep_val` intact.
            assert_eq!(host_cmd_rollback(cmd_ptr, cp), FfiResult::OK);
        }
        assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 2);
        assert_eq!(cmds.checkpoint(), 1);
        drop(cmds);

        storage.apply_to_world(&mut world, &snap);
        assert!(
            world
                .get_entity(Entity::from_bits(batch_ent.to_bits()))
                .is_err()
        );
        assert!(
            world
                .get_entity(Entity::from_bits(temp_ent.to_bits()))
                .is_err()
        );
        let dyn_comp_id = snap.get_by_handle(dyn_handle).unwrap().component_id;
        assert!(world.get_entity(e_keep).unwrap().contains_id(dyn_comp_id));
        assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 2);
        assert!(world.despawn(e_keep));
        assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn reserved_v03_slots_validate_arguments_before_returning_unsupported() {
        let mut world = World::new();
        let comp_id = world.register_component::<TestStats>();
        let mut registry = ModRegistry::new();
        let stats_handle = registry
            .register_component::<TestStats>(comp_id, ComponentOptions::default())
            .expect("register TestStats");
        let live_ent = FfiEntity::from_bits(world.spawn_empty().id().to_bits());
        let mut load_ctx = LoadContext::with_manifest_name(
            registry,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let host_ptr = load_ctx.host_ctx.as_ffi_ptr();

        let rel_uuid = ComponentUuid::from_canonical_name("v03_test::Rel");
        let rel_desc = BamRelationDesc::new(c"v03_test::Rel".as_ptr(), rel_uuid);
        let mut rel_out = BamComponent::INVALID;
        let mut sym = BamSymbol::INVALID;
        let set_desc = BamScheduleSetDesc::new(c"v03_test::Set".as_ptr());
        let mut set_out = BamSystemSet::INVALID;

        unsafe extern "C" fn stub_migrate(
            _src: *const c_void,
            _dst: *mut c_void,
            _ud: *mut c_void,
        ) -> FfiResult {
            FfiResult::OK
        }
        let mig_desc = ComponentDesc::new(c"v03_test::Stats".as_ptr(), TestStats::UUID, 8, 4);

        // SAFETY: Valid `load_ctx`, `host_ctx`, descriptors, and stack out-pointers; intentional
        // null descriptors test FFI null-pointer rejection.
        unsafe {
            assert_eq!(
                host_register_relation(load_ctx.as_ffi_ptr(), &rel_desc, &raw mut rel_out),
                FfiResult::OK
            );
            assert!(rel_out.is_valid());
            assert_eq!(
                host_register_relation(load_ctx.as_ffi_ptr(), null(), &raw mut rel_out),
                FfiResult::NULL_POINTER
            );

            assert_eq!(
                host_intern_symbol(host_ptr, StrView::from_str(""), &raw mut sym),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_intern_symbol(host_ptr, StrView::from_str("hp"), &raw mut sym),
                FfiResult::UNSUPPORTED
            );

            assert_eq!(
                host_bind_named_object(host_ptr, StrView::from_str("hero"), FfiEntity::NULL),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_bind_named_object(host_ptr, StrView::from_str("hero"), live_ent),
                FfiResult::UNSUPPORTED
            );

            assert_eq!(
                host_configure_schedule_set(load_ctx.as_ffi_ptr(), &set_desc, &raw mut set_out),
                FfiResult::UNSUPPORTED
            );
            assert_eq!(
                host_mount_system_in_schedule(
                    load_ctx.as_ffi_ptr(),
                    BamSystem::INVALID,
                    ScheduleId::UPDATE,
                    BamSystemSet::INVALID,
                ),
                FfiResult::INVALID_ARGUMENT
            );

            assert_eq!(
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    stats_handle,
                    TestStats::SCHEMA_FINGERPRINT,
                    &mig_desc,
                    None,
                    core::ptr::null_mut(),
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    stats_handle,
                    TestStats::SCHEMA_FINGERPRINT,
                    &mig_desc,
                    Some(stub_migrate),
                    core::ptr::null_mut(),
                ),
                FfiResult::UNSUPPORTED
            );
        }

        let mut storage = HostCommandsStorage::new();
        let snap = load_ctx.registry.snapshot_arc();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: `cmds` is active and valid on the stack; intentional null `data` with size 4 tests validation.
        unsafe {
            assert_eq!(
                host_cmd_insert_relation(
                    cmds.as_ffi_ptr(),
                    live_ent,
                    stats_handle,
                    live_ent,
                    null(),
                    4,
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_insert_relation(
                    cmds.as_ffi_ptr(),
                    live_ent,
                    stats_handle,
                    live_ent,
                    null(),
                    0,
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_insert_relation(cmds.as_ffi_ptr(), live_ent, rel_out, live_ent, null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_remove_relation(cmds.as_ffi_ptr(), live_ent, stats_handle, live_ent),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_remove_relation(cmds.as_ffi_ptr(), live_ent, rel_out, live_ent),
                FfiResult::OK
            );
        }

        load_ctx.finish_init();
    }
}
