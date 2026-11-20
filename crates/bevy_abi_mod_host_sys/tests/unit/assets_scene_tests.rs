use super::*;
use crate::HostCommands;
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
    let mut cmds_fail = HostCommands::new(&mut storage, world.entity_allocator(), &snap_fail_scene);
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
