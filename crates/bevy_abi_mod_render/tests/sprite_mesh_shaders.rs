//! Headless integration tests for Bevy `0.20` 2D `.wesl` `SpriteMaterial`, `ExtendedMaterial2d`,
//! unified `Mesh2d` + `SpriteMaterial` sprite bridges, and opt-in `MeshPipelineDescriptor`
//! mesh-shader pipelines with automatic `ModUnloadHooks` cleanup.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host::world_ext::ModScoped;
use bevy_abi_mod_host_sys::LoadOptions;
use bevy_abi_mod_render::prelude::*;
use bevy_abi_mod_render::{
    build_quad_mesh_2d, register_mod_extended_sprite_material_in_world,
    reset_mod_sprite_materials_on_render_startup, resolve_wesl_sprite_specialization,
};
use bevy_app::{App, AppLabel, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, AssetServer, Assets, Handle};
use bevy_ecs::entity::Entity;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_extract::ExtractSchedule;
use bevy_extract::sync_world::MainEntity;
use bevy_material::descriptor::{
    BindGroupLayoutDescriptor, CachedRenderPipelineId, MeshPipelineDescriptor, MeshState,
    RenderPipelineDescriptor,
};
use bevy_pbr::ViewKeyCache as PbrViewKeyCache;
use bevy_render::camera::{
    DirtySpecializationSystems, DirtySpecializations, clear_dirty_specializations,
};
use bevy_render::material_bind_groups::MaterialBindGroupAllocators;
use bevy_render::mesh::{BaseMeshPipelineKey, Mesh, Mesh2d, MeshVertexBufferLayouts};
use bevy_render::render_resource::{
    AsBindGroup, ColorTargetState, ColorWrites, PipelineCache, PrimitiveState,
    SpecializedMeshPipeline, SpecializedMeshPipelines, TextureFormat, WgpuFeatures,
};
use bevy_render::settings::{Backends, RenderResources, WgpuSettings};
use bevy_render::storage::{ShaderBuffer, ShaderBufferData};
use bevy_render::view::RetainedViewEntity;
use bevy_render::{RenderApp, RenderStartup};
use bevy_shader::{Shader, ShaderDefVal, Source, ValidateShader};
use bevy_sprite::{Anchor, Sprite, SpriteAlphaMode};
use bevy_sprite_render::{
    AlphaMode2d, EntitiesNeedingSpecialization, Material2d, Material2dKey, Material2dPipeline,
    Material2dPipelineSpecializer, MaterialExtension2d, Mesh2dPipeline, Mesh2dPipelineKey,
    MeshMaterial2d, SpecializedMaterial2dPipelineCache, SpriteMaterial, SpriteMeshMaterial,
    ViewKeyCache as SpriteViewKeyCache,
};
use core::sync::atomic::{AtomicU64, Ordering};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ directory")
        .parent()
        .expect("workspace root directory")
        .to_path_buf()
}

fn fixture_dylib(stem: &str) -> PathBuf {
    let dir = std::env::var_os("BAM_FIXTURE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .filter(|v| !v.is_empty())
                .map(|d| PathBuf::from(d).join("fixtures"))
        })
        .unwrap_or_else(|| workspace_root().join("target").join("fixtures"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let path = dir.join(format!("{prefix}{stem}.{ext}"));
    if !path.is_file() {
        panic!(
            "Fixture missing at {} — run 'cargo xtask fixtures' before 'cargo test'",
            path.display()
        );
    }
    path
}

struct TempDylibCopy {
    path: PathBuf,
    dir: PathBuf,
}

impl TempDylibCopy {
    fn new(original: &Path, tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ext = original
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dylib".to_owned());
        let dir = std::env::temp_dir().join(format!(
            "bevy_abi_mod_render_sprite_mesh_{}_{}",
            std::process::id(),
            id
        ));
        fs::create_dir_all(&dir).expect("create shadow dir");
        let path = dir.join(format!("{tag}_{id}.{ext}"));
        fs::copy(original, &path).expect("copy shadow dylib");
        Self { path, dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDylibCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(AppLabel, Debug, Hash, PartialEq, Eq, Clone, Default, Copy)]
struct CustomMeshRenderApp;

fn headless_material2d_pipeline(base_shader: Handle<Shader>) -> Material2dPipeline {
    let mesh2d_pipeline = Mesh2dPipeline {
        view_layout: BindGroupLayoutDescriptor::new("headless_mesh2d_view_layout", &[]),
        mesh_layout: BindGroupLayoutDescriptor::new("headless_mesh2d_layout", &[]),
        shader: base_shader,
        per_object_buffer_batch_size: None,
    };
    Material2dPipeline { mesh2d_pipeline }
}

fn specialize_base_mesh2d_descriptor(
    pipeline: &Material2dPipeline,
    mesh: &Mesh,
) -> (
    RenderPipelineDescriptor,
    bevy_render::mesh::MeshVertexBufferLayoutRef,
    Mesh2dPipelineKey,
) {
    let mut layouts = MeshVertexBufferLayouts::default();
    let layout = mesh.get_mesh_vertex_buffer_layout(&mut layouts);
    let base_key = BaseMeshPipelineKey::from_primitive_topology_and_strip_index(
        mesh.primitive_topology(),
        None,
    );
    let mesh_key = Mesh2dPipelineKey::from_bits_retain(base_key.bits())
        | Mesh2dPipelineKey::from_msaa_samples(1)
        | Mesh2dPipelineKey::from_target_format(TextureFormat::Bgra8UnormSrgb);
    let descriptor = pipeline
        .mesh2d_pipeline
        .specialize(mesh_key, &layout)
        .expect("specialize base Mesh2dPipeline headlessly");
    (descriptor, layout, mesh_key)
}

#[test]
fn headless_wesl_sprite_material_and_extended_material_2d_specialization_and_render_app_extraction()
{
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "wesl_sprite_spec");

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
    ));

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let vs_src = "@vertex fn vs_mod() -> @builtin(position) vec4<f32> { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }";
    let fs_v1_src = "import bevy_sprite_render::sprite_mesh::functions;\n@fragment fn fs_mod() -> @location(0) vec4<f32> { return vec4<f32>(0.2, 0.9, 1.0, 1.0); }";
    let fs_v2_src = "@fragment fn fs_mod_v2() -> @location(0) vec4<f32> { return vec4<f32>(1.0, 0.3, 0.1, 1.0); }";

    let vs_a = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl("sprite_vs", "mods/mod_a/sprite_vs.wesl", vs_src),
        )
        .expect("register .wesl vertex shader");
    let fs_a_v1 = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "sprite_fs_v1",
                "mods/mod_a/sprite_fs_v1.wesl",
                fs_v1_src,
            ),
        )
        .expect("register .wesl fragment shader v1");
    let fs_a_v2 = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "sprite_fs_v2",
                "mods/mod_a/sprite_fs_v2.wesl",
                fs_v2_src,
            ),
        )
        .expect("register .wesl fragment shader v2");
    let fs_b = app
        .register_mod_wesl_shader(
            mod_b,
            WeslShaderAssetSpec::from_wesl("mod_b_fs", "mods/mod_b/sprite.wesl", fs_v2_src),
        )
        .expect("register mod_b .wesl shader");

    {
        let shaders = app.world().resource::<Assets<Shader>>();
        let stored_vs = shaders.get(&vs_a).expect("vs_a stored");
        assert!(matches!(&stored_vs.source, Source::Wesl(src) if src.as_ref() == vs_src));
        assert!(matches!(stored_vs.validate_shader, ValidateShader::Enabled));
        let stored_fs = shaders.get(&fs_a_v1).expect("fs_a_v1 stored");
        assert!(matches!(&stored_fs.source, Source::Wesl(src) if src.as_ref() == fs_v1_src));
    }

    let buf_a = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("sprite_lut", 32, &[0.25, 0.5, 0.75, 1.0]),
        )
        .expect("register aligned ShaderBuffer");

    // 1. Register custom `.wesl` SpriteMaterial (`ModWeslSpriteExtension2d`).
    let sprite_ext_a = app
        .register_mod_wesl_sprite_extension(
            mod_a,
            ModWeslSpriteExtensionSpec::new("plasma_sprite_ext")
                .with_vertex_shader(vs_a.clone())
                .with_fragment_shader(fs_a_v1.clone())
                .with_vertex_entry_point("vs_mod")
                .with_fragment_entry_point("fs_mod")
                .with_shader_buffer(buf_a.clone())
                .with_alpha_mode(AlphaMode2d::Blend)
                .with_depth_bias(0.25)
                .with_params(ModMaterialParams {
                    tint: [0.3, 0.9, 1.0, 1.0],
                    intensity: 2.5,
                    time_scale: 1.5,
                    ..Default::default()
                })
                .with_shader_defs([
                    ModShaderDef::flag("MOD_GLOW"),
                    ModShaderDef::uint("WAVE_STEPS", 4),
                ]),
        )
        .expect("register ModWeslSpriteExtension2d");

    // 2. Register first-class `ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>`.
    let ext2d_a = app
        .register_mod_wesl_extended_sprite_material(
            mod_a,
            ModExtendedSpriteMaterialSpec::new("neon_extended_2d")
                .with_alpha_mode(AlphaMode2d::Mask(0.4))
                .with_flip(true, false)
                .with_color_rgba([0.9, 0.2, 0.8, 1.0])
                .with_fragment_shader(fs_a_v1.clone())
                .with_shader_buffer(buf_a.clone())
                .with_depth_bias(0.5)
                .with_params(ModMaterialParams {
                    tint: [1.0, 0.5, 0.2, 1.0],
                    intensity: 3.0,
                    time_scale: 2.0,
                    ..Default::default()
                })
                .with_shader_defs([ModShaderDef::flag("EXTENDED_NEON")]),
        )
        .expect("register ModExtendedSpriteMaterial2d");

    let ext_b = app
        .register_mod_wesl_sprite_extension(
            mod_b,
            ModWeslSpriteExtensionSpec::new("mod_b_ext").with_fragment_shader(fs_b.clone()),
        )
        .expect("register mod_b sprite extension");

    // 3. Verify REAL Bevy 0.20 `MaterialExtension2d::specialize` and `ExtendedMaterial2d::specialize`
    //    headlessly without a GPU!
    let quad_mesh = build_quad_mesh_2d(1.0, 1.0).expect("build quad mesh");
    let mat2d_pipeline = headless_material2d_pipeline(Handle::default());

    let ext_key_v1 = {
        let ext = app
            .world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&sprite_ext_a)
            .expect("ModWeslSpriteExtension2d stored");
        assert_eq!(ext.alpha_mode(), Some(AlphaMode2d::Blend));
        assert_eq!(ext.depth_bias(), Some(0.25));

        let key = ext.bind_group_data();
        assert_eq!(key.vertex_shader_id, Some(vs_a.id()));
        assert_eq!(key.fragment_shader_id, Some(fs_a_v1.id()));
        assert_ne!(key.specialization_slot, 0);

        let (mut desc, layout, mesh_key) =
            specialize_base_mesh2d_descriptor(&mat2d_pipeline, &quad_mesh);
        <ModWeslSpriteExtension2d as MaterialExtension2d>::specialize(
            &mat2d_pipeline,
            &mut desc,
            &layout,
            Material2dKey {
                mesh_key,
                bind_group_data: key,
            },
        )
        .expect("specialize ModWeslSpriteExtension2d");

        assert_eq!(desc.vertex.shader, vs_a);
        assert_eq!(desc.vertex.entry_point.as_deref(), Some("vs_mod"));
        let frag = desc.fragment.as_ref().expect("fragment state present");
        assert_eq!(frag.shader, fs_a_v1);
        assert_eq!(frag.entry_point.as_deref(), Some("fs_mod"));
        assert!(
            desc.vertex
                .shader_defs
                .contains(&ShaderDefVal::Bool(Cow::Borrowed("MOD_GLOW"), true))
        );
        assert!(
            frag.shader_defs
                .contains(&ShaderDefVal::UInt(Cow::Borrowed("WAVE_STEPS"), 4))
        );
        key
    };

    let ext2d_key_v1 = {
        let mat = app
            .world()
            .resource::<Assets<ModExtendedSpriteMaterial2d>>()
            .get(&ext2d_a)
            .expect("ModExtendedSpriteMaterial2d stored");
        assert_eq!(mat.alpha_mode(), AlphaMode2d::Mask(0.4));
        assert_eq!(mat.depth_bias(), 0.5);
        assert!(mat.base.flip_x);

        let combined_key = mat.bind_group_data();
        let ext_key = combined_key.extension;
        assert_eq!(ext_key.fragment_shader_id, Some(fs_a_v1.id()));

        let (mut desc, layout, mesh_key) =
            specialize_base_mesh2d_descriptor(&mat2d_pipeline, &quad_mesh);
        <ModExtendedSpriteMaterial2d as Material2d>::specialize(
            &mat2d_pipeline,
            &mut desc,
            &layout,
            Material2dKey {
                mesh_key,
                bind_group_data: combined_key,
            },
        )
        .expect("specialize ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>");

        let frag = desc.fragment.as_ref().expect("fragment state present");
        assert_eq!(frag.shader, fs_a_v1);
        assert!(
            frag.shader_defs
                .contains(&ShaderDefVal::Bool(Cow::Borrowed("EXTENDED_NEON"), true))
        );
        ext_key
    };

    // 4. Spawn entities for both SpriteMaterial and ExtendedMaterial2d, run headless RenderApp extraction.
    let sprite_entity = app
        .spawn_mod_sprite_with_material(
            mod_a,
            ModUnifiedSpriteSpec::new()
                .with_color_rgba([1.0, 0.8, 0.5, 1.0])
                .with_custom_size(64.0, 64.0)
                .with_sync_to_render_world(true),
            sprite_ext_a.clone(),
        )
        .expect("spawn unified SpriteMaterial entity");

    let ext2d_entity = app
        .spawn_mod_extended_mesh_sprite(
            mod_a,
            ModUnifiedSpriteSpec::new()
                .with_quad_size(32.0, 32.0)
                .with_sync_to_render_world(true),
            ext2d_a.clone(),
        )
        .expect("spawn ExtendedMaterial2d entity");

    app.update();

    {
        let ent = app.world().entity(sprite_entity);
        assert!(ent.contains::<Mesh2d>());
        assert!(ent.contains::<SpriteMaterial<ModWeslSpriteExtension2d>>());
        assert!(
            !ent.contains::<MeshMaterial2d<SpriteMeshMaterial>>(),
            "SpriteMaterialPlugin must suppress default SpriteMeshMaterial"
        );

        let needs_spec = app
            .world()
            .resource::<EntitiesNeedingSpecialization<ModExtendedSpriteMaterial2d>>();
        assert!(
            needs_spec.changed.contains(&ext2d_entity),
            "Material2dPlugin must flag ExtendedMaterial2d entity for specialization"
        );

        let render_world = app
            .get_sub_app(RenderApp)
            .expect("RenderApp present")
            .world();
        let dirty = render_world.resource::<DirtySpecializations>();
        assert!(
            dirty
                .changed_renderables
                .contains(&MainEntity::from(ext2d_entity)),
            "headless RenderApp must extract dirty ExtendedMaterial2d specialization"
        );
    }

    // 5. Runtime hot-swap of `.wesl` fragment shader and ownership enforcement.
    assert_eq!(
        app.update_mod_wesl_sprite_extension_shaders(
            mod_b,
            &sprite_ext_a,
            None,
            Some(fs_b.clone())
        ),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );
    app.update_mod_wesl_sprite_extension_shaders(
        mod_a,
        &sprite_ext_a,
        Some(vs_a.clone()),
        Some(fs_a_v2.clone()),
    )
    .expect("mod_a updates own .wesl shaders");

    let ext_key_v2 = {
        let ext = app
            .world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&sprite_ext_a)
            .unwrap();
        let key = ext.bind_group_data();
        assert_eq!(key.fragment_shader_id, Some(fs_a_v2.id()));
        assert_ne!(key.specialization_slot, ext_key_v1.specialization_slot);

        let (mut desc, layout, mesh_key) =
            specialize_base_mesh2d_descriptor(&mat2d_pipeline, &quad_mesh);
        <ModWeslSpriteExtension2d as MaterialExtension2d>::specialize(
            &mat2d_pipeline,
            &mut desc,
            &layout,
            Material2dKey {
                mesh_key,
                bind_group_data: key,
            },
        )
        .expect("re-specialize after shader swap");
        assert_eq!(desc.fragment.as_ref().unwrap().shader, fs_a_v2);
        key
    };

    // 6. Unload mod_a: verify all mod_a shaders, buffers, materials, and specialization entries
    //    are purged while mod_b remains intact.
    app.unload_mod(mod_a).expect("unload mod_a");
    app.update();

    assert!(!app.world().entities().contains(sprite_entity));
    assert!(!app.world().entities().contains(ext2d_entity));
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_a),
        0
    );
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&vs_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&fs_a_v1)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&fs_a_v2)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&buf_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&sprite_ext_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ModExtendedSpriteMaterial2d>>()
            .get(&ext2d_a)
            .is_none()
    );
    assert!(
        resolve_wesl_sprite_specialization(&ext_key_v1).is_none(),
        "old specialization slot referencing mod_a shaders must be evicted on unload"
    );
    assert!(
        resolve_wesl_sprite_specialization(&ext_key_v2).is_none(),
        "updated specialization slot referencing mod_a shaders must be evicted on unload"
    );
    assert!(
        resolve_wesl_sprite_specialization(&ext2d_key_v1).is_none(),
        "ExtendedMaterial2d specialization slot referencing mod_a shaders must be evicted on unload"
    );

    // mod_b stays intact.
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&fs_b)
            .is_some()
    );
    assert!(
        app.world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&ext_b)
            .is_some()
    );
    app.unload_mod(mod_b).expect("unload mod_b");
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&fs_b)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&ext_b)
            .is_none()
    );
}

#[test]
fn unified_mesh2d_sprite_material_spawning_attachment_param_mutation_and_unload_cleanup() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "unified_sprite_bridge");

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
    ));

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let shader_a = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "unified_fs",
                "mods/mod_a/unified.wesl",
                "@fragment fn fragment() -> @location(0) vec4<f32> { return vec4<f32>(0.4, 0.8, 1.0, 1.0); }",
            ),
        )
        .expect("register shader");
    let buf_1 = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("params_buf_1", 16, &[1.0, 2.0, 3.0, 4.0]),
        )
        .expect("register buf_1");
    let buf_2 = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("params_buf_2", 32, &[9.0, 8.0, 7.0, 6.0]),
        )
        .expect("register buf_2");
    let custom_quad = app
        .register_mod_quad_mesh(mod_a, "hero_quad", 2.0, 4.0)
        .expect("register mod quad mesh");

    let sprite_ext_a = app
        .register_mod_wesl_sprite_extension(
            mod_a,
            ModWeslSpriteExtensionSpec::new("hero_fx")
                .with_fragment_shader(shader_a.clone())
                .with_shader_buffer(buf_1.clone())
                .with_params(ModMaterialParams {
                    tint: [1.0, 1.0, 1.0, 1.0],
                    intensity: 1.0,
                    time_scale: 1.0,
                    ..Default::default()
                }),
        )
        .expect("register sprite_ext_a");

    let ext2d_a = app
        .register_mod_wesl_extended_sprite_material(
            mod_a,
            ModExtendedSpriteMaterialSpec::new("hero_ext2d")
                .with_fragment_shader(shader_a)
                .with_shader_buffer(buf_1.clone())
                .with_params(ModMaterialParams {
                    tint: [0.2, 0.4, 0.6, 1.0],
                    intensity: 0.75,
                    time_scale: 0.5,
                    ..Default::default()
                }),
        )
        .expect("register ext2d_a");

    let (legacy_ext_a, _legacy_ext2d_a) = register_mod_extended_sprite_material_in_world(
        app.world_mut(),
        mod_a,
        ModSpriteMaterialSpec::new("legacy_ext"),
        ModSpriteExtension2d {
            buffer: buf_1,
            params: ModMaterialParams::default(),
        },
    )
    .expect("register legacy ModSpriteExtension2d");

    // 1. Spawn ModScoped unified SpriteMaterial entity + ModScoped ExtendedMaterial2d mesh sprite.
    let scoped_sprite = app
        .spawn_mod_sprite_with_material(
            mod_a,
            ModUnifiedSpriteSpec::new()
                .with_color_rgba([0.5, 0.6, 0.7, 1.0])
                .with_custom_size(128.0, 64.0)
                .with_flip(true, false)
                .with_alpha_mode(SpriteAlphaMode::Blend)
                .with_anchor(Anchor::BOTTOM_LEFT)
                .with_mesh(custom_quad.clone()),
            sprite_ext_a.clone(),
        )
        .expect("spawn scoped unified sprite");

    let scoped_ext2d = app
        .spawn_mod_extended_mesh_sprite(
            mod_a,
            ModUnifiedSpriteSpec::new().with_mesh(custom_quad.clone()),
            ext2d_a.clone(),
        )
        .expect("spawn scoped extended mesh sprite");

    // 2. Spawn persistent host entities and attach mod_a materials to them.
    let host_sprite = app.world_mut().spawn(Sprite::default()).id();
    let host_sprite_detach = app.world_mut().spawn(Sprite::default()).id();
    let host_mesh = app.world_mut().spawn(Mesh2d(custom_quad.clone())).id();

    app.attach_mod_sprite_material(mod_a, host_sprite, sprite_ext_a.clone())
        .expect("attach SpriteMaterial to host_sprite");
    app.attach_mod_sprite_material(mod_a, host_sprite_detach, sprite_ext_a.clone())
        .expect("attach SpriteMaterial to host_sprite_detach");
    app.attach_mod_extended_material_2d(mod_a, host_mesh, ext2d_a.clone())
        .expect("attach ExtendedMaterial2d to host_mesh");

    // Ownership & entity error checks.
    assert_eq!(
        app.attach_mod_sprite_material(mod_b, host_sprite, sprite_ext_a.clone()),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );
    assert_eq!(
        app.attach_mod_sprite_material(
            mod_a,
            Entity::from_raw_u32(999_999).unwrap(),
            sprite_ext_a.clone()
        ),
        Err(ModMaterialError::EntityNotFound(
            Entity::from_raw_u32(999_999).unwrap()
        ))
    );

    // Inspect unified sprite components right after spawn.
    {
        let ent = app.world().entity(scoped_sprite);
        assert_eq!(ent.get::<ModScoped>(), Some(&ModScoped(mod_a)));
        assert_eq!(ent.get::<Anchor>(), Some(&Anchor::BOTTOM_LEFT));
        assert_eq!(ent.get::<Mesh2d>().map(|m| m.id()), Some(custom_quad.id()));
        assert_eq!(
            ent.get::<SpriteMaterial<ModWeslSpriteExtension2d>>()
                .map(|m| m.id()),
            Some(sprite_ext_a.id())
        );
        let sp = ent.get::<Sprite>().expect("Sprite component present");
        assert!(sp.flip_x);
        assert!(!sp.flip_y);

        let ent2 = app.world().entity(scoped_ext2d);
        assert_eq!(ent2.get::<ModScoped>(), Some(&ModScoped(mod_a)));
        assert_eq!(
            ent2.get::<MeshMaterial2d<ModExtendedSpriteMaterial2d>>()
                .map(|m| m.id()),
            Some(ext2d_a.id())
        );
    }

    app.update();

    // 3. Mutate uniform ModMaterialParams and ShaderBuffers across all material forms.
    let mutated_params = ModMaterialParams {
        tint: [0.1, 0.9, 0.4, 0.8],
        intensity: 4.25,
        time_scale: 3.0,
        ..Default::default()
    };
    assert_eq!(
        app.update_mod_wesl_sprite_extension_params(mod_b, &sprite_ext_a, mutated_params),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );
    assert_eq!(
        app.update_mod_extended_sprite_material_params(mod_b, &ext2d_a, mutated_params),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );

    app.update_mod_wesl_sprite_extension_params(mod_a, &sprite_ext_a, mutated_params)
        .expect("update ModWeslSpriteExtension2d params");
    app.update_mod_wesl_sprite_extension_buffer(mod_a, &sprite_ext_a, buf_2.clone())
        .expect("update ModWeslSpriteExtension2d buffer");
    app.update_mod_extended_sprite_material_params(mod_a, &ext2d_a, mutated_params)
        .expect("update ModExtendedSpriteMaterial2d params");
    app.update_mod_extended_sprite_material_buffer(mod_a, &ext2d_a, buf_2.clone())
        .expect("update ModExtendedSpriteMaterial2d buffer");
    app.update_mod_sprite_extension_params(mod_a, &legacy_ext_a, mutated_params)
        .expect("update legacy ModSpriteExtension2d params");
    app.update_mod_shader_buffer(mod_a, &buf_2, &[42, 43, 44, 45, 46])
        .expect("update ShaderBuffer payload");

    app.update();

    {
        let ext = app
            .world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&sprite_ext_a)
            .unwrap();
        assert_eq!(ext.params, mutated_params);
        assert_eq!(ext.buffer.id(), buf_2.id());

        let ext2d = app
            .world()
            .resource::<Assets<ModExtendedSpriteMaterial2d>>()
            .get(&ext2d_a)
            .unwrap();
        assert_eq!(ext2d.extension.params, mutated_params);
        assert_eq!(ext2d.extension.buffer.id(), buf_2.id());

        let legacy = app
            .world()
            .resource::<Assets<ModSpriteExtension2d>>()
            .get(&legacy_ext_a)
            .unwrap();
        assert_eq!(legacy.params, mutated_params);

        let buf = app
            .world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&buf_2)
            .unwrap();
        match &buf.data {
            ShaderBufferData::Initialized { data, buffer_size } => {
                assert_eq!(data.alignment(), 32);
                assert_eq!(*buffer_size, 8);
                assert_eq!(data.as_slice(), &[42, 43, 44, 45, 46, 0, 0, 0]);
            }
            ShaderBufferData::Uninitialized(_) => panic!("expected initialized buffer"),
        }
    }

    // 4. Explicit detach restores default SpriteMeshMaterial on next update.
    assert!(app.detach_mod_sprite_material(mod_a, host_sprite_detach));
    assert!(!app.detach_mod_sprite_material(mod_a, host_sprite_detach));
    app.update();
    {
        let ent = app.world().entity(host_sprite_detach);
        assert!(!ent.contains::<SpriteMaterial<ModWeslSpriteExtension2d>>());
        assert!(
            ent.contains::<MeshMaterial2d<SpriteMeshMaterial>>(),
            "detaching SpriteMaterial must allow SpriteMeshPlugin to restore default SpriteMeshMaterial"
        );
    }

    // 5. Unload mod_a: scoped sprites despawn, persistent host entities survive with mod materials
    //    cleanly detached, and mod-owned quad Mesh is purged from Assets<Mesh>.
    app.unload_mod(mod_a).expect("unload mod_a");

    assert!(!app.world().entities().contains(scoped_sprite));
    assert!(!app.world().entities().contains(scoped_ext2d));
    assert!(app.world().entities().contains(host_sprite));
    assert!(app.world().entities().contains(host_mesh));
    assert!(
        !app.world()
            .entity(host_sprite)
            .contains::<SpriteMaterial<ModWeslSpriteExtension2d>>(),
        "unload_mod must immediately detach mod SpriteMaterial from surviving host entities"
    );
    assert!(
        !app.world()
            .entity(host_mesh)
            .contains::<MeshMaterial2d<ModExtendedSpriteMaterial2d>>(),
        "unload_mod must immediately detach mod MeshMaterial2d from surviving host entities"
    );
    assert!(
        app.world()
            .resource::<Assets<Mesh>>()
            .get(&custom_quad)
            .is_none(),
        "unload_mod must purge mod-owned quad meshes from Assets<Mesh>"
    );
    assert_eq!(
        app.world()
            .resource::<ModSpriteMeshBridgeRegistry>()
            .total_tracked_for_mod(mod_a),
        0
    );
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_a),
        0
    );

    // After next update, surviving host_sprite cleanly receives default SpriteMeshMaterial again.
    app.update();
    assert!(
        app.world()
            .entity(host_sprite)
            .contains::<MeshMaterial2d<SpriteMeshMaterial>>()
    );

    app.unload_mod(mod_b).expect("unload mod_b");
}

#[test]
fn opt_in_mesh_shader_pipeline_registration_sub_app_extraction_and_unload_cleanup() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "mesh_shader_bridge");

    // 1. Verify opt-in gating: WITHOUT ModMeshShaderPlugin, registration MUST fail.
    {
        let mut gated_app = App::new();
        gated_app.add_plugins((AbiModPlugin, ModWeslMaterialPlugin));
        let mod_id = gated_app
            .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
            .expect("load mod");
        let mesh_shader = gated_app
            .register_mod_wesl_shader(
                mod_id,
                WeslShaderAssetSpec::from_wesl(
                    "mesh_stage",
                    "mods/gated/mesh.wesl",
                    "@mesh fn mesh_main() {}",
                ),
            )
            .expect("register mesh shader");
        assert_eq!(
            gated_app.register_mod_mesh_pipeline(
                mod_id,
                ModMeshPipelineSpec::new("should_fail", mesh_shader),
            ),
            Err(ModMaterialError::MeshShadersNotEnabled)
        );
        gated_app.unload_mod(mod_id).expect("unload gated mod");
    }

    // 2. Verify explicit disabled config also rejects registration.
    {
        let mut disabled_app = App::new();
        disabled_app.add_plugins((
            AbiModPlugin,
            ModWeslMaterialPlugin,
            ModMeshShaderPlugin::new(ModMeshShaderConfig::disabled()),
        ));
        let mod_id = disabled_app
            .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
            .expect("load mod");
        let mesh_shader = disabled_app
            .register_mod_wesl_shader(
                mod_id,
                WeslShaderAssetSpec::from_wesl(
                    "mesh_stage",
                    "mods/disabled/mesh.wesl",
                    "@mesh fn mesh_main() {}",
                ),
            )
            .expect("register mesh shader");
        assert_eq!(
            disabled_app.register_mod_mesh_pipeline(
                mod_id,
                ModMeshPipelineSpec::new("disabled_fail", mesh_shader),
            ),
            Err(ModMaterialError::MeshShadersNotEnabled)
        );
        disabled_app
            .unload_mod(mod_id)
            .expect("unload disabled mod");
    }

    // 3. Opt-in app with BOTH RenderApp and a custom SubApp extracting MeshPipelineDescriptors.
    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModSubAppPlugin::<CustomMeshRenderApp>::default(),
        ModWeslMaterialPlugin,
        ModMeshShaderPlugin::new(
            ModMeshShaderConfig::enabled()
                .with_max_pipelines_per_mod(2)
                .with_required_wgpu_features(WgpuFeatures::EXPERIMENTAL_MESH_SHADER),
        ),
        ModMeshShaderSubAppPlugin::<CustomMeshRenderApp>::default(),
    ));

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let task_a = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "task_a",
                "mods/mod_a/task.wesl",
                "@task fn task_main() {}",
            ),
        )
        .expect("register task shader");
    let mesh_a = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "mesh_a",
                "mods/mod_a/mesh.wesl",
                "@mesh fn mesh_main() {}",
            ),
        )
        .expect("register mesh shader");
    let frag_a = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "frag_a",
                "mods/mod_a/frag.wesl",
                "@fragment fn frag_main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }",
            ),
        )
        .expect("register frag shader");
    let mesh_a_alt = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "mesh_a_alt",
                "mods/mod_a/mesh_alt.wesl",
                "@mesh fn mesh_alt() {}",
            ),
        )
        .expect("register alt mesh shader");
    let mesh_b = app
        .register_mod_wesl_shader(
            mod_b,
            WeslShaderAssetSpec::from_wesl(
                "mesh_b",
                "mods/mod_b/mesh.wesl",
                "@mesh fn mesh_b() {}",
            ),
        )
        .expect("register mod_b mesh shader");
    let buf_a = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("meshlet_buf", 64, &[1.0, 2.0, 3.0, 4.0]),
        )
        .expect("register meshlet ShaderBuffer");

    // Validation errors.
    assert!(matches!(
        app.register_mod_mesh_pipeline(mod_a, ModMeshPipelineSpec::new("", mesh_a.clone())),
        Err(ModMaterialError::InvalidPipelineDescriptor(_))
    ));
    assert!(matches!(
        app.register_mod_mesh_pipeline(
            mod_a,
            ModMeshPipelineSpec::new("bad_handle", Handle::default())
        ),
        Err(ModMaterialError::InvalidPipelineDescriptor(_))
    ));

    // Pipeline 1 for mod_a: full task + mesh + fragment stages via ModMeshPipelineSpec.
    let pipe_a1 = app
        .register_mod_mesh_pipeline(
            mod_a,
            ModMeshPipelineSpec::new("mod_a_meshlets", mesh_a.clone())
                .with_task_stage(
                    ModMeshStageSpec::new(task_a.clone())
                        .with_entry_point("task_main")
                        .with_shader_defs([ShaderDefVal::Bool(Cow::Borrowed("MESHLET_CULL"), true)])
                        .with_constant("WORKGROUP_SIZE", 32.0),
                )
                .with_mesh_stage(
                    ModMeshStageSpec::new(mesh_a.clone())
                        .with_entry_point("mesh_main")
                        .with_shader_defs([ShaderDefVal::UInt(Cow::Borrowed("MAX_VERTS"), 64)]),
                )
                .with_fragment_stage(
                    ModMeshStageSpec::new(frag_a.clone()).with_entry_point("frag_main"),
                )
                .with_color_targets(vec![Some(ColorTargetState {
                    format: TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })])
                .with_layouts(vec![BindGroupLayoutDescriptor::new("meshlet_bgl", &[])])
                .with_immediate_size(16)
                .with_primitive(PrimitiveState::default())
                .with_shader_buffer(buf_a.clone()),
        )
        .expect("register pipe_a1");

    // Pipeline 2 for mod_a: direct pre-built Bevy MeshPipelineDescriptor.
    let raw_desc = MeshPipelineDescriptor {
        label: Some(Cow::Borrowed("mod_a_alt_mesh")),
        mesh: MeshState {
            shader: mesh_a_alt.clone(),
            entry_point: Some(Cow::Borrowed("mesh_alt")),
            ..Default::default()
        },
        zero_initialize_workgroup_memory: true,
        ..Default::default()
    };
    let pipe_a2 = app
        .register_mod_mesh_pipeline(
            mod_a,
            ModMeshPipelineSpec::from_descriptor("mod_a_alt_mesh", raw_desc),
        )
        .expect("register pipe_a2");

    // Quota check (limit = 2).
    assert_eq!(
        app.register_mod_mesh_pipeline(
            mod_a,
            ModMeshPipelineSpec::new("mod_a_over_quota", mesh_a.clone()),
        ),
        Err(ModMaterialError::MeshPipelineQuotaExceeded {
            mod_id: mod_a,
            limit: 2,
        })
    );

    // Pipeline for mod_b.
    let pipe_b1 = app
        .register_mod_mesh_pipeline(
            mod_b,
            ModMeshPipelineSpec::new("mod_b_meshlets", mesh_b.clone()),
        )
        .expect("register pipe_b1");

    // Ownership check on unregister.
    assert_eq!(
        app.unregister_mod_mesh_pipeline(mod_b, pipe_a1),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );

    // Verify MainWorld registry contents.
    {
        let reg = app.world().resource::<ModMeshShaderRegistry>();
        assert_eq!(reg.pipeline_count_for_mod(mod_a), 2);
        assert_eq!(reg.pipeline_count_for_mod(mod_b), 1);
        assert_eq!(reg.total_pipeline_count(), 3);
        assert!(reg.references_shader(task_a.id()));
        assert!(reg.references_shader(mesh_a.id()));
        assert!(reg.references_shader(frag_a.id()));
        assert!(reg.references_shader(mesh_a_alt.id()));
        assert!(reg.references_shader(mesh_b.id()));

        let entry = reg.get_by_label(mod_a, "mod_a_meshlets").unwrap();
        assert_eq!(entry.id, pipe_a1);
        assert_eq!(entry.descriptor.immediate_size, 16);
        assert!(entry.descriptor.zero_initialize_workgroup_memory);
        assert_eq!(
            entry.descriptor.task.as_ref().unwrap().shader.id(),
            task_a.id()
        );
        assert_eq!(entry.descriptor.mesh.shader.id(), mesh_a.id());
        assert_eq!(
            entry.descriptor.fragment.as_ref().unwrap().shader.id(),
            frag_a.id()
        );
        assert_eq!(entry.shader_buffers.len(), 1);
    }

    // Frame 1: Extract into RenderApp and CustomMeshRenderApp.
    app.update();

    for extracted in [
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
    ] {
        assert_eq!(extracted.len(), 3);
        assert_eq!(extracted.count_for_mod(mod_a), 2);
        assert_eq!(extracted.count_for_mod(mod_b), 1);
        assert!(extracted.contains(pipe_a1));
        assert!(extracted.contains(pipe_a2));
        assert!(extracted.contains(pipe_b1));
        assert!(extracted.references_shader(mesh_a.id()));
        assert!(extracted.references_shader(mesh_b.id()));
        let ex1 = extracted.get(pipe_a1).unwrap();
        assert_eq!(
            ex1.descriptor.mesh.entry_point.as_deref(),
            Some("mesh_main")
        );
        assert_eq!(
            ex1.descriptor.task.as_ref().unwrap().constants,
            vec![(Cow::Borrowed("WORKGROUP_SIZE"), 32.0)]
        );
    }

    // Explicitly unregister pipe_a2 and verify incremental SubApp eviction.
    app.unregister_mod_mesh_pipeline(mod_a, pipe_a2)
        .expect("unregister pipe_a2");
    app.update();
    {
        let render_extracted = app
            .get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>();
        assert_eq!(render_extracted.len(), 2);
        assert!(!render_extracted.contains(pipe_a2));
        assert!(!render_extracted.references_shader(mesh_a_alt.id()));
        assert!(render_extracted.evicted_pipeline_ids().contains(&pipe_a2));
        assert!(
            render_extracted
                .evicted_shader_ids()
                .contains(&mesh_a_alt.id())
        );
    }

    // Unload mod_a: ALL remaining mod_a pipelines, shaders, and ShaderBuffers MUST vanish cleanly
    // via ModUnloadHooks + next ExtractSchedule, leaving zero dangling state while mod_b survives!
    app.unload_mod(mod_a).expect("unload mod_a");
    app.update();

    {
        let mesh_reg = app.world().resource::<ModMeshShaderRegistry>();
        assert_eq!(mesh_reg.pipeline_count_for_mod(mod_a), 0);
        assert_eq!(mesh_reg.pipeline_count_for_mod(mod_b), 1);
        assert!(!mesh_reg.contains(pipe_a1));
        assert!(!mesh_reg.references_shader(task_a.id()));
        assert!(!mesh_reg.references_shader(mesh_a.id()));
        assert!(!mesh_reg.references_shader(frag_a.id()));
        assert!(mesh_reg.references_shader(mesh_b.id()));

        let mat_reg = app.world().resource::<ModWeslMaterialRegistry>();
        assert_eq!(mat_reg.total_assets_for_mod(mod_a), 0);
        assert_eq!(mat_reg.shader_count_for_mod(mod_b), 1);

        let shaders = app.world().resource::<Assets<Shader>>();
        assert!(shaders.get(&task_a).is_none());
        assert!(shaders.get(&mesh_a).is_none());
        assert!(shaders.get(&frag_a).is_none());
        assert!(shaders.get(&mesh_a_alt).is_none());
        assert!(shaders.get(&mesh_b).is_some());
        assert!(
            app.world()
                .resource::<Assets<ShaderBuffer>>()
                .get(&buf_a)
                .is_none()
        );
    }

    for extracted in [
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
    ] {
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted.count_for_mod(mod_a), 0);
        assert_eq!(extracted.count_for_mod(mod_b), 1);
        assert!(!extracted.contains(pipe_a1));
        assert!(extracted.contains(pipe_b1));
        assert!(!extracted.references_shader(task_a.id()));
        assert!(!extracted.references_shader(mesh_a.id()));
        assert!(!extracted.references_shader(frag_a.id()));
        assert!(extracted.evicted_pipeline_ids().contains(&pipe_a1));
        assert!(extracted.evicted_shader_ids().contains(&task_a.id()));
        assert!(extracted.evicted_shader_ids().contains(&mesh_a.id()));
        assert!(extracted.evicted_shader_ids().contains(&frag_a.id()));
    }

    // Unload mod_b: everything drops to zero.
    app.unload_mod(mod_b).expect("unload mod_b");
    app.update();

    assert_eq!(
        app.world()
            .resource::<ModMeshShaderRegistry>()
            .total_pipeline_count(),
        0
    );
    assert!(
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>()
            .is_empty()
    );
    assert!(
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>()
            .is_empty()
    );
}

#[test]
fn headless_without_asset_plugin_sprite_and_mesh_shader_bridges_work_and_cleanup() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "no_asset_plugin");

    // Minimal headless App WITHOUT AssetPlugin.
    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModSpriteMaterialPlugin::default(),
        ModMeshShaderPlugin::default(),
    ));

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");

    let shader = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl(
                "minimal_fs",
                "mods/minimal/fs.wesl",
                "@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }",
            ),
        )
        .expect("register shader");
    let ext = app
        .register_mod_wesl_sprite_extension(
            mod_id,
            ModWeslSpriteExtensionSpec::new("minimal_ext").with_fragment_shader(shader.clone()),
        )
        .expect("register extension");
    let entity = app
        .spawn_mod_sprite_with_material(mod_id, ModUnifiedSpriteSpec::new(), ext)
        .expect("spawn unified sprite");
    let pipe = app
        .register_mod_mesh_pipeline(mod_id, ModMeshPipelineSpec::new("minimal_mesh", shader))
        .expect("register mesh pipeline");

    assert!(app.world().entity(entity).contains::<Mesh2d>());
    assert!(
        app.world()
            .entity(entity)
            .contains::<SpriteMaterial<ModWeslSpriteExtension2d>>()
    );
    assert!(
        app.world()
            .resource::<ModMeshShaderRegistry>()
            .contains(pipe)
    );

    app.unload_mod(mod_id).expect("unload mod");

    assert!(!app.world().entities().contains(entity));
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_id),
        0
    );
    assert_eq!(
        app.world()
            .resource::<ModSpriteMeshBridgeRegistry>()
            .total_tracked_for_mod(mod_id),
        0
    );
    assert_eq!(
        app.world()
            .resource::<ModMeshShaderRegistry>()
            .total_pipeline_count(),
        0
    );
}

#[test]
fn shared_shader_def_specialization_slot_preserved_for_surviving_mod_on_unload() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "shared_spec_slot");

    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModSpriteMaterialPlugin::default()));

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let shared_def = ModShaderDef::flag("SHARED_MOD_SPRITE_FX_UNIQUE_TEST");
    let ext_a = app
        .register_mod_wesl_sprite_extension(
            mod_a,
            ModWeslSpriteExtensionSpec::new("a_shared").with_shader_defs([shared_def.clone()]),
        )
        .expect("register ext_a");
    let ext_b = app
        .register_mod_wesl_sprite_extension(
            mod_b,
            ModWeslSpriteExtensionSpec::new("b_shared").with_shader_defs([shared_def]),
        )
        .expect("register ext_b");

    let key_a = app
        .world()
        .resource::<Assets<ModWeslSpriteExtension2d>>()
        .get(&ext_a)
        .unwrap()
        .bind_group_data();
    let key_b = app
        .world()
        .resource::<Assets<ModWeslSpriteExtension2d>>()
        .get(&ext_b)
        .unwrap()
        .bind_group_data();
    assert_eq!(key_a, key_b);
    assert_ne!(key_a.specialization_slot, 0);
    assert!(resolve_wesl_sprite_specialization(&key_b).is_some());

    // Unloading mod_a MUST keep the shared slot alive while mod_b still uses it.
    app.unload_mod(mod_a).expect("unload mod_a");
    assert!(
        resolve_wesl_sprite_specialization(&key_b).is_some(),
        "shared specialization slot must stay alive for surviving mod_b"
    );

    // Unloading mod_b now leaves zero live users, so the slot is evicted cleanly.
    app.unload_mod(mod_b).expect("unload mod_b");
    assert!(
        resolve_wesl_sprite_specialization(&key_b).is_none(),
        "specialization slot must be evicted once all referencing mods unload"
    );
}

#[test]
fn independent_worlds_with_colliding_shader_asset_indices_do_not_evict_each_other() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "multi_world_isolation");

    let mut app_a = App::new();
    app_a.add_plugins((AbiModPlugin, ModSpriteMaterialPlugin::default()));
    let mut app_b = App::new();
    app_b.add_plugins((AbiModPlugin, ModSpriteMaterialPlugin::default()));

    let mod_a = app_a
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app_b
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let frag_src = "@fragment fn fs_isolated() -> @location(0) vec4<f32> { return vec4<f32>(0.1, 0.2, 0.3, 1.0); }";
    let shader_a = app_a
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl("fs_shared_idx", "mods/a/fs.wesl", frag_src),
        )
        .expect("register shader_a");
    let shader_b = app_b
        .register_mod_wesl_shader(
            mod_b,
            WeslShaderAssetSpec::from_wesl("fs_shared_idx", "mods/b/fs.wesl", frag_src),
        )
        .expect("register shader_b");
    assert_eq!(
        shader_a.id(),
        shader_b.id(),
        "independent Worlds allocate matching AssetId<Shader> indices"
    );

    let ext_a = app_a
        .register_mod_wesl_sprite_extension(
            mod_a,
            ModWeslSpriteExtensionSpec::new("ext_a")
                .with_fragment_shader(shader_a.clone())
                .with_fragment_entry_point("fs_isolated"),
        )
        .expect("register ext_a");
    let ext_b = app_b
        .register_mod_wesl_sprite_extension(
            mod_b,
            ModWeslSpriteExtensionSpec::new("ext_b")
                .with_fragment_shader(shader_b)
                .with_fragment_entry_point("fs_isolated"),
        )
        .expect("register ext_b");

    let key_a = app_a
        .world()
        .resource::<Assets<ModWeslSpriteExtension2d>>()
        .get(&ext_a)
        .unwrap()
        .bind_group_data();
    let key_b = app_b
        .world()
        .resource::<Assets<ModWeslSpriteExtension2d>>()
        .get(&ext_b)
        .unwrap()
        .bind_group_data();
    assert_ne!(
        key_a.specialization_slot, key_b.specialization_slot,
        "independent Worlds must receive isolated specialization slots even on colliding shader handles"
    );

    // Unloading mod_b and dropping app_b MUST NOT evict app_a's live specialization slot!
    app_b.unload_mod(mod_b).expect("unload mod_b");
    drop(app_b);

    assert!(
        resolve_wesl_sprite_specialization(&key_b).is_none(),
        "dropped app_b's specialization slot should be cleaned up"
    );
    assert!(
        resolve_wesl_sprite_specialization(&key_a).is_some(),
        "app_a specialization slot must survive app_b mod unload and World drop"
    );

    let quad_mesh = build_quad_mesh_2d(1.0, 1.0).expect("build quad mesh");
    let mat2d_pipeline = headless_material2d_pipeline(Handle::default());
    let (mut desc, layout, mesh_key) =
        specialize_base_mesh2d_descriptor(&mat2d_pipeline, &quad_mesh);
    <ModWeslSpriteExtension2d as MaterialExtension2d>::specialize(
        &mat2d_pipeline,
        &mut desc,
        &layout,
        Material2dKey {
            mesh_key,
            bind_group_data: key_a,
        },
    )
    .expect("specialize surviving app_a extension");
    assert_eq!(desc.fragment.as_ref().unwrap().shader, shader_a);
}

#[test]
fn render_startup_clears_stale_mesh_pipelines_and_reextracts_cleanly() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "mesh_render_startup");

    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModSubAppPlugin::<CustomMeshRenderApp>::default(),
        ModWeslMaterialPlugin,
        ModMeshShaderPlugin::default(),
        ModMeshShaderSubAppPlugin::<CustomMeshRenderApp>::default(),
    ));

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");
    let mesh_1 = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl("mesh_1", "mods/rs/mesh1.wesl", "@mesh fn m1() {}"),
        )
        .expect("register mesh_1");
    let mesh_2 = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl("mesh_2", "mods/rs/mesh2.wesl", "@mesh fn m2() {}"),
        )
        .expect("register mesh_2");

    let pipe_1 = app
        .register_mod_mesh_pipeline(mod_id, ModMeshPipelineSpec::new("rs_pipe_1", mesh_1))
        .expect("register pipe_1");
    let pipe_2 = app
        .register_mod_mesh_pipeline(mod_id, ModMeshPipelineSpec::new("rs_pipe_2", mesh_2))
        .expect("register pipe_2");

    // Frame 1: Extract pipelines into both RenderApp and CustomMeshRenderApp.
    app.update();
    for extracted in [
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
    ] {
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted.recovery_epoch(), 0);
        assert_eq!(extracted.last_reset_cleared_count(), 0);
        assert!(extracted.contains(pipe_1));
        assert!(extracted.contains(pipe_2));
    }

    // Simulate `RenderStartup` firing on both sub-apps (device-loss recovery / shader reload).
    app.get_sub_app_mut(RenderApp)
        .unwrap()
        .world_mut()
        .run_schedule(RenderStartup);
    app.get_sub_app_mut(CustomMeshRenderApp)
        .unwrap()
        .world_mut()
        .run_schedule(RenderStartup);

    for extracted in [
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
    ] {
        assert!(
            extracted.is_empty(),
            "RenderStartup must clear active cached mesh pipelines"
        );
        assert_eq!(extracted.recovery_epoch(), 1);
        assert_eq!(extracted.last_reset_cleared_count(), 2);
        assert!(
            extracted.evicted_pipeline_ids().is_empty(),
            "RenderStartup reset must not pollute unload eviction telemetry"
        );
        assert!(extracted.evicted_shader_ids().is_empty());
    }

    // Next ExtractSchedule re-extracts all live mod mesh pipelines cleanly.
    app.update();
    for extracted in [
        app.get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
        app.get_sub_app(CustomMeshRenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>(),
    ] {
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted.recovery_epoch(), 1);
        assert!(extracted.contains(pipe_1));
        assert!(extracted.contains(pipe_2));
    }

    // A second RenderStartup cycle advances recovery_epoch to 2 and re-extracts cleanly again.
    app.get_sub_app_mut(RenderApp)
        .unwrap()
        .world_mut()
        .run_schedule(RenderStartup);
    app.update();
    {
        let extracted = app
            .get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>();
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted.recovery_epoch(), 2);
        assert_eq!(extracted.last_reset_cleared_count(), 2);
    }

    // Mod unload still evicts and records eviction telemetry accurately afterwards.
    app.unload_mod(mod_id).expect("unload mod");
    app.update();
    {
        let extracted = app
            .get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>();
        assert!(extracted.is_empty());
        assert_eq!(extracted.evicted_pipeline_ids().len(), 2);
        assert!(extracted.evicted_pipeline_ids().contains(&pipe_1));
        assert!(extracted.evicted_pipeline_ids().contains(&pipe_2));
    }
}

#[test]
fn pipeline_cache_swap_on_render_startup_requeues_fresh_cached_pipeline_ids() {
    let Ok(RenderResources(device, ..)) = std::panic::catch_unwind(|| {
        bevy_platform::future::block_on(bevy_render::renderer::initialize_renderer(
            Backends::all(),
            None,
            &WgpuSettings::default(),
        ))
    }) else {
        // Skip only in GPU-less environments lacking even a software wgpu adapter;
        // `render_startup_clears_stale_mesh_pipelines_and_reextracts_cleanly` still exercises
        // RenderStartup clearing headlessly everywhere.
        return;
    };

    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "pipeline_cache_swap");

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
        ModMeshShaderPlugin::new(
            ModMeshShaderConfig::enabled().with_required_wgpu_features(WgpuFeatures::empty()),
        ),
        ModRenderRecoveryPlugin::default(),
    ));

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");
    let mesh_1 = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl("swap_m1", "mods/swap/m1.wesl", "@mesh fn m1() {}"),
        )
        .expect("register mesh_1");
    let mesh_2 = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl("swap_m2", "mods/swap/m2.wesl", "@mesh fn m2() {}"),
        )
        .expect("register mesh_2");

    let pipe_1 = app
        .register_mod_mesh_pipeline(mod_id, ModMeshPipelineSpec::new("recovery_mesh_1", mesh_1))
        .expect("register pipe_1");
    let pipe_2 = app
        .register_mod_mesh_pipeline(mod_id, ModMeshPipelineSpec::new("recovery_mesh_2", mesh_2))
        .expect("register pipe_2");

    // Seed RenderApp with a live RenderDevice, AssetServer, MaterialBindGroupAllocators, and
    // PipelineCache #1 pre-populated with 3 dummy pipelines so our mod mesh pipelines receive
    // IDs >= 3 (out of range for a fresh cache!).
    {
        let asset_server = app.world().resource::<AssetServer>().clone();
        let cache_v1 = PipelineCache::new(device.clone(), false);
        for _ in 0..3 {
            let _ = cache_v1.queue_render_pipeline(RenderPipelineDescriptor::default());
        }
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        render_world.insert_resource(asset_server);
        render_world.init_resource::<MaterialBindGroupAllocators>();
        render_world.insert_resource(device.clone());
        render_world.insert_resource(cache_v1);
    }

    // Frame 1: Extract queues both mod mesh pipelines on PipelineCache #1.
    app.update();
    let (old_id_1, old_id_2) = {
        let render_world = app.get_sub_app(RenderApp).unwrap().world();
        let extracted = render_world.resource::<ExtractedModMeshPipelines>();
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted.recovery_epoch(), 0);
        let id1 = extracted
            .get(pipe_1)
            .and_then(|e| e.cached_pipeline_id)
            .expect("pipe_1 queued on PipelineCache #1");
        let id2 = extracted
            .get(pipe_2)
            .and_then(|e| e.cached_pipeline_id)
            .expect("pipe_2 queued on PipelineCache #1");
        assert!(id1.id() >= 3 && id2.id() >= 3);
        (id1, id2)
    };

    // Simulate GPU device loss recovery: unpack brand-new PipelineCache #2 and run RenderStartup.
    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        render_world.insert_resource(PipelineCache::new(device, false));
        render_world.run_schedule(RenderStartup);

        let extracted = render_world.resource::<ExtractedModMeshPipelines>();
        assert!(extracted.is_empty());
        assert_eq!(extracted.recovery_epoch(), 1);
        assert_eq!(extracted.last_reset_cleared_count(), 2);
        assert!(extracted.evicted_pipeline_ids().is_empty());
        assert_eq!(
            render_world
                .resource::<ModRenderRecoveryRenderState>()
                .render_startup_generations(),
            1,
            "RenderStartup must also run sprite/material recovery reset once"
        );
    }

    // Frame 2: ExtractSchedule re-queues both live pipelines onto PipelineCache #2 with fresh IDs.
    app.update();
    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        let (new_id_1, new_id_2) = {
            let extracted = render_world.resource::<ExtractedModMeshPipelines>();
            assert_eq!(extracted.len(), 2);
            assert_eq!(extracted.recovery_epoch(), 1);
            let id1 = extracted
                .get(pipe_1)
                .and_then(|e| e.cached_pipeline_id)
                .expect("pipe_1 re-queued on PipelineCache #2");
            let id2 = extracted
                .get(pipe_2)
                .and_then(|e| e.cached_pipeline_id)
                .expect("pipe_2 re-queued on PipelineCache #2");
            assert_ne!(id1, old_id_1);
            assert_ne!(id2, old_id_2);
            assert!(id1.id() < 2 && id2.id() < 2);
            (id1, id2)
        };

        let mut cache_v2 = render_world.resource_mut::<PipelineCache>();
        cache_v2.process_queue();
        assert_eq!(
            cache_v2
                .get_mesh_pipeline_descriptor(new_id_1)
                .label
                .as_deref(),
            Some("recovery_mesh_1")
        );
        assert_eq!(
            cache_v2
                .get_mesh_pipeline_descriptor(new_id_2)
                .label
                .as_deref(),
            Some("recovery_mesh_2")
        );
    }

    // Verify recovery also re-checks queueing eligibility (e.g. if auto_queue / device features change).
    app.world_mut()
        .resource_mut::<ModMeshShaderConfig>()
        .auto_queue_in_render_app = false;
    app.get_sub_app_mut(RenderApp)
        .unwrap()
        .world_mut()
        .run_schedule(RenderStartup);
    app.update();
    {
        let extracted = app
            .get_sub_app(RenderApp)
            .unwrap()
            .world()
            .resource::<ExtractedModMeshPipelines>();
        assert_eq!(extracted.recovery_epoch(), 2);
        assert_eq!(extracted.get(pipe_1).unwrap().cached_pipeline_id, None);
        assert_eq!(extracted.get(pipe_2).unwrap().cached_pipeline_id, None);
    }

    app.unload_mod(mod_id).expect("unload mod");
}

#[test]
fn render_startup_clears_2d_view_keys_and_redirties_mod_sprite_materials() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "sprite_render_startup");

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
    ));

    // Mirror CameraPlugin's DirtySpecializationSystems::Clear -> CheckForChanges ordering.
    {
        let render_app = app.get_sub_app_mut(RenderApp).expect("RenderApp present");
        render_app.configure_sets(
            ExtractSchedule,
            DirtySpecializationSystems::Clear.before(DirtySpecializationSystems::CheckForChanges),
        );
        render_app.add_systems(
            ExtractSchedule,
            clear_dirty_specializations.in_set(DirtySpecializationSystems::Clear),
        );
    }

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");
    let frag = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl(
                "sprite_rs_fs",
                "mods/sprite_rs/fs.wesl",
                "@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }",
            ),
        )
        .expect("register fragment shader");
    let sprite_ext = app
        .register_mod_wesl_sprite_extension(
            mod_id,
            ModWeslSpriteExtensionSpec::new("sprite_rs_ext").with_fragment_shader(frag.clone()),
        )
        .expect("register sprite extension");
    let ext2d = app
        .register_mod_wesl_extended_sprite_material(
            mod_id,
            ModExtendedSpriteMaterialSpec::new("ext2d_rs").with_fragment_shader(frag),
        )
        .expect("register extended 2d material");

    let sprite_entity = app
        .spawn_mod_sprite_with_material(
            mod_id,
            ModUnifiedSpriteSpec::new().with_sync_to_render_world(true),
            sprite_ext,
        )
        .expect("spawn unified sprite");
    let ext2d_entity = app
        .spawn_mod_extended_mesh_sprite(
            mod_id,
            ModUnifiedSpriteSpec::new().with_sync_to_render_world(true),
            ext2d,
        )
        .expect("spawn extended 2d mesh sprite");

    // Warm up until initial spawn + `SpriteMaterialPlugin` deferred `MeshMaterial2d` insertion +
    // `AssetChanged` propagation have settled into a clean steady-state frame.
    for _ in 0..4 {
        app.update();
    }

    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        let dirty_before = render_world
            .resource::<DirtySpecializations>()
            .changed_renderables
            .clone();
        assert!(
            dirty_before.is_empty(),
            "steady-state frame should have empty DirtySpecializations, got {dirty_before:?}"
        );

        // Populate synthetic cached GPU handles across 2D & 3D caches as if rendered before loss.
        let mut sprite_view_keys = SpriteViewKeyCache::default();
        sprite_view_keys.insert(
            MainEntity::from(Entity::PLACEHOLDER),
            Mesh2dPipelineKey::NONE,
        );
        render_world.insert_resource(sprite_view_keys);

        let mut spec_cache = SpecializedMaterial2dPipelineCache::default();
        let view_entity = RetainedViewEntity::new(MainEntity::from(Entity::PLACEHOLDER), None, 0);
        spec_cache.entry(view_entity).or_default().insert(
            MainEntity::from(ext2d_entity),
            CachedRenderPipelineId::new(42),
        );
        render_world.insert_resource(spec_cache);

        render_world.init_resource::<SpecializedMeshPipelines<Material2dPipelineSpecializer>>();
        render_world.init_resource::<SpecializedMeshPipelines<Mesh2dPipeline>>();

        let mut pbr_view_keys = PbrViewKeyCache::default();
        pbr_view_keys.insert(view_entity, bevy_pbr::MeshPipelineKey::empty());
        render_world.insert_resource(pbr_view_keys);
    }

    // Run our RenderStartup cache invalidation system (in a full RenderPlugin app this runs
    // automatically inside the `RenderStartup` schedule alongside Bevy's GPU startup systems;
    // headless RenderApp lacks `RenderDevice` required by Bevy's `Material2dPlugin` startup hook).
    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        reset_mod_sprite_materials_on_render_startup(render_world);

        assert!(render_world.resource::<SpriteViewKeyCache>().is_empty());
        assert!(
            render_world
                .resource::<SpecializedMaterial2dPipelineCache>()
                .is_empty()
        );
        assert!(render_world.resource::<PbrViewKeyCache>().is_empty());

        let state = render_world.resource::<ModRenderRecoveryRenderState>();
        assert_eq!(state.render_startup_generations(), 1);
        assert!(state.pending_force_dirty_2d_materials());
        assert_eq!(state.last_invalidation().sprite_view_keys_cleared, 1);
        assert_eq!(state.last_invalidation().sprite_material_caches_cleared, 1);
        assert!(state.last_invalidation().material2d_specializer_reset);
        assert!(state.last_invalidation().mesh2d_specializer_reset);
        assert_eq!(state.last_invalidation().pbr_view_keys_cleared, 1);
    }

    // Frame 3 (with ZERO main-world mutations): ExtractSchedule must force-dirty both unchanged
    // mod 2D material entities AFTER DirtySpecializationSystems::Clear runs!
    app.update();
    {
        let render_world = app.get_sub_app(RenderApp).unwrap().world();
        let dirty = render_world.resource::<DirtySpecializations>();
        assert!(
            dirty
                .changed_renderables
                .contains(&MainEntity::from(sprite_entity)),
            "unchanged SpriteMaterial entity must be re-dirtied after RenderStartup"
        );
        assert!(
            dirty
                .changed_renderables
                .contains(&MainEntity::from(ext2d_entity)),
            "unchanged ModExtendedSpriteMaterial2d entity must be re-dirtied after RenderStartup"
        );
        let state = render_world.resource::<ModRenderRecoveryRenderState>();
        assert!(!state.pending_force_dirty_2d_materials());
        assert!(state.last_requeued_2d_entities() >= 2);
    }

    // Frame 4: One-shot flag is consumed; steady-state frame stays clean with zero churn.
    app.update();
    {
        let render_world = app.get_sub_app(RenderApp).unwrap().world();
        assert!(
            render_world
                .resource::<DirtySpecializations>()
                .changed_renderables
                .is_empty()
        );
    }

    app.unload_mod(mod_id).expect("unload mod");
}
