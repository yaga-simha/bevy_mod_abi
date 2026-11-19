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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::commands::HostCommandsStorage;
    use crate::registry::ModRegistry;
    use bevy_ecs::component::Component;
    use core::ptr::{null, null_mut};
    use std::sync::{Arc, Mutex};

    #[derive(Component)]
    struct SceneMarker;

    struct MockAssetBackend {
        fail: bool,
    }

    impl HostAssetBackend for MockAssetBackend {
        fn load_asset(
            &self,
            _snapshot: &RegistrySnapshot,
            _owner_namespace: Option<&str>,
            asset_type: BamComponent,
            _path: &str,
        ) -> Result<FfiAssetHandle, (FfiResult, String)> {
            if self.fail {
                Err((FfiResult::NOT_FOUND, String::from("missing asset")))
            } else {
                Ok(FfiAssetHandle::from_index(asset_type, 7, 1))
            }
        }

        fn get_load_state(
            &self,
            _snapshot: &RegistrySnapshot,
            _handle: &FfiAssetHandle,
        ) -> Result<FfiAssetLoadState, (FfiResult, String)> {
            if self.fail {
                Err((FfiResult::NOT_FOUND, String::from("unknown handle")))
            } else {
                Ok(FfiAssetLoadState::LOADED)
            }
        }

        fn release_asset(
            &self,
            _snapshot: &RegistrySnapshot,
            _owner_namespace: Option<&str>,
            _handle: &FfiAssetHandle,
        ) -> Result<(), (FfiResult, String)> {
            if self.fail {
                Err((FfiResult::NOT_FOUND, String::from("already released")))
            } else {
                Ok(())
            }
        }
    }

    struct MockSceneBackend {
        fail_validate: bool,
        spawned: Mutex<Vec<(Entity, Option<Entity>, FfiAssetHandle)>>,
    }

    impl HostSceneBackend for MockSceneBackend {
        fn validate_scene_handle(
            &self,
            _snapshot: &RegistrySnapshot,
            _scene_handle: &FfiAssetHandle,
        ) -> Result<(), (FfiResult, String)> {
            if self.fail_validate {
                Err((FfiResult::INVALID_ARGUMENT, String::from("bad scene")))
            } else {
                Ok(())
            }
        }

        fn spawn_scene(
            &self,
            world: &mut World,
            _snapshot: &RegistrySnapshot,
            root_entity: Entity,
            parent: Option<Entity>,
            scene_handle: &FfiAssetHandle,
        ) -> Result<(), FfiResult> {
            world.entity_mut(root_entity).insert(SceneMarker);
            self.spawned
                .lock()
                .unwrap()
                .push((root_entity, parent, *scene_handle));
            Ok(())
        }
    }

    struct DefaultValidateSceneBackend;

    impl HostSceneBackend for DefaultValidateSceneBackend {
        fn spawn_scene(
            &self,
            _world: &mut World,
            _snapshot: &RegistrySnapshot,
            _root_entity: Entity,
            _parent: Option<Entity>,
            _scene_handle: &FfiAssetHandle,
        ) -> Result<(), FfiResult> {
            Ok(())
        }
    }

    #[test]
    #[allow(clippy::cast_ptr_alignment)]
    fn asset_and_scene_guard_branches_and_backends() {
        let mut world = World::new();
        let mut reg_no_backend = ModRegistry::new();
        let asset_h = BamComponent::from_raw(1);

        let snap_no = reg_no_backend.snapshot_arc();
        let ctx_no = HostContext::new(Arc::clone(&snap_no)).with_namespace(Some(Arc::from("ns")));
        let mut poisoned_ctx = HostContext::new(Arc::clone(&snap_no));
        poisoned_ctx.header.poison();
        let poisoned_ptr = poisoned_ctx.as_ffi_ptr();

        let mut unaligned_buf = [0u64; 2];
        let unaligned_u8 = (&raw mut unaligned_buf).cast::<u8>().wrapping_add(1);

        let mut out_handle = FfiAssetHandle::NONE;
        let mut out_state = FfiAssetLoadState::NOT_LOADED;
        let valid_handle = FfiAssetHandle::from_index(asset_h, 3, 1);
        let mut bad_handle = FfiAssetHandle::NONE;
        bad_handle.kind = 99;
        let bad_utf8 = StrView {
            ptr: [0xFFu8, 0xFE].as_ptr(),
            len: 2,
        };

        // SAFETY: Valid stack/null/intentionally misaligned test pointers; misaligned pointers are
        // rejected by `is_aligned()` before any read or write.
        unsafe {
            assert_eq!(
                host_asset_load(
                    ctx_no.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str("a.png"),
                    null_mut()
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_asset_load(
                    ctx_no.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str("a.png"),
                    unaligned_u8.cast::<FfiAssetHandle>(),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_load(
                    null(),
                    asset_h,
                    StrView::from_str("a.png"),
                    &raw mut out_handle
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_asset_load(
                    poisoned_ptr,
                    asset_h,
                    StrView::from_str("a.png"),
                    &raw mut out_handle
                ),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_asset_load(ctx_no.as_ffi_ptr(), asset_h, bad_utf8, &raw mut out_handle),
                FfiResult::INVALID_UTF8
            );
            assert_eq!(
                host_asset_load(
                    ctx_no.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str(""),
                    &raw mut out_handle
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_load(
                    ctx_no.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str("a.png"),
                    &raw mut out_handle
                ),
                FfiResult::UNSUPPORTED
            );

            // `host_asset_get_load_state` guards
            assert_eq!(
                host_asset_get_load_state(ctx_no.as_ffi_ptr(), null(), &raw mut out_state),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_asset_get_load_state(ctx_no.as_ffi_ptr(), &valid_handle, null_mut()),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_asset_get_load_state(
                    ctx_no.as_ffi_ptr(),
                    unaligned_u8.cast::<FfiAssetHandle>(),
                    &raw mut out_state
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_get_load_state(
                    ctx_no.as_ffi_ptr(),
                    &valid_handle,
                    unaligned_u8.cast::<FfiAssetLoadState>()
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_get_load_state(poisoned_ptr, &valid_handle, &raw mut out_state),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_asset_get_load_state(ctx_no.as_ffi_ptr(), &bad_handle, &raw mut out_state),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_get_load_state(ctx_no.as_ffi_ptr(), &valid_handle, &raw mut out_state),
                FfiResult::UNSUPPORTED
            );

            // `host_asset_release` guards
            assert_eq!(
                host_asset_release(ctx_no.as_ffi_ptr(), null()),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_asset_release(ctx_no.as_ffi_ptr(), unaligned_u8.cast::<FfiAssetHandle>()),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_release(poisoned_ptr, &valid_handle),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_asset_release(ctx_no.as_ffi_ptr(), &bad_handle),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_asset_release(ctx_no.as_ffi_ptr(), &valid_handle),
                FfiResult::UNSUPPORTED
            );
        }

        // Now attach failing and succeeding backends.
        let fail_backend = Arc::new(MockAssetBackend { fail: true });
        fail_backend.release_all_for_owner("ns");
        reg_no_backend.set_asset_backend(fail_backend);
        let ctx_fail = HostContext::new(reg_no_backend.snapshot_arc());

        let mut reg_ok = ModRegistry::new();
        reg_ok.set_asset_backend(Arc::new(MockAssetBackend { fail: false }));
        let scene_backend = Arc::new(MockSceneBackend {
            fail_validate: false,
            spawned: Mutex::new(Vec::new()),
        });
        reg_ok.set_scene_backend(Arc::clone(&scene_backend) as Arc<dyn HostSceneBackend>);
        let snap_ok = reg_ok.snapshot_arc();
        let ctx_ok = HostContext::new(Arc::clone(&snap_ok));

        // SAFETY: Valid `HostContext`s and aligned stack outputs.
        unsafe {
            assert_eq!(
                host_asset_load(
                    ctx_fail.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str("missing.png"),
                    &raw mut out_handle
                ),
                FfiResult::NOT_FOUND
            );
            assert_eq!(ctx_fail.last_error_message(), "missing asset");
            assert_eq!(
                host_asset_get_load_state(ctx_fail.as_ffi_ptr(), &valid_handle, &raw mut out_state),
                FfiResult::NOT_FOUND
            );
            assert_eq!(
                host_asset_release(ctx_fail.as_ffi_ptr(), &valid_handle),
                FfiResult::NOT_FOUND
            );

            assert_eq!(
                host_asset_load(
                    ctx_ok.as_ffi_ptr(),
                    asset_h,
                    StrView::from_str("ok.png"),
                    &raw mut out_handle
                ),
                FfiResult::OK
            );
            assert_eq!(out_handle, FfiAssetHandle::from_index(asset_h, 7, 1));
            assert_eq!(
                host_asset_get_load_state(
                    ctx_ok.as_ffi_ptr(),
                    &FfiAssetHandle::NONE,
                    &raw mut out_state
                ),
                FfiResult::OK
            );
            assert_eq!(out_state, FfiAssetLoadState::NOT_LOADED);
            assert_eq!(
                host_asset_get_load_state(ctx_ok.as_ffi_ptr(), &out_handle, &raw mut out_state),
                FfiResult::OK
            );
            assert_eq!(out_state, FfiAssetLoadState::LOADED);
            assert_eq!(
                host_asset_release(ctx_ok.as_ffi_ptr(), &FfiAssetHandle::NONE),
                FfiResult::OK
            );
            assert_eq!(
                host_asset_release(ctx_ok.as_ffi_ptr(), &out_handle),
                FfiResult::OK
            );
        }

        // `host_cmd_spawn_scene` guards and happy paths
        let parent_ent = world.spawn_empty().id();
        let parent_ffi = FfiEntity::from_bits(parent_ent.to_bits());
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap_ok);
        let mut out_ent = FfiEntity::NULL;
        let bad_ent = FfiEntity::from_bits(1u64 << 32);
        let placeholder_ffi = FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits());

        // SAFETY: Valid `HostCommands` and aligned stack outputs; intentionally misaligned pointers
        // are rejected before read/write.
        unsafe {
            assert_eq!(
                host_cmd_spawn_scene(cmds.as_ffi_ptr(), null(), FfiEntity::NULL, &raw mut out_ent),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &valid_handle,
                    FfiEntity::NULL,
                    null_mut()
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    unaligned_u8.cast::<FfiAssetHandle>(),
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &valid_handle,
                    FfiEntity::NULL,
                    unaligned_u8.cast::<FfiEntity>()
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_spawn_scene(null_mut(), &valid_handle, FfiEntity::NULL, &raw mut out_ent),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    poisoned_ptr.cast_mut().cast::<BamCommands>(),
                    &valid_handle,
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &bad_handle,
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &FfiAssetHandle::NONE,
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_spawn_scene(cmds.as_ffi_ptr(), &valid_handle, bad_ent, &raw mut out_ent),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &valid_handle,
                    placeholder_ffi,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds.as_ffi_ptr(),
                    &valid_handle,
                    parent_ffi,
                    &raw mut out_ent
                ),
                FfiResult::OK
            );
        }
        drop(cmds);
        storage.apply_to_world(&mut world, &snap_ok);
        let root_e = Entity::try_from_bits(out_ent.to_bits()).unwrap();
        assert!(world.entity(root_e).contains::<SceneMarker>());
        assert_eq!(scene_backend.spawned.lock().unwrap().len(), 1);

        // Failing scene validation, default `validate_scene_handle`, & no scene backend
        assert!(
            DefaultValidateSceneBackend
                .validate_scene_handle(&snap_ok, &valid_handle)
                .is_ok()
        );
        let mut reg_fail_scene = ModRegistry::new();
        reg_fail_scene.set_scene_backend(Arc::new(MockSceneBackend {
            fail_validate: true,
            spawned: Mutex::new(Vec::new()),
        }));
        let snap_fail_scene = reg_fail_scene.snapshot_arc();
        let mut cmds_fail =
            HostCommands::new(&mut storage, world.entity_allocator(), &snap_fail_scene);
        // SAFETY: Valid `HostCommands` and aligned stack outputs.
        unsafe {
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds_fail.as_ffi_ptr(),
                    &valid_handle,
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::INVALID_ARGUMENT
            );
        }
        drop(cmds_fail);
        let mut cmds_none = HostCommands::new(&mut storage, world.entity_allocator(), &snap_no);
        // SAFETY: Valid `HostCommands` and aligned stack outputs.
        unsafe {
            assert_eq!(
                host_cmd_spawn_scene(
                    cmds_none.as_ffi_ptr(),
                    &valid_handle,
                    FfiEntity::NULL,
                    &raw mut out_ent
                ),
                FfiResult::UNSUPPORTED
            );
        }
    }
}
