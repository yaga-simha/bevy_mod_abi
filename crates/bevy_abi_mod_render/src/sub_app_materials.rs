//! Safe (`#![forbid(unsafe_code)]`) multi-world `SubApp` (`RenderApp`, `AudioApp`, custom sub-apps)
//! extraction via `bevy_extract`, data-driven `.wesl` [`Shader`] assets, [`AlignedVec`]-backed
//! [`ShaderBuffer`] storage assets, and Bevy `0.20` 2D/3D material management.

use crate::sprite_mesh_shaders::{
    ModExtendedSpriteMaterial2d, ModWeslExtendedMaterialEntry, ModWeslSpriteExtension2d,
    ModWeslSpriteExtensionEntry, evict_wesl_sprite_specializations_for_shaders,
    lookup_wesl_specialization_slot,
};
use bevy_abi_mod_host::register_mod_unload_hook_in_world;
use bevy_abi_mod_host_sys::{
    ModHost, ModId, ModRegistry, ensure_component_in_target_world,
    extract_component_between_worlds, extract_resource_between_worlds,
};
use bevy_abi_mod_sys::{BamComponent, FfiResult};
use bevy_app::{App, AppLabel, Plugin, SubApp};
use bevy_asset::{Asset, AssetApp, AssetId, AssetServer, Assets, Handle, RenderAssetUsages};
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, Schedule, ScheduleLabel, SystemSet};
use bevy_ecs::world::World;
use bevy_extract::sync_world::{MainEntity, SyncToSubWorld, SyncWorldPlugin};
use bevy_extract::{ExtractPlugin, ExtractSchedule, MainWorld};
use bevy_image::Image;
use bevy_material::AlphaMode;
use bevy_pbr::StandardMaterial;
use bevy_platform::collections::{AlignedVec, HashMap, HashSet};
use bevy_reflect::Reflect;
use bevy_render::render_resource::{AsBindGroup, BufferUsages, ShaderType};
use bevy_render::storage::{ShaderBuffer, ShaderBufferData};
use bevy_shader::{Shader, ShaderDefVal, ValidateShader};
use bevy_sprite::SpriteImageMode;
use bevy_sprite_render::{
    AlphaMode2d, ExtendedMaterial2d, MaterialExtension2d, SpriteMeshMaterial,
};
use core::fmt;
use core::marker::PhantomData;
use std::borrow::Cow;
use std::path::Path;

/// Default [`ScheduleLabel`] installed when [`ModSubAppPlugin`] initializes a standalone
/// `SubApp` that has not yet been registered by another plugin (such as Bevy's `RenderPlugin`).
#[derive(ScheduleLabel, Debug, Hash, PartialEq, Eq, Clone, Default)]
pub struct ModSubAppSchedule;

/// System sets used by [`ModSubAppSchedule`] when [`ModSubAppPlugin`] installs [`ExtractPlugin`].
#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub enum ModSubAppSystems {
    /// Applies deferred commands produced during [`ExtractSchedule`].
    ExtractCommands,
    /// Despawns temporary extracted entities at the end of the sub-app update.
    PostCleanup,
}

impl ModSubAppSchedule {
    /// Constructs the base [`Schedule`] configuring ordered [`ModSubAppSystems`] sets.
    pub fn base_schedule() -> Schedule {
        let mut schedule = Schedule::new(Self);
        schedule.configure_sets(
            (
                ModSubAppSystems::ExtractCommands,
                ModSubAppSystems::PostCleanup,
            )
                .chain(),
        );
        schedule
    }
}

/// Configuration controlling which registered [`BamComponent`] components and singleton
/// resources are extracted from [`MainWorld`] into a target [`SubApp`].
#[derive(Resource, Clone, Debug, Default)]
pub struct ModSubAppConfig {
    components: Vec<BamComponent>,
    component_names: Vec<String>,
    resources: Vec<BamComponent>,
    resource_names: Vec<String>,
}

impl ModSubAppConfig {
    /// Creates an empty [`ModSubAppConfig`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a pre-registered [`BamComponent`] component handle to extract on synced entities.
    pub fn with_component(mut self, component: BamComponent) -> Self {
        self.push_component(component);
        self
    }

    /// Adds multiple [`BamComponent`] component handles to extract on synced entities.
    pub fn with_components(mut self, components: impl IntoIterator<Item = BamComponent>) -> Self {
        for component in components {
            self.push_component(component);
        }
        self
    }

    /// Adds a canonical component name (host or mod-qualified) resolved dynamically from
    /// [`MainWorld`]'s [`ModRegistry`] during [`ExtractSchedule`].
    pub fn with_component_name(mut self, name: impl Into<String>) -> Self {
        self.push_component_name(name);
        self
    }

    /// Adds a pre-registered singleton resource [`BamComponent`] handle to extract into the `SubApp`.
    pub fn with_resource(mut self, resource: BamComponent) -> Self {
        self.push_resource(resource);
        self
    }

    /// Adds multiple singleton resource [`BamComponent`] handles to extract into the `SubApp`.
    pub fn with_resources(mut self, resources: impl IntoIterator<Item = BamComponent>) -> Self {
        for resource in resources {
            self.push_resource(resource);
        }
        self
    }

    /// Adds a canonical singleton resource name resolved dynamically from [`MainWorld`]'s
    /// [`ModRegistry`] during [`ExtractSchedule`].
    pub fn with_resource_name(mut self, name: impl Into<String>) -> Self {
        self.push_resource_name(name);
        self
    }

    /// Inserts `component` if valid and not already present.
    pub fn push_component(&mut self, component: BamComponent) {
        if component.is_valid() && !self.components.contains(&component) {
            self.components.push(component);
        }
    }

    /// Inserts `name` if non-empty and not already present.
    pub fn push_component_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !name.is_empty() && !self.component_names.contains(&name) {
            self.component_names.push(name);
        }
    }

    /// Inserts `resource` if valid and not already present.
    pub fn push_resource(&mut self, resource: BamComponent) {
        if resource.is_valid() && !self.resources.contains(&resource) {
            self.resources.push(resource);
        }
    }

    /// Inserts `name` if non-empty and not already present.
    pub fn push_resource_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !name.is_empty() && !self.resource_names.contains(&name) {
            self.resource_names.push(name);
        }
    }

    /// Removes `component` from the extraction list, returning `true` if it was present.
    pub fn remove_component(&mut self, component: BamComponent) -> bool {
        if let Some(pos) = self.components.iter().position(|c| *c == component) {
            self.components.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Removes a component name from the extraction list, returning `true` if it was present.
    pub fn remove_component_name(&mut self, name: &str) -> bool {
        if let Some(pos) = self.component_names.iter().position(|n| n == name) {
            self.component_names.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Removes `resource` from the extraction list, returning `true` if it was present.
    pub fn remove_resource(&mut self, resource: BamComponent) -> bool {
        if let Some(pos) = self.resources.iter().position(|r| *r == resource) {
            self.resources.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Removes a resource name from the extraction list, returning `true` if it was present.
    pub fn remove_resource_name(&mut self, name: &str) -> bool {
        if let Some(pos) = self.resource_names.iter().position(|n| n == name) {
            self.resource_names.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Returns the configured [`BamComponent`] component handles.
    pub fn components(&self) -> &[BamComponent] {
        &self.components
    }

    /// Returns the configured component names.
    pub fn component_names(&self) -> &[String] {
        &self.component_names
    }

    /// Returns the configured singleton resource [`BamComponent`] handles.
    pub fn resources(&self) -> &[BamComponent] {
        &self.resources
    }

    /// Returns the configured singleton resource names.
    pub fn resource_names(&self) -> &[String] {
        &self.resource_names
    }
}

/// Tracks mod components and singleton resources extracted into a [`SubApp`] so removals and
/// `unload_mod` cleanly strip extracted state on the next sync/extract cycle.
#[derive(Resource)]
struct ExtractedSubState<L: AppLabel> {
    entity_components: HashMap<(Entity, BamComponent), ComponentId>,
    resources: HashMap<BamComponent, ComponentId>,
    marker: PhantomData<fn() -> L>,
}

impl<L: AppLabel> Default for ExtractedSubState<L> {
    fn default() -> Self {
        Self {
            entity_components: HashMap::default(),
            resources: HashMap::default(),
            marker: PhantomData,
        }
    }
}

/// Plugin configuring multi-world `bevy_extract` synchronization and safe [`ModRegistry`]
/// component/resource extraction into [`SubApp`] `L` (e.g. `RenderApp`, `AudioApp`, or custom
/// simulation sub-apps).
///
/// If a `SubApp` for `L` does not already exist when [`ModSubAppPlugin::build`] runs, it installs
/// [`ExtractPlugin<L>`] using [`ModSubAppSchedule`] and sets the sub-app's `update_schedule` so
/// `app.update()` drives sync and extraction out of the box. If `L` already exists (for instance
/// when `RenderPlugin` or a custom [`ExtractPlugin<L>`] was added earlier), it preserves the
/// existing `SubApp` and augments its [`ExtractSchedule`].
#[derive(Clone, Debug)]
pub struct ModSubAppPlugin<L: AppLabel + Default + Copy + Eq> {
    config: ModSubAppConfig,
    marker: PhantomData<L>,
}

impl<L: AppLabel + Default + Copy + Eq> Default for ModSubAppPlugin<L> {
    fn default() -> Self {
        Self {
            config: ModSubAppConfig::default(),
            marker: PhantomData,
        }
    }
}

impl<L: AppLabel + Default + Copy + Eq> ModSubAppPlugin<L> {
    /// Creates a [`ModSubAppPlugin`] with an initial [`ModSubAppConfig`].
    pub fn new(config: ModSubAppConfig) -> Self {
        Self {
            config,
            marker: PhantomData,
        }
    }
}

impl<L: AppLabel + Default + Copy + Eq> Plugin for ModSubAppPlugin<L> {
    fn build(&self, app: &mut App) {
        if app.get_sub_app(L::default()).is_none() {
            app.add_plugins(ExtractPlugin::<L>::new(
                |_, _| {},
                ModSubAppSchedule::base_schedule,
                ModSubAppSchedule.intern(),
                ModSubAppSystems::ExtractCommands.intern(),
                ModSubAppSystems::PostCleanup.intern(),
            ));
            if let Some(sub_app) = app.get_sub_app_mut(L::default()) {
                sub_app.update_schedule = Some(ModSubAppSchedule.intern());
            }
        } else if !app.is_plugin_added::<SyncWorldPlugin<L>>() {
            app.add_plugins(SyncWorldPlugin::<L>::default());
        }

        if let Some(sub_app) = app.get_sub_app_mut(L::default()) {
            let mut merged = self.config.clone();
            if let Some(existing) = sub_app.world().get_resource::<ModSubAppConfig>() {
                for &c in existing.components() {
                    merged.push_component(c);
                }
                for name in existing.component_names() {
                    merged.push_component_name(name.clone());
                }
                for &r in existing.resources() {
                    merged.push_resource(r);
                }
                for name in existing.resource_names() {
                    merged.push_resource_name(name.clone());
                }
            }
            sub_app
                .insert_resource(merged)
                .init_resource::<ExtractedSubState<L>>();

            if sub_app
                .world()
                .get_resource::<bevy_ecs::schedule::Schedules>()
                .is_none_or(|s| !s.contains(ExtractSchedule))
            {
                let mut extract_schedule = Schedule::new(ExtractSchedule);
                extract_schedule.set_apply_final_deferred(false);
                sub_app.add_schedule(extract_schedule);
            }

            sub_app.add_systems(ExtractSchedule, extract_mod_data_for_sub_app::<L>);
        }
    }
}

fn cleanup_unloaded_mods_in_sub_world<L: AppLabel>(
    main_world: &World,
    sub_world: &mut World,
    state: &mut ExtractedSubState<L>,
) {
    let main_host = main_world.get_resource::<ModHost>();
    let main_reg = main_world.get_resource::<ModRegistry>();

    let mut evicted_components = Vec::<(BamComponent, ComponentId, bool)>::new();

    if let Some(sub_reg) = sub_world.get_resource::<ModRegistry>() {
        for entry in sub_reg.snapshot().entries() {
            if let Some(owner_id) = entry.owner {
                let owner_live = main_host.is_some_and(|h| h.contains_mod(owner_id));
                let entry_live = main_reg.is_some_and(|r| r.get_by_uuid(&entry.uuid).is_some());
                if !owner_live || !entry_live {
                    evicted_components.push((
                        entry.handle,
                        entry.component_id,
                        entry.is_resource(),
                    ));
                }
            }
        }
    }

    if evicted_components.is_empty() {
        return;
    }

    let sub_entities: Vec<Entity> = sub_world.query::<Entity>().iter(sub_world).collect();
    for (_handle, target_cid, is_resource) in &evicted_components {
        if *is_resource {
            sub_world.remove_resource_by_id(*target_cid);
        } else {
            for &entity in &sub_entities {
                if let Ok(mut ent) = sub_world.get_entity_mut(entity)
                    && ent.contains_id(*target_cid)
                {
                    ent.remove_by_id(*target_cid);
                }
            }
        }
    }

    // Clear all tracked extracted resources and dynamic non-native components before resetting
    // `sub_world`'s `ModRegistry` mirror; live components/resources are immediately re-extracted
    // in the same `ExtractSchedule` pass with zero duplicate entities or stale entries.
    for (_, res_cid) in state.resources.drain() {
        sub_world.remove_resource_by_id(res_cid);
    }
    for ((sub_ent, _), comp_cid) in state.entity_components.drain() {
        if let Ok(mut ent) = sub_world.get_entity_mut(sub_ent)
            && ent.contains_id(comp_cid)
        {
            ent.remove_by_id(comp_cid);
        }
    }

    if let Some(mut sub_reg) = sub_world.get_resource_mut::<ModRegistry>() {
        *sub_reg = ModRegistry::new();
    }
}

fn resolve_configured_handles(
    main_world: &World,
    handles: &[BamComponent],
    names: &[String],
) -> Vec<BamComponent> {
    let Some(reg) = main_world.get_resource::<ModRegistry>() else {
        return Vec::new();
    };
    let snap = reg.snapshot();
    let mut resolved = Vec::with_capacity(handles.len() + names.len());
    for &handle in handles {
        if snap.get_by_handle(handle).is_some() && !resolved.contains(&handle) {
            resolved.push(handle);
        }
    }
    for name in names {
        if let Some(entry) = snap.get_by_name(name)
            && !resolved.contains(&entry.handle)
        {
            resolved.push(entry.handle);
        }
    }
    resolved
}

fn extract_mod_data_for_sub_app<L: AppLabel>(sub_world: &mut World) {
    if !sub_world.contains_resource::<MainWorld>() {
        return;
    }
    if !sub_world.contains_resource::<ModSubAppConfig>() {
        sub_world.init_resource::<ModSubAppConfig>();
    }
    if !sub_world.contains_resource::<ExtractedSubState<L>>() {
        sub_world.init_resource::<ExtractedSubState<L>>();
    }

    let config = sub_world.resource::<ModSubAppConfig>().clone();
    sub_world.resource_scope::<MainWorld, _>(|sub_world, main_world| {
        sub_world.resource_scope::<ExtractedSubState<L>, _>(|sub_world, mut state| {
            cleanup_unloaded_mods_in_sub_world(&main_world, sub_world, &mut state);

            let active_components = resolve_configured_handles(
                &main_world,
                config.components(),
                config.component_names(),
            );
            let active_resources = resolve_configured_handles(
                &main_world,
                config.resources(),
                config.resource_names(),
            );

            // Remove previously extracted resources that are no longer active/configured.
            let stale_resources: Vec<BamComponent> = state
                .resources
                .keys()
                .copied()
                .filter(|h| !active_resources.contains(h))
                .collect();
            for res_handle in stale_resources {
                if let Some(cid) = state.resources.remove(&res_handle) {
                    sub_world.remove_resource_by_id(cid);
                }
            }

            // Extract configured singleton resources.
            for res_handle in &active_resources {
                match extract_resource_between_worlds(&main_world, sub_world, *res_handle) {
                    Ok(true) => {
                        if let Ok(target_cid) =
                            ensure_component_in_target_world(&main_world, sub_world, *res_handle)
                        {
                            state.resources.insert(*res_handle, target_cid);
                        }
                    }
                    Ok(false) | Err(FfiResult::INVALID_COMPONENT) => {
                        if let Some(cid) = state.resources.remove(res_handle) {
                            sub_world.remove_resource_by_id(cid);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(?res_handle, ?err, "sub_app resource extraction skipped");
                    }
                }
            }

            // Prune component tracking entries for despawned sub-entities or removed components.
            let synced_entities: Vec<(Entity, Entity)> = sub_world
                .query::<(Entity, &MainEntity)>()
                .iter(sub_world)
                .map(|(sub_entity, main_entity)| (sub_entity, main_entity.id()))
                .collect();
            let live_sub_set: HashSet<Entity> =
                synced_entities.iter().map(|(sub_e, _)| *sub_e).collect();

            let stale_entity_keys: Vec<(Entity, BamComponent)> = state
                .entity_components
                .keys()
                .copied()
                .filter(|(sub_e, comp_h)| {
                    !live_sub_set.contains(sub_e) || !active_components.contains(comp_h)
                })
                .collect();
            for key in stale_entity_keys {
                if let Some(target_cid) = state.entity_components.remove(&key)
                    && let Ok(mut ent) = sub_world.get_entity_mut(key.0)
                {
                    ent.remove_by_id(target_cid);
                }
            }

            // Extract configured components across all synced MainEntity <-> SubEntity pairs.
            for (sub_entity, main_entity) in synced_entities {
                for &comp_handle in &active_components {
                    match extract_component_between_worlds(
                        &main_world,
                        sub_world,
                        main_entity,
                        sub_entity,
                        comp_handle,
                    ) {
                        Ok(true) => {
                            if let Ok(target_cid) = ensure_component_in_target_world(
                                &main_world,
                                sub_world,
                                comp_handle,
                            ) {
                                state
                                    .entity_components
                                    .insert((sub_entity, comp_handle), target_cid);
                            }
                        }
                        Ok(false) | Err(FfiResult::INVALID_COMPONENT) => {
                            if let Some(target_cid) =
                                state.entity_components.remove(&(sub_entity, comp_handle))
                                && let Ok(mut ent) = sub_world.get_entity_mut(sub_entity)
                            {
                                ent.remove_by_id(target_cid);
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                ?main_entity,
                                ?sub_entity,
                                ?comp_handle,
                                ?err,
                                "sub_app component extraction skipped"
                            );
                        }
                    }
                }
            }
        });
    });
}

/// Extension trait on [`App`] for configuring multi-world `SubApp` component and resource
/// extraction with [`ModSubAppPlugin`].
pub trait ModSubAppExt {
    /// Mutates the [`ModSubAppConfig`] stored on the target [`SubApp`] `label`.
    fn configure_mod_sub_app<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        f: impl FnOnce(&mut ModSubAppConfig),
    ) -> &mut Self;

    /// Registers a [`BamComponent`] handle for automatic extraction into [`SubApp`] `label`.
    fn register_sub_app_mod_component<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        component: BamComponent,
    ) -> &mut Self;

    /// Registers a component name (host or mod-qualified) for automatic extraction into [`SubApp`]
    /// `label`.
    fn register_sub_app_mod_component_name<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        name: &str,
    ) -> &mut Self;

    /// Registers a singleton resource [`BamComponent`] handle for automatic extraction into
    /// [`SubApp`] `label`.
    fn register_sub_app_mod_resource<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        resource: BamComponent,
    ) -> &mut Self;

    /// Registers a singleton resource name for automatic extraction into [`SubApp`] `label`.
    fn register_sub_app_mod_resource_name<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        name: &str,
    ) -> &mut Self;

    /// Automatically attaches [`SyncToSubWorld<L>`] whenever host component `T` is inserted onto an
    /// entity in [`MainWorld`].
    fn sync_entities_with_mod_component<L: AppLabel + Default + Copy + Eq, T: Component>(
        &mut self,
    ) -> &mut Self;

    /// Marks an existing [`MainWorld`] entity with [`SyncToSubWorld<L>`] so it synchronizes with
    /// [`SubApp`] `label` on the next extract cycle.
    fn attach_sync_to_sub_app<L: AppLabel + Default + Copy + Eq>(&mut self, entity: Entity)
    -> bool;
}

impl ModSubAppExt for App {
    fn configure_mod_sub_app<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        f: impl FnOnce(&mut ModSubAppConfig),
    ) -> &mut Self {
        if let Some(sub_app) = self.get_sub_app_mut(label) {
            sub_app.configure_mod_extraction(f);
        }
        self
    }

    fn register_sub_app_mod_component<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        component: BamComponent,
    ) -> &mut Self {
        self.configure_mod_sub_app(label, |cfg| cfg.push_component(component))
    }

    fn register_sub_app_mod_component_name<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        name: &str,
    ) -> &mut Self {
        self.configure_mod_sub_app(label, |cfg| cfg.push_component_name(name))
    }

    fn register_sub_app_mod_resource<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        resource: BamComponent,
    ) -> &mut Self {
        self.configure_mod_sub_app(label, |cfg| cfg.push_resource(resource))
    }

    fn register_sub_app_mod_resource_name<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        label: L,
        name: &str,
    ) -> &mut Self {
        self.configure_mod_sub_app(label, |cfg| cfg.push_resource_name(name))
    }

    fn sync_entities_with_mod_component<L: AppLabel + Default + Copy + Eq, T: Component>(
        &mut self,
    ) -> &mut Self {
        self.register_required_components::<T, SyncToSubWorld<L>>();
        self
    }

    fn attach_sync_to_sub_app<L: AppLabel + Default + Copy + Eq>(
        &mut self,
        entity: Entity,
    ) -> bool {
        if let Ok(mut ent) = self.world_mut().get_entity_mut(entity) {
            ent.insert(SyncToSubWorld::<L>::default());
            true
        } else {
            false
        }
    }
}

/// Extension trait directly on [`SubApp`] for managing [`ModSubAppConfig`].
pub trait ModSubWorldExt {
    /// Mutates the [`ModSubAppConfig`] resource inside this [`SubApp`], initializing it if absent.
    fn configure_mod_extraction(&mut self, f: impl FnOnce(&mut ModSubAppConfig)) -> &mut Self;

    /// Adds a [`BamComponent`] component handle to extract into this [`SubApp`].
    fn register_extracted_mod_component(&mut self, component: BamComponent) -> &mut Self;

    /// Adds a component name to extract into this [`SubApp`].
    fn register_extracted_mod_component_name(&mut self, name: &str) -> &mut Self;

    /// Adds a singleton resource [`BamComponent`] handle to extract into this [`SubApp`].
    fn register_extracted_mod_resource(&mut self, resource: BamComponent) -> &mut Self;

    /// Adds a singleton resource name to extract into this [`SubApp`].
    fn register_extracted_mod_resource_name(&mut self, name: &str) -> &mut Self;
}

impl ModSubWorldExt for SubApp {
    fn configure_mod_extraction(&mut self, f: impl FnOnce(&mut ModSubAppConfig)) -> &mut Self {
        if !self.world().contains_resource::<ModSubAppConfig>() {
            self.init_resource::<ModSubAppConfig>();
        }
        let mut cfg = self.world_mut().resource_mut::<ModSubAppConfig>();
        f(&mut cfg);
        self
    }

    fn register_extracted_mod_component(&mut self, component: BamComponent) -> &mut Self {
        self.configure_mod_extraction(|cfg| cfg.push_component(component))
    }

    fn register_extracted_mod_component_name(&mut self, name: &str) -> &mut Self {
        self.configure_mod_extraction(|cfg| cfg.push_component_name(name))
    }

    fn register_extracted_mod_resource(&mut self, resource: BamComponent) -> &mut Self {
        self.configure_mod_extraction(|cfg| cfg.push_resource(resource))
    }

    fn register_extracted_mod_resource_name(&mut self, name: &str) -> &mut Self {
        self.configure_mod_extraction(|cfg| cfg.push_resource_name(name))
    }
}

/// Errors returned by `.wesl` shader, [`ShaderBuffer`], sprite material, and mesh-shader
/// pipeline registration APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModMaterialError {
    /// Alignment was zero, not a power of two, or exceeded [`isize::MAX`].
    InvalidAlignment(usize),
    /// Buffer padding or byte-size computation overflowed.
    BufferSizeOverflow,
    /// Provided `mod://` asset path was malformed or empty.
    InvalidModAssetPath(String),
    /// Reading a `.wesl` shader file from disk failed.
    ShaderReadFailed {
        /// Filesystem path that failed to load.
        path: String,
        /// Underlying I/O error message.
        message: String,
    },
    /// Required [`Assets<T>`] store could not be accessed.
    AssetStoreUnavailable(&'static str),
    /// Referenced asset or pipeline handle is not owned by the specified [`ModId`].
    ModNotOwner {
        /// Mod attempting the mutation.
        mod_id: ModId,
    },
    /// Opt-in mesh shader pipeline registration was not enabled by the host
    /// (`ModMeshShaderPlugin` / `ModMeshShaderConfig`).
    MeshShadersNotEnabled,
    /// Mod exceeded its configured mesh-shader pipeline registration quota.
    MeshPipelineQuotaExceeded {
        /// Mod attempting to register another mesh-shader pipeline.
        mod_id: ModId,
        /// Configured maximum number of pipelines per mod.
        limit: usize,
    },
    /// Provided mesh-shader or sprite descriptor failed validation.
    InvalidPipelineDescriptor(String),
    /// Referenced ECS [`Entity`] does not exist in the target [`World`].
    EntityNotFound(Entity),
    /// Referenced mod mesh-shader pipeline handle was not found.
    PipelineNotFound(u64),
}

impl fmt::Display for ModMaterialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAlignment(align) => {
                write!(
                    f,
                    "invalid ShaderBuffer alignment {align} (must be nonzero power of two)"
                )
            }
            Self::BufferSizeOverflow => write!(f, "ShaderBuffer padded size overflowed"),
            Self::InvalidModAssetPath(path) => write!(f, "invalid mod shader path: {path}"),
            Self::ShaderReadFailed { path, message } => {
                write!(f, "failed to read shader '{path}': {message}")
            }
            Self::AssetStoreUnavailable(name) => {
                write!(f, "asset store '{name}' is unavailable in World")
            }
            Self::ModNotOwner { mod_id } => {
                write!(f, "asset handle is not owned by mod {:?}", mod_id)
            }
            Self::MeshShadersNotEnabled => {
                write!(
                    f,
                    "mesh shader pipeline bridge is opt-in and not enabled in this World"
                )
            }
            Self::MeshPipelineQuotaExceeded { mod_id, limit } => {
                write!(
                    f,
                    "mod {:?} exceeded mesh shader pipeline quota of {limit}",
                    mod_id
                )
            }
            Self::InvalidPipelineDescriptor(reason) => {
                write!(f, "invalid pipeline or sprite descriptor: {reason}")
            }
            Self::EntityNotFound(entity) => {
                write!(f, "entity {entity:?} does not exist in World")
            }
            Self::PipelineNotFound(id) => {
                write!(f, "mesh shader pipeline id {id} was not found")
            }
        }
    }
}

impl std::error::Error for ModMaterialError {}

/// Source descriptor for creating or loading a mod [`Shader`] asset.
#[derive(Debug, Clone)]
pub enum WeslShaderSource {
    /// Inline WESL source text compiled via [`Shader::from_wesl`].
    InlineWesl(Cow<'static, str>),
    /// Inline WGSL source text compiled via [`Shader::from_wgsl`].
    InlineWgsl(Cow<'static, str>),
    /// Virtual `mod://<mod_name>/...` (or asset) path loaded via [`AssetServer`].
    ModAssetPath(String),
}

/// Data-driven specification for a mod-owned `.wesl` (or `.wgsl`) [`Shader`] asset.
#[derive(Debug, Clone)]
pub struct WeslShaderAssetSpec {
    /// Registry label for looking up the shader within its owning [`ModId`].
    pub label: String,
    /// Asset path or virtual module path (used for [`Shader::import_path`] and `wesl` imports).
    pub virtual_path: String,
    /// Shader source code or `mod://` asset path.
    pub source: WeslShaderSource,
    /// Preprocessor shader definitions attached to the [`Shader`].
    pub shader_defs: Vec<ShaderDefVal>,
    /// Runtime shader bounds validation setting (defaults to [`ValidateShader::Enabled`]).
    pub validate_shader: ValidateShader,
}

impl WeslShaderAssetSpec {
    /// Creates an inline `.wesl` shader specification with [`ValidateShader::Enabled`].
    pub fn from_wesl(
        label: impl Into<String>,
        virtual_path: impl Into<String>,
        source: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            label: label.into(),
            virtual_path: virtual_path.into(),
            source: WeslShaderSource::InlineWesl(source.into()),
            shader_defs: Vec::new(),
            validate_shader: ValidateShader::Enabled,
        }
    }

    /// Creates a `.wesl` shader specification referencing a `mod://<mod_name>/...` asset path.
    pub fn from_mod_path(label: impl Into<String>, mod_path: impl Into<String>) -> Self {
        let mod_path = mod_path.into();
        Self {
            label: label.into(),
            virtual_path: mod_path.clone(),
            source: WeslShaderSource::ModAssetPath(mod_path),
            shader_defs: Vec::new(),
            validate_shader: ValidateShader::Enabled,
        }
    }

    /// Reads a `.wesl` shader file synchronously from `disk_path` and stores it as inline WESL
    /// source under `virtual_path`.
    pub fn from_wesl_file(
        label: impl Into<String>,
        virtual_path: impl Into<String>,
        disk_path: impl AsRef<Path>,
    ) -> Result<Self, ModMaterialError> {
        let path_ref = disk_path.as_ref();
        let text = std::fs::read_to_string(path_ref).map_err(|err| {
            ModMaterialError::ShaderReadFailed {
                path: path_ref.display().to_string(),
                message: err.to_string(),
            }
        })?;
        Ok(Self::from_wesl(label, virtual_path, text))
    }

    /// Appends preprocessor [`ShaderDefVal`] entries to this shader specification.
    pub fn with_shader_defs(mut self, defs: impl IntoIterator<Item = ShaderDefVal>) -> Self {
        self.shader_defs.extend(defs);
        self
    }

    /// Overrides the [`ValidateShader`] policy for this shader specification.
    pub fn with_validate_shader(mut self, validate: ValidateShader) -> Self {
        self.validate_shader = validate;
        self
    }

    fn build_inline_shader(&self) -> Option<Shader> {
        let mut shader = match &self.source {
            WeslShaderSource::InlineWesl(src) => {
                Shader::from_wesl(src.clone(), self.virtual_path.clone())
            }
            WeslShaderSource::InlineWgsl(src) => {
                Shader::from_wgsl(src.clone(), self.virtual_path.clone())
            }
            WeslShaderSource::ModAssetPath(_) => return None,
        };
        shader.shader_defs = self.shader_defs.clone();
        shader.validate_shader = self.validate_shader.clone();
        Some(shader)
    }
}

/// Initial contents for a mod-owned [`ShaderBuffer`] asset.
#[derive(Debug, Clone)]
pub enum ModShaderBufferInit {
    /// Initialized CPU bytes backed by an [`AlignedVec`].
    Bytes(Vec<u8>),
    /// Uninitialized GPU storage buffer of the given byte length.
    Uninitialized(u64),
}

/// Data-driven specification for a mod-owned [`ShaderBuffer`] backed by [`AlignedVec`].
#[derive(Debug, Clone)]
pub struct ModShaderBufferSpec {
    /// Registry/debug label for this buffer.
    pub label: String,
    /// Power-of-two byte alignment for [`AlignedVec`] (defaults to `16`).
    pub align: usize,
    /// Initial data payload or uninitialized byte length.
    pub data: ModShaderBufferInit,
    /// Legal wgpu [`BufferUsages`] flags (defaults to `STORAGE | COPY_SRC | COPY_DST`).
    pub buffer_usage: BufferUsages,
    /// Bevy [`RenderAssetUsages`] flags.
    pub asset_usage: RenderAssetUsages,
    /// Whether existing GPU contents are copied on buffer resize.
    pub copy_on_resize: bool,
}

impl ModShaderBufferSpec {
    /// Creates an initialized [`ModShaderBufferSpec`] with 16-byte [`AlignedVec`] alignment.
    pub fn from_bytes(label: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self::from_aligned_bytes(label, 16, bytes)
    }

    /// Creates an initialized [`ModShaderBufferSpec`] with explicit power-of-two `align`.
    pub fn from_aligned_bytes(label: impl Into<String>, align: usize, bytes: Vec<u8>) -> Self {
        Self {
            label: label.into(),
            align,
            data: ModShaderBufferInit::Bytes(bytes),
            buffer_usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST,
            asset_usage: RenderAssetUsages::default(),
            copy_on_resize: false,
        }
    }

    /// Creates an initialized [`ModShaderBufferSpec`] from little-endian `f32` values.
    pub fn from_f32_slice(label: impl Into<String>, align: usize, values: &[f32]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for &v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Self::from_aligned_bytes(label, align, bytes)
    }

    /// Creates an uninitialized [`ModShaderBufferSpec`] of `size` bytes.
    pub fn uninitialized(label: impl Into<String>, size: u64) -> Self {
        Self {
            label: label.into(),
            align: 16,
            data: ModShaderBufferInit::Uninitialized(size),
            buffer_usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST,
            asset_usage: RenderAssetUsages::default(),
            copy_on_resize: false,
        }
    }

    /// Overrides the [`AlignedVec`] alignment requirement.
    pub fn with_align(mut self, align: usize) -> Self {
        self.align = align;
        self
    }

    /// Overrides [`BufferUsages`].
    pub fn with_buffer_usage(mut self, usage: BufferUsages) -> Self {
        self.buffer_usage = usage;
        self
    }

    /// Overrides [`RenderAssetUsages`].
    pub fn with_asset_usage(mut self, usage: RenderAssetUsages) -> Self {
        self.asset_usage = usage;
        self
    }

    /// Sets [`ShaderBuffer::copy_on_resize`].
    pub fn with_copy_on_resize(mut self, copy_on_resize: bool) -> Self {
        self.copy_on_resize = copy_on_resize;
        self
    }

    /// Builds the [`ShaderBuffer`], allocating an [`AlignedVec`] aligned to `self.align` and
    /// padding byte lengths up to a 4-byte multiple as required by `wgpu`.
    pub fn build_shader_buffer(&self) -> Result<ShaderBuffer, ModMaterialError> {
        let data = match &self.data {
            ModShaderBufferInit::Bytes(bytes) => {
                let aligned = build_aligned_vec(self.align, bytes)?;
                let buffer_size = aligned.len() as u64;
                ShaderBufferData::Initialized {
                    data: aligned,
                    buffer_size,
                }
            }
            ModShaderBufferInit::Uninitialized(size) => {
                let padded = size
                    .checked_add(3)
                    .ok_or(ModMaterialError::BufferSizeOverflow)?
                    & !3;
                ShaderBufferData::Uninitialized(padded)
            }
        };

        Ok(ShaderBuffer {
            data,
            label: Cow::Owned(self.label.clone()),
            buffer_usage: self.buffer_usage,
            asset_usage: self.asset_usage,
            copy_on_resize: self.copy_on_resize,
        })
    }
}

fn build_aligned_vec(align: usize, bytes: &[u8]) -> Result<AlignedVec, ModMaterialError> {
    if align == 0 || !align.is_power_of_two() || align >= isize::MAX as usize {
        return Err(ModMaterialError::InvalidAlignment(align));
    }
    let padded_len = bytes
        .len()
        .checked_add(3)
        .map(|n| n & !3)
        .ok_or(ModMaterialError::BufferSizeOverflow)?;
    let max_size = (isize::MAX as usize) + 1 - align;
    if padded_len > max_size {
        return Err(ModMaterialError::BufferSizeOverflow);
    }
    let mut vec = AlignedVec::with_capacity(align, padded_len);
    vec.extend_from_slice(bytes);
    let pad = padded_len - bytes.len();
    if pad > 0 {
        vec.extend_from_slice(&[0u8; 3][..pad]);
    }
    Ok(vec)
}

/// Uniform parameters attached to [`ModSpriteExtension2d`].
#[derive(ShaderType, Reflect, Debug, Clone, Copy, PartialEq)]
pub struct ModMaterialParams {
    /// RGBA tint multiplier (`[r, g, b, a]`).
    pub tint: [f32; 4],
    /// Scalar effect intensity.
    pub intensity: f32,
    /// Animation time scale.
    pub time_scale: f32,
    /// Reserved alignment padding.
    pub _pad0: f32,
    /// Reserved alignment padding.
    pub _pad1: f32,
}

impl Default for ModMaterialParams {
    fn default() -> Self {
        Self {
            tint: [1.0, 1.0, 1.0, 1.0],
            intensity: 1.0,
            time_scale: 1.0,
            _pad0: 0.0,
            _pad1: 0.0,
        }
    }
}

/// Concrete `2D` [`MaterialExtension2d`] combining uniform [`ModMaterialParams`] with an
/// [`AlignedVec`]-backed storage [`Handle<ShaderBuffer>`] for [`SpriteMaterial`](bevy_sprite_render::SpriteMaterial) /
/// [`ExtendedMaterial2d`].
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
#[reflect(Debug, Clone)]
pub struct ModSpriteExtension2d {
    /// Storage buffer bound at `@binding(100)` (`var<storage, read>`).
    #[storage(100, read_only)]
    pub buffer: Handle<ShaderBuffer>,
    /// Uniform parameters bound at `@binding(101)`.
    #[uniform(101)]
    pub params: ModMaterialParams,
}

impl MaterialExtension2d for ModSpriteExtension2d {}

/// Data-driven descriptor for instantiating a Bevy `0.20` 2D [`SpriteMeshMaterial`] (and optional
/// custom `.wesl` shader + [`ShaderBuffer`] metadata).
#[derive(Debug, Clone)]
pub struct ModSpriteMaterialSpec {
    /// Registry label for lookup under the owning [`ModId`].
    pub label: String,
    /// Optional sprite base texture handle.
    pub image: Option<Handle<Image>>,
    /// 2D alpha blending mode.
    pub alpha_mode: AlphaMode2d,
    /// Horizontal UV flip.
    pub flip_x: bool,
    /// Vertical UV flip.
    pub flip_y: bool,
    /// Sprite tiling / slicing mode.
    pub image_mode: SpriteImageMode,
    /// Linear/sRGBA tint channels (`[r, g, b, a]`).
    pub color_rgba: [f32; 4],
    /// Optional custom `.wesl` shader associated with this material.
    pub shader: Option<Handle<Shader>>,
    /// Optional [`ShaderBuffer`] associated with this material.
    pub shader_buffer: Option<Handle<ShaderBuffer>>,
    /// Optional full native [`SpriteMeshMaterial`] override.
    pub override_material: Option<SpriteMeshMaterial>,
}

impl ModSpriteMaterialSpec {
    /// Creates a default [`ModSpriteMaterialSpec`] with the given registry `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            image: None,
            alpha_mode: AlphaMode2d::default(),
            flip_x: false,
            flip_y: false,
            image_mode: SpriteImageMode::default(),
            color_rgba: [1.0, 1.0, 1.0, 1.0],
            shader: None,
            shader_buffer: None,
            override_material: None,
        }
    }

    /// Creates a [`ModSpriteMaterialSpec`] wrapping an existing [`SpriteMeshMaterial`].
    pub fn from_material(label: impl Into<String>, material: SpriteMeshMaterial) -> Self {
        Self {
            label: label.into(),
            override_material: Some(material),
            ..Self::new("")
        }
    }

    /// Associates a custom [`Handle<Shader>`] with this material descriptor.
    pub fn with_shader(mut self, shader: Handle<Shader>) -> Self {
        self.shader = Some(shader);
        self
    }

    /// Associates a [`Handle<ShaderBuffer>`] with this material descriptor.
    pub fn with_shader_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.shader_buffer = Some(buffer);
        self
    }

    /// Sets [`AlphaMode2d`] for the generated [`SpriteMeshMaterial`].
    pub fn with_alpha_mode(mut self, alpha_mode: AlphaMode2d) -> Self {
        self.alpha_mode = alpha_mode;
        self
    }

    /// Sets `flip_x` / `flip_y` flags on the generated [`SpriteMeshMaterial`].
    pub fn with_flip(mut self, flip_x: bool, flip_y: bool) -> Self {
        self.flip_x = flip_x;
        self.flip_y = flip_y;
        self
    }

    /// Sets `color_rgba` (`[r, g, b, a]`).
    pub fn with_color_rgba(mut self, rgba: [f32; 4]) -> Self {
        self.color_rgba = rgba;
        self
    }

    /// Converts this descriptor into a Bevy [`SpriteMeshMaterial`].
    pub fn build_material(&self) -> SpriteMeshMaterial {
        if let Some(mat) = &self.override_material {
            return mat.clone();
        }
        let mut mat = SpriteMeshMaterial {
            image: self.image.clone().unwrap_or_default(),
            alpha_mode: self.alpha_mode,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
            image_mode: self.image_mode.clone(),
            ..Default::default()
        };
        mat.color = bevy_color::Color::srgba(
            self.color_rgba[0],
            self.color_rgba[1],
            self.color_rgba[2],
            self.color_rgba[3],
        );
        mat
    }
}

/// Data-driven descriptor for instantiating a Bevy `0.20` 3D [`StandardMaterial`] (plus optional
/// custom `.wesl` shader + [`ShaderBuffer`] metadata).
#[derive(Debug, Clone)]
pub struct ModStandardMaterialSpec {
    /// Registry label for lookup under the owning [`ModId`].
    pub label: String,
    /// Perceptual roughness in `[0.089, 1.0]`.
    pub perceptual_roughness: f32,
    /// Metallic factor in `[0.0, 1.0]`.
    pub metallic: f32,
    /// Specular reflectance in `[0.0, 1.0]`.
    pub reflectance: f32,
    /// 3D alpha blending mode.
    pub alpha_mode: AlphaMode,
    /// Whether backface culling is disabled.
    pub double_sided: bool,
    /// Whether lighting is skipped for this material.
    pub unlit: bool,
    /// Optional base-color texture handle.
    pub base_color_texture: Option<Handle<Image>>,
    /// Optional emissive texture handle.
    pub emissive_texture: Option<Handle<Image>>,
    /// Base color sRGBA channels.
    pub base_color_rgba: [f32; 4],
    /// Linear emissive RGBA channels.
    pub emissive_rgba: [f32; 4],
    /// Optional custom `.wesl` shader associated with this material.
    pub shader: Option<Handle<Shader>>,
    /// Optional [`ShaderBuffer`] associated with this material.
    pub shader_buffer: Option<Handle<ShaderBuffer>>,
    /// Optional full native [`StandardMaterial`] override.
    pub override_material: Option<StandardMaterial>,
}

impl ModStandardMaterialSpec {
    /// Creates a default [`ModStandardMaterialSpec`] with the given registry `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            perceptual_roughness: 0.5,
            metallic: 0.0,
            reflectance: 0.5,
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            unlit: false,
            base_color_texture: None,
            emissive_texture: None,
            base_color_rgba: [1.0, 1.0, 1.0, 1.0],
            emissive_rgba: [0.0, 0.0, 0.0, 1.0],
            shader: None,
            shader_buffer: None,
            override_material: None,
        }
    }

    /// Creates a [`ModStandardMaterialSpec`] wrapping an existing [`StandardMaterial`].
    pub fn from_material(label: impl Into<String>, material: StandardMaterial) -> Self {
        Self {
            label: label.into(),
            override_material: Some(material),
            ..Self::new("")
        }
    }

    /// Sets metallic and perceptual roughness PBR scalars.
    pub fn with_pbr(mut self, metallic: f32, perceptual_roughness: f32) -> Self {
        self.metallic = metallic;
        self.perceptual_roughness = perceptual_roughness;
        self
    }

    /// Sets [`AlphaMode`].
    pub fn with_alpha_mode(mut self, alpha_mode: AlphaMode) -> Self {
        self.alpha_mode = alpha_mode;
        self
    }

    /// Sets `unlit` and `double_sided` flags.
    pub fn with_flags(mut self, unlit: bool, double_sided: bool) -> Self {
        self.unlit = unlit;
        self.double_sided = double_sided;
        self
    }

    /// Sets `base_color_rgba` (`[r, g, b, a]`).
    pub fn with_base_color_rgba(mut self, rgba: [f32; 4]) -> Self {
        self.base_color_rgba = rgba;
        self
    }

    /// Sets `emissive_rgba` (`[r, g, b, a]`).
    pub fn with_emissive_rgba(mut self, rgba: [f32; 4]) -> Self {
        self.emissive_rgba = rgba;
        self
    }

    /// Associates a custom [`Handle<Shader>`] with this material descriptor.
    pub fn with_shader(mut self, shader: Handle<Shader>) -> Self {
        self.shader = Some(shader);
        self
    }

    /// Associates a [`Handle<ShaderBuffer>`] with this material descriptor.
    pub fn with_shader_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.shader_buffer = Some(buffer);
        self
    }

    /// Converts this descriptor into a Bevy [`StandardMaterial`].
    pub fn build_material(&self) -> StandardMaterial {
        if let Some(mat) = &self.override_material {
            return mat.clone();
        }
        let mut mat = StandardMaterial {
            perceptual_roughness: self.perceptual_roughness,
            metallic: self.metallic,
            reflectance: self.reflectance,
            alpha_mode: self.alpha_mode,
            double_sided: self.double_sided,
            unlit: self.unlit,
            base_color_texture: self.base_color_texture.clone(),
            emissive_texture: self.emissive_texture.clone(),
            ..Default::default()
        };
        mat.base_color = bevy_color::Color::srgba(
            self.base_color_rgba[0],
            self.base_color_rgba[1],
            self.base_color_rgba[2],
            self.base_color_rgba[3],
        );
        mat.emissive = bevy_color::LinearRgba::new(
            self.emissive_rgba[0],
            self.emissive_rgba[1],
            self.emissive_rgba[2],
            self.emissive_rgba[3],
        );
        mat
    }
}

/// Recorded sprite material metadata owned by a loaded mod.
#[derive(Clone, Debug)]
pub struct ModSpriteMaterialEntry {
    /// Material label within the mod.
    pub label: String,
    /// Registered [`SpriteMeshMaterial`] handle.
    pub handle: Handle<SpriteMeshMaterial>,
    /// Optional custom `.wesl` shader handle linked to this material.
    pub shader: Option<Handle<Shader>>,
    /// Optional [`ShaderBuffer`] handle linked to this material.
    pub shader_buffer: Option<Handle<ShaderBuffer>>,
}

/// Recorded 3D standard material metadata owned by a loaded mod.
#[derive(Clone, Debug)]
pub struct ModStandardMaterialEntry {
    /// Material label within the mod.
    pub label: String,
    /// Registered [`StandardMaterial`] handle.
    pub handle: Handle<StandardMaterial>,
    /// Optional custom `.wesl` shader handle linked to this material.
    pub shader: Option<Handle<Shader>>,
    /// Optional [`ShaderBuffer`] handle linked to this material.
    pub shader_buffer: Option<Handle<ShaderBuffer>>,
}

type CustomAssetCleanupFn = Box<dyn FnOnce(&mut World) + Send + Sync>;

type Extended2dMaterialHandle =
    Handle<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>;

/// Host [`Resource`] tracking all [`ModId`]-scoped `.wesl` [`Shader`]s, [`ShaderBuffer`]s,
/// [`SpriteMeshMaterial`]s, [`StandardMaterial`]s, and custom asset handles.
///
/// Automatically purged during [`ModWorldExt::unload_mod`](bevy_abi_mod_host::ModWorldExt::unload_mod),
/// [`ModWorldExt::unload_all_mods`](bevy_abi_mod_host::ModWorldExt::unload_all_mods), and cascading
/// [`ModWorldExt::reload_mod`](bevy_abi_mod_host::ModWorldExt::reload_mod).
#[derive(Resource, Default)]
pub struct ModWeslMaterialRegistry {
    shaders: HashMap<ModId, Vec<(String, Handle<Shader>)>>,
    buffers: HashMap<ModId, Vec<(String, Handle<ShaderBuffer>)>>,
    sprite_materials: HashMap<ModId, Vec<ModSpriteMaterialEntry>>,
    standard_materials: HashMap<ModId, Vec<ModStandardMaterialEntry>>,
    sprite_extensions: HashMap<ModId, Vec<(String, Handle<ModSpriteExtension2d>)>>,
    extended_2d_materials: HashMap<ModId, Vec<(String, Extended2dMaterialHandle)>>,
    pub(crate) wesl_sprite_extensions: HashMap<ModId, Vec<ModWeslSpriteExtensionEntry>>,
    pub(crate) wesl_extended_2d_materials: HashMap<ModId, Vec<ModWeslExtendedMaterialEntry>>,
    custom_cleanups: HashMap<ModId, Vec<CustomAssetCleanupFn>>,
}

impl ModWeslMaterialRegistry {
    /// Creates an empty [`ModWeslMaterialRegistry`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of [`Shader`] assets currently tracked for `mod_id`.
    pub fn shader_count_for_mod(&self, mod_id: ModId) -> usize {
        self.shaders.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the number of [`ShaderBuffer`] assets currently tracked for `mod_id`.
    pub fn buffer_count_for_mod(&self, mod_id: ModId) -> usize {
        self.buffers.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the number of [`SpriteMeshMaterial`] assets currently tracked for `mod_id`.
    pub fn sprite_material_count_for_mod(&self, mod_id: ModId) -> usize {
        self.sprite_materials.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the number of [`StandardMaterial`] assets currently tracked for `mod_id`.
    pub fn standard_material_count_for_mod(&self, mod_id: ModId) -> usize {
        self.standard_materials.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the number of [`ModWeslSpriteExtension2d`] assets currently tracked for `mod_id`.
    pub fn wesl_sprite_extension_count_for_mod(&self, mod_id: ModId) -> usize {
        self.wesl_sprite_extensions.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the number of [`ModExtendedSpriteMaterial2d`] assets currently tracked for `mod_id`.
    pub fn wesl_extended_2d_material_count_for_mod(&self, mod_id: ModId) -> usize {
        self.wesl_extended_2d_materials
            .get(&mod_id)
            .map_or(0, Vec::len)
    }

    /// Returns the total number of tracked shader, buffer, and material assets for `mod_id`.
    pub fn total_assets_for_mod(&self, mod_id: ModId) -> usize {
        self.shader_count_for_mod(mod_id)
            + self.buffer_count_for_mod(mod_id)
            + self.sprite_material_count_for_mod(mod_id)
            + self.standard_material_count_for_mod(mod_id)
            + self.custom_cleanups.get(&mod_id).map_or(0, Vec::len)
            + self.sprite_extensions.get(&mod_id).map_or(0, Vec::len)
            + self.extended_2d_materials.get(&mod_id).map_or(0, Vec::len)
            + self.wesl_sprite_extension_count_for_mod(mod_id)
            + self.wesl_extended_2d_material_count_for_mod(mod_id)
    }

    /// Looks up a mod-owned [`Handle<Shader>`] by `mod_id` and `label`.
    pub fn get_shader(&self, mod_id: ModId, label: &str) -> Option<&Handle<Shader>> {
        self.shaders
            .get(&mod_id)?
            .iter()
            .find_map(|(k, h)| (k == label).then_some(h))
    }

    /// Looks up a mod-owned [`Handle<ShaderBuffer>`] by `mod_id` and `label`.
    pub fn get_buffer(&self, mod_id: ModId, label: &str) -> Option<&Handle<ShaderBuffer>> {
        self.buffers
            .get(&mod_id)?
            .iter()
            .find_map(|(k, h)| (k == label).then_some(h))
    }

    /// Looks up a mod-owned [`ModSpriteMaterialEntry`] by `mod_id` and `label`.
    pub fn get_sprite_material(
        &self,
        mod_id: ModId,
        label: &str,
    ) -> Option<&ModSpriteMaterialEntry> {
        self.sprite_materials
            .get(&mod_id)?
            .iter()
            .find(|entry| entry.label == label)
    }

    /// Looks up a mod-owned [`ModStandardMaterialEntry`] by `mod_id` and `label`.
    pub fn get_standard_material(
        &self,
        mod_id: ModId,
        label: &str,
    ) -> Option<&ModStandardMaterialEntry> {
        self.standard_materials
            .get(&mod_id)?
            .iter()
            .find(|entry| entry.label == label)
    }

    /// Looks up a mod-owned [`ModWeslSpriteExtensionEntry`] by `mod_id` and `label`.
    pub fn get_wesl_sprite_extension(
        &self,
        mod_id: ModId,
        label: &str,
    ) -> Option<&ModWeslSpriteExtensionEntry> {
        self.wesl_sprite_extensions
            .get(&mod_id)?
            .iter()
            .find(|entry| entry.label == label)
    }

    /// Looks up a mod-owned [`ModWeslExtendedMaterialEntry`] by `mod_id` and `label`.
    pub fn get_wesl_extended_2d_material(
        &self,
        mod_id: ModId,
        label: &str,
    ) -> Option<&ModWeslExtendedMaterialEntry> {
        self.wesl_extended_2d_materials
            .get(&mod_id)?
            .iter()
            .find(|entry| entry.label == label)
    }

    /// Returns `true` if `shader_id` is currently owned by `mod_id`.
    pub fn is_shader_owned_by(&self, mod_id: ModId, shader_id: AssetId<Shader>) -> bool {
        self.shaders
            .get(&mod_id)
            .is_some_and(|list| list.iter().any(|(_, h)| h.id() == shader_id))
    }

    /// Returns `true` if `buffer_id` is currently owned by `mod_id`.
    pub fn is_buffer_owned_by(&self, mod_id: ModId, buffer_id: AssetId<ShaderBuffer>) -> bool {
        self.buffers
            .get(&mod_id)
            .is_some_and(|list| list.iter().any(|(_, h)| h.id() == buffer_id))
    }

    /// Returns `true` if `ext_id` is a [`ModSpriteExtension2d`] owned by `mod_id`.
    pub fn is_sprite_extension_owned_by(
        &self,
        mod_id: ModId,
        ext_id: AssetId<ModSpriteExtension2d>,
    ) -> bool {
        self.sprite_extensions
            .get(&mod_id)
            .is_some_and(|list| list.iter().any(|(_, h)| h.id() == ext_id))
    }

    /// Returns `true` if `ext_id` is a [`ModWeslSpriteExtension2d`] owned by `mod_id`.
    pub fn is_wesl_sprite_extension_owned_by(
        &self,
        mod_id: ModId,
        ext_id: AssetId<ModWeslSpriteExtension2d>,
    ) -> bool {
        self.wesl_sprite_extensions
            .get(&mod_id)
            .is_some_and(|list| list.iter().any(|entry| entry.handle.id() == ext_id))
    }

    /// Returns `true` if `mat_id` is a [`ModExtendedSpriteMaterial2d`] owned by `mod_id`.
    pub fn is_wesl_extended_2d_material_owned_by(
        &self,
        mod_id: ModId,
        mat_id: AssetId<ModExtendedSpriteMaterial2d>,
    ) -> bool {
        self.wesl_extended_2d_materials
            .get(&mod_id)
            .is_some_and(|list| list.iter().any(|entry| entry.handle.id() == mat_id))
    }
}

/// Initializes [`ModWeslMaterialRegistry`] and headless-safe [`Assets`] stores for `.wesl`
/// [`Shader`], [`ShaderBuffer`], [`SpriteMeshMaterial`], [`ModWeslSpriteExtension2d`],
/// [`ModExtendedSpriteMaterial2d`], and [`StandardMaterial`] assets.
#[derive(Default, Debug, Clone, Copy)]
pub struct ModWeslMaterialPlugin;

const WESL_MATERIAL_UNLOAD_HOOK_KEY: &str = "bevy_abi_mod_render::wesl_materials";

fn cleanup_mod_wesl_materials_hook(world: &mut World, mod_id: ModId) {
    let _ = cleanup_mod_wesl_materials_in_world(world, mod_id);
}

impl Plugin for ModWeslMaterialPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ModWeslMaterialRegistry>() {
            app.insert_resource(ModWeslMaterialRegistry::new());
        }
        register_mod_unload_hook_in_world(
            app.world_mut(),
            WESL_MATERIAL_UNLOAD_HOOK_KEY,
            cleanup_mod_wesl_materials_hook,
        );
        ensure_asset_store_in_app::<Shader>(app);
        ensure_asset_store_in_app::<ShaderBuffer>(app);
        ensure_asset_store_in_app::<SpriteMeshMaterial>(app);
        ensure_asset_store_in_app::<StandardMaterial>(app);
        ensure_asset_store_in_app::<ModSpriteExtension2d>(app);
        ensure_asset_store_in_app::<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>(
            app,
        );
        ensure_asset_store_in_app::<ModWeslSpriteExtension2d>(app);
        ensure_asset_store_in_app::<ModExtendedSpriteMaterial2d>(app);
    }
}

pub(crate) fn ensure_asset_store_in_app<A: Asset>(app: &mut App) {
    if !app.world().contains_resource::<Assets<A>>() {
        if app.world().contains_resource::<AssetServer>() {
            app.init_asset::<A>();
        } else {
            app.world_mut().init_resource::<Assets<A>>();
        }
    }
}

pub(crate) fn ensure_asset_store_in_world<A: Asset>(world: &mut World) {
    if !world.contains_resource::<Assets<A>>() {
        let assets = Assets::<A>::default();
        if let Some(server) = world.get_resource::<AssetServer>().cloned() {
            server.register_asset(&assets);
        }
        world.insert_resource(assets);
    }
}

pub(crate) fn ensure_material_registry(world: &mut World) {
    if !world.contains_resource::<ModWeslMaterialRegistry>() {
        world.insert_resource(ModWeslMaterialRegistry::new());
    }
    register_mod_unload_hook_in_world(
        world,
        WESL_MATERIAL_UNLOAD_HOOK_KEY,
        cleanup_mod_wesl_materials_hook,
    );
}

/// Registers a [`ModId`]-owned `.wesl` (or `.wgsl`) [`Shader`] in `world` and returns its
/// strong [`Handle<Shader>`].
pub fn register_mod_wesl_shader_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: WeslShaderAssetSpec,
) -> Result<Handle<Shader>, ModMaterialError> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<Shader>(world);

    let handle = match &spec.source {
        WeslShaderSource::ModAssetPath(path) => {
            if path.is_empty() {
                return Err(ModMaterialError::InvalidModAssetPath(path.clone()));
            }
            let server = world
                .get_resource::<AssetServer>()
                .ok_or(ModMaterialError::AssetStoreUnavailable("AssetServer"))?;
            server.load::<Shader>(path.clone())
        }
        WeslShaderSource::InlineWesl(_) | WeslShaderSource::InlineWgsl(_) => {
            let shader = spec
                .build_inline_shader()
                .ok_or(ModMaterialError::AssetStoreUnavailable("Shader"))?;
            world.resource_mut::<Assets<Shader>>().add(shader)
        }
    };

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .shaders
        .entry(mod_id)
        .or_default()
        .push((spec.label, handle.clone()));

    Ok(handle)
}

/// Registers a [`ModId`]-owned [`ShaderBuffer`] backed by [`AlignedVec`] in `world` and returns
/// its strong [`Handle<ShaderBuffer>`].
pub fn register_mod_shader_buffer_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModShaderBufferSpec,
) -> Result<Handle<ShaderBuffer>, ModMaterialError> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<ShaderBuffer>(world);

    let buffer = spec.build_shader_buffer()?;
    let handle = world.resource_mut::<Assets<ShaderBuffer>>().add(buffer);

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .buffers
        .entry(mod_id)
        .or_default()
        .push((spec.label, handle.clone()));

    Ok(handle)
}

/// Replaces the CPU payload of a [`ModId`]-owned [`ShaderBuffer`] while preserving its
/// [`AlignedVec`] alignment and padding its size to a 4-byte multiple.
pub fn update_mod_shader_buffer_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ShaderBuffer>,
    bytes: &[u8],
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_buffer_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }

    let mut buffers = world.get_resource_mut::<Assets<ShaderBuffer>>().ok_or(
        ModMaterialError::AssetStoreUnavailable("Assets<ShaderBuffer>"),
    )?;
    let mut buffer = buffers
        .get_mut(handle.id())
        .ok_or(ModMaterialError::AssetStoreUnavailable("ShaderBuffer"))?;

    let align = match &buffer.data {
        ShaderBufferData::Initialized { data, .. } => data.alignment(),
        ShaderBufferData::Uninitialized(_) => 16,
    };
    let aligned = build_aligned_vec(align, bytes)?;
    let buffer_size = aligned.len() as u64;
    buffer.data = ShaderBufferData::Initialized {
        data: aligned,
        buffer_size,
    };
    Ok(())
}

/// Registers a [`ModId`]-owned [`SpriteMeshMaterial`] from a [`ModSpriteMaterialSpec`] and
/// returns its strong [`Handle<SpriteMeshMaterial>`].
pub fn register_mod_sprite_material_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModSpriteMaterialSpec,
) -> Result<Handle<SpriteMeshMaterial>, ModMaterialError> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<SpriteMeshMaterial>(world);

    let material = spec.build_material();
    let handle = world
        .resource_mut::<Assets<SpriteMeshMaterial>>()
        .add(material);

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .sprite_materials
        .entry(mod_id)
        .or_default()
        .push(ModSpriteMaterialEntry {
            label: spec.label,
            handle: handle.clone(),
            shader: spec.shader,
            shader_buffer: spec.shader_buffer,
        });

    Ok(handle)
}

/// Registers a [`ModId`]-owned [`ModSpriteExtension2d`] and paired
/// [`ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>`], returning both handles.
pub fn register_mod_extended_sprite_material_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModSpriteMaterialSpec,
    extension: ModSpriteExtension2d,
) -> Result<
    (
        Handle<ModSpriteExtension2d>,
        Handle<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>,
    ),
    ModMaterialError,
> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<ModSpriteExtension2d>(world);
    ensure_asset_store_in_world::<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>(
        world,
    );

    let base = spec.build_material();
    let ext_handle = world
        .resource_mut::<Assets<ModSpriteExtension2d>>()
        .add(extension.clone());
    let extended_handle = world
        .resource_mut::<Assets<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>()
        .add(ExtendedMaterial2d { base, extension });

    let mut reg = world.resource_mut::<ModWeslMaterialRegistry>();
    reg.sprite_extensions
        .entry(mod_id)
        .or_default()
        .push((spec.label.clone(), ext_handle.clone()));
    reg.extended_2d_materials
        .entry(mod_id)
        .or_default()
        .push((spec.label, extended_handle.clone()));

    Ok((ext_handle, extended_handle))
}

/// Registers a [`ModId`]-owned [`StandardMaterial`] from a [`ModStandardMaterialSpec`] and returns
/// its strong [`Handle<StandardMaterial>`].
pub fn register_mod_standard_material_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModStandardMaterialSpec,
) -> Result<Handle<StandardMaterial>, ModMaterialError> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<StandardMaterial>(world);

    let material = spec.build_material();
    let handle = world
        .resource_mut::<Assets<StandardMaterial>>()
        .add(material);

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .standard_materials
        .entry(mod_id)
        .or_default()
        .push(ModStandardMaterialEntry {
            label: spec.label,
            handle: handle.clone(),
            shader: spec.shader,
            shader_buffer: spec.shader_buffer,
        });

    Ok(handle)
}

/// Registers an arbitrary typed [`Asset`] `asset` owned by `mod_id`, scheduling automatic removal
/// from [`Assets<A>`] when `mod_id` is unloaded.
pub fn register_mod_custom_asset_in_world<A: Asset>(
    world: &mut World,
    mod_id: ModId,
    asset: A,
) -> Handle<A> {
    ensure_material_registry(world);
    ensure_asset_store_in_world::<A>(world);

    let handle = world.resource_mut::<Assets<A>>().add(asset);
    let asset_id = handle.id();
    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .custom_cleanups
        .entry(mod_id)
        .or_default()
        .push(Box::new(move |w| {
            if let Some(mut assets) = w.get_resource_mut::<Assets<A>>() {
                let _ = assets.remove(asset_id);
            }
        }));
    handle
}

/// Removes all `.wesl` [`Shader`], [`ShaderBuffer`], [`SpriteMeshMaterial`], [`StandardMaterial`],
/// and custom typed assets owned by `mod_id` from `world`'s [`Assets`] stores, returning the
/// number of cleaned assets.
pub fn cleanup_mod_wesl_materials_in_world(world: &mut World, mod_id: ModId) -> usize {
    let shaders;
    let buffers;
    let sprite_mats;
    let standard_mats;
    let sprite_exts;
    let ext_2d_mats;
    let wesl_sprite_exts;
    let wesl_ext_2d_mats;
    let custom;
    {
        let Some(mut reg) = world.get_resource_mut::<ModWeslMaterialRegistry>() else {
            return 0;
        };
        shaders = reg.shaders.remove(&mod_id).unwrap_or_default();
        buffers = reg.buffers.remove(&mod_id).unwrap_or_default();
        sprite_mats = reg.sprite_materials.remove(&mod_id).unwrap_or_default();
        standard_mats = reg.standard_materials.remove(&mod_id).unwrap_or_default();
        sprite_exts = reg.sprite_extensions.remove(&mod_id).unwrap_or_default();
        ext_2d_mats = reg
            .extended_2d_materials
            .remove(&mod_id)
            .unwrap_or_default();
        wesl_sprite_exts = reg
            .wesl_sprite_extensions
            .remove(&mod_id)
            .unwrap_or_default();
        wesl_ext_2d_mats = reg
            .wesl_extended_2d_materials
            .remove(&mod_id)
            .unwrap_or_default();
        custom = reg.custom_cleanups.remove(&mod_id).unwrap_or_default();
    }

    let mut removed = 0usize;
    let mut removed_shader_ids = Vec::with_capacity(shaders.len());
    let mut removed_spec_slots = Vec::new();

    if let Some(mut assets) = world.get_resource_mut::<Assets<Shader>>() {
        for (_, handle) in shaders {
            let id = handle.id();
            removed_shader_ids.push(id);
            if assets.remove(id).is_some() {
                removed += 1;
            }
        }
    } else {
        for (_, handle) in shaders {
            removed_shader_ids.push(handle.id());
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<ShaderBuffer>>() {
        for (_, handle) in buffers {
            if assets.remove(handle.id()).is_some() {
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<SpriteMeshMaterial>>() {
        for entry in sprite_mats {
            if assets.remove(entry.handle.id()).is_some() {
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<StandardMaterial>>() {
        for entry in standard_mats {
            if assets.remove(entry.handle.id()).is_some() {
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<ModSpriteExtension2d>>() {
        for (_, handle) in sprite_exts {
            if assets.remove(handle.id()).is_some() {
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world
        .get_resource_mut::<Assets<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>()
    {
        for (_, handle) in ext_2d_mats {
            if assets.remove(handle.id()).is_some() {
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<ModWeslSpriteExtension2d>>() {
        for entry in wesl_sprite_exts {
            if let Some(ext) = assets.remove(entry.handle.id()) {
                let slot = lookup_wesl_specialization_slot(&ext);
                if slot != 0 {
                    removed_spec_slots.push(slot);
                }
                removed += 1;
            }
        }
    }
    if let Some(mut assets) = world.get_resource_mut::<Assets<ModExtendedSpriteMaterial2d>>() {
        for entry in wesl_ext_2d_mats {
            if let Some(mat) = assets.remove(entry.handle.id()) {
                let slot = lookup_wesl_specialization_slot(&mat.extension);
                if slot != 0 {
                    removed_spec_slots.push(slot);
                }
                removed += 1;
            }
        }
    }
    let mut live_slots = HashSet::default();
    if !removed_spec_slots.is_empty() {
        if let Some(assets) = world.get_resource::<Assets<ModWeslSpriteExtension2d>>() {
            for (_, ext) in assets.iter() {
                let slot = lookup_wesl_specialization_slot(ext);
                if slot != 0 {
                    live_slots.insert(slot);
                }
            }
        }
        if let Some(assets) = world.get_resource::<Assets<ModExtendedSpriteMaterial2d>>() {
            for (_, mat) in assets.iter() {
                let slot = lookup_wesl_specialization_slot(&mat.extension);
                if slot != 0 {
                    live_slots.insert(slot);
                }
            }
        }
    }
    let _ = evict_wesl_sprite_specializations_for_shaders(
        &removed_shader_ids,
        &removed_spec_slots,
        &live_slots,
    );
    for cleanup in custom {
        cleanup(world);
        removed += 1;
    }

    removed
}
