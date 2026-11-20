#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr
)]

//! Integration tests for `bevy_abi_mod_render`:
//! 1. Multi-world `SubApp` extraction (`RenderTestApp`, `AudioTestApp`) of host-registered and
//!    guest-dynamic components plus singleton resources via `bevy_extract`, including cleanup on
//!    `unload_mod`.
//! 2. Data-driven `.wesl` (`Source::Wesl`) [`Shader`] assets, [`AlignedVec`]-backed
//!    [`ShaderBuffer`] storage buffers, `SpriteMeshMaterial` / `ExtendedMaterial2d` /
//!    `StandardMaterial` assets, `mod://` asset protocol loading, and deterministic cleanup on
//!    `unload_mod`.

use bevy_abi_mod_bridges::asset::{ModAssetAppExt, ModAssetPlugin};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host::world_ext::ModScoped;
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, LoadOptions, ModHost, host_cmd_spawn,
    host_cmd_spawn_persistent, host_register_component,
};
use bevy_abi_mod_render::prelude::*;
use bevy_abi_mod_render::register_mod_extended_sprite_material_in_world;
use bevy_abi_mod_sys::{ComponentDesc, FfiResult};
use bevy_app::{App, AppLabel, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_extract::sync_world::{MainEntity, SubEntity};
use bevy_material::AlphaMode;
use bevy_pbr::StandardMaterial;
use bevy_render::storage::{ShaderBuffer, ShaderBufferData};
use bevy_shader::{Shader, ShaderImport, Source, ValidateShader};
use bevy_sprite_render::{AlphaMode2d, ExtendedMaterial2d, SpriteMeshMaterial};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Position")]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

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
            "bevy_abi_mod_render_shadow_{}_{}",
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
struct RenderTestApp;

#[derive(AppLabel, Debug, Hash, PartialEq, Eq, Clone, Default, Copy)]
struct AudioTestApp;

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "sub_app_test::RenderPalette")]
struct RenderPalette {
    exposure: f32,
    gamma: f32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "sub_app_test::AudioMixConfig")]
struct AudioMixConfig {
    master_gain: f32,
    channels: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct DynamicGlow {
    intensity: f32,
    radius: f32,
}

struct TempAssetDir {
    path: PathBuf,
}

impl TempAssetDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bam_subapp_assets_{tag}_{}_{}",
            std::process::id(),
            id
        ));
        fs::create_dir_all(&path).expect("create temp asset directory");
        Self { path }
    }
}

impl Drop for TempAssetDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn multi_sub_app_extracts_host_and_guest_dynamic_components_resources_and_cleans_up_on_unload() {
    let ordered_dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "sub_app_extract");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let pos_h = app
        .register_mod_component::<Position>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .expect("register Position");
    let palette_h = app
        .register_mod_resource::<RenderPalette>()
        .expect("register RenderPalette");
    let audio_h = app
        .register_mod_resource::<AudioMixConfig>()
        .expect("register AudioMixConfig");

    app.add_plugins((
        ModSubAppPlugin::<RenderTestApp>::new(
            ModSubAppConfig::new()
                .with_component(pos_h)
                .with_component_name("sub_app_test::DynamicGlow")
                .with_resource(palette_h),
        ),
        ModSubAppPlugin::<AudioTestApp>::new(
            ModSubAppConfig::new()
                .with_component(pos_h)
                .with_resource(audio_h),
        ),
    ));

    // Automatically attach `SyncToSubWorld<RenderTestApp>` whenever host `Position` is inserted.
    app.sync_entities_with_mod_component::<RenderTestApp, Position>();

    app.insert_resource(RenderPalette {
        exposure: 1.25,
        gamma: 2.2,
    });
    app.insert_resource(AudioMixConfig {
        master_gain: 0.85,
        channels: 2,
    });

    let mod_id = app
        .load_mod_with(
            ordered_dylib.path(),
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("load mod");

    // Register a guest-dynamic POD component owned by `mod_id` (`drop_fn = None`, `is_abi_pod = true`).
    let dyn_name = CString::new("sub_app_test::DynamicGlow").unwrap();
    let dyn_uuid = ComponentUuid::from_parts(
        "sub_app_test::DynamicGlow",
        core::mem::size_of::<DynamicGlow>(),
        core::mem::align_of::<DynamicGlow>(),
        0,
    );
    let dyn_desc = ComponentDesc::new(
        dyn_name.as_ptr(),
        dyn_uuid,
        core::mem::size_of::<DynamicGlow>(),
        core::mem::align_of::<DynamicGlow>(),
    );
    let mod_active = Arc::new(AtomicBool::new(true));
    let dyn_h = {
        let registry = app.world().resource::<ModRegistry>().clone();
        let world_ptr: *mut bevy_ecs::world::World = app.world_mut();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            registry,
            Arc::clone(&mod_active),
            world_ptr,
            Some(mod_id),
        );
        let mut handle = BamComponent::INVALID;
        // SAFETY: `load_ctx.as_ffi_ptr()` is active and `dyn_desc` has valid C-string name & POD layout.
        let reg_res =
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &dyn_desc, &mut handle) };
        assert_eq!(reg_res, FfiResult::OK);
        load_ctx.finish_init();
        handle
    };

    // Spawn:
    // 1. `e_scoped`: ModScoped(mod_id) carrying `Position` + `DynamicGlow` (auto-synced to RenderTestApp,
    //    manually tagged for AudioTestApp too).
    // 2. `e_persistent`: persistent entity carrying `Position` + `DynamicGlow` (survives unload_mod,
    //    while `DynamicGlow` must be stripped cleanly in RenderTestApp).
    let snap = app.world().resource::<ModRegistry>().snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let mut ffi_scoped = FfiEntity::NULL;
    let mut ffi_persistent = FfiEntity::NULL;
    {
        let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap)
            .with_mod_context(Some(mod_id), Some(mod_active));
        let raw_cmds = cmds.as_ffi_ptr();

        let pos1 = Position {
            x: 10.0,
            y: 20.0,
            z: 30.0,
        };
        let glow1 = DynamicGlow {
            intensity: 3.5,
            radius: 12.0,
        };
        let comps = [pos_h, dyn_h];
        let ptrs1 = [
            (&raw const pos1).cast::<u8>(),
            (&raw const glow1).cast::<u8>(),
        ];

        let pos2 = Position {
            x: -5.0,
            y: 4.0,
            z: 8.0,
        };
        let glow2 = DynamicGlow {
            intensity: 1.5,
            radius: 6.0,
        };
        let ptrs2 = [
            (&raw const pos2).cast::<u8>(),
            (&raw const glow2).cast::<u8>(),
        ];

        // SAFETY: Active `raw_cmds`, valid handles, and initialized payload pointers.
        unsafe {
            assert_eq!(
                host_cmd_spawn(
                    raw_cmds,
                    comps.as_ptr(),
                    ptrs1.as_ptr(),
                    comps.len(),
                    &mut ffi_scoped,
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn_persistent(
                    raw_cmds,
                    comps.as_ptr(),
                    ptrs2.as_ptr(),
                    comps.len(),
                    &mut ffi_persistent,
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(app.world_mut(), &snap);

    let main_scoped = Entity::from_bits(ffi_scoped.to_bits());
    let main_persistent = Entity::from_bits(ffi_persistent.to_bits());
    assert!(app.attach_sync_to_sub_app::<AudioTestApp>(main_scoped));
    assert_eq!(
        app.world().entity(main_scoped).get::<ModScoped>(),
        Some(&ModScoped(mod_id))
    );
    assert_eq!(app.world().entity(main_persistent).get::<ModScoped>(), None);

    // Frame 1: sync + extract into both RenderTestApp and AudioTestApp.
    app.update();

    let render_scoped_sub = app
        .world()
        .entity(main_scoped)
        .get::<SubEntity<RenderTestApp>>()
        .expect("main_scoped synced to RenderTestApp")
        .id();
    let render_persistent_sub = app
        .world()
        .entity(main_persistent)
        .get::<SubEntity<RenderTestApp>>()
        .expect("main_persistent synced to RenderTestApp")
        .id();
    let audio_scoped_sub = app
        .world()
        .entity(main_scoped)
        .get::<SubEntity<AudioTestApp>>()
        .expect("main_scoped synced to AudioTestApp")
        .id();

    {
        let render_world = app.get_sub_app(RenderTestApp).unwrap().world();
        assert_eq!(
            render_world.entity(render_scoped_sub).get::<MainEntity>(),
            Some(&MainEntity::from(main_scoped))
        );
        assert_eq!(
            render_world.entity(render_scoped_sub).get::<Position>(),
            Some(&Position {
                x: 10.0,
                y: 20.0,
                z: 30.0
            })
        );
        let dyn_cid = render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::DynamicGlow")
            .expect("DynamicGlow registered in RenderTestApp")
            .component_id;
        let ptr = render_world
            .entity(render_scoped_sub)
            .get_by_id(dyn_cid)
            .expect("DynamicGlow extracted onto RenderTestApp entity");
        // SAFETY: `dyn_cid` was registered with `DynamicGlow` layout.
        let extracted_glow = unsafe { *ptr.deref::<DynamicGlow>() };
        assert_eq!(
            extracted_glow,
            DynamicGlow {
                intensity: 3.5,
                radius: 12.0
            }
        );

        let palette_cid = render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::RenderPalette")
            .expect("RenderPalette registered in RenderTestApp")
            .component_id;
        let palette_ptr = render_world
            .get_resource_by_id(palette_cid)
            .expect("RenderPalette extracted to RenderTestApp");
        // SAFETY: `palette_cid` was registered with `RenderPalette` layout.
        let extracted_palette = unsafe { *palette_ptr.deref::<RenderPalette>() };
        assert_eq!(
            extracted_palette,
            RenderPalette {
                exposure: 1.25,
                gamma: 2.2
            }
        );
    }

    {
        let audio_world = app.get_sub_app(AudioTestApp).unwrap().world();
        assert_eq!(
            audio_world.entity(audio_scoped_sub).get::<Position>(),
            Some(&Position {
                x: 10.0,
                y: 20.0,
                z: 30.0
            })
        );
        let audio_cid = audio_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::AudioMixConfig")
            .expect("AudioMixConfig registered in AudioTestApp")
            .component_id;
        let audio_ptr = audio_world
            .get_resource_by_id(audio_cid)
            .expect("AudioMixConfig extracted to AudioTestApp");
        // SAFETY: `audio_cid` was registered with `AudioMixConfig` layout.
        let extracted_mix = unsafe { *audio_ptr.deref::<AudioMixConfig>() };
        assert_eq!(
            extracted_mix,
            AudioMixConfig {
                master_gain: 0.85,
                channels: 2
            }
        );
    }

    // Frame 2: Mutate components & resources in MainWorld, verify SubApps mirror new values.
    app.world_mut().entity_mut(main_scoped).insert(Position {
        x: 99.0,
        y: -12.0,
        z: 4.5,
    });
    app.world_mut().insert_resource(RenderPalette {
        exposure: 2.5,
        gamma: 1.8,
    });
    app.world_mut().remove_resource::<AudioMixConfig>();

    app.update();

    {
        let render_world = app.get_sub_app(RenderTestApp).unwrap().world();
        assert_eq!(
            render_world.entity(render_scoped_sub).get::<Position>(),
            Some(&Position {
                x: 99.0,
                y: -12.0,
                z: 4.5
            })
        );
        let palette_cid = render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::RenderPalette")
            .unwrap()
            .component_id;
        let palette_ptr = render_world.get_resource_by_id(palette_cid).unwrap();
        // SAFETY: `palette_cid` matches `RenderPalette`.
        let updated_palette = unsafe { *palette_ptr.deref::<RenderPalette>() };
        assert_eq!(
            updated_palette,
            RenderPalette {
                exposure: 2.5,
                gamma: 1.8
            }
        );
    }
    {
        let audio_world = app.get_sub_app(AudioTestApp).unwrap().world();
        assert_eq!(
            audio_world.entity(audio_scoped_sub).get::<Position>(),
            Some(&Position {
                x: 99.0,
                y: -12.0,
                z: 4.5
            })
        );
        let audio_cid = audio_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::AudioMixConfig")
            .unwrap()
            .component_id;
        assert!(
            audio_world.get_resource_by_id(audio_cid).is_none(),
            "removed MainWorld resource must be removed from AudioTestApp on next extract cycle"
        );
    }

    // Verify `DynamicGlow` also extracted onto `render_persistent_sub`, then test component removal
    // propagation from `MainWorld` -> `RenderTestApp` before `unload_mod`.
    let main_dyn_cid = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("sub_app_test::DynamicGlow")
        .unwrap()
        .component_id;
    {
        let render_world = app.get_sub_app(RenderTestApp).unwrap().world();
        let sub_dyn_cid = render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::DynamicGlow")
            .unwrap()
            .component_id;
        assert!(
            render_world
                .entity(render_persistent_sub)
                .get_by_id(sub_dyn_cid)
                .is_ok()
        );
    }
    app.world_mut()
        .entity_mut(main_persistent)
        .remove_by_id(main_dyn_cid);
    app.update();
    {
        let render_world = app.get_sub_app(RenderTestApp).unwrap().world();
        let sub_dyn_cid = render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::DynamicGlow")
            .unwrap()
            .component_id;
        assert!(
            render_world
                .entity(render_persistent_sub)
                .get_by_id(sub_dyn_cid)
                .is_err(),
            "removing DynamicGlow from MainWorld persistent entity must strip it in RenderTestApp"
        );
    }

    // Unload mod_id in MainWorld and advance one frame for SubApp sync/extract cleanup.
    app.unload_mod(mod_id).expect("unload mod");
    app.update();

    assert!(!app.world().entities().contains(main_scoped));
    assert!(app.world().entities().contains(main_persistent));

    let render_world = app.get_sub_app(RenderTestApp).unwrap().world();
    assert!(
        !render_world.entities().contains(render_scoped_sub),
        "ModScoped entity must despawn in RenderTestApp on extract/sync cycle after unload_mod"
    );
    assert!(
        render_world.entities().contains(render_persistent_sub),
        "persistent entity must survive in RenderTestApp"
    );
    assert_eq!(
        render_world.entity(render_persistent_sub).get::<Position>(),
        Some(&Position {
            x: -5.0,
            y: 4.0,
            z: 8.0
        })
    );
    assert!(
        render_world
            .resource::<ModRegistry>()
            .get_by_name("sub_app_test::DynamicGlow")
            .is_none(),
        "unloaded mod's component entry must be evicted from RenderTestApp ModRegistry"
    );

    let audio_world = app.get_sub_app(AudioTestApp).unwrap().world();
    assert!(
        !audio_world.entities().contains(audio_scoped_sub),
        "ModScoped entity must despawn in AudioTestApp on extract/sync cycle after unload_mod"
    );
}

#[test]
fn wesl_shaders_aligned_shader_buffers_and_materials_cleanup_on_mod_unload() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "wesl_materials");
    let temp_dir = TempAssetDir::new("wesl_files");
    let wesl_disk_path = temp_dir.path.join("from_disk.wesl");
    let disk_wesl_src = "@fragment fn fragment() -> @location(0) vec4<f32> { return vec4<f32>(0.2, 0.8, 1.0, 1.0); }";
    fs::write(&wesl_disk_path, disk_wesl_src).expect("write .wesl file");

    // Headless App WITHOUT AssetPlugin to verify headless Assets<T> operation.
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModWeslMaterialPlugin));

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    // 1. Register inline `.wesl` + disk `.wesl` shaders for mod_a.
    let inline_wesl_src = "import super::common::tint;\n@fragment fn fs_main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }";
    let shader_a_inline = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl("plasma", "mods/mod_a/plasma.wesl", inline_wesl_src),
        )
        .expect("register inline wesl shader");
    let shader_a_disk = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl_file(
                "disk_fx",
                "mods/mod_a/from_disk.wesl",
                &wesl_disk_path,
            )
            .expect("load wesl file"),
        )
        .expect("register disk wesl shader");

    {
        let shaders = app.world().resource::<Assets<Shader>>();
        let s1 = shaders.get(&shader_a_inline).expect("inline shader stored");
        assert!(matches!(&s1.source, Source::Wesl(src) if src.as_ref() == inline_wesl_src));
        assert!(matches!(s1.validate_shader, ValidateShader::Enabled));
        assert_eq!(
            s1.import_path,
            ShaderImport::AssetPath("/mods/mod_a/plasma".to_string())
        );

        let s2 = shaders.get(&shader_a_disk).expect("disk shader stored");
        assert!(matches!(&s2.source, Source::Wesl(src) if src.as_ref() == disk_wesl_src));
    }

    // 2. Register AlignedVec-backed ShaderBuffers (64-byte and 32-byte odd-length padded to % 4).
    let buf_f32 = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("particle_buf", 64, &[1.0, 2.5, 4.0]),
        )
        .expect("register f32 ShaderBuffer");
    let buf_odd = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_aligned_bytes("odd_buf", 32, vec![1, 2, 3, 4, 5]),
        )
        .expect("register odd-length ShaderBuffer");

    {
        let buffers = app.world().resource::<Assets<ShaderBuffer>>();
        let b1 = buffers.get(&buf_f32).expect("particle_buf stored");
        match &b1.data {
            ShaderBufferData::Initialized { data, buffer_size } => {
                assert_eq!(data.alignment(), 64);
                assert_eq!(data.as_ptr().align_offset(64), 0);
                assert_eq!(*buffer_size, 12);
                assert_eq!(data.len(), 12);
            }
            ShaderBufferData::Uninitialized(_) => panic!("expected Initialized buffer"),
        }

        let b2 = buffers.get(&buf_odd).expect("odd_buf stored");
        match &b2.data {
            ShaderBufferData::Initialized { data, buffer_size } => {
                assert_eq!(data.alignment(), 32);
                assert_eq!(data.as_ptr().align_offset(32), 0);
                assert_eq!(*buffer_size, 8);
                assert_eq!(data.as_slice(), &[1, 2, 3, 4, 5, 0, 0, 0]);
            }
            ShaderBufferData::Uninitialized(_) => panic!("expected Initialized buffer"),
        }
    }

    // Update buffer owned by mod_a (preserving 64-byte alignment) and reject non-owner mod_b.
    assert_eq!(
        app.update_mod_shader_buffer(mod_b, &buf_f32, &[9, 8, 7, 6]),
        Err(ModMaterialError::ModNotOwner { mod_id: mod_b })
    );
    app.update_mod_shader_buffer(mod_a, &buf_f32, &[10, 20, 30, 40, 50])
        .expect("mod_a updates own ShaderBuffer");
    {
        let buffers = app.world().resource::<Assets<ShaderBuffer>>();
        let b1 = buffers.get(&buf_f32).unwrap();
        match &b1.data {
            ShaderBufferData::Initialized { data, buffer_size } => {
                assert_eq!(data.alignment(), 64);
                assert_eq!(data.as_ptr().align_offset(64), 0);
                assert_eq!(*buffer_size, 8);
                assert_eq!(data.as_slice(), &[10, 20, 30, 40, 50, 0, 0, 0]);
            }
            ShaderBufferData::Uninitialized(_) => panic!("expected Initialized buffer"),
        }
    }

    // 3. Register 2D SpriteMeshMaterial, ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>,
    //    and 3D StandardMaterial for mod_a + a separate StandardMaterial for mod_b.
    let sprite_mat_a = app
        .register_mod_sprite_material(
            mod_a,
            ModSpriteMaterialSpec::new("plasma_sprite")
                .with_alpha_mode(AlphaMode2d::Blend)
                .with_flip(true, false)
                .with_color_rgba([0.2, 0.9, 0.8, 1.0])
                .with_shader(shader_a_inline.clone())
                .with_shader_buffer(buf_f32.clone()),
        )
        .expect("register SpriteMeshMaterial");

    let (ext_a, extended_2d_a) = register_mod_extended_sprite_material_in_world(
        app.world_mut(),
        mod_a,
        ModSpriteMaterialSpec::new("plasma_sprite_ext").with_alpha_mode(AlphaMode2d::Blend),
        ModSpriteExtension2d {
            buffer: buf_f32.clone(),
            params: ModMaterialParams {
                tint: [0.5, 1.0, 0.25, 1.0],
                intensity: 2.0,
                ..Default::default()
            },
        },
    )
    .expect("register ExtendedMaterial2d");

    let pbr_mat_a = app
        .register_mod_standard_material(
            mod_a,
            ModStandardMaterialSpec::new("plasma_pbr")
                .with_pbr(0.9, 0.15)
                .with_alpha_mode(AlphaMode::Blend)
                .with_flags(true, true)
                .with_shader(shader_a_disk.clone())
                .with_shader_buffer(buf_f32.clone()),
        )
        .expect("register StandardMaterial for mod_a");

    let pbr_mat_b = app
        .register_mod_standard_material(
            mod_b,
            ModStandardMaterialSpec::new("mod_b_metal").with_pbr(1.0, 0.2),
        )
        .expect("register StandardMaterial for mod_b");

    {
        let reg = app.world().resource::<ModWeslMaterialRegistry>();
        assert_eq!(reg.shader_count_for_mod(mod_a), 2);
        assert_eq!(reg.buffer_count_for_mod(mod_a), 2);
        assert_eq!(reg.sprite_material_count_for_mod(mod_a), 1);
        assert_eq!(reg.standard_material_count_for_mod(mod_a), 1);
        assert_eq!(reg.standard_material_count_for_mod(mod_b), 1);
        assert_eq!(
            reg.get_sprite_material(mod_a, "plasma_sprite")
                .unwrap()
                .shader
                .as_ref()
                .map(|h| h.id()),
            Some(shader_a_inline.id())
        );

        let sprite_assets = app.world().resource::<Assets<SpriteMeshMaterial>>();
        let sprite = sprite_assets.get(&sprite_mat_a).unwrap();
        assert!(sprite.flip_x);
        assert!(!sprite.flip_y);
        assert_eq!(sprite.alpha_mode, AlphaMode2d::Blend);

        let pbr_assets = app.world().resource::<Assets<StandardMaterial>>();
        let pbr = pbr_assets.get(&pbr_mat_a).unwrap();
        assert_eq!(pbr.metallic, 0.9);
        assert_eq!(pbr.perceptual_roughness, 0.15);
        assert!(pbr.unlit);
        assert!(pbr.double_sided);
    }

    // 4. Unload mod_a: all mod_a shaders, buffers, and 2D/3D materials MUST be purged from Assets<T>,
    //    while mod_b's material MUST remain intact!
    app.unload_mod(mod_a).expect("unload mod_a");

    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_a),
        0
    );
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&shader_a_inline)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&shader_a_disk)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&buf_f32)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&buf_odd)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<SpriteMeshMaterial>>()
            .get(&sprite_mat_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ModSpriteExtension2d>>()
            .get(&ext_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>()
            .get(&extended_2d_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&pbr_mat_a)
            .is_none()
    );
    assert!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&pbr_mat_b)
            .is_some(),
        "mod_b StandardMaterial must remain unaffected when mod_a unloads"
    );

    // 5. Unload mod_b: remaining material cleans up too.
    app.unload_mod(mod_b).expect("unload mod_b");
    assert!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&pbr_mat_b)
            .is_none()
    );
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_b),
        0
    );
}

#[test]
fn mod_asset_plugin_mod_protocol_wesl_shader_loading_and_unload_cleanup() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "mod_proto_wesl");
    let asset_dir = TempAssetDir::new("mod_proto_wesl");
    let shaders_dir = asset_dir.path.join("shaders");
    fs::create_dir_all(&shaders_dir).expect("create shaders subdir");
    fs::write(
        shaders_dir.join("custom.wesl"),
        "@fragment fn fragment() -> @location(0) vec4<f32> { return vec4<f32>(1.0, 0.5, 0.0, 1.0); }",
    )
    .expect("write custom.wesl");

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        ModAssetPlugin,
        AssetPlugin::default(),
        AbiModPlugin,
        ModWeslMaterialPlugin,
    ));

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");
    let mod_name = app
        .world()
        .resource::<ModHost>()
        .get_mod(mod_id)
        .unwrap()
        .name()
        .to_string();
    app.mount_mod_asset_dir(&mod_name, &asset_dir.path)
        .expect("mount mod asset dir");

    let handle = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_mod_path(
                "custom_wesl",
                format!("mod://{mod_name}/shaders/custom.wesl"),
            ),
        )
        .expect("queue mod:// wesl shader load");

    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .get_shader(mod_id, "custom_wesl")
            .map(|h| h.id()),
        Some(handle.id())
    );
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .shader_count_for_mod(mod_id),
        1
    );

    app.unload_mod(mod_id).expect("unload mod");
    assert_eq!(
        app.world()
            .resource::<ModWeslMaterialRegistry>()
            .total_assets_for_mod(mod_id),
        0
    );
}
