#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) multi-world `SubApp` extraction, WESL shaders,
//! [`AlignedVec`](bevy_platform::collections::AlignedVec)-backed [`ShaderBuffer`] storage assets,
//! and Bevy `0.20` 2D/3D material modding for `bevy_abi_mod` (`ABI 0.3.0`).
//!
//! Installing [`ModWeslMaterialPlugin`] (or calling any registration helper on [`ModMaterialAppExt`]
//! / [`ModMaterialWorldExt`]) automatically registers a mod-unload cleanup hook with
//! `bevy_abi_mod_host` so shaders, storage buffers, and material assets owned by an unloaded or
//! hot-reloaded mod are purged deterministically from `Assets<T>`.

pub mod sprite_mesh_shaders;
pub mod sub_app_materials;

pub use sprite_mesh_shaders::{
    ExtractedModMeshPipeline, ExtractedModMeshPipelines, ModExtendedSpriteMaterial2d,
    ModExtendedSpriteMaterialSpec, ModMeshPipelineEntry, ModMeshPipelineId, ModMeshPipelineSpec,
    ModMeshShaderAppExt, ModMeshShaderConfig, ModMeshShaderPlugin, ModMeshShaderRegistry,
    ModMeshShaderSubAppPlugin, ModMeshShaderWorldExt, ModMeshStageSpec, ModShaderDef,
    ModSpriteBindingKind, ModSpriteMaterial, ModSpriteMaterialPlugin, ModSpriteMeshAppExt,
    ModSpriteMeshBridgeRegistry, ModSpriteMeshWorldExt, ModUnifiedSpriteSpec,
    ModWeslExtendedMaterialEntry, ModWeslSpriteExtension2d, ModWeslSpriteExtension2dKey,
    ModWeslSpriteExtensionEntry, ModWeslSpriteExtensionSpec, ModWeslSpriteSpecialization,
    apply_wesl_sprite_specialization, attach_mod_extended_material_2d_in_world,
    attach_mod_sprite_material_in_world, build_quad_mesh_2d, cleanup_mod_mesh_pipelines_in_world,
    cleanup_mod_sprite_mesh_bindings_in_world, detach_mod_extended_material_2d_in_world,
    detach_mod_sprite_material_in_world, register_mod_mesh_pipeline_in_world,
    register_mod_quad_mesh_in_world, register_mod_wesl_extended_sprite_material_in_world,
    register_mod_wesl_sprite_extension_in_world, resolve_wesl_sprite_specialization,
    spawn_mod_extended_mesh_sprite_in_world, spawn_mod_sprite_with_material_in_world,
    unregister_mod_mesh_pipeline_in_world, update_mod_extended_sprite_material_buffer_in_world,
    update_mod_extended_sprite_material_params_in_world,
    update_mod_extended_sprite_material_shaders_in_world,
    update_mod_sprite_extension_params_in_world, update_mod_wesl_sprite_extension_buffer_in_world,
    update_mod_wesl_sprite_extension_params_in_world,
    update_mod_wesl_sprite_extension_shaders_in_world,
};
pub use sub_app_materials::{
    ModMaterialError, ModMaterialParams, ModShaderBufferInit, ModShaderBufferSpec,
    ModSpriteExtension2d, ModSpriteMaterialEntry, ModSpriteMaterialSpec, ModStandardMaterialEntry,
    ModStandardMaterialSpec, ModSubAppConfig, ModSubAppExt, ModSubAppPlugin, ModSubAppSchedule,
    ModSubAppSystems, ModSubWorldExt, ModWeslMaterialPlugin, ModWeslMaterialRegistry,
    WeslShaderAssetSpec, WeslShaderSource, cleanup_mod_wesl_materials_in_world,
    register_mod_custom_asset_in_world, register_mod_extended_sprite_material_in_world,
    register_mod_shader_buffer_in_world, register_mod_sprite_material_in_world,
    register_mod_standard_material_in_world, register_mod_wesl_shader_in_world,
    update_mod_shader_buffer_in_world,
};

use bevy_abi_mod_host_sys::ModId;
use bevy_app::App;
use bevy_asset::Handle;
use bevy_ecs::world::World;
use bevy_pbr::StandardMaterial;
use bevy_render::storage::ShaderBuffer;
use bevy_shader::Shader;
use bevy_sprite_render::SpriteMeshMaterial;

/// Extension trait on [`World`] for registering and cleaning up mod-owned `.wesl` shaders,
/// [`ShaderBuffer`]s, and Bevy `0.20` 2D/3D materials.
pub trait ModMaterialWorldExt {
    /// Registers a `ModId`-owned `.wesl` (or `.wgsl`) [`Shader`] asset in this [`World`].
    fn register_mod_wesl_shader(
        &mut self,
        mod_id: ModId,
        spec: WeslShaderAssetSpec,
    ) -> Result<Handle<Shader>, ModMaterialError>;

    /// Registers a `ModId`-owned [`ShaderBuffer`] backed by
    /// [`AlignedVec`](bevy_platform::collections::AlignedVec).
    fn register_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        spec: ModShaderBufferSpec,
    ) -> Result<Handle<ShaderBuffer>, ModMaterialError>;

    /// Updates a `ModId`-owned [`ShaderBuffer`]'s CPU byte payload while preserving alignment.
    fn update_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ShaderBuffer>,
        bytes: &[u8],
    ) -> Result<(), ModMaterialError>;

    /// Registers a `ModId`-owned [`SpriteMeshMaterial`] from a [`ModSpriteMaterialSpec`].
    fn register_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModSpriteMaterialSpec,
    ) -> Result<Handle<SpriteMeshMaterial>, ModMaterialError>;

    /// Registers a `ModId`-owned [`StandardMaterial`] from a [`ModStandardMaterialSpec`].
    fn register_mod_standard_material(
        &mut self,
        mod_id: ModId,
        spec: ModStandardMaterialSpec,
    ) -> Result<Handle<StandardMaterial>, ModMaterialError>;

    /// Removes all `.wesl` shaders, [`ShaderBuffer`]s, and material assets owned by `mod_id`.
    fn cleanup_mod_materials(&mut self, mod_id: ModId) -> usize;
}

impl ModMaterialWorldExt for World {
    #[inline]
    fn register_mod_wesl_shader(
        &mut self,
        mod_id: ModId,
        spec: WeslShaderAssetSpec,
    ) -> Result<Handle<Shader>, ModMaterialError> {
        register_mod_wesl_shader_in_world(self, mod_id, spec)
    }

    #[inline]
    fn register_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        spec: ModShaderBufferSpec,
    ) -> Result<Handle<ShaderBuffer>, ModMaterialError> {
        register_mod_shader_buffer_in_world(self, mod_id, spec)
    }

    #[inline]
    fn update_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ShaderBuffer>,
        bytes: &[u8],
    ) -> Result<(), ModMaterialError> {
        update_mod_shader_buffer_in_world(self, mod_id, handle, bytes)
    }

    #[inline]
    fn register_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModSpriteMaterialSpec,
    ) -> Result<Handle<SpriteMeshMaterial>, ModMaterialError> {
        register_mod_sprite_material_in_world(self, mod_id, spec)
    }

    #[inline]
    fn register_mod_standard_material(
        &mut self,
        mod_id: ModId,
        spec: ModStandardMaterialSpec,
    ) -> Result<Handle<StandardMaterial>, ModMaterialError> {
        register_mod_standard_material_in_world(self, mod_id, spec)
    }

    #[inline]
    fn cleanup_mod_materials(&mut self, mod_id: ModId) -> usize {
        cleanup_mod_wesl_materials_in_world(self, mod_id)
    }
}

/// Extension trait on [`App`] for registering and cleaning up mod-owned `.wesl` shaders,
/// [`ShaderBuffer`]s, and Bevy `0.20` 2D/3D materials.
pub trait ModMaterialAppExt {
    /// Registers a `ModId`-owned `.wesl` (or `.wgsl`) [`Shader`] asset in this [`App`].
    fn register_mod_wesl_shader(
        &mut self,
        mod_id: ModId,
        spec: WeslShaderAssetSpec,
    ) -> Result<Handle<Shader>, ModMaterialError>;

    /// Registers a `ModId`-owned [`ShaderBuffer`] backed by
    /// [`AlignedVec`](bevy_platform::collections::AlignedVec).
    fn register_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        spec: ModShaderBufferSpec,
    ) -> Result<Handle<ShaderBuffer>, ModMaterialError>;

    /// Updates a `ModId`-owned [`ShaderBuffer`]'s CPU byte payload while preserving alignment.
    fn update_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ShaderBuffer>,
        bytes: &[u8],
    ) -> Result<(), ModMaterialError>;

    /// Registers a `ModId`-owned [`SpriteMeshMaterial`] from a [`ModSpriteMaterialSpec`].
    fn register_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModSpriteMaterialSpec,
    ) -> Result<Handle<SpriteMeshMaterial>, ModMaterialError>;

    /// Registers a `ModId`-owned [`StandardMaterial`] from a [`ModStandardMaterialSpec`].
    fn register_mod_standard_material(
        &mut self,
        mod_id: ModId,
        spec: ModStandardMaterialSpec,
    ) -> Result<Handle<StandardMaterial>, ModMaterialError>;

    /// Removes all `.wesl` shaders, [`ShaderBuffer`]s, and material assets owned by `mod_id`.
    fn cleanup_mod_materials(&mut self, mod_id: ModId) -> usize;
}

impl ModMaterialAppExt for App {
    #[inline]
    fn register_mod_wesl_shader(
        &mut self,
        mod_id: ModId,
        spec: WeslShaderAssetSpec,
    ) -> Result<Handle<Shader>, ModMaterialError> {
        self.world_mut().register_mod_wesl_shader(mod_id, spec)
    }

    #[inline]
    fn register_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        spec: ModShaderBufferSpec,
    ) -> Result<Handle<ShaderBuffer>, ModMaterialError> {
        self.world_mut().register_mod_shader_buffer(mod_id, spec)
    }

    #[inline]
    fn update_mod_shader_buffer(
        &mut self,
        mod_id: ModId,
        handle: &Handle<ShaderBuffer>,
        bytes: &[u8],
    ) -> Result<(), ModMaterialError> {
        self.world_mut()
            .update_mod_shader_buffer(mod_id, handle, bytes)
    }

    #[inline]
    fn register_mod_sprite_material(
        &mut self,
        mod_id: ModId,
        spec: ModSpriteMaterialSpec,
    ) -> Result<Handle<SpriteMeshMaterial>, ModMaterialError> {
        self.world_mut().register_mod_sprite_material(mod_id, spec)
    }

    #[inline]
    fn register_mod_standard_material(
        &mut self,
        mod_id: ModId,
        spec: ModStandardMaterialSpec,
    ) -> Result<Handle<StandardMaterial>, ModMaterialError> {
        self.world_mut()
            .register_mod_standard_material(mod_id, spec)
    }

    #[inline]
    fn cleanup_mod_materials(&mut self, mod_id: ModId) -> usize {
        self.world_mut().cleanup_mod_materials(mod_id)
    }
}

/// Convenient prelude re-exporting `SubApp` extraction, WESL/material, 2D sprite shader, and
/// opt-in mesh-shader plugins, traits, and specs.
pub mod prelude {
    pub use crate::sprite_mesh_shaders::{
        ExtractedModMeshPipeline, ExtractedModMeshPipelines, ModExtendedSpriteMaterial2d,
        ModExtendedSpriteMaterialSpec, ModMeshPipelineEntry, ModMeshPipelineId,
        ModMeshPipelineSpec, ModMeshShaderAppExt, ModMeshShaderConfig, ModMeshShaderPlugin,
        ModMeshShaderRegistry, ModMeshShaderSubAppPlugin, ModMeshShaderWorldExt, ModMeshStageSpec,
        ModShaderDef, ModSpriteBindingKind, ModSpriteMaterial, ModSpriteMaterialPlugin,
        ModSpriteMeshAppExt, ModSpriteMeshBridgeRegistry, ModSpriteMeshWorldExt,
        ModUnifiedSpriteSpec, ModWeslExtendedMaterialEntry, ModWeslSpriteExtension2d,
        ModWeslSpriteExtension2dKey, ModWeslSpriteExtensionEntry, ModWeslSpriteExtensionSpec,
        ModWeslSpriteSpecialization,
    };
    pub use crate::sub_app_materials::{
        ModMaterialError, ModMaterialParams, ModShaderBufferInit, ModShaderBufferSpec,
        ModSpriteExtension2d, ModSpriteMaterialSpec, ModStandardMaterialSpec, ModSubAppConfig,
        ModSubAppExt, ModSubAppPlugin, ModSubAppSchedule, ModSubAppSystems, ModSubWorldExt,
        ModWeslMaterialPlugin, ModWeslMaterialRegistry, WeslShaderAssetSpec, WeslShaderSource,
    };
    pub use crate::{ModMaterialAppExt, ModMaterialWorldExt};
}
