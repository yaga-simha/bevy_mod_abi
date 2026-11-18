//! Safe (`#![forbid(unsafe_code)]`) Bevy Scene Notation (`bevy_scene`) spawning and `Ready`
//! observer host bridge (`feature = "bridge-bsn"`).

use bevy_abi_mod_host_sys::{
    HostSceneBackend, ModHost, ModId, ModRegistry, ModScoped, RegistrySnapshot,
};
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiResult};
use bevy_app::{App, Plugin};
use bevy_asset::{AssetServer, Assets, Handle};
use bevy_ecs::bundle::BundleScratch;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use bevy_scene::{Ready, ResolvedSceneRoot, Scene, ScenePatch, ScenePlugin};
use std::sync::{Arc, Mutex, RwLock};

use crate::bridges::asset::ModAssetWorldExt;

/// Canonical registry name for Bevy's [`Ready`] entity event.
pub const CANONICAL_READY_NAME: &str = "bevy_scene::Ready";
/// Short alias for the exposed [`ScenePatch`] asset type in [`ModRegistry`].
pub const SCENE_PATCH_ALIAS: &str = "ScenePatch";

type SceneSpawnerFn = Arc<dyn Fn(&mut World, Entity) -> Result<(), FfiResult> + Send + Sync>;

#[derive(Clone)]
struct ExposedSceneEntry {
    canonical_name: String,
    handle: FfiAssetHandle,
    owner_mod: Option<ModId>,
    spawner: SceneSpawnerFn,
}

#[derive(Default)]
struct HostSceneInner {
    scene_type_handle: Option<BamComponent>,
    scenes: HashMap<ComponentUuid, ExposedSceneEntry>,
    by_name: HashMap<String, ComponentUuid>,
}

/// Host resource storing exposed Bevy `BSN` scenes and deterministic UUID handles resolvable
/// across the C ABI via `cmd_spawn_scene`.
#[derive(Resource, Clone, Default)]
pub struct HostSceneRegistry {
    inner: Arc<RwLock<HostSceneInner>>,
}

impl HostSceneRegistry {
    /// Creates an empty [`HostSceneRegistry`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the registered [`BamComponent`] handle for `ScenePatch`, if initialized.
    pub fn scene_type_handle(&self) -> Option<BamComponent> {
        self.inner.read().ok().and_then(|g| g.scene_type_handle)
    }

    /// Looks up the [`FfiAssetHandle`] for a previously exposed scene `canonical_name`.
    pub fn get_handle(&self, canonical_name: &str) -> Option<FfiAssetHandle> {
        let guard = self.inner.read().ok()?;
        let uuid = guard.by_name.get(canonical_name)?;
        guard.scenes.get(uuid).map(|entry| entry.handle)
    }

    /// Returns `true` if `canonical_name` is exposed in this registry.
    pub fn contains(&self, canonical_name: &str) -> bool {
        self.inner
            .read()
            .is_ok_and(|g| g.by_name.contains_key(canonical_name))
    }

    /// Returns the number of exposed scenes.
    pub fn len(&self) -> usize {
        self.inner.read().map_or(0, |g| g.scenes.len())
    }

    /// Returns `true` if no scenes have been exposed yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn set_scene_type_handle(&self, handle: BamComponent) {
        if let Ok(mut guard) = self.inner.write() {
            guard.scene_type_handle = Some(handle);
        }
    }

    fn insert_scene(
        &self,
        canonical_name: &str,
        owner_mod: Option<ModId>,
        spawner: SceneSpawnerFn,
    ) -> Result<FfiAssetHandle, FfiResult> {
        if canonical_name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let mut guard = self.inner.write().map_err(|_| FfiResult::INTERNAL_ERROR)?;
        let Some(scene_type) = guard.scene_type_handle else {
            return Err(FfiResult::INVALID_STATE);
        };
        let uuid = ComponentUuid::from_canonical_name(canonical_name);
        let handle = FfiAssetHandle::from_uuid(scene_type, uuid);
        guard.by_name.insert(canonical_name.to_owned(), uuid);
        guard.scenes.insert(
            uuid,
            ExposedSceneEntry {
                canonical_name: canonical_name.to_owned(),
                handle,
                owner_mod,
                spawner,
            },
        );
        Ok(handle)
    }
}

struct BevyHostSceneBackend {
    inner: Arc<RwLock<HostSceneInner>>,
}

impl HostSceneBackend for BevyHostSceneBackend {
    fn validate_scene_handle(
        &self,
        _snapshot: &RegistrySnapshot,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        let validation = scene_handle.validate();
        if !validation.is_ok() {
            return Err((validation, format!("invalid scene handle: {validation:?}")));
        }
        if scene_handle.kind != FfiAssetHandle::KIND_UUID {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                "expected UUID-backed FfiAssetHandle for BSN scene".to_owned(),
            ));
        }
        let guard = self.inner.read().map_err(|_| {
            (
                FfiResult::INTERNAL_ERROR,
                "HostSceneRegistry lock poisoned".to_owned(),
            )
        })?;
        if guard.scenes.contains_key(&scene_handle.uuid) {
            Ok(())
        } else {
            Err((
                FfiResult::NOT_FOUND,
                format!("unknown exposed BSN scene UUID {:?}", scene_handle.uuid),
            ))
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
        let entry = {
            let guard = self.inner.read().map_err(|_| FfiResult::INTERNAL_ERROR)?;
            guard
                .scenes
                .get(&scene_handle.uuid)
                .cloned()
                .ok_or(FfiResult::NOT_FOUND)?
        };

        let inferred_owner = parent
            .and_then(|p| world.get::<ModScoped>(p).map(|m| m.0))
            .or(entry.owner_mod)
            .or_else(|| {
                let host = world.get_resource::<ModHost>()?;
                let mut active = host.loaded_mods().iter().filter(|m| m.is_active());
                let first = active.next()?;
                if active.next().is_none() {
                    Some(first.id())
                } else {
                    None
                }
            });

        if let Some(mod_id) = inferred_owner
            && let Ok(mut ent_mut) = world.get_entity_mut(root_entity)
            && !ent_mut.contains::<ModScoped>()
        {
            ent_mut.insert(ModScoped(mod_id));
        }

        let _name = &entry.canonical_name;
        (entry.spawner)(world, root_entity)
    }
}

/// Bevy [`Plugin`] installing [`HostSceneRegistry`], wiring [`HostSceneBackend`] on
/// [`ModRegistry`], exposing `ScenePatch`, and registering `bevy_scene::Ready` for guest
/// `BAM_OBSERVER_EVENT_READY` observers (`#![forbid(unsafe_code)]`).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModBsnPlugin;

impl Plugin for ModBsnPlugin {
    fn build(&self, app: &mut App) {
        ensure_bsn_world_setup(app.world_mut());
        if app.world().contains_resource::<AssetServer>() && !app.is_plugin_added::<ScenePlugin>() {
            app.add_plugins(ScenePlugin);
        }
    }

    fn finish(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>() && !app.is_plugin_added::<ScenePlugin>() {
            app.add_plugins(ScenePlugin);
        }
        ensure_bsn_world_setup(app.world_mut());
    }
}

fn ensure_bsn_world_setup(world: &mut World) {
    if !world.contains_resource::<Assets<ScenePatch>>() {
        world.insert_resource(Assets::<ScenePatch>::default());
    }
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<HostSceneRegistry>() {
        world.insert_resource(HostSceneRegistry::new());
    }

    let existing_type_handle = world
        .resource::<ModRegistry>()
        .get_by_name(SCENE_PATCH_ALIAS)
        .map(|e| e.handle);
    let scene_type_handle = match existing_type_handle {
        Some(h) => h,
        None => world
            .register_mod_asset_with_alias::<ScenePatch>(Some(SCENE_PATCH_ALIAS))
            .expect("register ScenePatch asset type"),
    };

    let scene_reg = world.resource::<HostSceneRegistry>().clone();
    scene_reg.set_scene_type_handle(scene_type_handle);

    let backend: Arc<dyn HostSceneBackend> = Arc::new(BevyHostSceneBackend {
        inner: Arc::clone(&scene_reg.inner),
    });
    world
        .resource_mut::<ModRegistry>()
        .set_scene_backend(backend);

    if world
        .resource::<ModRegistry>()
        .get_by_name(CANONICAL_READY_NAME)
        .is_none()
    {
        world.resource_scope(|world, mut reg: bevy_ecs::prelude::Mut<'_, ModRegistry>| {
            reg.register_bridged_entity_event_with::<Ready>(
                world,
                CANONICAL_READY_NAME,
                Some("Ready"),
                |w, entity| {
                    w.trigger(Ready { entity });
                },
            )
            .expect("register bevy_scene::Ready entity event");
        });
    }
}

/// Safe [`World`] extension methods for exposing Bevy `BSN` scenes to native mods
/// (`#![forbid(unsafe_code)]`).
pub trait ModBsnWorldExt {
    /// Exposes a host scene builder closure under `canonical_name`, minting a deterministic
    /// [`FfiAssetHandle`] (`KIND_UUID`). The closure populates `root_entity` and triggers
    /// [`Ready`].
    fn expose_mod_scene(
        &mut self,
        canonical_name: &str,
        spawner: impl Fn(&mut World, Entity) -> Result<(), FfiResult> + Send + Sync + 'static,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Exposes a typed Bevy [`Scene`] (`bsn! { ... }`) under `canonical_name`, resolving and
    /// applying its [`ResolvedSceneRoot`] onto the spawned `root_entity` and triggering [`Ready`].
    fn expose_mod_scene_bsn<S: Scene>(
        &mut self,
        canonical_name: &str,
        scene: S,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Exposes a loaded `Handle<ScenePatch>` asset under `canonical_name`.
    fn expose_mod_scene_patch(
        &mut self,
        canonical_name: &str,
        patch: Handle<ScenePatch>,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Resolves a previously exposed scene [`FfiAssetHandle`] by `canonical_name`.
    fn resolve_mod_scene(&self, canonical_name: &str) -> Option<FfiAssetHandle>;
}

impl ModBsnWorldExt for World {
    fn expose_mod_scene(
        &mut self,
        canonical_name: &str,
        spawner: impl Fn(&mut World, Entity) -> Result<(), FfiResult> + Send + Sync + 'static,
    ) -> Result<FfiAssetHandle, FfiResult> {
        ensure_bsn_world_setup(self);
        let wrapped: SceneSpawnerFn = Arc::new(move |world, root_entity| {
            spawner(world, root_entity)?;
            world.trigger(Ready {
                entity: root_entity,
            });
            Ok(())
        });
        self.resource::<HostSceneRegistry>()
            .insert_scene(canonical_name, None, wrapped)
    }

    fn expose_mod_scene_bsn<S: Scene>(
        &mut self,
        canonical_name: &str,
        scene: S,
    ) -> Result<FfiAssetHandle, FfiResult> {
        ensure_bsn_world_setup(self);
        let pending_scene: Mutex<Option<Box<dyn Scene>>> = Mutex::new(Some(Box::new(scene)));
        let resolved_cache: Mutex<Option<Arc<ResolvedSceneRoot>>> = Mutex::new(None);
        let wrapped: SceneSpawnerFn = Arc::new(move |world, root_entity| {
            let resolved = {
                let mut cache_guard = resolved_cache
                    .lock()
                    .map_err(|_| FfiResult::INTERNAL_ERROR)?;
                if let Some(existing) = cache_guard.as_ref() {
                    Arc::clone(existing)
                } else {
                    let boxed_scene = pending_scene
                        .lock()
                        .map_err(|_| FfiResult::INTERNAL_ERROR)?
                        .take()
                        .ok_or(FfiResult::INVALID_STATE)?;
                    let root = world.resource_scope(
                        |world, patches: bevy_ecs::prelude::Mut<'_, Assets<ScenePatch>>| {
                            let server = world
                                .get_resource::<AssetServer>()
                                .ok_or(FfiResult::INVALID_STATE)?;
                            ResolvedSceneRoot::resolve(boxed_scene, server, &patches)
                                .map_err(|_| FfiResult::INVALID_STATE)
                        },
                    )?;
                    let arc = Arc::new(root);
                    *cache_guard = Some(Arc::clone(&arc));
                    arc
                }
            };
            let mut ent_mut = world
                .get_entity_mut(root_entity)
                .map_err(|_| FfiResult::INVALID_ENTITY)?;
            let mut scratch = BundleScratch::default();
            resolved
                .apply(&mut ent_mut, &mut scratch)
                .map_err(|_| FfiResult::INTERNAL_ERROR)
        });
        self.resource::<HostSceneRegistry>()
            .insert_scene(canonical_name, None, wrapped)
    }

    fn expose_mod_scene_patch(
        &mut self,
        canonical_name: &str,
        patch_handle: Handle<ScenePatch>,
    ) -> Result<FfiAssetHandle, FfiResult> {
        ensure_bsn_world_setup(self);
        let wrapped: SceneSpawnerFn = Arc::new(move |world, root_entity| {
            let resolved = {
                let patches = world
                    .get_resource::<Assets<ScenePatch>>()
                    .ok_or(FfiResult::INVALID_STATE)?;
                let patch = patches.get(&patch_handle).ok_or(FfiResult::NOT_FOUND)?;
                patch
                    .resolved
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or(FfiResult::NOT_FOUND)?
            };
            let mut ent_mut = world
                .get_entity_mut(root_entity)
                .map_err(|_| FfiResult::INVALID_ENTITY)?;
            let mut scratch = BundleScratch::default();
            resolved
                .apply(&mut ent_mut, &mut scratch)
                .map_err(|_| FfiResult::INTERNAL_ERROR)
        });
        self.resource::<HostSceneRegistry>()
            .insert_scene(canonical_name, None, wrapped)
    }

    #[inline]
    fn resolve_mod_scene(&self, canonical_name: &str) -> Option<FfiAssetHandle> {
        self.get_resource::<HostSceneRegistry>()
            .and_then(|reg| reg.get_handle(canonical_name))
    }
}

/// Safe [`App`] extension methods mirroring [`ModBsnWorldExt`] (`#![forbid(unsafe_code)]`).
pub trait ModBsnAppExt {
    /// Exposes a host scene builder closure under `canonical_name`.
    fn expose_mod_scene(
        &mut self,
        canonical_name: &str,
        spawner: impl Fn(&mut World, Entity) -> Result<(), FfiResult> + Send + Sync + 'static,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Exposes a typed Bevy [`Scene`] (`bsn! { ... }`) under `canonical_name`.
    fn expose_mod_scene_bsn<S: Scene>(
        &mut self,
        canonical_name: &str,
        scene: S,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Exposes a loaded `Handle<ScenePatch>` asset under `canonical_name`.
    fn expose_mod_scene_patch(
        &mut self,
        canonical_name: &str,
        patch: Handle<ScenePatch>,
    ) -> Result<FfiAssetHandle, FfiResult>;

    /// Resolves a previously exposed scene [`FfiAssetHandle`] by `canonical_name`.
    fn resolve_mod_scene(&self, canonical_name: &str) -> Option<FfiAssetHandle>;
}

impl ModBsnAppExt for App {
    #[inline]
    fn expose_mod_scene(
        &mut self,
        canonical_name: &str,
        spawner: impl Fn(&mut World, Entity) -> Result<(), FfiResult> + Send + Sync + 'static,
    ) -> Result<FfiAssetHandle, FfiResult> {
        self.world_mut().expose_mod_scene(canonical_name, spawner)
    }

    #[inline]
    fn expose_mod_scene_bsn<S: Scene>(
        &mut self,
        canonical_name: &str,
        scene: S,
    ) -> Result<FfiAssetHandle, FfiResult> {
        self.world_mut().expose_mod_scene_bsn(canonical_name, scene)
    }

    #[inline]
    fn expose_mod_scene_patch(
        &mut self,
        canonical_name: &str,
        patch: Handle<ScenePatch>,
    ) -> Result<FfiAssetHandle, FfiResult> {
        self.world_mut()
            .expose_mod_scene_patch(canonical_name, patch)
    }

    #[inline]
    fn resolve_mod_scene(&self, canonical_name: &str) -> Option<FfiAssetHandle> {
        self.world().resolve_mod_scene(canonical_name)
    }
}
