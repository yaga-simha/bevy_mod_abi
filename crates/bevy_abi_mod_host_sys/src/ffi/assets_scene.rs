//! Cross-ABI asset (`asset_*`) and scene (`cmd_spawn_scene`) host entrypoints delegating to safe
//! [`HostAssetBackend`] and [`HostSceneBackend`] trait hooks on [`crate::ModRegistry`].

use bevy_abi_mod_sys::{
    BamCommands, BamComponent, BamHostCtx, FfiAssetHandle, FfiAssetLoadState, FfiEntity, FfiResult,
    StrView,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;

use crate::{
    handle_header::{
        HostContext, decode_optional_entity, ffi_try, init_out_ptr, validate_commands_mut,
        validate_host_ctx,
    },
    registry::RegistrySnapshot,
};

/// Safe backend trait implemented in `bevy_abi_mod_host` (`#![forbid(unsafe_code)]`) to service
/// `BamHostApi::asset_load`, `asset_get_load_state`, and `asset_release` without adding `bevy_asset`
/// to `bevy_abi_mod_host_sys`.
pub trait HostAssetBackend: Send + Sync + 'static {
    /// Initiates asynchronous asset loading for `path` under pre-resolved `asset_type`
    /// (`BamComponent::INVALID` when untyped loading is requested), retaining a strong handle in
    /// host memory under `owner_namespace` and returning its wire [`FfiAssetHandle`].
    fn load_asset(
        &self,
        snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<FfiAssetHandle, (FfiResult, String)>;

    /// Queries the current [`FfiAssetLoadState`] of `handle`.
    fn get_load_state(
        &self,
        snapshot: &RegistrySnapshot,
        handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, (FfiResult, String)>;

    /// Releases the host-retained strong handle corresponding to `handle`.
    fn release_asset(
        &self,
        snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)>;

    /// Releases all retained strong handles associated with `owner_namespace` when a mod unloads.
    fn release_all_for_owner(&self, owner_namespace: &str) {
        let _ = owner_namespace;
    }
}

/// Safe backend trait implemented in `bevy_abi_mod_host` (`#![forbid(unsafe_code)]`) to service
/// deferred `BamHostApi::cmd_spawn_scene` commands without adding `bevy_scene` to
/// `bevy_abi_mod_host_sys`.
pub trait HostSceneBackend: Send + Sync + 'static {
    /// Validates `scene_handle` synchronously when `cmd_spawn_scene` is queued.
    fn validate_scene_handle(
        &self,
        snapshot: &RegistrySnapshot,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        let _ = (snapshot, scene_handle);
        Ok(())
    }

    /// Instantiates `scene_handle` onto the already-spawned `root_entity` in `world` during
    /// deferred command application (`parent` has already been attached via `ChildOf` when `Some`).
    fn spawn_scene(
        &self,
        world: &mut World,
        snapshot: &RegistrySnapshot,
        root_entity: Entity,
        parent: Option<Entity>,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), FfiResult>;
}

/// Validates a guest-supplied `*const FfiAssetHandle` and active [`HostContext`], resolving the
/// registered [`HostAssetBackend`].
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
#[inline]
unsafe fn validate_asset_handle_call<'a>(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
    op: &str,
) -> Result<(&'a HostContext, FfiAssetHandle, &'a dyn HostAssetBackend), FfiResult> {
    if handle.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if !handle.is_aligned() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = unsafe { validate_host_ctx(ctx) }?;
    // SAFETY: `handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        host_ctx.set_last_error(format!("{op}: invalid FfiAssetHandle ({val})"));
        return Err(val);
    }
    let Some(backend) = host_ctx.snapshot().asset_backend() else {
        host_ctx.set_last_error(format!(
            "{op}: no HostAssetBackend registered on ModRegistry"
        ));
        return Err(FfiResult::UNSUPPORTED);
    };
    Ok((host_ctx, parsed_handle, backend.as_ref()))
}

/// Requests asynchronous asset loading via the registered [`HostAssetBackend`], writing the
/// resulting [`FfiAssetHandle`] into `*out_handle` (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If `path.len > 0` and `!path.ptr.is_null()`, `path` must reference `path.len` readable bytes.
/// - `out_handle` must point to writable [`FfiAssetHandle`] storage, or null.
pub unsafe extern "C" fn host_asset_load(
    ctx: *const BamHostCtx,
    asset_type: BamComponent,
    path: StrView,
    out_handle: *mut FfiAssetHandle,
) -> FfiResult {
    // SAFETY: `out_handle` is validated non-null and aligned before writing `NONE`.
    ffi_try!(unsafe { init_out_ptr(out_handle, FfiAssetHandle::NONE) });
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });

    // SAFETY: Caller guarantees `path` points to initialized bytes when `path.len > 0 && !path.ptr.is_null()`.
    let path_str = match unsafe { path.try_as_str() } {
        Ok(s) => s,
        Err(err) => {
            host_ctx.set_last_error(format!("asset_load: invalid path StrView ({err})"));
            return err;
        }
    };
    if path_str.is_empty() {
        host_ctx.set_last_error("asset_load: asset path must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    let snap = host_ctx.snapshot();
    let Some(backend) = snap.asset_backend() else {
        host_ctx.set_last_error("asset_load: no HostAssetBackend registered on ModRegistry");
        return FfiResult::UNSUPPORTED;
    };

    match backend.load_asset(snap, host_ctx.default_namespace(), asset_type, path_str) {
        Ok(handle) => {
            // SAFETY: `out_handle` was verified non-null and aligned above.
            unsafe {
                *out_handle = handle;
            }
            FfiResult::OK
        }
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Queries [`FfiAssetLoadState`] for `handle` via the registered [`HostAssetBackend`]
/// (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
/// - `out_state` must point to writable [`FfiAssetLoadState`] storage, or null.
pub unsafe extern "C" fn host_asset_get_load_state(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
    out_state: *mut FfiAssetLoadState,
) -> FfiResult {
    if handle.is_null() || out_state.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !handle.is_aligned() || !out_state.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_state` is non-null, aligned, and writable.
    unsafe {
        *out_state = FfiAssetLoadState::NOT_LOADED;
    }
    // SAFETY: Validates `ctx`, `handle`, and backend presence under caller contract.
    let (host_ctx, parsed_handle, backend) =
        ffi_try!(unsafe { validate_asset_handle_call(ctx, handle, "asset_get_load_state") });
    if parsed_handle.is_none() {
        return FfiResult::OK;
    }
    match backend.get_load_state(host_ctx.snapshot(), &parsed_handle) {
        Ok(state) => {
            // SAFETY: `out_state` was verified non-null and aligned above.
            unsafe {
                *out_state = state;
            }
            FfiResult::OK
        }
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Releases a host-retained strong asset handle via the registered [`HostAssetBackend`]
/// (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
pub unsafe extern "C" fn host_asset_release(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
) -> FfiResult {
    // SAFETY: Validates `ctx`, `handle`, and backend presence under caller contract.
    let (host_ctx, parsed_handle, backend) =
        ffi_try!(unsafe { validate_asset_handle_call(ctx, handle, "asset_release") });
    if parsed_handle.is_none() {
        return FfiResult::OK;
    }
    match backend.release_asset(
        host_ctx.snapshot(),
        host_ctx.default_namespace(),
        &parsed_handle,
    ) {
        Ok(()) => FfiResult::OK,
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Reserves a root entity and queues deferred scene instantiation via the registered
/// [`HostSceneBackend`], writing the reserved [`FfiEntity`] into `*out_entity`
/// (`FfiResult::UNSUPPORTED` if no scene backend is installed).
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - `scene_handle` must point to a readable [`FfiAssetHandle`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
pub unsafe extern "C" fn host_cmd_spawn_scene(
    commands: *mut BamCommands,
    scene_handle: *const FfiAssetHandle,
    parent: FfiEntity,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if scene_handle.is_null() || out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !scene_handle.is_aligned() || !out_entity.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_entity` is non-null, aligned, and writable.
    unsafe {
        *out_entity = FfiEntity::NULL;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });

    // SAFETY: `scene_handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *scene_handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        return val;
    }
    if parsed_handle.is_none() {
        return FfiResult::INVALID_ARGUMENT;
    }

    let parent_entity = ffi_try!(decode_optional_entity(parent));

    match host_cmds.queue_spawn_scene(parsed_handle, parent_entity) {
        Ok(entity) => {
            // SAFETY: `out_entity` was verified non-null and aligned above.
            unsafe {
                *out_entity = FfiEntity::from_bits(entity.to_bits());
            }
            FfiResult::OK
        }
        Err(err) => err,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/assets_scene_tests.rs"]
mod tests;
