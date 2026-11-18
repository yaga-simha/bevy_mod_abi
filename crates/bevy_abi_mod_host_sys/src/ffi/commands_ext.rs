//! Extended deferred entity command entrypoints (`cmd_spawn_persistent`, `cmd_spawn_batch`,
//! `cmd_despawn_all`, and `cmd_insert_if_new`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{BamCommands, BamComponent, BamTermDesc, FfiEntity, FfiResult};

use crate::{
    ffi::commands::HostCommands,
    handle_header::{COMMANDS_MAGIC, validate_handle_mut},
};

/// Reserves a fresh entity whose lifecycle persists across mod unload and queues a batched spawn.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
/// - When `count > 0`, `components` and `data_ptrs` must each point to `count` readable entries.
pub unsafe extern "C" fn host_cmd_spawn_persistent(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() || (count > 0 && (components.is_null() || data_ptrs.is_null())) {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    FfiResult::UNSUPPORTED
}

/// Reserves `entity_count` fresh entities and queues a bulk SoA spawn across `components[0..component_count]`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `component_count > 0`, `components` and `column_ptrs` must be non-null and readable.
/// - When `out_entities` is non-null and `entity_count > 0`, `out_entities` must be writable for `entity_count` slots.
pub unsafe extern "C" fn host_cmd_spawn_batch(
    commands: *mut BamCommands,
    components: *const BamComponent,
    column_ptrs: *const *const u8,
    component_count: usize,
    _entity_count: usize,
    _out_entities: *mut FfiEntity,
) -> FfiResult {
    if component_count > 0 && (components.is_null() || column_ptrs.is_null()) {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    FfiResult::UNSUPPORTED
}

/// Queues deferred bulk despawning (`World::despawn_all`) of non-resource entities matching `terms[0..term_count]`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `term_count > 0`, `terms` must point to `term_count` readable [`BamTermDesc`] elements.
pub unsafe extern "C" fn host_cmd_despawn_all(
    commands: *mut BamCommands,
    terms: *const BamTermDesc,
    term_count: usize,
) -> FfiResult {
    if term_count > 0 && terms.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    FfiResult::UNSUPPORTED
}

/// Queues deferred conditional insertion (`InsertMode::Keep`) of `component` onto `entity` iff absent.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_len > 0`, `data_ptr` must point to `data_len` readable bytes.
pub unsafe extern "C" fn host_cmd_insert_if_new(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    if !component.is_valid() {
        return FfiResult::INVALID_COMPONENT;
    }
    FfiResult::UNSUPPORTED
}
