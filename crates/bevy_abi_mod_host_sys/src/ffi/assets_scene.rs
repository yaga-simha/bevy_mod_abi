//! Cross-ABI asset (`asset_*`) and scene (`cmd_spawn_scene`) host entrypoints delegating to safe
//! [`HostAssetBackend`] and [`HostSceneBackend`] trait hooks on [`crate::ModRegistry`].

use core::ffi::c_void;

use bevy_abi_mod_sys::{
    BamCommands, BamComponent, BamHostCtx, FfiAssetHandle, FfiAssetLoadState, FfiEntity, FfiResult,
    StrView,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;

use crate::{
    ffi::commands::HostCommands,
    handle_header::{
        COMMANDS_MAGIC, HOST_CTX_MAGIC, HostContext, validate_handle_mut, validate_handle_ref,
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
    if out_handle.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_handle.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_handle` is non-null, aligned, and writable; initialize to `NONE` upfront.
    unsafe {
        *out_handle = FfiAssetHandle::NONE;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

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
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        host_ctx.set_last_error(format!(
            "asset_get_load_state: invalid FfiAssetHandle ({val})"
        ));
        return val;
    }

    let snap = host_ctx.snapshot();
    let Some(backend) = snap.asset_backend() else {
        host_ctx
            .set_last_error("asset_get_load_state: no HostAssetBackend registered on ModRegistry");
        return FfiResult::UNSUPPORTED;
    };

    if parsed_handle.is_none() {
        return FfiResult::OK;
    }

    match backend.get_load_state(snap, &parsed_handle) {
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
    if handle.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !handle.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        host_ctx.set_last_error(format!("asset_release: invalid FfiAssetHandle ({val})"));
        return val;
    }

    let snap = host_ctx.snapshot();
    let Some(backend) = snap.asset_backend() else {
        host_ctx.set_last_error("asset_release: no HostAssetBackend registered on ModRegistry");
        return FfiResult::UNSUPPORTED;
    };

    if parsed_handle.is_none() {
        return FfiResult::OK;
    }

    match backend.release_asset(snap, host_ctx.default_namespace(), &parsed_handle) {
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
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
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
    let Ok(host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };

    // SAFETY: `scene_handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *scene_handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        return val;
    }
    if parsed_handle.is_none() {
        return FfiResult::INVALID_ARGUMENT;
    }

    let parent_entity = if parent.is_null() {
        None
    } else {
        let Some(e) = Entity::try_from_bits(parent.to_bits()) else {
            return FfiResult::INVALID_ENTITY;
        };
        if e == Entity::PLACEHOLDER {
            return FfiResult::INVALID_ENTITY;
        }
        Some(e)
    };

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
