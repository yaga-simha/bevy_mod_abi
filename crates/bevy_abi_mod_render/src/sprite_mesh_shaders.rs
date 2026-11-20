//! First-class Bevy `0.20` 2D `.wesl` [`SpriteMaterial`], [`ExtendedMaterial2d`], unified
//! [`Mesh2d`] + [`SpriteMaterial`] sprite component bridges, and opt-in [`MeshPipelineDescriptor`]
//! mesh-shader pipeline registration with automatic [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks)
//! cleanup.
//!
//! # Architecture
//!
//! - **Runtime `.wesl` 2D Sprite Materials ([`ModWeslSpriteExtension2d`], [`ModSpriteMaterialPlugin`]):**
//!   Implements Bevy `0.20`'s [`MaterialExtension2d`] with a reflectable specialization key
//!   ([`ModWeslSpriteExtension2dKey`]) extracted via [`AsBindGroup::bind_group_data`]. Each mod can
//!   bind distinct runtime `.wesl` (`Source::Wesl`) vertex/fragment [`Handle<Shader>`] assets,
//!   entry points, preprocessor definitions ([`ModShaderDef`]), [`ShaderBuffer`]s, and
//!   [`ModMaterialParams`] uniforms through both [`SpriteMaterial<ModWeslSpriteExtension2d>`] and
//!   [`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`].
//! - **Unified Bevy `0.20` `Mesh2d` + `SpriteMaterial` Bridge ([`ModUnifiedSpriteSpec`],
//!   [`ModSpriteMeshBridgeRegistry`]):**
//!   Spawns or attaches Bevy `0.20` unified [`Sprite`] / [`Mesh2d`] / [`SpriteMaterial`] /
//!   [`MeshMaterial2d`] components with `ModId`-scoped entity and mesh tracking. When a mod
//!   unloads, its [`ModScoped`] entities despawn, its mod-owned [`Mesh`] assets are purged, and any
//!   mod material components attached to surviving host entities are cleanly detached.
//! - **Opt-In Mesh Shader Pipelines ([`ModMeshShaderPlugin`], [`ModMeshShaderConfig`],
//!   [`ModMeshShaderRegistry`], [`ExtractedModMeshPipelines`]):**
//!   Disabled by default until the host explicitly installs [`ModMeshShaderPlugin`] (with optional
//!   per-mod quotas and [`WgpuFeatures`] gating). Records mod-owned [`MeshPipelineDescriptor`]s,
//!   mirrors them into `RenderApp` or custom [`SubApp`](bevy_app::SubApp)s via [`ExtractSchedule`],
//!   queues them on [`PipelineCache`] when hardware mesh-shader support is available, and purges
//!   all descriptors and shader cache entries deterministically on `unload_mod`.

use crate::sub_app_materials::{
    ModMaterialError, ModMaterialParams, ModSpriteExtension2d, ModSpriteMaterialSpec,
    ModWeslMaterialPlugin, ModWeslMaterialRegistry, ensure_asset_store_in_app,
    ensure_asset_store_in_world, ensure_material_registry,
};
use bevy_abi_mod_host::{ModScoped, register_mod_unload_hook_in_world};
use bevy_abi_mod_host_sys::ModId;
use bevy_app::{App, AppLabel, Plugin};
use bevy_asset::{AssetId, AssetServer, Assets, Handle, RenderAssetUsages};
use bevy_color::Color;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::Schedule;
use bevy_ecs::world::World;
use bevy_extract::{ExtractSchedule, MainWorld};
use bevy_image::{Image, TextureAtlasLayout};
use bevy_material::descriptor::{
    BindGroupLayoutDescriptor, CachedRenderPipelineId, FragmentState, MeshPipelineDescriptor,
    MeshState, RenderPipelineDescriptor, TaskState,
};
use bevy_material::specialize::SpecializedMeshPipelineError;
use bevy_platform::collections::{HashMap, HashSet};
use bevy_reflect::Reflect;
use bevy_render::RenderApp;
use bevy_render::camera::DirtySpecializations;
use bevy_render::mesh::{Indices, Mesh, Mesh2d, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy_render::render_resource::{
    AsBindGroup, ColorTargetState, DepthStencilState, MultisampleState, PipelineCache,
    PrimitiveState, WgpuFeatures,
};
use bevy_render::renderer::RenderDevice;
use bevy_render::storage::ShaderBuffer;
use bevy_render::sync_world::SyncToRenderWorld;
use bevy_shader::{Shader, ShaderDefVal};
use bevy_sprite::{Anchor, Sprite, SpriteAlphaMode, SpriteImageMode};
use bevy_sprite_render::{
    AlphaMode2d, ExtendedMaterial2d, Material2dKey, Material2dPipeline, Material2dPlugin,
    MaterialExtension2d, MeshMaterial2d, RenderMaterial2dInstances, SpriteMaterial,
    SpriteMaterialPlugin, SpriteMeshMaterial, SpriteMeshPlugin,
};
use core::marker::PhantomData;
use std::borrow::Cow;
use std::sync::{OnceLock, RwLock};

// ============================================================================
// Section 1: Custom `.wesl` SpriteMaterial & ExtendedMaterial2d
// ============================================================================

/// Reflectable preprocessor shader definition attached to a [`ModWeslSpriteExtension2d`].
///
/// Mirrors [`ShaderDefVal`] while deriving [`Reflect`] so [`ModWeslSpriteExtension2dKey`] satisfies
/// Bevy `0.20`'s `Reflect + Clone + Hash + Eq` bounds on [`SpriteMaterialPlugin`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Reflect)]
pub enum ModShaderDef {
    /// Boolean preprocessor flag (`@if(NAME)`).
    Bool(String, bool),
    /// Signed 32-bit integer preprocessor value.
    Int(String, i32),
    /// Unsigned 32-bit integer preprocessor value.
    UInt(String, u32),
}

impl ModShaderDef {
    /// Creates an enabled boolean shader definition (`Bool(name, true)`).
    pub fn flag(name: impl Into<String>) -> Self {
        Self::Bool(name.into(), true)
    }

    /// Creates a boolean shader definition.
    pub fn bool(name: impl Into<String>, value: bool) -> Self {
        Self::Bool(name.into(), value)
    }

    /// Creates an integer shader definition.
    pub fn int(name: impl Into<String>, value: i32) -> Self {
        Self::Int(name.into(), value)
    }

    /// Creates an unsigned integer shader definition.
    pub fn uint(name: impl Into<String>, value: u32) -> Self {
        Self::UInt(name.into(), value)
    }

    /// Converts this reflectable definition into a Bevy [`ShaderDefVal`].
    pub fn to_shader_def_val(&self) -> ShaderDefVal {
        match self {
            Self::Bool(k, v) => ShaderDefVal::Bool(Cow::Owned(k.clone()), *v),
            Self::Int(k, v) => ShaderDefVal::Int(Cow::Owned(k.clone()), *v),
            Self::UInt(k, v) => ShaderDefVal::UInt(Cow::Owned(k.clone()), *v),
        }
    }
}

impl From<&str> for ModShaderDef {
    fn from(name: &str) -> Self {
        Self::flag(name)
    }
}

impl From<String> for ModShaderDef {
    fn from(name: String) -> Self {
        Self::flag(name)
    }
}

impl From<ShaderDefVal> for ModShaderDef {
    fn from(def: ShaderDefVal) -> Self {
        match def {
            ShaderDefVal::Bool(k, v) => Self::Bool(k.into_owned(), v),
            ShaderDefVal::Int(k, v) => Self::Int(k.into_owned(), v),
            ShaderDefVal::UInt(k, v) => Self::UInt(k.into_owned(), v),
        }
    }
}

/// Resolved runtime `.wesl` specialization payload referenced by a [`ModWeslSpriteExtension2dKey`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct ModWeslSpriteSpecialization {
    /// Optional runtime `.wesl` vertex shader handle overriding the base sprite vertex stage.
    pub vertex_shader: Option<Handle<Shader>>,
    /// Optional runtime `.wesl` fragment shader handle overriding the base sprite fragment stage.
    pub fragment_shader: Option<Handle<Shader>>,
    /// Optional vertex shader entry point name.
    pub vertex_entry_point: Option<String>,
    /// Optional fragment shader entry point name.
    pub fragment_entry_point: Option<String>,
    /// Additional shader preprocessor definitions appended during specialization.
    pub shader_defs: Vec<ModShaderDef>,
}

impl ModWeslSpriteSpecialization {
    /// Returns `true` if no shader handles, entry points, or preprocessor definitions are set.
    pub fn is_empty(&self) -> bool {
        self.vertex_shader.is_none()
            && self.fragment_shader.is_none()
            && self.vertex_entry_point.is_none()
            && self.fragment_entry_point.is_none()
            && self.shader_defs.is_empty()
    }
}

struct WeslSpecializationTable {
    next_slot: u64,
    by_slot: HashMap<u64, (u64, ModWeslSpriteSpecialization)>,
    by_spec: HashMap<(u64, ModWeslSpriteSpecialization), u64>,
}

impl WeslSpecializationTable {
    fn new() -> Self {
        Self {
            next_slot: 1,
            by_slot: HashMap::default(),
            by_spec: HashMap::default(),
        }
    }
}

fn wesl_specialization_table() -> &'static RwLock<WeslSpecializationTable> {
    static TABLE: OnceLock<RwLock<WeslSpecializationTable>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(WeslSpecializationTable::new()))
}

fn intern_wesl_specialization(scope_id: u64, spec: ModWeslSpriteSpecialization) -> u64 {
    if spec.is_empty() {
        return 0;
    }
    let key = (scope_id, spec);
    let table = wesl_specialization_table();
    if let Ok(read) = table.read()
        && let Some(&slot) = read.by_spec.get(&key)
    {
        return slot;
    }
    let mut write = table
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&slot) = write.by_spec.get(&key) {
        return slot;
    }
    let slot = write.next_slot;
    write.next_slot = write.next_slot.saturating_add(1).max(1);
    write.by_spec.insert(key.clone(), slot);
    write.by_slot.insert(slot, key);
    slot
}

/// Resolves the [`ModWeslSpriteSpecialization`] payload associated with `key`, if any.
pub fn resolve_wesl_sprite_specialization(
    key: &ModWeslSpriteExtension2dKey,
) -> Option<ModWeslSpriteSpecialization> {
    if key.specialization_slot == 0 {
        return None;
    }
    let read = wesl_specialization_table()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    read.by_slot
        .get(&key.specialization_slot)
        .map(|(_, spec)| spec.clone())
}

pub(crate) fn lookup_wesl_specialization_slot(ext: &ModWeslSpriteExtension2d) -> u64 {
    let spec = ModWeslSpriteSpecialization {
        vertex_shader: ext.vertex_shader.clone(),
        fragment_shader: ext.fragment_shader.clone(),
        vertex_entry_point: ext.vertex_entry_point.clone(),
        fragment_entry_point: ext.fragment_entry_point.clone(),
        shader_defs: ext.shader_defs.clone(),
    };
    if spec.is_empty() {
        return 0;
    }
    let key = (ext.registry_scope_id, spec);
    let read = wesl_specialization_table()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    read.by_spec.get(&key).copied().unwrap_or(0)
}

pub(crate) fn evict_wesl_sprite_specializations_for_shaders(
    scope_id: u64,
    shader_ids: &[AssetId<Shader>],
    candidate_slots: &[u64],
    live_slots: &HashSet<u64>,
) -> usize {
    if shader_ids.is_empty() && candidate_slots.is_empty() {
        return 0;
    }
    let mut write = wesl_specialization_table()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let to_remove: Vec<u64> = write
        .by_slot
        .iter()
        .filter_map(|(&slot, (entry_scope_id, spec))| {
            if *entry_scope_id != scope_id {
                return None;
            }
            let matches_candidate = candidate_slots.contains(&slot) && !live_slots.contains(&slot);
            let matches_vertex = spec
                .vertex_shader
                .as_ref()
                .is_some_and(|h| shader_ids.contains(&h.id()));
            let matches_fragment = spec
                .fragment_shader
                .as_ref()
                .is_some_and(|h| shader_ids.contains(&h.id()));
            (matches_candidate || matches_vertex || matches_fragment).then_some(slot)
        })
        .collect();
    let count = to_remove.len();
    for slot in to_remove {
        if let Some(key) = write.by_slot.remove(&slot) {
            write.by_spec.remove(&key);
        }
    }
    count
}

pub(crate) fn evict_wesl_sprite_specializations_for_scope(scope_id: u64) -> usize {
    let mut write = wesl_specialization_table()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let to_remove: Vec<u64> = write
        .by_slot
        .iter()
        .filter_map(|(&slot, (entry_scope_id, _))| (*entry_scope_id == scope_id).then_some(slot))
        .collect();
    let count = to_remove.len();
    for slot in to_remove {
        if let Some(key) = write.by_slot.remove(&slot) {
            write.by_spec.remove(&key);
        }
    }
    count
}

/// `Copy` specialization key derived from [`ModWeslSpriteExtension2d`] via
/// [`AsBindGroup::bind_group_data`].
///
/// Bevy `0.20`'s `CombinedBindGroupData<B, E>` is `#[repr(C, packed)]`, which requires extension
/// keys to be `Copy + Clone + PartialEq + Eq + Hash + Reflect`. This key stores the `Copy`
/// vertex/fragment [`AssetId<Shader>`] identities alongside an interned slot identifier resolved
/// safely inside [`MaterialExtension2d::specialize`] and purged automatically on `unload_mod`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default, Reflect)]
pub struct ModWeslSpriteExtension2dKey {
    /// Optional runtime `.wesl` vertex shader [`AssetId<Shader>`] identity.
    pub vertex_shader_id: Option<AssetId<Shader>>,
    /// Optional runtime `.wesl` fragment shader [`AssetId<Shader>`] identity.
    pub fragment_shader_id: Option<AssetId<Shader>>,
    /// Interned slot resolving full shader handles, entry points, and `shader_defs` (`0` = none).
    pub specialization_slot: u64,
}

/// Runtime `.wesl`-capable 2D [`MaterialExtension2d`] combining a mod-owned [`ShaderBuffer`]
/// (`@binding(100)`), uniform [`ModMaterialParams`] (`@binding(101)`), and dynamic `.wesl`
/// (`Source::Wesl`) vertex/fragment shader handles applied during pipeline specialization.
///
/// Works seamlessly with both Bevy `0.20`'s [`SpriteMaterial<ModWeslSpriteExtension2d>`]
/// (via [`SpriteMaterialPlugin<ModWeslSpriteExtension2d>`]) and direct
/// [`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`]
/// (via [`Material2dPlugin<ModExtendedSpriteMaterial2d>`]).
#[derive(bevy_asset::Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
#[bind_group_data(ModWeslSpriteExtension2dKey)]
#[reflect(Debug, Clone)]
pub struct ModWeslSpriteExtension2d {
    /// Storage buffer bound at `@binding(100)` (`var<storage, read>`).
    #[storage(100, read_only)]
    pub buffer: Handle<ShaderBuffer>,
    /// Uniform parameters bound at `@binding(101)`.
    #[uniform(101)]
    pub params: ModMaterialParams,
    /// Optional runtime `.wesl` vertex shader override bound during specialization.
    pub vertex_shader: Option<Handle<Shader>>,
    /// Optional runtime `.wesl` fragment shader override bound during specialization.
    pub fragment_shader: Option<Handle<Shader>>,
    /// Optional vertex shader entry point override.
    pub vertex_entry_point: Option<String>,
    /// Optional fragment shader entry point override.
    pub fragment_entry_point: Option<String>,
    /// Optional [`AlphaMode2d`] override (inherits base sprite alpha mode when `None`).
    pub alpha_mode: Option<AlphaMode2d>,
    /// Optional depth bias override (inherits base depth bias when `None`).
    pub depth_bias: Option<f32>,
    /// Additional preprocessor definitions applied during specialization.
    pub shader_defs: Vec<ModShaderDef>,
    pub(crate) registry_scope_id: u64,
}

impl ModWeslSpriteExtension2d {
    /// Creates a default [`ModWeslSpriteExtension2d`] with identity [`ModMaterialParams`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`ShaderBuffer`] bound at `@binding(100)`.
    pub fn with_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.buffer = buffer;
        self
    }

    /// Sets the uniform [`ModMaterialParams`] bound at `@binding(101)`.
    pub fn with_params(mut self, params: ModMaterialParams) -> Self {
        self.params = params;
        self
    }

    /// Sets the runtime `.wesl` vertex shader override.
    pub fn with_vertex_shader(mut self, shader: Handle<Shader>) -> Self {
        self.vertex_shader = Some(shader);
        self
    }

    /// Sets the runtime `.wesl` fragment shader override.
    pub fn with_fragment_shader(mut self, shader: Handle<Shader>) -> Self {
        self.fragment_shader = Some(shader);
        self
    }

    /// Overrides the vertex shader entry point name.
    pub fn with_vertex_entry_point(mut self, entry_point: impl Into<String>) -> Self {
        self.vertex_entry_point = Some(entry_point.into());
        self
    }

    /// Overrides the fragment shader entry point name.
    pub fn with_fragment_entry_point(mut self, entry_point: impl Into<String>) -> Self {
        self.fragment_entry_point = Some(entry_point.into());
        self
    }

    /// Sets an [`AlphaMode2d`] override for this extension.
    pub fn with_alpha_mode(mut self, alpha_mode: AlphaMode2d) -> Self {
        self.alpha_mode = Some(alpha_mode);
        self
    }

    /// Sets a depth bias override for this extension.
    pub fn with_depth_bias(mut self, depth_bias: f32) -> Self {
        self.depth_bias = Some(depth_bias);
        self
    }

    /// Appends preprocessor [`ModShaderDef`] entries applied during pipeline specialization.
    pub fn with_shader_defs(mut self, defs: impl IntoIterator<Item = ModShaderDef>) -> Self {
        self.shader_defs.extend(defs);
        self
    }
}

impl From<&ModWeslSpriteExtension2d> for ModWeslSpriteExtension2dKey {
    fn from(ext: &ModWeslSpriteExtension2d) -> Self {
        let spec = ModWeslSpriteSpecialization {
            vertex_shader: ext.vertex_shader.clone(),
            fragment_shader: ext.fragment_shader.clone(),
            vertex_entry_point: ext.vertex_entry_point.clone(),
            fragment_entry_point: ext.fragment_entry_point.clone(),
            shader_defs: ext.shader_defs.clone(),
        };
        let specialization_slot = intern_wesl_specialization(ext.registry_scope_id, spec);
        Self {
            vertex_shader_id: ext.vertex_shader.as_ref().map(Handle::id),
            fragment_shader_id: ext.fragment_shader.as_ref().map(Handle::id),
            specialization_slot,
        }
    }
}

/// Applies [`ModWeslSpriteExtension2dKey`] runtime `.wesl` shader handles, entry points, and
/// preprocessor definitions onto a Bevy [`RenderPipelineDescriptor`].
pub fn apply_wesl_sprite_specialization(
    descriptor: &mut RenderPipelineDescriptor,
    key: &ModWeslSpriteExtension2dKey,
) {
    let Some(spec) = resolve_wesl_sprite_specialization(key) else {
        return;
    };
    if let Some(vertex_shader) = &spec.vertex_shader {
        descriptor.vertex.shader = vertex_shader.clone();
    }
    if let Some(entry_point) = &spec.vertex_entry_point {
        descriptor.vertex.entry_point = Some(Cow::Owned(entry_point.clone()));
    }
    if let Some(fragment_shader) = &spec.fragment_shader {
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment.shader = fragment_shader.clone();
        } else {
            descriptor.fragment = Some(FragmentState {
                shader: fragment_shader.clone(),
                ..Default::default()
            });
        }
    }
    if let Some(entry_point) = &spec.fragment_entry_point
        && let Some(fragment) = descriptor.fragment.as_mut()
    {
        fragment.entry_point = Some(Cow::Owned(entry_point.clone()));
    }
    for def in &spec.shader_defs {
        let shader_def = def.to_shader_def_val();
        if !descriptor.vertex.shader_defs.contains(&shader_def) {
            descriptor.vertex.shader_defs.push(shader_def.clone());
        }
        if let Some(fragment) = descriptor.fragment.as_mut()
            && !fragment.shader_defs.contains(&shader_def)
        {
            fragment.shader_defs.push(shader_def);
        }
    }
}

impl MaterialExtension2d for ModWeslSpriteExtension2d {
    fn depth_bias(&self) -> Option<f32> {
        self.depth_bias
    }

    fn alpha_mode(&self) -> Option<AlphaMode2d> {
        self.alpha_mode
    }

    fn specialize(
        _pipeline: &Material2dPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        apply_wesl_sprite_specialization(descriptor, &key.bind_group_data);
        Ok(())
    }
}

/// Type alias for Bevy `0.20`'s [`ExtendedMaterial2d`] pairing [`SpriteMeshMaterial`] with
/// [`ModWeslSpriteExtension2d`].
pub type ModExtendedSpriteMaterial2d =
    ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>;

/// Type alias for Bevy `0.20`'s [`SpriteMaterial`] bound to [`ModWeslSpriteExtension2d`].
pub type ModSpriteMaterial = SpriteMaterial<ModWeslSpriteExtension2d>;

/// Data-driven specification for registering a `ModId`-owned [`ModWeslSpriteExtension2d`] asset.
#[derive(Debug, Clone)]
pub struct ModWeslSpriteExtensionSpec {
    /// Registry label for lookup under the owning [`ModId`].
    pub label: String,
    /// Configured [`ModWeslSpriteExtension2d`] asset payload.
    pub extension: ModWeslSpriteExtension2d,
}

impl ModWeslSpriteExtensionSpec {
    /// Creates a new [`ModWeslSpriteExtensionSpec`] with the given registry `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            extension: ModWeslSpriteExtension2d::default(),
        }
    }

    /// Creates a [`ModWeslSpriteExtensionSpec`] wrapping an existing [`ModWeslSpriteExtension2d`].
    pub fn from_extension(label: impl Into<String>, extension: ModWeslSpriteExtension2d) -> Self {
        Self {
            label: label.into(),
            extension,
        }
    }

    /// Sets the [`ShaderBuffer`] bound at `@binding(100)`.
    pub fn with_shader_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.extension.buffer = buffer;
        self
    }

    /// Sets the uniform [`ModMaterialParams`] bound at `@binding(101)`.
    pub fn with_params(mut self, params: ModMaterialParams) -> Self {
        self.extension.params = params;
        self
    }

    /// Sets the runtime `.wesl` vertex shader override.
    pub fn with_vertex_shader(mut self, shader: Handle<Shader>) -> Self {
        self.extension.vertex_shader = Some(shader);
        self
    }

    /// Sets the runtime `.wesl` fragment shader override.
    pub fn with_fragment_shader(mut self, shader: Handle<Shader>) -> Self {
        self.extension.fragment_shader = Some(shader);
        self
    }

    /// Overrides the vertex shader entry point name.
    pub fn with_vertex_entry_point(mut self, entry_point: impl Into<String>) -> Self {
        self.extension.vertex_entry_point = Some(entry_point.into());
        self
    }

    /// Overrides the fragment shader entry point name.
    pub fn with_fragment_entry_point(mut self, entry_point: impl Into<String>) -> Self {
        self.extension.fragment_entry_point = Some(entry_point.into());
        self
    }

    /// Sets an [`AlphaMode2d`] override.
    pub fn with_alpha_mode(mut self, alpha_mode: AlphaMode2d) -> Self {
        self.extension.alpha_mode = Some(alpha_mode);
        self
    }

    /// Sets a depth bias override.
    pub fn with_depth_bias(mut self, depth_bias: f32) -> Self {
        self.extension.depth_bias = Some(depth_bias);
        self
    }

    /// Appends preprocessor [`ModShaderDef`] entries.
    pub fn with_shader_defs(mut self, defs: impl IntoIterator<Item = ModShaderDef>) -> Self {
        self.extension.shader_defs.extend(defs);
        self
    }
}

/// Data-driven specification for registering a `ModId`-owned [`ModExtendedSpriteMaterial2d`]
/// (`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`).
#[derive(Debug, Clone)]
pub struct ModExtendedSpriteMaterialSpec {
    /// Base Bevy `0.20` [`SpriteMeshMaterial`] configuration.
    pub base: ModSpriteMaterialSpec,
    /// Runtime `.wesl` extension configuration.
    pub extension: ModWeslSpriteExtension2d,
}

impl ModExtendedSpriteMaterialSpec {
    /// Creates a default [`ModExtendedSpriteMaterialSpec`] with the given registry `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            base: ModSpriteMaterialSpec::new(label),
            extension: ModWeslSpriteExtension2d::default(),
        }
    }

    /// Sets the base [`ModSpriteMaterialSpec`].
    pub fn with_base(mut self, base: ModSpriteMaterialSpec) -> Self {
        self.base = base;
        self
    }

    /// Sets the [`ModWeslSpriteExtension2d`] extension payload.
    pub fn with_extension(mut self, extension: ModWeslSpriteExtension2d) -> Self {
        self.extension = extension;
        self
    }

    /// Sets the base sprite sRGBA color multiplier (`[r, g, b, a]`).
    pub fn with_color_rgba(mut self, rgba: [f32; 4]) -> Self {
        self.base.color_rgba = rgba;
        self
    }

    /// Sets the base sprite [`AlphaMode2d`].
    pub fn with_alpha_mode(mut self, alpha_mode: AlphaMode2d) -> Self {
        self.base.alpha_mode = alpha_mode;
        self
    }

    /// Sets the base sprite `flip_x` and `flip_y` flags.
    pub fn with_flip(mut self, flip_x: bool, flip_y: bool) -> Self {
        self.base.flip_x = flip_x;
        self.base.flip_y = flip_y;
        self
    }

    /// Sets the extension [`ShaderBuffer`] bound at `@binding(100)`.
    pub fn with_shader_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.extension.buffer = buffer.clone();
        self.base.shader_buffer = Some(buffer);
        self
    }

    /// Sets the extension [`ModMaterialParams`] uniform bound at `@binding(101)`.
    pub fn with_params(mut self, params: ModMaterialParams) -> Self {
        self.extension.params = params;
        self
    }

    /// Sets the runtime `.wesl` vertex shader override.
    pub fn with_vertex_shader(mut self, shader: Handle<Shader>) -> Self {
        self.extension.vertex_shader = Some(shader);
        self
    }

    /// Sets the runtime `.wesl` fragment shader override.
    pub fn with_fragment_shader(mut self, shader: Handle<Shader>) -> Self {
        self.base.shader = Some(shader.clone());
        self.extension.fragment_shader = Some(shader);
        self
    }

    /// Overrides the extension depth bias.
    pub fn with_depth_bias(mut self, depth_bias: f32) -> Self {
        self.extension.depth_bias = Some(depth_bias);
        self
    }

    /// Appends preprocessor [`ModShaderDef`] entries.
    pub fn with_shader_defs(mut self, defs: impl IntoIterator<Item = ModShaderDef>) -> Self {
        self.extension.shader_defs.extend(defs);
        self
    }

    /// Builds the concrete [`ModExtendedSpriteMaterial2d`] asset.
    pub fn build_material(&self) -> ModExtendedSpriteMaterial2d {
        ExtendedMaterial2d {
            base: self.base.build_material(),
            extension: self.extension.clone(),
        }
    }
}

/// Recorded [`ModWeslSpriteExtension2d`] metadata owned by a loaded mod.
#[derive(Clone, Debug)]
pub struct ModWeslSpriteExtensionEntry {
    /// Extension label within the mod.
    pub label: String,
    /// Registered [`ModWeslSpriteExtension2d`] asset handle.
    pub handle: Handle<ModWeslSpriteExtension2d>,
    /// Optional runtime `.wesl` vertex shader handle.
    pub vertex_shader: Option<Handle<Shader>>,
    /// Optional runtime `.wesl` fragment shader handle.
    pub fragment_shader: Option<Handle<Shader>>,
    /// Bound storage [`ShaderBuffer`] handle.
    pub shader_buffer: Handle<ShaderBuffer>,
}

/// Recorded [`ModExtendedSpriteMaterial2d`] metadata owned by a loaded mod.
#[derive(Clone, Debug)]
pub struct ModWeslExtendedMaterialEntry {
    /// Material label within the mod.
    pub label: String,
    /// Registered [`ModExtendedSpriteMaterial2d`] asset handle.
    pub handle: Handle<ModExtendedSpriteMaterial2d>,
    /// Optional runtime `.wesl` vertex shader handle.
    pub vertex_shader: Option<Handle<Shader>>,
    /// Optional runtime `.wesl` fragment shader handle.
    pub fragment_shader: Option<Handle<Shader>>,
    /// Bound storage [`ShaderBuffer`] handle.
    pub shader_buffer: Handle<ShaderBuffer>,
}

/// Installs Bevy `0.20`'s [`SpriteMaterialPlugin`] and [`Material2dPlugin`] registrations for
/// mod-owned `.wesl` sprite materials alongside [`ModSpriteMeshBridgeRegistry`] and automatic
/// `unload_mod` cleanup hooks.
///
/// Works in both full `AssetPlugin` apps (installing real [`SpriteMeshPlugin`],
/// [`SpriteMaterialPlugin<ModWeslSpriteExtension2d>`],
/// [`SpriteMaterialPlugin<ModSpriteExtension2d>`],
/// [`Material2dPlugin<ModExtendedSpriteMaterial2d>`], and
/// [`Material2dPlugin<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>`]) and
/// minimal headless apps without `AssetPlugin` (initializing `Assets<T>` stores safely).
#[derive(Debug, Clone, Copy)]
pub struct ModSpriteMaterialPlugin {
    /// Whether to install Bevy's [`SpriteMeshPlugin`] automatically when [`AssetServer`] is
    /// present and [`SpriteMeshPlugin`] has not yet been added. Defaults to `true`.
    pub install_sprite_mesh_plugin: bool,
}

impl Default for ModSpriteMaterialPlugin {
    fn default() -> Self {
        Self {
            install_sprite_mesh_plugin: true,
        }
    }
}

impl ModSpriteMaterialPlugin {
    /// Creates a [`ModSpriteMaterialPlugin`] with [`Self::install_sprite_mesh_plugin`] configured.
    pub fn with_sprite_mesh_plugin(install_sprite_mesh_plugin: bool) -> Self {
        Self {
            install_sprite_mesh_plugin,
        }
    }
}

const SPRITE_MESH_BINDINGS_UNLOAD_HOOK_KEY: &str = "bevy_abi_mod_render::sprite_mesh_bindings";

fn cleanup_mod_sprite_mesh_bindings_hook(world: &mut World, mod_id: ModId) {
    let _ = cleanup_mod_sprite_mesh_bindings_in_world(world, mod_id);
}

fn ensure_headless_render_app_sprite_resources(app: &mut App) {
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        if !render_app
            .world()
            .contains_resource::<DirtySpecializations>()
        {
            render_app.init_resource::<DirtySpecializations>();
        }
        if !render_app
            .world()
            .contains_resource::<RenderMaterial2dInstances>()
        {
            render_app.init_resource::<RenderMaterial2dInstances>();
        }
    }
}

impl Plugin for ModSpriteMaterialPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<ModWeslMaterialPlugin>() {
            app.add_plugins(ModWeslMaterialPlugin);
        }
        if !app
            .world()
            .contains_resource::<ModSpriteMeshBridgeRegistry>()
        {
            app.insert_resource(ModSpriteMeshBridgeRegistry::new());
        }
        register_mod_unload_hook_in_world(
            app.world_mut(),
            SPRITE_MESH_BINDINGS_UNLOAD_HOOK_KEY,
            cleanup_mod_sprite_mesh_bindings_hook,
        );

        if app.world().contains_resource::<AssetServer>() {
            ensure_asset_store_in_app::<Shader>(app);
            ensure_asset_store_in_app::<Mesh>(app);
            ensure_asset_store_in_app::<Image>(app);
            ensure_asset_store_in_app::<TextureAtlasLayout>(app);

            if self.install_sprite_mesh_plugin && !app.is_plugin_added::<SpriteMeshPlugin>() {
                app.add_plugins(SpriteMeshPlugin);
            }
            if !app.is_plugin_added::<SpriteMaterialPlugin<ModWeslSpriteExtension2d>>() {
                app.add_plugins(SpriteMaterialPlugin::<ModWeslSpriteExtension2d>::default());
            }
            if !app.is_plugin_added::<SpriteMaterialPlugin<ModSpriteExtension2d>>() {
                app.add_plugins(SpriteMaterialPlugin::<ModSpriteExtension2d>::default());
            }
            if !app.is_plugin_added::<Material2dPlugin<ModExtendedSpriteMaterial2d>>() {
                app.add_plugins(Material2dPlugin::<ModExtendedSpriteMaterial2d>::default());
            }
            if !app.is_plugin_added::<Material2dPlugin<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>() {
                app.add_plugins(Material2dPlugin::<
                    ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>,
                >::default());
            }

            ensure_headless_render_app_sprite_resources(app);
        } else {
            ensure_asset_store_in_app::<Mesh>(app);
            ensure_asset_store_in_app::<ModWeslSpriteExtension2d>(app);
            ensure_asset_store_in_app::<ModExtendedSpriteMaterial2d>(app);
        }
    }

    fn finish(&self, app: &mut App) {
        ensure_headless_render_app_sprite_resources(app);
    }
}

// ============================================================================
// Section 2: Unified Bevy 0.20 `Mesh2d` + `SpriteMaterial` Bridge
// ============================================================================

/// Kind of mod-attached 2D sprite material component recorded in [`ModSpriteMeshBridgeRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSpriteBindingKind {
    /// Entity carries [`SpriteMaterial<ModWeslSpriteExtension2d>`].
    WeslSpriteMaterial,
    /// Entity carries [`SpriteMaterial<ModSpriteExtension2d>`].
    LegacySpriteMaterial,
    /// Entity carries [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`].
    WeslExtendedMeshMaterial2d,
    /// Entity carries [`MeshMaterial2d<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>`].
    LegacyExtendedMeshMaterial2d,
}

/// Data-driven descriptor for spawning a unified Bevy `0.20` 2D sprite carrying [`Sprite`],
/// [`Anchor`], [`Mesh2d`], and a mod-owned material component.
#[derive(Debug, Clone)]
pub struct ModUnifiedSpriteSpec {
    /// Bevy [`Sprite`] component configuration.
    pub sprite: Sprite,
    /// Sprite [`Anchor`] offset (defaults to [`Anchor::CENTER`]).
    pub anchor: Anchor,
    /// Explicit [`Mesh`] handle for [`Mesh2d`]. When `None` and [`Self::attach_unit_quad_mesh`] is
    /// `true`, a mod-owned quad mesh sized to [`Self::quad_size`] is created automatically.
    pub mesh: Option<Handle<Mesh>>,
    /// Whether to attach a quad [`Mesh2d`] immediately on spawn when [`Self::mesh`] is `None`.
    pub attach_unit_quad_mesh: bool,
    /// Quad dimensions (`[width, height]`) used when generating an automatic [`Mesh2d`] quad.
    pub quad_size: [f32; 2],
    /// Whether the spawned entity is tagged with [`ModScoped`] so it despawns automatically when
    /// the owning mod unloads (defaults to `true`).
    pub scoped: bool,
    /// Whether to also attach [`SyncToRenderWorld`] to the spawned entity (defaults to `false`).
    pub sync_to_render_world: bool,
}

impl Default for ModUnifiedSpriteSpec {
    fn default() -> Self {
        Self {
            sprite: Sprite::default(),
            anchor: Anchor::CENTER,
            mesh: None,
            attach_unit_quad_mesh: true,
            quad_size: [1.0, 1.0],
            scoped: true,
            sync_to_render_world: false,
        }
    }
}

impl ModUnifiedSpriteSpec {
    /// Creates a default [`ModUnifiedSpriteSpec`] (`ModScoped`, centered anchor, 1x1 unit quad).
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a [`ModUnifiedSpriteSpec`] from an existing Bevy [`Sprite`].
    pub fn from_sprite(sprite: Sprite) -> Self {
        Self {
            sprite,
            ..Self::default()
        }
    }

    /// Sets the sprite's sRGBA tint color (`[r, g, b, a]`).
    pub fn with_color_rgba(mut self, rgba: [f32; 4]) -> Self {
        self.sprite.color = Color::srgba(rgba[0], rgba[1], rgba[2], rgba[3]);
        self
    }

    /// Sets [`Sprite::custom_size`] (`width`, `height`).
    pub fn with_custom_size(mut self, width: f32, height: f32) -> Self {
        self.sprite.custom_size = Some([width, height].into());
        self
    }

    /// Sets [`Sprite::flip_x`] and [`Sprite::flip_y`].
    pub fn with_flip(mut self, flip_x: bool, flip_y: bool) -> Self {
        self.sprite.flip_x = flip_x;
        self.sprite.flip_y = flip_y;
        self
    }

    /// Sets [`Sprite::image`].
    pub fn with_image(mut self, image: Handle<Image>) -> Self {
        self.sprite.image = image;
        self
    }

    /// Sets [`Sprite::image_mode`].
    pub fn with_image_mode(mut self, image_mode: SpriteImageMode) -> Self {
        self.sprite.image_mode = image_mode;
        self
    }

    /// Sets [`Sprite::alpha_mode`].
    pub fn with_alpha_mode(mut self, alpha_mode: SpriteAlphaMode) -> Self {
        self.sprite.alpha_mode = alpha_mode;
        self
    }

    /// Sets the [`Anchor`] component for the spawned sprite.
    pub fn with_anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Supplies an explicit [`Handle<Mesh>`] for the entity's [`Mesh2d`] component.
    pub fn with_mesh(mut self, mesh: Handle<Mesh>) -> Self {
        self.mesh = Some(mesh);
        self
    }

    /// Configures the automatic quad dimensions used when [`Self::mesh`] is `None`.
    pub fn with_quad_size(mut self, width: f32, height: f32) -> Self {
        self.quad_size = [width, height];
        self
    }

    /// Skips inserting an initial [`Mesh2d`] at spawn time (for example, letting
    /// [`SpriteMeshPlugin`] insert its shared unit quad in `PostUpdate`).
    pub fn without_initial_mesh(mut self) -> Self {
        self.attach_unit_quad_mesh = false;
        self
    }

    /// Marks whether the spawned entity is [`ModScoped`] (`true`) or persistent (`false`).
    pub fn with_scoped(mut self, scoped: bool) -> Self {
        self.scoped = scoped;
        self
    }

    /// Marks the spawned entity as persistent (not [`ModScoped`]); on `unload_mod`, the entity
    /// survives while its mod-owned material component is cleanly detached.
    pub fn persistent(mut self) -> Self {
        self.scoped = false;
        self
    }

    /// Controls whether [`SyncToRenderWorld`] is attached on spawn.
    pub fn with_sync_to_render_world(mut self, sync: bool) -> Self {
        self.sync_to_render_world = sync;
        self
    }
}

/// Host [`Resource`] tracking mod-owned 2D quad [`Mesh`] assets and mod-attached
/// [`SpriteMaterial`] / [`MeshMaterial2d`] entity bindings for deterministic cleanup on
/// `unload_mod`.
#[derive(Resource, Default, Debug)]
pub struct ModSpriteMeshBridgeRegistry {
    meshes: HashMap<ModId, Vec<(String, Handle<Mesh>)>>,
    bindings: HashMap<ModId, Vec<(Entity, ModSpriteBindingKind)>>,
}

impl ModSpriteMeshBridgeRegistry {
    /// Creates an empty [`ModSpriteMeshBridgeRegistry`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of mod-owned [`Mesh`] assets tracked for `mod_id`.
    pub fn mesh_count_for_mod(&self, mod_id: ModId) -> usize {
        self.meshes.get(&mod_id).map_or(0, Vec::len)
    }

    /// Looks up a mod-owned [`Handle<Mesh>`] by `mod_id` and `label`.
    pub fn get_mesh(&self, mod_id: ModId, label: &str) -> Option<&Handle<Mesh>> {
        self.meshes
            .get(&mod_id)?
            .iter()
            .find_map(|(k, h)| (k == label).then_some(h))
    }

    /// Returns the number of tracked entity material bindings owned by `mod_id`.
    pub fn binding_count_for_mod(&self, mod_id: ModId) -> usize {
        self.bindings.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns `true` if `entity` has a tracked `kind` binding owned by `mod_id`.
    pub fn has_binding(&self, mod_id: ModId, entity: Entity, kind: ModSpriteBindingKind) -> bool {
        self.bindings
            .get(&mod_id)
            .is_some_and(|list| list.contains(&(entity, kind)))
    }

    /// Returns the total number of meshes and entity material bindings tracked for `mod_id`.
    pub fn total_tracked_for_mod(&self, mod_id: ModId) -> usize {
        self.mesh_count_for_mod(mod_id) + self.binding_count_for_mod(mod_id)
    }

    fn record_binding(&mut self, mod_id: ModId, entity: Entity, kind: ModSpriteBindingKind) {
        let entries = self.bindings.entry(mod_id).or_default();
        if !entries.contains(&(entity, kind)) {
            entries.push((entity, kind));
        }
    }

    fn remove_binding(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        kind: ModSpriteBindingKind,
    ) -> bool {
        let Some(entries) = self.bindings.get_mut(&mod_id) else {
            return false;
        };
        if let Some(pos) = entries.iter().position(|e| *e == (entity, kind)) {
            entries.swap_remove(pos);
            if entries.is_empty() {
                self.bindings.remove(&mod_id);
            }
            true
        } else {
            false
        }
    }
}

fn ensure_sprite_bridge_registry(world: &mut World) {
    ensure_material_registry(world);
    if !world.contains_resource::<ModSpriteMeshBridgeRegistry>() {
        world.insert_resource(ModSpriteMeshBridgeRegistry::new());
    }
    register_mod_unload_hook_in_world(
        world,
        SPRITE_MESH_BINDINGS_UNLOAD_HOOK_KEY,
        cleanup_mod_sprite_mesh_bindings_hook,
    );
}

/// Builds a centered 2D quad [`Mesh`] (`width x height`, `TriangleList`, `POSITION` + `UV_0` +
/// `U16` indices).
pub fn build_quad_mesh_2d(width: f32, height: f32) -> Result<Mesh, ModMaterialError> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(ModMaterialError::InvalidPipelineDescriptor(format!(
            "quad dimensions must be finite positive numbers (got {width}x{height})"
        )));
    }
    let hw = width * 0.5;
    let hh = height * 0.5;
    let mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(
        Mesh::ATTRIBUTE_POSITION,
        vec![
            [-hw, -hh, 0.0],
            [hw, -hh, 0.0],
            [hw, hh, 0.0],
            [-hw, hh, 0.0],
        ],
    )
    .with_inserted_attribute(
        Mesh::ATTRIBUTE_UV_0,
        vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
    )
    .with_inserted_indices(Indices::U16(vec![0, 1, 2, 0, 2, 3]));
    Ok(mesh)
}

/// Registers a `ModId`-owned 2D quad [`Mesh`] (`width x height`) in `world`'s [`Assets<Mesh>`],
/// automatically purged when `mod_id` unloads.
pub fn register_mod_quad_mesh_in_world(
    world: &mut World,
    mod_id: ModId,
    label: impl Into<String>,
    width: f32,
    height: f32,
) -> Result<Handle<Mesh>, ModMaterialError> {
    let label = label.into();
    if label.is_empty() {
        return Err(ModMaterialError::InvalidPipelineDescriptor(
            "quad mesh label must not be empty".to_owned(),
        ));
    }
    let mesh = build_quad_mesh_2d(width, height)?;
    ensure_sprite_bridge_registry(world);
    ensure_asset_store_in_world::<Mesh>(world);

    let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
    world
        .resource_mut::<ModSpriteMeshBridgeRegistry>()
        .meshes
        .entry(mod_id)
        .or_default()
        .push((label, handle.clone()));
    Ok(handle)
}

/// Registers a `ModId`-owned [`ModWeslSpriteExtension2d`] asset in `world` suitable for binding
/// via [`SpriteMaterial<ModWeslSpriteExtension2d>`].
pub fn register_mod_wesl_sprite_extension_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModWeslSpriteExtensionSpec,
) -> Result<Handle<ModWeslSpriteExtension2d>, ModMaterialError> {
    if spec.label.is_empty() {
        return Err(ModMaterialError::InvalidPipelineDescriptor(
            "sprite extension label must not be empty".to_owned(),
        ));
    }
    ensure_material_registry(world);
    ensure_asset_store_in_world::<ModWeslSpriteExtension2d>(world);

    let scope_id = world.resource::<ModWeslMaterialRegistry>().scope_id;
    let mut extension = spec.extension;
    extension.registry_scope_id = scope_id;
    let entry_vertex = extension.vertex_shader.clone();
    let entry_fragment = extension.fragment_shader.clone();
    let entry_buffer = extension.buffer.clone();
    let handle = world
        .resource_mut::<Assets<ModWeslSpriteExtension2d>>()
        .add(extension);

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .wesl_sprite_extensions
        .entry(mod_id)
        .or_default()
        .push(ModWeslSpriteExtensionEntry {
            label: spec.label,
            handle: handle.clone(),
            vertex_shader: entry_vertex,
            fragment_shader: entry_fragment,
            shader_buffer: entry_buffer,
        });

    Ok(handle)
}

/// Registers a `ModId`-owned [`ModExtendedSpriteMaterial2d`]
/// (`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`) in `world`.
pub fn register_mod_wesl_extended_sprite_material_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModExtendedSpriteMaterialSpec,
) -> Result<Handle<ModExtendedSpriteMaterial2d>, ModMaterialError> {
    if spec.base.label.is_empty() {
        return Err(ModMaterialError::InvalidPipelineDescriptor(
            "extended sprite material label must not be empty".to_owned(),
        ));
    }
    ensure_material_registry(world);
    ensure_asset_store_in_world::<ModExtendedSpriteMaterial2d>(world);

    let scope_id = world.resource::<ModWeslMaterialRegistry>().scope_id;
    let mut material = spec.build_material();
    material.extension.registry_scope_id = scope_id;
    let entry_vertex = material.extension.vertex_shader.clone();
    let entry_fragment = material.extension.fragment_shader.clone();
    let entry_buffer = material.extension.buffer.clone();
    let handle = world
        .resource_mut::<Assets<ModExtendedSpriteMaterial2d>>()
        .add(material);

    world
        .resource_mut::<ModWeslMaterialRegistry>()
        .wesl_extended_2d_materials
        .entry(mod_id)
        .or_default()
        .push(ModWeslExtendedMaterialEntry {
            label: spec.base.label,
            handle: handle.clone(),
            vertex_shader: entry_vertex,
            fragment_shader: entry_fragment,
            shader_buffer: entry_buffer,
        });

    Ok(handle)
}

/// Updates [`ModWeslSpriteExtension2d::params`] on a `mod_id`-owned handle, triggering Bevy
/// `AssetEvent::Modified` propagation into [`SpriteMaterialPlugin`].
pub fn update_mod_wesl_sprite_extension_params_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModWeslSpriteExtension2d>,
    params: ModMaterialParams,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_sprite_extension_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    let mut assets = world
        .get_resource_mut::<Assets<ModWeslSpriteExtension2d>>()
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "Assets<ModWeslSpriteExtension2d>",
        ))?;
    let mut ext = assets
        .get_mut(handle.id())
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "ModWeslSpriteExtension2d",
        ))?;
    ext.params = params;
    Ok(())
}

/// Replaces the runtime `.wesl` vertex and/or fragment shader handles on a `mod_id`-owned
/// [`ModWeslSpriteExtension2d`], updating the specialization key and triggering re-specialization.
pub fn update_mod_wesl_sprite_extension_shaders_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModWeslSpriteExtension2d>,
    vertex_shader: Option<Handle<Shader>>,
    fragment_shader: Option<Handle<Shader>>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_sprite_extension_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    {
        let mut assets = world
            .get_resource_mut::<Assets<ModWeslSpriteExtension2d>>()
            .ok_or(ModMaterialError::AssetStoreUnavailable(
                "Assets<ModWeslSpriteExtension2d>",
            ))?;
        let mut ext =
            assets
                .get_mut(handle.id())
                .ok_or(ModMaterialError::AssetStoreUnavailable(
                    "ModWeslSpriteExtension2d",
                ))?;
        ext.vertex_shader = vertex_shader.clone();
        ext.fragment_shader = fragment_shader.clone();
    }
    if let Some(mut reg) = world.get_resource_mut::<ModWeslMaterialRegistry>()
        && let Some(entries) = reg.wesl_sprite_extensions.get_mut(&mod_id)
        && let Some(entry) = entries.iter_mut().find(|e| e.handle.id() == handle.id())
    {
        entry.vertex_shader = vertex_shader;
        entry.fragment_shader = fragment_shader;
    }
    Ok(())
}

/// Replaces the bound [`ShaderBuffer`] handle on a `mod_id`-owned [`ModWeslSpriteExtension2d`].
pub fn update_mod_wesl_sprite_extension_buffer_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModWeslSpriteExtension2d>,
    buffer: Handle<ShaderBuffer>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_sprite_extension_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    {
        let mut assets = world
            .get_resource_mut::<Assets<ModWeslSpriteExtension2d>>()
            .ok_or(ModMaterialError::AssetStoreUnavailable(
                "Assets<ModWeslSpriteExtension2d>",
            ))?;
        let mut ext =
            assets
                .get_mut(handle.id())
                .ok_or(ModMaterialError::AssetStoreUnavailable(
                    "ModWeslSpriteExtension2d",
                ))?;
        ext.buffer = buffer.clone();
    }
    if let Some(mut reg) = world.get_resource_mut::<ModWeslMaterialRegistry>()
        && let Some(entries) = reg.wesl_sprite_extensions.get_mut(&mod_id)
        && let Some(entry) = entries.iter_mut().find(|e| e.handle.id() == handle.id())
    {
        entry.shader_buffer = buffer;
    }
    Ok(())
}

/// Updates [`ModMaterialParams`] on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
pub fn update_mod_extended_sprite_material_params_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModExtendedSpriteMaterial2d>,
    params: ModMaterialParams,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_extended_2d_material_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    let mut assets = world
        .get_resource_mut::<Assets<ModExtendedSpriteMaterial2d>>()
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "Assets<ModExtendedSpriteMaterial2d>",
        ))?;
    let mut mat = assets
        .get_mut(handle.id())
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "ModExtendedSpriteMaterial2d",
        ))?;
    mat.extension.params = params;
    Ok(())
}

/// Replaces the runtime `.wesl` vertex/fragment shaders on a `mod_id`-owned
/// [`ModExtendedSpriteMaterial2d`].
pub fn update_mod_extended_sprite_material_shaders_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModExtendedSpriteMaterial2d>,
    vertex_shader: Option<Handle<Shader>>,
    fragment_shader: Option<Handle<Shader>>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_extended_2d_material_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    {
        let mut assets = world
            .get_resource_mut::<Assets<ModExtendedSpriteMaterial2d>>()
            .ok_or(ModMaterialError::AssetStoreUnavailable(
                "Assets<ModExtendedSpriteMaterial2d>",
            ))?;
        let mut mat =
            assets
                .get_mut(handle.id())
                .ok_or(ModMaterialError::AssetStoreUnavailable(
                    "ModExtendedSpriteMaterial2d",
                ))?;
        mat.extension.vertex_shader = vertex_shader.clone();
        mat.extension.fragment_shader = fragment_shader.clone();
    }
    if let Some(mut reg) = world.get_resource_mut::<ModWeslMaterialRegistry>()
        && let Some(entries) = reg.wesl_extended_2d_materials.get_mut(&mod_id)
        && let Some(entry) = entries.iter_mut().find(|e| e.handle.id() == handle.id())
    {
        entry.vertex_shader = vertex_shader;
        entry.fragment_shader = fragment_shader;
    }
    Ok(())
}

/// Replaces the bound [`ShaderBuffer`] handle on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
pub fn update_mod_extended_sprite_material_buffer_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModExtendedSpriteMaterial2d>,
    buffer: Handle<ShaderBuffer>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_extended_2d_material_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    {
        let mut assets = world
            .get_resource_mut::<Assets<ModExtendedSpriteMaterial2d>>()
            .ok_or(ModMaterialError::AssetStoreUnavailable(
                "Assets<ModExtendedSpriteMaterial2d>",
            ))?;
        let mut mat =
            assets
                .get_mut(handle.id())
                .ok_or(ModMaterialError::AssetStoreUnavailable(
                    "ModExtendedSpriteMaterial2d",
                ))?;
        mat.extension.buffer = buffer.clone();
    }
    if let Some(mut reg) = world.get_resource_mut::<ModWeslMaterialRegistry>()
        && let Some(entries) = reg.wesl_extended_2d_materials.get_mut(&mod_id)
        && let Some(entry) = entries.iter_mut().find(|e| e.handle.id() == handle.id())
    {
        entry.shader_buffer = buffer;
    }
    Ok(())
}

/// Updates [`ModMaterialParams`] on a `mod_id`-owned legacy [`ModSpriteExtension2d`].
pub fn update_mod_sprite_extension_params_in_world(
    world: &mut World,
    mod_id: ModId,
    handle: &Handle<ModSpriteExtension2d>,
    params: ModMaterialParams,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_sprite_extension_owned_by(mod_id, handle.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    let mut assets = world
        .get_resource_mut::<Assets<ModSpriteExtension2d>>()
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "Assets<ModSpriteExtension2d>",
        ))?;
    let mut ext = assets
        .get_mut(handle.id())
        .ok_or(ModMaterialError::AssetStoreUnavailable(
            "ModSpriteExtension2d",
        ))?;
    ext.params = params;
    Ok(())
}

fn resolve_sprite_quad_mesh(
    world: &mut World,
    mod_id: ModId,
    spec: &ModUnifiedSpriteSpec,
) -> Result<Option<Handle<Mesh>>, ModMaterialError> {
    if let Some(mesh) = &spec.mesh {
        return Ok(Some(mesh.clone()));
    }
    if !spec.attach_unit_quad_mesh {
        return Ok(None);
    }
    let auto_label = format!(
        "__auto_quad_{}x{}",
        spec.quad_size[0].to_bits(),
        spec.quad_size[1].to_bits()
    );
    if let Some(existing) = world
        .get_resource::<ModSpriteMeshBridgeRegistry>()
        .and_then(|reg| reg.get_mesh(mod_id, &auto_label))
        .cloned()
    {
        return Ok(Some(existing));
    }
    let mesh = register_mod_quad_mesh_in_world(
        world,
        mod_id,
        auto_label,
        spec.quad_size[0],
        spec.quad_size[1],
    )?;
    Ok(Some(mesh))
}

/// Spawns a unified Bevy `0.20` 2D sprite entity carrying [`Sprite`], [`Anchor`], optional
/// [`Mesh2d`], and [`SpriteMaterial<ModWeslSpriteExtension2d>`], tracked under `mod_id`.
///
/// Validates that `material` is owned by `mod_id`. When `spec.scoped` is `true`, [`ModScoped`]
/// is attached so the entity despawns automatically on `unload_mod`.
pub fn spawn_mod_sprite_with_material_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModUnifiedSpriteSpec,
    material: Handle<ModWeslSpriteExtension2d>,
) -> Result<Entity, ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_sprite_extension_owned_by(mod_id, material.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    ensure_sprite_bridge_registry(world);
    let mesh_handle = resolve_sprite_quad_mesh(world, mod_id, &spec)?;

    let entity = {
        let mut ent = world.spawn((spec.sprite, spec.anchor, SpriteMaterial(material)));
        if let Some(mesh) = mesh_handle {
            ent.insert(Mesh2d(mesh));
        }
        if spec.scoped {
            ent.insert(ModScoped(mod_id));
        }
        if spec.sync_to_render_world {
            ent.insert(SyncToRenderWorld::default());
        }
        ent.id()
    };
    world.flush();

    world
        .resource_mut::<ModSpriteMeshBridgeRegistry>()
        .record_binding(mod_id, entity, ModSpriteBindingKind::WeslSpriteMaterial);

    Ok(entity)
}

/// Spawns a direct 2D mesh-sprite entity carrying [`Mesh2d`] and
/// [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`], tracked under `mod_id`.
///
/// Validates that `material` is owned by `mod_id`. When `spec.scoped` is `true`, [`ModScoped`]
/// is attached so the entity despawns automatically on `unload_mod`.
pub fn spawn_mod_extended_mesh_sprite_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModUnifiedSpriteSpec,
    material: Handle<ModExtendedSpriteMaterial2d>,
) -> Result<Entity, ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_extended_2d_material_owned_by(mod_id, material.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    ensure_sprite_bridge_registry(world);
    let mesh_handle = match resolve_sprite_quad_mesh(world, mod_id, &spec)? {
        Some(h) => h,
        None => register_mod_quad_mesh_in_world(
            world,
            mod_id,
            "__default_extended_quad",
            spec.quad_size[0],
            spec.quad_size[1],
        )?,
    };

    let entity = {
        let mut ent = world.spawn((Mesh2d(mesh_handle), MeshMaterial2d(material)));
        if spec.scoped {
            ent.insert(ModScoped(mod_id));
        }
        if spec.sync_to_render_world {
            ent.insert(SyncToRenderWorld::default());
        }
        ent.id()
    };
    world.flush();

    world
        .resource_mut::<ModSpriteMeshBridgeRegistry>()
        .record_binding(
            mod_id,
            entity,
            ModSpriteBindingKind::WeslExtendedMeshMaterial2d,
        );

    Ok(entity)
}

/// Attaches a `mod_id`-owned [`SpriteMaterial<ModWeslSpriteExtension2d>`] onto an existing
/// `entity` (for example a persistent host sprite) and tracks it for automatic detachment when
/// `mod_id` unloads.
pub fn attach_mod_sprite_material_in_world(
    world: &mut World,
    mod_id: ModId,
    entity: Entity,
    material: Handle<ModWeslSpriteExtension2d>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_sprite_extension_owned_by(mod_id, material.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    ensure_sprite_bridge_registry(world);

    let Ok(mut ent) = world.get_entity_mut(entity) else {
        return Err(ModMaterialError::EntityNotFound(entity));
    };
    ent.insert(SpriteMaterial(material));
    world.flush();

    world
        .resource_mut::<ModSpriteMeshBridgeRegistry>()
        .record_binding(mod_id, entity, ModSpriteBindingKind::WeslSpriteMaterial);
    Ok(())
}

/// Attaches a `mod_id`-owned [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] onto an existing
/// `entity` and tracks it for automatic detachment when `mod_id` unloads.
pub fn attach_mod_extended_material_2d_in_world(
    world: &mut World,
    mod_id: ModId,
    entity: Entity,
    material: Handle<ModExtendedSpriteMaterial2d>,
) -> Result<(), ModMaterialError> {
    let owned = world
        .get_resource::<ModWeslMaterialRegistry>()
        .is_some_and(|reg| reg.is_wesl_extended_2d_material_owned_by(mod_id, material.id()));
    if !owned {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    ensure_sprite_bridge_registry(world);

    let Ok(mut ent) = world.get_entity_mut(entity) else {
        return Err(ModMaterialError::EntityNotFound(entity));
    };
    ent.insert(MeshMaterial2d(material));
    world.flush();

    world
        .resource_mut::<ModSpriteMeshBridgeRegistry>()
        .record_binding(
            mod_id,
            entity,
            ModSpriteBindingKind::WeslExtendedMeshMaterial2d,
        );
    Ok(())
}

/// Detaches [`SpriteMaterial<ModWeslSpriteExtension2d>`] from `entity` if tracked under `mod_id`,
/// returning `true` if a binding was removed.
pub fn detach_mod_sprite_material_in_world(
    world: &mut World,
    mod_id: ModId,
    entity: Entity,
) -> bool {
    let removed = world
        .get_resource_mut::<ModSpriteMeshBridgeRegistry>()
        .is_some_and(|mut reg| {
            reg.remove_binding(mod_id, entity, ModSpriteBindingKind::WeslSpriteMaterial)
        });
    if removed {
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.remove::<SpriteMaterial<ModWeslSpriteExtension2d>>();
        }
        world.flush();
    }
    removed
}

/// Detaches [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] from `entity` if tracked under
/// `mod_id`, returning `true` if a binding was removed.
pub fn detach_mod_extended_material_2d_in_world(
    world: &mut World,
    mod_id: ModId,
    entity: Entity,
) -> bool {
    let removed = world
        .get_resource_mut::<ModSpriteMeshBridgeRegistry>()
        .is_some_and(|mut reg| {
            reg.remove_binding(
                mod_id,
                entity,
                ModSpriteBindingKind::WeslExtendedMeshMaterial2d,
            )
        });
    if removed {
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.remove::<MeshMaterial2d<ModExtendedSpriteMaterial2d>>();
        }
        world.flush();
    }
    removed
}

/// Detaches all mod-attached 2D sprite material components from surviving entities and purges all
/// `mod_id`-owned quad [`Mesh`] assets from [`Assets<Mesh>`].
///
/// Invoked automatically via [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks) on `unload_mod`.
pub fn cleanup_mod_sprite_mesh_bindings_in_world(world: &mut World, mod_id: ModId) -> usize {
    let (meshes, bindings) = {
        let Some(mut reg) = world.get_resource_mut::<ModSpriteMeshBridgeRegistry>() else {
            return 0;
        };
        (
            reg.meshes.remove(&mod_id).unwrap_or_default(),
            reg.bindings.remove(&mod_id).unwrap_or_default(),
        )
    };

    let mut cleaned = 0usize;

    for (entity, kind) in bindings {
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            match kind {
                ModSpriteBindingKind::WeslSpriteMaterial => {
                    if ent.contains::<SpriteMaterial<ModWeslSpriteExtension2d>>() {
                        ent.remove::<SpriteMaterial<ModWeslSpriteExtension2d>>();
                        cleaned += 1;
                    }
                }
                ModSpriteBindingKind::LegacySpriteMaterial => {
                    if ent.contains::<SpriteMaterial<ModSpriteExtension2d>>() {
                        ent.remove::<SpriteMaterial<ModSpriteExtension2d>>();
                        cleaned += 1;
                    }
                }
                ModSpriteBindingKind::WeslExtendedMeshMaterial2d => {
                    if ent.contains::<MeshMaterial2d<ModExtendedSpriteMaterial2d>>() {
                        ent.remove::<MeshMaterial2d<ModExtendedSpriteMaterial2d>>();
                        cleaned += 1;
                    }
                }
                ModSpriteBindingKind::LegacyExtendedMeshMaterial2d => {
                    if ent.contains::<MeshMaterial2d<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>() {
                        ent.remove::<MeshMaterial2d<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>();
                        cleaned += 1;
                    }
                }
            }
        }
    }
    // Flush component lifecycle hooks (such as `SpriteMaterial::on_remove`) immediately so
    // `MeshMaterial2d<SpriteExt<M>>` is stripped before `unload_mod` returns.
    world.flush();

    if let Some(mut mesh_assets) = world.get_resource_mut::<Assets<Mesh>>() {
        for (_, handle) in meshes {
            if mesh_assets.remove(handle.id()).is_some() {
                cleaned += 1;
            }
        }
    }

    cleaned
}

// ============================================================================
// Section 3: Opt-In `MeshPipelineDescriptor` / Mesh-Shader Pipeline Bridge
// ============================================================================

/// Opaque, non-zero identifier for a registered mod-owned [`MeshPipelineDescriptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModMeshPipelineId(u64);

impl ModMeshPipelineId {
    /// Returns the raw numeric identifier.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Host opt-in configuration controlling whether mods may register [`MeshPipelineDescriptor`]s.
///
/// Mesh shaders require explicit host opt-in because they depend on hardware capabilities
/// ([`WgpuFeatures::EXPERIMENTAL_MESH_SHADER`]). Installing [`ModMeshShaderPlugin`] inserts this
/// resource with `enabled: true`.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct ModMeshShaderConfig {
    /// Master opt-in switch; when `false` (or when this resource is absent), registration returns
    /// [`ModMaterialError::MeshShadersNotEnabled`].
    pub enabled: bool,
    /// Optional per-mod limit on simultaneously registered mesh-shader pipelines.
    pub max_pipelines_per_mod: Option<usize>,
    /// Required [`WgpuFeatures`] checked before queueing pipelines onto a live [`PipelineCache`].
    pub required_wgpu_features: WgpuFeatures,
    /// Whether `ExtractSchedule` automatically queues descriptors onto [`PipelineCache`] when a
    /// live [`RenderDevice`] with [`Self::required_wgpu_features`] is present.
    pub auto_queue_in_render_app: bool,
}

impl Default for ModMeshShaderConfig {
    fn default() -> Self {
        Self::enabled()
    }
}

impl ModMeshShaderConfig {
    /// Creates an enabled [`ModMeshShaderConfig`] requiring
    /// [`WgpuFeatures::EXPERIMENTAL_MESH_SHADER`] for live GPU pipeline queueing.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            max_pipelines_per_mod: None,
            required_wgpu_features: WgpuFeatures::EXPERIMENTAL_MESH_SHADER,
            auto_queue_in_render_app: true,
        }
    }

    /// Creates a disabled [`ModMeshShaderConfig`] that rejects mod mesh-shader registrations.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::enabled()
        }
    }

    /// Sets an upper bound on how many mesh-shader pipelines a single mod may register at once.
    pub fn with_max_pipelines_per_mod(mut self, limit: usize) -> Self {
        self.max_pipelines_per_mod = Some(limit);
        self
    }

    /// Overrides the required [`WgpuFeatures`] bitmask checked before live GPU queueing.
    pub fn with_required_wgpu_features(mut self, features: WgpuFeatures) -> Self {
        self.required_wgpu_features = features;
        self
    }

    /// Configures whether live [`RenderApp`] extraction automatically calls
    /// [`PipelineCache::queue_mesh_pipeline`].
    pub fn with_auto_queue_in_render_app(mut self, auto_queue: bool) -> Self {
        self.auto_queue_in_render_app = auto_queue;
        self
    }
}

/// Shader stage configuration for a task, mesh, or fragment stage inside [`ModMeshPipelineSpec`].
#[derive(Debug, Clone)]
pub struct ModMeshStageSpec {
    /// Compiled `.wesl` (or `.wgsl`) shader asset handle.
    pub shader: Handle<Shader>,
    /// Optional entry point function name.
    pub entry_point: Option<Cow<'static, str>>,
    /// Preprocessor shader definitions for this stage.
    pub shader_defs: Vec<ShaderDefVal>,
    /// Pipeline-overridable constant values (`override` declarations).
    pub constants: Vec<(Cow<'static, str>, f64)>,
}

impl ModMeshStageSpec {
    /// Creates a stage specification backed by `shader` using its default entry point.
    pub fn new(shader: Handle<Shader>) -> Self {
        Self {
            shader,
            entry_point: None,
            shader_defs: Vec::new(),
            constants: Vec::new(),
        }
    }

    /// Overrides the stage entry point function name.
    pub fn with_entry_point(mut self, entry_point: impl Into<Cow<'static, str>>) -> Self {
        self.entry_point = Some(entry_point.into());
        self
    }

    /// Appends preprocessor [`ShaderDefVal`] entries to this stage.
    pub fn with_shader_defs(mut self, defs: impl IntoIterator<Item = ShaderDefVal>) -> Self {
        self.shader_defs.extend(defs);
        self
    }

    /// Appends a pipeline-overridable numeric constant to this stage.
    pub fn with_constant(mut self, name: impl Into<Cow<'static, str>>, value: f64) -> Self {
        self.constants.push((name.into(), value));
        self
    }

    fn into_task_state(self) -> TaskState {
        TaskState {
            shader: self.shader,
            shader_defs: self.shader_defs,
            entry_point: self.entry_point,
            constants: self.constants,
        }
    }

    fn into_mesh_state(self) -> MeshState {
        MeshState {
            shader: self.shader,
            shader_defs: self.shader_defs,
            entry_point: self.entry_point,
            constants: self.constants,
        }
    }

    fn into_fragment_state(self, targets: Vec<Option<ColorTargetState>>) -> FragmentState {
        FragmentState {
            shader: self.shader,
            shader_defs: self.shader_defs,
            entry_point: self.entry_point,
            targets,
            constants: self.constants,
        }
    }
}

/// Data-driven specification for registering a mod-owned Bevy `0.20` [`MeshPipelineDescriptor`].
#[derive(Debug, Clone)]
pub struct ModMeshPipelineSpec {
    /// Registry and debug label for the mesh pipeline.
    pub label: String,
    /// Optional task (amplification) shader stage.
    pub task: Option<ModMeshStageSpec>,
    /// Required mesh shader stage.
    pub mesh: ModMeshStageSpec,
    /// Optional fragment shader stage.
    pub fragment: Option<ModMeshStageSpec>,
    /// Color render target states attached to the fragment stage.
    pub color_targets: Vec<Option<ColorTargetState>>,
    /// Bind group layout descriptors for the pipeline.
    pub layout: Vec<BindGroupLayoutDescriptor>,
    /// Push constant / immediate byte size (defaults to `0`).
    pub immediate_size: u32,
    /// Primitive assembly and rasterization state.
    pub primitive: PrimitiveState,
    /// Optional depth/stencil state.
    pub depth_stencil: Option<DepthStencilState>,
    /// Multisample state.
    pub multisample: MultisampleState,
    /// Whether to zero-initialize workgroup memory (defaults to `true` for safety).
    pub zero_initialize_workgroup_memory: bool,
    /// Associated [`ShaderBuffer`] storage handles referenced by this mesh pipeline.
    pub shader_buffers: Vec<Handle<ShaderBuffer>>,
    /// Optional pre-constructed [`MeshPipelineDescriptor`] override.
    pub override_descriptor: Option<MeshPipelineDescriptor>,
}

impl ModMeshPipelineSpec {
    /// Creates a [`ModMeshPipelineSpec`] with the given `label` and required `mesh_shader`.
    pub fn new(label: impl Into<String>, mesh_shader: Handle<Shader>) -> Self {
        Self {
            label: label.into(),
            task: None,
            mesh: ModMeshStageSpec::new(mesh_shader),
            fragment: None,
            color_targets: Vec::new(),
            layout: Vec::new(),
            immediate_size: 0,
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            zero_initialize_workgroup_memory: true,
            shader_buffers: Vec::new(),
            override_descriptor: None,
        }
    }

    /// Creates a [`ModMeshPipelineSpec`] wrapping a pre-built Bevy [`MeshPipelineDescriptor`].
    pub fn from_descriptor(label: impl Into<String>, descriptor: MeshPipelineDescriptor) -> Self {
        let mesh_shader = descriptor.mesh.shader.clone();
        Self {
            label: label.into(),
            mesh: ModMeshStageSpec::new(mesh_shader),
            override_descriptor: Some(descriptor),
            ..Self::new("", Handle::default())
        }
    }

    /// Replaces the mesh shader stage specification.
    pub fn with_mesh_stage(mut self, stage: ModMeshStageSpec) -> Self {
        self.mesh = stage;
        self
    }

    /// Attaches an optional task (amplification) shader stage.
    pub fn with_task_stage(mut self, stage: ModMeshStageSpec) -> Self {
        self.task = Some(stage);
        self
    }

    /// Attaches an optional fragment shader stage.
    pub fn with_fragment_stage(mut self, stage: ModMeshStageSpec) -> Self {
        self.fragment = Some(stage);
        self
    }

    /// Sets the fragment color target states.
    pub fn with_color_targets(mut self, targets: Vec<Option<ColorTargetState>>) -> Self {
        self.color_targets = targets;
        self
    }

    /// Sets the pipeline's [`BindGroupLayoutDescriptor`]s.
    pub fn with_layouts(mut self, layouts: Vec<BindGroupLayoutDescriptor>) -> Self {
        self.layout = layouts;
        self
    }

    /// Sets the pipeline's immediate / push-constant byte size.
    pub fn with_immediate_size(mut self, immediate_size: u32) -> Self {
        self.immediate_size = immediate_size;
        self
    }

    /// Sets [`PrimitiveState`].
    pub fn with_primitive(mut self, primitive: PrimitiveState) -> Self {
        self.primitive = primitive;
        self
    }

    /// Sets [`DepthStencilState`].
    pub fn with_depth_stencil(mut self, depth_stencil: DepthStencilState) -> Self {
        self.depth_stencil = Some(depth_stencil);
        self
    }

    /// Sets [`MultisampleState`].
    pub fn with_multisample(mut self, multisample: MultisampleState) -> Self {
        self.multisample = multisample;
        self
    }

    /// Sets `zero_initialize_workgroup_memory`.
    pub fn with_zero_initialize_workgroup_memory(mut self, zero_init: bool) -> Self {
        self.zero_initialize_workgroup_memory = zero_init;
        self
    }

    /// Associates a storage [`Handle<ShaderBuffer>`] with this pipeline registration.
    pub fn with_shader_buffer(mut self, buffer: Handle<ShaderBuffer>) -> Self {
        self.shader_buffers.push(buffer);
        self
    }

    /// Validates this spec and constructs a Bevy `0.20` [`MeshPipelineDescriptor`].
    pub fn build_descriptor(&self) -> Result<MeshPipelineDescriptor, ModMaterialError> {
        if self.label.is_empty() {
            return Err(ModMaterialError::InvalidPipelineDescriptor(
                "mesh pipeline label must not be empty".to_owned(),
            ));
        }
        let mut descriptor = if let Some(override_desc) = &self.override_descriptor {
            override_desc.clone()
        } else {
            MeshPipelineDescriptor {
                label: Some(Cow::Owned(self.label.clone())),
                layout: self.layout.clone(),
                immediate_size: self.immediate_size,
                task: self.task.clone().map(ModMeshStageSpec::into_task_state),
                mesh: self.mesh.clone().into_mesh_state(),
                primitive: self.primitive,
                depth_stencil: self.depth_stencil.clone(),
                multisample: self.multisample,
                fragment: self
                    .fragment
                    .clone()
                    .map(|frag| frag.into_fragment_state(self.color_targets.clone())),
                zero_initialize_workgroup_memory: self.zero_initialize_workgroup_memory,
            }
        };

        if descriptor.label.is_none() {
            descriptor.label = Some(Cow::Owned(self.label.clone()));
        }
        if descriptor.mesh.shader == Handle::default() {
            return Err(ModMaterialError::InvalidPipelineDescriptor(
                "mesh shader stage requires a registered Shader handle".to_owned(),
            ));
        }
        if let Some(task) = &descriptor.task
            && task.shader == Handle::default()
        {
            return Err(ModMaterialError::InvalidPipelineDescriptor(
                "task shader stage requires a registered Shader handle".to_owned(),
            ));
        }
        if let Some(fragment) = &descriptor.fragment
            && fragment.shader == Handle::default()
        {
            return Err(ModMaterialError::InvalidPipelineDescriptor(
                "fragment shader stage requires a registered Shader handle".to_owned(),
            ));
        }

        Ok(descriptor)
    }
}

fn descriptor_shader_ids(descriptor: &MeshPipelineDescriptor) -> Vec<AssetId<Shader>> {
    let mut ids = Vec::with_capacity(3);
    ids.push(descriptor.mesh.shader.id());
    if let Some(task) = &descriptor.task {
        let id = task.shader.id();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if let Some(fragment) = &descriptor.fragment {
        let id = fragment.shader.id();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// Recorded mod-owned mesh-shader pipeline entry stored in [`ModMeshShaderRegistry`].
#[derive(Debug, Clone)]
pub struct ModMeshPipelineEntry {
    /// Unique pipeline handle.
    pub id: ModMeshPipelineId,
    /// Owning [`ModId`].
    pub mod_id: ModId,
    /// Registry/debug label.
    pub label: String,
    /// Complete Bevy `0.20` [`MeshPipelineDescriptor`].
    pub descriptor: MeshPipelineDescriptor,
    /// Associated [`ShaderBuffer`] storage handles.
    pub shader_buffers: Vec<Handle<ShaderBuffer>>,
}

impl ModMeshPipelineEntry {
    /// Returns all unique [`AssetId<Shader>`] handles referenced by this pipeline's task, mesh,
    /// and fragment stages.
    pub fn referenced_shader_ids(&self) -> Vec<AssetId<Shader>> {
        descriptor_shader_ids(&self.descriptor)
    }
}

/// Host [`Resource`] recording all mod-owned opt-in [`MeshPipelineDescriptor`] registrations.
///
/// Automatically purged during [`ModWorldExt::unload_mod`](bevy_abi_mod_host::ModWorldExt::unload_mod),
/// [`ModWorldExt::unload_all_mods`](bevy_abi_mod_host::ModWorldExt::unload_all_mods), and
/// [`ModWorldExt::reload_mod`](bevy_abi_mod_host::ModWorldExt::reload_mod) via
/// [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks).
#[derive(Resource, Debug)]
pub struct ModMeshShaderRegistry {
    next_id: u64,
    entries_by_mod: HashMap<ModId, Vec<ModMeshPipelineEntry>>,
    owner_by_id: HashMap<ModMeshPipelineId, ModId>,
}

impl Default for ModMeshShaderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModMeshShaderRegistry {
    /// Creates an empty [`ModMeshShaderRegistry`].
    pub fn new() -> Self {
        Self {
            next_id: 1,
            entries_by_mod: HashMap::default(),
            owner_by_id: HashMap::default(),
        }
    }

    /// Returns the number of registered mesh-shader pipelines currently owned by `mod_id`.
    pub fn pipeline_count_for_mod(&self, mod_id: ModId) -> usize {
        self.entries_by_mod.get(&mod_id).map_or(0, Vec::len)
    }

    /// Returns the total number of registered mesh-shader pipelines across all loaded mods.
    pub fn total_pipeline_count(&self) -> usize {
        self.owner_by_id.len()
    }

    /// Returns `true` if `id` is currently registered.
    pub fn contains(&self, id: ModMeshPipelineId) -> bool {
        self.owner_by_id.contains_key(&id)
    }

    /// Returns `true` if `id` is currently owned by `mod_id`.
    pub fn is_owned_by(&self, mod_id: ModId, id: ModMeshPipelineId) -> bool {
        self.owner_by_id.get(&id) == Some(&mod_id)
    }

    /// Looks up a [`ModMeshPipelineEntry`] by [`ModMeshPipelineId`].
    pub fn get(&self, id: ModMeshPipelineId) -> Option<&ModMeshPipelineEntry> {
        let mod_id = self.owner_by_id.get(&id)?;
        self.entries_by_mod
            .get(mod_id)?
            .iter()
            .find(|entry| entry.id == id)
    }

    /// Looks up a [`ModMeshPipelineEntry`] by `mod_id` and `label`.
    pub fn get_by_label(&self, mod_id: ModId, label: &str) -> Option<&ModMeshPipelineEntry> {
        self.entries_by_mod
            .get(&mod_id)?
            .iter()
            .find(|entry| entry.label == label)
    }

    /// Returns all mesh-shader pipeline entries owned by `mod_id`.
    pub fn pipelines_for_mod(&self, mod_id: ModId) -> &[ModMeshPipelineEntry] {
        self.entries_by_mod.get(&mod_id).map_or(&[], Vec::as_slice)
    }

    /// Iterates over every active [`ModMeshPipelineEntry`] across all mods.
    pub fn all_pipelines(&self) -> impl Iterator<Item = &ModMeshPipelineEntry> {
        self.entries_by_mod.values().flat_map(|v| v.iter())
    }

    /// Returns `true` if any active mesh-shader pipeline still references `shader_id`.
    pub fn references_shader(&self, shader_id: AssetId<Shader>) -> bool {
        self.all_pipelines()
            .any(|entry| entry.referenced_shader_ids().contains(&shader_id))
    }

    fn alloc_id(&mut self) -> ModMeshPipelineId {
        let id = ModMeshPipelineId(self.next_id);
        self.next_id = self.next_id.saturating_add(1).max(1);
        id
    }
}

/// Sub-world mirror of a single extracted mod mesh-shader pipeline.
#[derive(Debug, Clone)]
pub struct ExtractedModMeshPipeline {
    /// Pipeline identifier from [`ModMeshShaderRegistry`].
    pub id: ModMeshPipelineId,
    /// Owning [`ModId`].
    pub mod_id: ModId,
    /// Registry/debug label.
    pub label: String,
    /// Extracted Bevy `0.20` [`MeshPipelineDescriptor`].
    pub descriptor: MeshPipelineDescriptor,
    /// Cached GPU render pipeline ID if queued onto a live [`PipelineCache`].
    pub cached_pipeline_id: Option<CachedRenderPipelineId>,
}

/// Resource living inside [`RenderApp`] (or a custom [`SubApp`](bevy_app::SubApp)) mirroring
/// active mod [`MeshPipelineDescriptor`]s and tracking pipelines/shaders evicted on `unload_mod`.
#[derive(Resource, Default, Debug)]
pub struct ExtractedModMeshPipelines {
    active: HashMap<ModMeshPipelineId, ExtractedModMeshPipeline>,
    evicted_pipeline_ids: Vec<ModMeshPipelineId>,
    evicted_shader_ids: Vec<AssetId<Shader>>,
}

impl ExtractedModMeshPipelines {
    /// Creates an empty [`ExtractedModMeshPipelines`] store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of active extracted mod mesh pipelines.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// Returns `true` when no mod mesh pipelines are currently extracted.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Returns `true` if `id` is currently active in the sub-world.
    pub fn contains(&self, id: ModMeshPipelineId) -> bool {
        self.active.contains_key(&id)
    }

    /// Looks up an extracted pipeline by [`ModMeshPipelineId`].
    pub fn get(&self, id: ModMeshPipelineId) -> Option<&ExtractedModMeshPipeline> {
        self.active.get(&id)
    }

    /// Returns the number of active extracted pipelines owned by `mod_id`.
    pub fn count_for_mod(&self, mod_id: ModId) -> usize {
        self.active
            .values()
            .filter(|entry| entry.mod_id == mod_id)
            .count()
    }

    /// Returns `true` if any active extracted pipeline still references `shader_id`.
    pub fn references_shader(&self, shader_id: AssetId<Shader>) -> bool {
        self.active
            .values()
            .any(|entry| descriptor_shader_ids(&entry.descriptor).contains(&shader_id))
    }

    /// Pipeline IDs evicted during the most recent [`ExtractSchedule`] pass.
    pub fn evicted_pipeline_ids(&self) -> &[ModMeshPipelineId] {
        &self.evicted_pipeline_ids
    }

    /// Shader IDs purged from extracted pipelines during the most recent [`ExtractSchedule`] pass.
    pub fn evicted_shader_ids(&self) -> &[AssetId<Shader>] {
        &self.evicted_shader_ids
    }
}

#[derive(Resource, Default)]
struct ModMeshPipelineExtractInstalled;

fn install_mesh_pipeline_extraction_in_sub_app<L: AppLabel + Default + Copy + Eq>(app: &mut App) {
    let Some(sub_app) = app.get_sub_app_mut(L::default()) else {
        return;
    };
    if sub_app
        .world()
        .contains_resource::<ModMeshPipelineExtractInstalled>()
    {
        return;
    }
    sub_app
        .init_resource::<ExtractedModMeshPipelines>()
        .init_resource::<ModMeshPipelineExtractInstalled>();

    if sub_app
        .world()
        .get_resource::<bevy_ecs::schedule::Schedules>()
        .is_none_or(|s| !s.contains(ExtractSchedule))
    {
        let mut extract_schedule = Schedule::new(ExtractSchedule);
        extract_schedule.set_apply_final_deferred(false);
        sub_app.add_schedule(extract_schedule);
    }

    sub_app.add_systems(ExtractSchedule, extract_mod_mesh_pipelines);
}

fn extract_mod_mesh_pipelines(sub_world: &mut World) {
    if !sub_world.contains_resource::<MainWorld>() {
        return;
    }
    if !sub_world.contains_resource::<ExtractedModMeshPipelines>() {
        sub_world.init_resource::<ExtractedModMeshPipelines>();
    }

    let (main_entries, config) = {
        let main_world = sub_world.resource::<MainWorld>();
        let entries: Vec<ModMeshPipelineEntry> = main_world
            .get_resource::<ModMeshShaderRegistry>()
            .map(|reg| reg.all_pipelines().cloned().collect())
            .unwrap_or_default();
        let cfg = main_world
            .get_resource::<ModMeshShaderConfig>()
            .cloned()
            .unwrap_or_else(ModMeshShaderConfig::disabled);
        (entries, cfg)
    };

    let live_ids: HashSet<ModMeshPipelineId> = main_entries.iter().map(|e| e.id).collect();
    let live_shaders: HashSet<AssetId<Shader>> = main_entries
        .iter()
        .flat_map(ModMeshPipelineEntry::referenced_shader_ids)
        .collect();

    let can_queue_gpu = config.enabled
        && config.auto_queue_in_render_app
        && sub_world
            .get_resource::<RenderDevice>()
            .is_some_and(|dev| dev.features().contains(config.required_wgpu_features));

    let mut evicted_shaders_to_purge = Vec::<AssetId<Shader>>::new();

    sub_world.resource_scope::<ExtractedModMeshPipelines, _>(|sub_world, mut extracted| {
        extracted.evicted_pipeline_ids.clear();
        extracted.evicted_shader_ids.clear();

        let removed_ids: Vec<ModMeshPipelineId> = extracted
            .active
            .keys()
            .copied()
            .filter(|id| !live_ids.contains(id))
            .collect();

        for id in removed_ids {
            if let Some(old) = extracted.active.remove(&id) {
                extracted.evicted_pipeline_ids.push(id);
                for shader_id in descriptor_shader_ids(&old.descriptor) {
                    if !live_shaders.contains(&shader_id)
                        && !evicted_shaders_to_purge.contains(&shader_id)
                    {
                        evicted_shaders_to_purge.push(shader_id);
                    }
                }
            }
        }
        extracted
            .evicted_shader_ids
            .extend_from_slice(&evicted_shaders_to_purge);

        if config.enabled {
            for entry in main_entries {
                let needs_insert = extracted
                    .active
                    .get(&entry.id)
                    .is_none_or(|cur| cur.descriptor != entry.descriptor);
                if needs_insert {
                    let cached_pipeline_id = if can_queue_gpu {
                        sub_world
                            .get_resource::<PipelineCache>()
                            .map(|cache| cache.queue_mesh_pipeline(entry.descriptor.clone()))
                    } else {
                        None
                    };
                    extracted.active.insert(
                        entry.id,
                        ExtractedModMeshPipeline {
                            id: entry.id,
                            mod_id: entry.mod_id,
                            label: entry.label,
                            descriptor: entry.descriptor,
                            cached_pipeline_id,
                        },
                    );
                }
            }
        }
    });

    if !evicted_shaders_to_purge.is_empty()
        && let Some(mut cache) = sub_world.get_resource_mut::<PipelineCache>()
    {
        for shader_id in evicted_shaders_to_purge {
            cache.remove_shader(shader_id);
        }
    }
}

const MESH_SHADER_PIPELINES_UNLOAD_HOOK_KEY: &str = "bevy_abi_mod_render::mesh_shader_pipelines";

fn cleanup_mod_mesh_pipelines_hook(world: &mut World, mod_id: ModId) {
    let _ = cleanup_mod_mesh_pipelines_in_world(world, mod_id);
}

/// Opt-in Bevy [`Plugin`] enabling mod-owned [`MeshPipelineDescriptor`] registrations with
/// automatic [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks) cleanup and [`RenderApp`]
/// extraction.
///
/// Hosts that do not install [`ModMeshShaderPlugin`] reject mesh-shader pipeline registrations
/// with [`ModMaterialError::MeshShadersNotEnabled`].
#[derive(Debug, Clone)]
pub struct ModMeshShaderPlugin {
    config: ModMeshShaderConfig,
}

impl Default for ModMeshShaderPlugin {
    fn default() -> Self {
        Self::new(ModMeshShaderConfig::enabled())
    }
}

impl ModMeshShaderPlugin {
    /// Creates a [`ModMeshShaderPlugin`] with an explicit [`ModMeshShaderConfig`].
    pub fn new(config: ModMeshShaderConfig) -> Self {
        Self { config }
    }
}

impl Plugin for ModMeshShaderPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<ModWeslMaterialPlugin>() {
            app.add_plugins(ModWeslMaterialPlugin);
        }
        app.insert_resource(self.config.clone());
        if !app.world().contains_resource::<ModMeshShaderRegistry>() {
            app.insert_resource(ModMeshShaderRegistry::new());
        }
        register_mod_unload_hook_in_world(
            app.world_mut(),
            MESH_SHADER_PIPELINES_UNLOAD_HOOK_KEY,
            cleanup_mod_mesh_pipelines_hook,
        );
        install_mesh_pipeline_extraction_in_sub_app::<RenderApp>(app);
    }

    fn finish(&self, app: &mut App) {
        install_mesh_pipeline_extraction_in_sub_app::<RenderApp>(app);
    }
}

/// Companion plugin mirroring [`ModMeshShaderRegistry`] into an arbitrary [`SubApp`](bevy_app::SubApp)
/// `L` (such as [`RenderApp`] or a headless test sub-app) during [`ExtractSchedule`].
#[derive(Debug, Clone)]
pub struct ModMeshShaderSubAppPlugin<L: AppLabel + Default + Copy + Eq> {
    marker: PhantomData<L>,
}

impl<L: AppLabel + Default + Copy + Eq> Default for ModMeshShaderSubAppPlugin<L> {
    fn default() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<L: AppLabel + Default + Copy + Eq> Plugin for ModMeshShaderSubAppPlugin<L> {
    fn build(&self, app: &mut App) {
        install_mesh_pipeline_extraction_in_sub_app::<L>(app);
    }

    fn finish(&self, app: &mut App) {
        install_mesh_pipeline_extraction_in_sub_app::<L>(app);
    }
}

/// Registers a `ModId`-owned [`MeshPipelineDescriptor`] in `world`'s [`ModMeshShaderRegistry`].
///
/// Requires [`ModMeshShaderPlugin`] (or an enabled [`ModMeshShaderConfig`] resource) to be
/// installed in `world`; otherwise returns [`ModMaterialError::MeshShadersNotEnabled`].
pub fn register_mod_mesh_pipeline_in_world(
    world: &mut World,
    mod_id: ModId,
    spec: ModMeshPipelineSpec,
) -> Result<ModMeshPipelineId, ModMaterialError> {
    let config = world
        .get_resource::<ModMeshShaderConfig>()
        .cloned()
        .ok_or(ModMaterialError::MeshShadersNotEnabled)?;
    if !config.enabled {
        return Err(ModMaterialError::MeshShadersNotEnabled);
    }

    let descriptor = spec.build_descriptor()?;

    if !world.contains_resource::<ModMeshShaderRegistry>() {
        world.insert_resource(ModMeshShaderRegistry::new());
    }
    register_mod_unload_hook_in_world(
        world,
        MESH_SHADER_PIPELINES_UNLOAD_HOOK_KEY,
        cleanup_mod_mesh_pipelines_hook,
    );

    let mut reg = world.resource_mut::<ModMeshShaderRegistry>();
    if let Some(limit) = config.max_pipelines_per_mod
        && reg.pipeline_count_for_mod(mod_id) >= limit
    {
        return Err(ModMaterialError::MeshPipelineQuotaExceeded { mod_id, limit });
    }

    let id = reg.alloc_id();
    let entry = ModMeshPipelineEntry {
        id,
        mod_id,
        label: spec.label,
        descriptor,
        shader_buffers: spec.shader_buffers,
    };
    reg.owner_by_id.insert(id, mod_id);
    reg.entries_by_mod.entry(mod_id).or_default().push(entry);
    Ok(id)
}

/// Unregisters a single `ModId`-owned mesh-shader pipeline by [`ModMeshPipelineId`].
pub fn unregister_mod_mesh_pipeline_in_world(
    world: &mut World,
    mod_id: ModId,
    id: ModMeshPipelineId,
) -> Result<(), ModMaterialError> {
    let Some(mut reg) = world.get_resource_mut::<ModMeshShaderRegistry>() else {
        return Err(ModMaterialError::PipelineNotFound(id.raw()));
    };
    let Some(&owner) = reg.owner_by_id.get(&id) else {
        return Err(ModMaterialError::PipelineNotFound(id.raw()));
    };
    if owner != mod_id {
        return Err(ModMaterialError::ModNotOwner { mod_id });
    }
    reg.owner_by_id.remove(&id);
    if let Some(entries) = reg.entries_by_mod.get_mut(&mod_id) {
        entries.retain(|entry| entry.id != id);
        if entries.is_empty() {
            reg.entries_by_mod.remove(&mod_id);
        }
    }
    Ok(())
}

/// Removes all registered [`MeshPipelineDescriptor`] entries owned by `mod_id`, returning the
/// number of removed pipelines.
///
/// Invoked automatically via [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks) on `unload_mod`.
pub fn cleanup_mod_mesh_pipelines_in_world(world: &mut World, mod_id: ModId) -> usize {
    let Some(mut reg) = world.get_resource_mut::<ModMeshShaderRegistry>() else {
        return 0;
    };
    let removed_entries = reg.entries_by_mod.remove(&mod_id).unwrap_or_default();
    let count = removed_entries.len();
    for entry in removed_entries {
        reg.owner_by_id.remove(&entry.id);
    }
    count
}

// ============================================================================
// Section 4: Extension Traits on `World` & `App`
// ============================================================================

/// Extension trait on [`World`] for `.wesl` [`SpriteMaterial`], [`ExtendedMaterial2d`], and
/// unified Bevy `0.20` [`Mesh2d`] + [`SpriteMaterial`] entity bridge operations.
pub trait ModSpriteMeshWorldExt {
    /// Registers a `ModId`-owned [`ModWeslSpriteExtension2d`] asset for [`SpriteMaterial`].
    fn register_mod_wesl_sprite_extension(
        &mut self,
        mod_id: ModId,
        spec: ModWeslSpriteExtensionSpec,
    ) -> Result<Handle<ModWeslSpriteExtension2d>, ModMaterialError>;

    /// Registers a `ModId`-owned [`ModExtendedSpriteMaterial2d`]
    /// (`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`).
    fn register_mod_wesl_extended_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModExtendedSpriteMaterialSpec,
    ) -> Result<Handle<ModExtendedSpriteMaterial2d>, ModMaterialError>;

    /// Registers a `ModId`-owned 2D quad [`Mesh`] (`width x height`).
    fn register_mod_quad_mesh(
        &mut self,
        mod_id: ModId,
        label: impl Into<String>,
        width: f32,
        height: f32,
    ) -> Result<Handle<Mesh>, ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Updates runtime `.wesl` vertex/fragment shader handles on a `mod_id`-owned
    /// [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError>;

    /// Updates the bound [`ShaderBuffer`] on a `mod_id`-owned [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Updates runtime `.wesl` vertex/fragment shader handles on a `mod_id`-owned
    /// [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError>;

    /// Updates the bound [`ShaderBuffer`] on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned legacy [`ModSpriteExtension2d`].
    fn update_mod_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Spawns a unified Bevy `0.20` 2D sprite carrying [`Sprite`], [`Anchor`], [`Mesh2d`], and
    /// [`SpriteMaterial<ModWeslSpriteExtension2d>`].
    fn spawn_mod_sprite_with_material(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<Entity, ModMaterialError>;

    /// Spawns a 2D mesh-sprite entity carrying [`Mesh2d`] and
    /// [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`].
    fn spawn_mod_extended_mesh_sprite(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<Entity, ModMaterialError>;

    /// Attaches a `mod_id`-owned [`SpriteMaterial<ModWeslSpriteExtension2d>`] to `entity`.
    fn attach_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<(), ModMaterialError>;

    /// Attaches a `mod_id`-owned [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] to `entity`.
    fn attach_mod_extended_material_2d(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<(), ModMaterialError>;

    /// Detaches a tracked [`SpriteMaterial<ModWeslSpriteExtension2d>`] from `entity`.
    fn detach_mod_sprite_material(&mut self, mod_id: ModId, entity: Entity) -> bool;

    /// Detaches a tracked [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] from `entity`.
    fn detach_mod_extended_material_2d(&mut self, mod_id: ModId, entity: Entity) -> bool;

    /// Cleans up all tracked 2D sprite material bindings and quad meshes owned by `mod_id`.
    fn cleanup_mod_sprite_mesh_bindings(&mut self, mod_id: ModId) -> usize;
}

impl ModSpriteMeshWorldExt for World {
    #[inline]
    fn register_mod_wesl_sprite_extension(
        &mut self,
        mod_id: ModId,
        spec: ModWeslSpriteExtensionSpec,
    ) -> Result<Handle<ModWeslSpriteExtension2d>, ModMaterialError> {
        register_mod_wesl_sprite_extension_in_world(self, mod_id, spec)
    }

    #[inline]
    fn register_mod_wesl_extended_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModExtendedSpriteMaterialSpec,
    ) -> Result<Handle<ModExtendedSpriteMaterial2d>, ModMaterialError> {
        register_mod_wesl_extended_sprite_material_in_world(self, mod_id, spec)
    }

    #[inline]
    fn register_mod_quad_mesh(
        &mut self,
        mod_id: ModId,
        label: impl Into<String>,
        width: f32,
        height: f32,
    ) -> Result<Handle<Mesh>, ModMaterialError> {
        register_mod_quad_mesh_in_world(self, mod_id, label, width, height)
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        update_mod_wesl_sprite_extension_params_in_world(self, mod_id, handle, params)
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError> {
        update_mod_wesl_sprite_extension_shaders_in_world(
            self,
            mod_id,
            handle,
            vertex_shader,
            fragment_shader,
        )
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError> {
        update_mod_wesl_sprite_extension_buffer_in_world(self, mod_id, handle, buffer)
    }

    #[inline]
    fn update_mod_extended_sprite_material_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        update_mod_extended_sprite_material_params_in_world(self, mod_id, handle, params)
    }

    #[inline]
    fn update_mod_extended_sprite_material_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError> {
        update_mod_extended_sprite_material_shaders_in_world(
            self,
            mod_id,
            handle,
            vertex_shader,
            fragment_shader,
        )
    }

    #[inline]
    fn update_mod_extended_sprite_material_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError> {
        update_mod_extended_sprite_material_buffer_in_world(self, mod_id, handle, buffer)
    }

    #[inline]
    fn update_mod_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        update_mod_sprite_extension_params_in_world(self, mod_id, handle, params)
    }

    #[inline]
    fn spawn_mod_sprite_with_material(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<Entity, ModMaterialError> {
        spawn_mod_sprite_with_material_in_world(self, mod_id, spec, material)
    }

    #[inline]
    fn spawn_mod_extended_mesh_sprite(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<Entity, ModMaterialError> {
        spawn_mod_extended_mesh_sprite_in_world(self, mod_id, spec, material)
    }

    #[inline]
    fn attach_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<(), ModMaterialError> {
        attach_mod_sprite_material_in_world(self, mod_id, entity, material)
    }

    #[inline]
    fn attach_mod_extended_material_2d(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<(), ModMaterialError> {
        attach_mod_extended_material_2d_in_world(self, mod_id, entity, material)
    }

    #[inline]
    fn detach_mod_sprite_material(&mut self, mod_id: ModId, entity: Entity) -> bool {
        detach_mod_sprite_material_in_world(self, mod_id, entity)
    }

    #[inline]
    fn detach_mod_extended_material_2d(&mut self, mod_id: ModId, entity: Entity) -> bool {
        detach_mod_extended_material_2d_in_world(self, mod_id, entity)
    }

    #[inline]
    fn cleanup_mod_sprite_mesh_bindings(&mut self, mod_id: ModId) -> usize {
        cleanup_mod_sprite_mesh_bindings_in_world(self, mod_id)
    }
}

/// Extension trait on [`App`] for `.wesl` [`SpriteMaterial`], [`ExtendedMaterial2d`], and unified
/// Bevy `0.20` [`Mesh2d`] + [`SpriteMaterial`] entity bridge operations.
pub trait ModSpriteMeshAppExt {
    /// Registers a `ModId`-owned [`ModWeslSpriteExtension2d`] asset for [`SpriteMaterial`].
    fn register_mod_wesl_sprite_extension(
        &mut self,
        mod_id: ModId,
        spec: ModWeslSpriteExtensionSpec,
    ) -> Result<Handle<ModWeslSpriteExtension2d>, ModMaterialError>;

    /// Registers a `ModId`-owned [`ModExtendedSpriteMaterial2d`]
    /// (`ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`).
    fn register_mod_wesl_extended_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModExtendedSpriteMaterialSpec,
    ) -> Result<Handle<ModExtendedSpriteMaterial2d>, ModMaterialError>;

    /// Registers a `ModId`-owned 2D quad [`Mesh`] (`width x height`).
    fn register_mod_quad_mesh(
        &mut self,
        mod_id: ModId,
        label: impl Into<String>,
        width: f32,
        height: f32,
    ) -> Result<Handle<Mesh>, ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Updates runtime `.wesl` vertex/fragment shader handles on a `mod_id`-owned
    /// [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError>;

    /// Updates the bound [`ShaderBuffer`] on a `mod_id`-owned [`ModWeslSpriteExtension2d`].
    fn update_mod_wesl_sprite_extension_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Updates runtime `.wesl` vertex/fragment shader handles on a `mod_id`-owned
    /// [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError>;

    /// Updates the bound [`ShaderBuffer`] on a `mod_id`-owned [`ModExtendedSpriteMaterial2d`].
    fn update_mod_extended_sprite_material_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError>;

    /// Updates [`ModMaterialParams`] on a `mod_id`-owned legacy [`ModSpriteExtension2d`].
    fn update_mod_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError>;

    /// Spawns a unified Bevy `0.20` 2D sprite carrying [`Sprite`], [`Anchor`], [`Mesh2d`], and
    /// [`SpriteMaterial<ModWeslSpriteExtension2d>`].
    fn spawn_mod_sprite_with_material(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<Entity, ModMaterialError>;

    /// Spawns a 2D mesh-sprite entity carrying [`Mesh2d`] and
    /// [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`].
    fn spawn_mod_extended_mesh_sprite(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<Entity, ModMaterialError>;

    /// Attaches a `mod_id`-owned [`SpriteMaterial<ModWeslSpriteExtension2d>`] to `entity`.
    fn attach_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<(), ModMaterialError>;

    /// Attaches a `mod_id`-owned [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] to `entity`.
    fn attach_mod_extended_material_2d(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<(), ModMaterialError>;

    /// Detaches a tracked [`SpriteMaterial<ModWeslSpriteExtension2d>`] from `entity`.
    fn detach_mod_sprite_material(&mut self, mod_id: ModId, entity: Entity) -> bool;

    /// Detaches a tracked [`MeshMaterial2d<ModExtendedSpriteMaterial2d>`] from `entity`.
    fn detach_mod_extended_material_2d(&mut self, mod_id: ModId, entity: Entity) -> bool;

    /// Cleans up all tracked 2D sprite material bindings and quad meshes owned by `mod_id`.
    fn cleanup_mod_sprite_mesh_bindings(&mut self, mod_id: ModId) -> usize;
}

impl ModSpriteMeshAppExt for App {
    #[inline]
    fn register_mod_wesl_sprite_extension(
        &mut self,
        mod_id: ModId,
        spec: ModWeslSpriteExtensionSpec,
    ) -> Result<Handle<ModWeslSpriteExtension2d>, ModMaterialError> {
        self.world_mut()
            .register_mod_wesl_sprite_extension(mod_id, spec)
    }

    #[inline]
    fn register_mod_wesl_extended_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModExtendedSpriteMaterialSpec,
    ) -> Result<Handle<ModExtendedSpriteMaterial2d>, ModMaterialError> {
        self.world_mut()
            .register_mod_wesl_extended_sprite_material(mod_id, spec)
    }

    #[inline]
    fn register_mod_quad_mesh(
        &mut self,
        mod_id: ModId,
        label: impl Into<String>,
        width: f32,
        height: f32,
    ) -> Result<Handle<Mesh>, ModMaterialError> {
        self.world_mut()
            .register_mod_quad_mesh(mod_id, label, width, height)
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_wesl_sprite_extension_params(mod_id, handle, params)
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut().update_mod_wesl_sprite_extension_shaders(
            mod_id,
            handle,
            vertex_shader,
            fragment_shader,
        )
    }

    #[inline]
    fn update_mod_wesl_sprite_extension_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModWeslSpriteExtension2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_wesl_sprite_extension_buffer(mod_id, handle, buffer)
    }

    #[inline]
    fn update_mod_extended_sprite_material_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_extended_sprite_material_params(mod_id, handle, params)
    }

    #[inline]
    fn update_mod_extended_sprite_material_shaders(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        vertex_shader: Option<Handle<Shader>>,
        fragment_shader: Option<Handle<Shader>>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_extended_sprite_material_shaders(
                mod_id,
                handle,
                vertex_shader,
                fragment_shader,
            )
    }

    #[inline]
    fn update_mod_extended_sprite_material_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModExtendedSpriteMaterial2d>,
        buffer: Handle<ShaderBuffer>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_extended_sprite_material_buffer(mod_id, handle, buffer)
    }

    #[inline]
    fn update_mod_sprite_extension_params(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ModSpriteExtension2d>,
        params: ModMaterialParams,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_sprite_extension_params(mod_id, handle, params)
    }

    #[inline]
    fn spawn_mod_sprite_with_material(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<Entity, ModMaterialError> {
        self.world_mut()
            .spawn_mod_sprite_with_material(mod_id, spec, material)
    }

    #[inline]
    fn spawn_mod_extended_mesh_sprite(
        &mut self,
        mod_id: ModId,
        spec: ModUnifiedSpriteSpec,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<Entity, ModMaterialError> {
        self.world_mut()
            .spawn_mod_extended_mesh_sprite(mod_id, spec, material)
    }

    #[inline]
    fn attach_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModWeslSpriteExtension2d>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .attach_mod_sprite_material(mod_id, entity, material)
    }

    #[inline]
    fn attach_mod_extended_material_2d(
        &mut self,
        mod_id: ModId,
        entity: Entity,
        material: Handle<ModExtendedSpriteMaterial2d>,
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .attach_mod_extended_material_2d(mod_id, entity, material)
    }

    #[inline]
    fn detach_mod_sprite_material(&mut self, mod_id: ModId, entity: Entity) -> bool {
        self.world_mut().detach_mod_sprite_material(mod_id, entity)
    }

    #[inline]
    fn detach_mod_extended_material_2d(&mut self, mod_id: ModId, entity: Entity) -> bool {
        self.world_mut()
            .detach_mod_extended_material_2d(mod_id, entity)
    }

    #[inline]
    fn cleanup_mod_sprite_mesh_bindings(&mut self, mod_id: ModId) -> usize {
        self.world_mut().cleanup_mod_sprite_mesh_bindings(mod_id)
    }
}

/// Extension trait on [`World`] for opt-in mod [`MeshPipelineDescriptor`] registration and
/// cleanup.
pub trait ModMeshShaderWorldExt {
    /// Registers a `ModId`-owned [`MeshPipelineDescriptor`] (requires [`ModMeshShaderPlugin`]).
    fn register_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        spec: ModMeshPipelineSpec,
    ) -> Result<ModMeshPipelineId, ModMaterialError>;

    /// Unregisters a single `ModId`-owned mesh-shader pipeline by [`ModMeshPipelineId`].
    fn unregister_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        id: ModMeshPipelineId,
    ) -> Result<(), ModMaterialError>;

    /// Removes all registered mesh-shader pipelines owned by `mod_id`.
    fn cleanup_mod_mesh_pipelines(&mut self, mod_id: ModId) -> usize;
}

impl ModMeshShaderWorldExt for World {
    #[inline]
    fn register_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        spec: ModMeshPipelineSpec,
    ) -> Result<ModMeshPipelineId, ModMaterialError> {
        register_mod_mesh_pipeline_in_world(self, mod_id, spec)
    }

    #[inline]
    fn unregister_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        id: ModMeshPipelineId,
    ) -> Result<(), ModMaterialError> {
        unregister_mod_mesh_pipeline_in_world(self, mod_id, id)
    }

    #[inline]
    fn cleanup_mod_mesh_pipelines(&mut self, mod_id: ModId) -> usize {
        cleanup_mod_mesh_pipelines_in_world(self, mod_id)
    }
}

/// Extension trait on [`App`] for opt-in mod [`MeshPipelineDescriptor`] registration and cleanup.
pub trait ModMeshShaderAppExt {
    /// Registers a `ModId`-owned [`MeshPipelineDescriptor`] (requires [`ModMeshShaderPlugin`]).
    fn register_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        spec: ModMeshPipelineSpec,
    ) -> Result<ModMeshPipelineId, ModMaterialError>;

    /// Unregisters a single `ModId`-owned mesh-shader pipeline by [`ModMeshPipelineId`].
    fn unregister_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        id: ModMeshPipelineId,
    ) -> Result<(), ModMaterialError>;

    /// Removes all registered mesh-shader pipelines owned by `mod_id`.
    fn cleanup_mod_mesh_pipelines(&mut self, mod_id: ModId) -> usize;
}

impl ModMeshShaderAppExt for App {
    #[inline]
    fn register_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        spec: ModMeshPipelineSpec,
    ) -> Result<ModMeshPipelineId, ModMaterialError> {
        self.world_mut().register_mod_mesh_pipeline(mod_id, spec)
    }

    #[inline]
    fn unregister_mod_mesh_pipeline(
        &mut self,
        mod_id: ModId,
        id: ModMeshPipelineId,
    ) -> Result<(), ModMaterialError> {
        self.world_mut().unregister_mod_mesh_pipeline(mod_id, id)
    }

    #[inline]
    fn cleanup_mod_mesh_pipelines(&mut self, mod_id: ModId) -> usize {
        self.world_mut().cleanup_mod_mesh_pipelines(mod_id)
    }
}
