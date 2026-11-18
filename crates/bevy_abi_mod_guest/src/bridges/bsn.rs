//! Safe (`#![forbid(unsafe_code)]`), zero-Bevy guest helpers for Bevy Scene Notation (`BSN`)
//! deferred scene spawning (`cmd_spawn_scene`) and `Ready` lifecycle observers.

use bevy_abi_mod_guest_sys::{Host, resolve_component};
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiEntity, FfiResult};

use crate::commands::GuestCommands;

/// Canonical registry name for Bevy's `bevy_scene::Ready` entity event.
pub const CANONICAL_READY_NAME: &str = "bevy_scene::Ready";
/// Short alias for the host-exposed `ScenePatch` asset type handle.
pub const SCENE_PATCH_ALIAS: &str = "ScenePatch";

/// Safe guest handle representing a host-exposed Bevy `BSN` scene (`ModBsnPlugin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GuestScene {
    raw: FfiAssetHandle,
}

impl GuestScene {
    /// Wraps an already-constructed [`FfiAssetHandle`].
    #[inline]
    pub const fn from_raw(raw: FfiAssetHandle) -> Self {
        Self { raw }
    }

    /// Resolves a host-exposed scene by `canonical_name`, minting its deterministic UUID handle
    /// tagged with the host's `"ScenePatch"` [`BamComponent`] handle.
    pub fn resolve(host: &Host, canonical_name: &str) -> Result<Self, FfiResult> {
        if canonical_name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let scene_type = resolve_scene_patch_type(host)?;
        let uuid = ComponentUuid::from_canonical_name(canonical_name);
        Ok(Self {
            raw: FfiAssetHandle::from_uuid(scene_type, uuid),
        })
    }

    /// Returns the underlying wire [`FfiAssetHandle`].
    #[inline]
    pub const fn raw(&self) -> FfiAssetHandle {
        self.raw
    }

    /// Spawns this BSN scene as a new root entity via `BamHostApi::cmd_spawn_scene`.
    #[inline]
    pub fn spawn(&self, cmds: &mut GuestCommands<'_>) -> Result<FfiEntity, FfiResult> {
        cmds.spawn_scene(self)
    }

    /// Spawns this BSN scene attached under `parent` (`ChildOf(parent)`) via
    /// `BamHostApi::cmd_spawn_scene`.
    #[inline]
    pub fn spawn_as_child(
        &self,
        cmds: &mut GuestCommands<'_>,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult> {
        cmds.spawn_scene_as_child(self, parent)
    }
}

impl From<GuestScene> for FfiAssetHandle {
    #[inline]
    fn from(scene: GuestScene) -> Self {
        scene.raw
    }
}

/// Resolves the host-registered `"ScenePatch"` asset type [`BamComponent`] handle.
#[inline]
pub fn resolve_scene_patch_type(host: &Host) -> Result<BamComponent, FfiResult> {
    resolve_component(host, Some(c"bevy_scene::scene_patch::ScenePatch"), None)
        .or_else(|_| resolve_component(host, Some(c"ScenePatch"), None))
}

/// Extension trait on [`GuestCommands`] for spawning host-exposed Bevy BSN scenes.
pub trait GuestCommandsBsnExt {
    /// Queues deferred spawning of `scene` at world root, returning its reserved root [`FfiEntity`].
    fn spawn_scene(&mut self, scene: &GuestScene) -> Result<FfiEntity, FfiResult>;

    /// Queues deferred spawning of `scene` attached under `parent` (`ChildOf(parent)`), returning
    /// its reserved root [`FfiEntity`].
    fn spawn_scene_as_child(
        &mut self,
        scene: &GuestScene,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult>;

    /// Queues deferred spawning of raw `scene_handle` (with optional `parent`), returning its
    /// reserved root [`FfiEntity`].
    fn spawn_scene_handle(
        &mut self,
        scene_handle: &FfiAssetHandle,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult>;
}

impl GuestCommandsBsnExt for GuestCommands<'_> {
    #[inline]
    fn spawn_scene(&mut self, scene: &GuestScene) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_scene(&scene.raw(), FfiEntity::NULL)
    }

    #[inline]
    fn spawn_scene_as_child(
        &mut self,
        scene: &GuestScene,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_scene(&scene.raw(), parent)
    }

    #[inline]
    fn spawn_scene_handle(
        &mut self,
        scene_handle: &FfiAssetHandle,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_scene(scene_handle, parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_scene_handle_uuid_is_deterministic_and_validates() {
        let scene_ty = BamComponent::from_raw(7);
        let h1 = FfiAssetHandle::from_uuid(
            scene_ty,
            ComponentUuid::from_canonical_name("ui::InventoryPanel"),
        );
        let h2 = FfiAssetHandle::from_uuid(
            scene_ty,
            ComponentUuid::from_canonical_name("ui::InventoryPanel"),
        );
        assert_eq!(h1, h2);
        assert_eq!(h1.validate(), FfiResult::OK);
        let scene = GuestScene::from_raw(h1);
        assert_eq!(scene.raw(), h1);
    }
}
