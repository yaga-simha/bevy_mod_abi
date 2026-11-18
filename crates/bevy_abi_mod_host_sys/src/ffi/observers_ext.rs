//! Extended deferred trigger (`cmd_trigger_ex`) and runtime observer (`cmd_observe`) entrypoints.

use core::ffi::c_void;

use bevy_abi_mod_sys::{
    BamCommands, BamObserverDesc, BamTriggerDesc, FfiEntity, FfiResult, read_descriptor,
};

use crate::{
    ffi::commands::HostCommands,
    handle_header::{COMMANDS_MAGIC, validate_handle_mut},
};

/// Queues an extended deferred trigger described by [`BamTriggerDesc`].
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamTriggerDesc`], or null.
pub unsafe extern "C" fn host_cmd_trigger_ex(
    commands: *mut BamCommands,
    desc: *const BamTriggerDesc,
) -> FfiResult {
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
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamTriggerDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamTriggerDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }
    FfiResult::UNSUPPORTED
}

/// Queues deferred spawning of a runtime observer described by [`BamObserverDesc`], optionally
/// writing the reserved observer [`FfiEntity`] into `*out_entity` when non-null.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamObserverDesc`], or null.
/// - When `out_entity` is non-null, it must point to writable [`FfiEntity`] storage.
pub unsafe extern "C" fn host_cmd_observe(
    commands: *mut BamCommands,
    desc: *const BamObserverDesc,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if !out_entity.is_null() {
        if !out_entity.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `out_entity` is non-null, aligned, and writable.
        unsafe {
            *out_entity = FfiEntity::NULL;
        }
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
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamObserverDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }
    FfiResult::UNSUPPORTED
}
