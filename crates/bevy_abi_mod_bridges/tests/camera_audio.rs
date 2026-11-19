#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment
)]

//! Headless integration tests for `ModCameraLightBridgePlugin` (`bridge-light`,
//! `bridge-camera-controller`, `bridge-solari`) and `ModAudioBridgePlugin` (`bridge-audio`).
//!
//! Covers zero-padding ABI layout, `ModRegistry` registration, headless spawn, guest-to-Bevy
//! parameter mutation, Bevy-to-guest bidirectional pull sync, raw FFI `host_cmd_spawn`, and
//! `ModScoped` entity despawn on mod unload without requiring a GPU window or OS audio device.

#[cfg(feature = "bridge-light")]
mod light_camera_tests {
    use bevy_abi_mod_bridges::camera_light::{
        CANONICAL_CAMERA_2D_NAME, CANONICAL_CAMERA_3D_NAME, CANONICAL_DIRECTIONAL_LIGHT_NAME,
        CANONICAL_POINT_LIGHT_NAME, CANONICAL_SPOT_LIGHT_NAME, CLEAR_COLOR_CUSTOM,
        CLEAR_COLOR_DEFAULT, CLEAR_COLOR_NONE, DEPTH_LOAD_OP_CLEAR, DEPTH_LOAD_OP_LOAD,
        FfiCamera2d, FfiCamera3d, FfiDirectionalLight, FfiPointLight, FfiSpotLight,
        ModCameraLightBridgePlugin, TONEMAPPING_ACES_FITTED, TONEMAPPING_AGX,
        TONEMAPPING_BLENDER_FILMIC, TONEMAPPING_KHRONOS_PBR_NEUTRAL, TONEMAPPING_LINEAR,
        TONEMAPPING_NONE, TONEMAPPING_REINHARD, TONEMAPPING_REINHARD_LUMINANCE,
        TONEMAPPING_SOMEWHAT_BORING_DISPLAY_TRANSFORM, TONEMAPPING_TONY_MC_MAPFACE,
        TRANSMISSION_QUALITY_HIGH, TRANSMISSION_QUALITY_LOW, TRANSMISSION_QUALITY_MEDIUM,
        TRANSMISSION_QUALITY_ULTRA,
    };
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::loader::cleanup_orphaned_mod_scoped_entities;
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_cmd_spawn, host_sys_commands,
    };
    use bevy_abi_mod_sys::{BamSystemCtx, FfiBool, FfiEntity, FfiResult};
    use bevy_app::{App, Update};
    use bevy_camera::{
        Camera, Camera2d, Camera3d, Camera3dDepthLoadOp, ClearColorConfig, Exposure, Hdr,
    };
    use bevy_color::Color;
    use bevy_ecs::entity::Entity;
    use bevy_light::{DirectionalLight, PointLight, SpotLight};
    use bevy_pbr::{ScreenSpaceTransmission, ScreenSpaceTransmissionQuality};
    use bevy_render::view::{DebandDither, Tonemapping};
    use core::ffi::c_void;
    use std::sync::{Arc, Mutex};

    fn setup_camera_light_app() -> App {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, ModCameraLightBridgePlugin));
        app
    }

    #[test]
    fn camera_2d_spawn_bidirectional_sync_and_hdr() {
        let mut app = setup_camera_light_app();

        {
            let reg = app.world().resource::<ModRegistry>();
            let entry = reg
                .get_by_name(CANONICAL_CAMERA_2D_NAME)
                .expect("FfiCamera2d registered");
            assert!(entry.is_abi_pod);
            assert!(entry.insert_raw_fn.is_some());
            assert!(entry.insert_default_fn.is_some());
            assert_eq!(entry.size, 24);
            assert_eq!(entry.align, 4);
        }

        let cam_entity = app
            .world_mut()
            .spawn((
                FfiCamera2d {
                    order: 3,
                    clear_color: [0.1, 0.2, 0.3, 1.0],
                    is_active: FfiBool::TRUE,
                    clear_color_mode: CLEAR_COLOR_CUSTOM,
                    invert_culling: FfiBool::TRUE,
                    hdr: FfiBool::TRUE,
                },
                ModScoped(ModId(1)),
            ))
            .id();

        app.update();

        assert!(app.world().get::<Camera2d>(cam_entity).is_some());
        assert!(app.world().get::<Hdr>(cam_entity).is_some());
        let cam = app
            .world()
            .get::<Camera>(cam_entity)
            .expect("Camera spawned");
        assert_eq!(cam.order, 3);
        assert!(cam.is_active);
        assert!(cam.invert_culling);
        match cam.clear_color {
            ClearColorConfig::Custom(color) => {
                let lin = color.to_linear();
                assert!((lin.red - 0.1).abs() < 1e-6);
                assert!((lin.green - 0.2).abs() < 1e-6);
                assert!((lin.blue - 0.3).abs() < 1e-6);
            }
            other => panic!("expected Custom clear color, got {other:?}"),
        }

        // Guest mutates FfiCamera2d -> pushes into Camera and removes Hdr.
        {
            let mut ffi = app.world_mut().get_mut::<FfiCamera2d>(cam_entity).unwrap();
            ffi.order = -2;
            ffi.is_active = FfiBool::FALSE;
            ffi.clear_color_mode = CLEAR_COLOR_NONE;
            ffi.invert_culling = FfiBool::FALSE;
            ffi.hdr = FfiBool::FALSE;
        }
        app.update();

        assert!(app.world().get::<Hdr>(cam_entity).is_none());
        let cam = app.world().get::<Camera>(cam_entity).unwrap();
        assert_eq!(cam.order, -2);
        assert!(!cam.is_active);
        assert!(!cam.invert_culling);
        assert!(matches!(cam.clear_color, ClearColorConfig::None));

        // Host mutates Camera directly and re-adds Hdr -> pulls back into FfiCamera2d.
        {
            let mut em = app.world_mut().entity_mut(cam_entity);
            let mut cam = em.get_mut::<Camera>().unwrap();
            cam.order = 9;
            cam.is_active = true;
            cam.clear_color = ClearColorConfig::Default;
            em.insert(Hdr);
        }
        app.update();

        let ffi = *app.world().get::<FfiCamera2d>(cam_entity).unwrap();
        assert_eq!(ffi.order, 9);
        assert!(ffi.is_active.get());
        assert_eq!(ffi.clear_color_mode, CLEAR_COLOR_DEFAULT);
        assert!(ffi.hdr.get());

        // Steady-state frame causes no drift.
        app.update();
        assert_eq!(app.world().get::<Camera>(cam_entity).unwrap().order, 9);
        assert!(app.world().get::<Hdr>(cam_entity).is_some());
    }

    #[test]
    fn camera_3d_tonemapping_deband_dither_and_screen_space_transmission() {
        let mut app = setup_camera_light_app();

        {
            let reg = app.world().resource::<ModRegistry>();
            let entry = reg
                .get_by_name(CANONICAL_CAMERA_3D_NAME)
                .expect("FfiCamera3d registered");
            assert!(entry.is_abi_pod);
            assert_eq!(entry.size, 48);
            assert_eq!(entry.align, 4);
        }

        // Verify all 10 Bevy 0.20 Tonemapping discriminants roundtrip cleanly.
        let all_tonemappers = [
            (TONEMAPPING_NONE, Tonemapping::None),
            (TONEMAPPING_LINEAR, Tonemapping::Linear),
            (TONEMAPPING_REINHARD, Tonemapping::Reinhard),
            (
                TONEMAPPING_REINHARD_LUMINANCE,
                Tonemapping::ReinhardLuminance,
            ),
            (TONEMAPPING_ACES_FITTED, Tonemapping::AcesFitted),
            (TONEMAPPING_AGX, Tonemapping::AgX),
            (
                TONEMAPPING_SOMEWHAT_BORING_DISPLAY_TRANSFORM,
                Tonemapping::SomewhatBoringDisplayTransform,
            ),
            (TONEMAPPING_TONY_MC_MAPFACE, Tonemapping::TonyMcMapface),
            (TONEMAPPING_BLENDER_FILMIC, Tonemapping::BlenderFilmic),
            (
                TONEMAPPING_KHRONOS_PBR_NEUTRAL,
                Tonemapping::KhronosPbrNeutral,
            ),
        ];
        for (tag, expected) in all_tonemappers {
            let ffi = FfiCamera3d {
                tonemapping: tag,
                ..Default::default()
            };
            assert_eq!(ffi.to_tonemapping(), expected);
        }

        let cam3d_ent = app
            .world_mut()
            .spawn((
                FfiCamera3d {
                    order: 1,
                    clear_color: [0.25, 0.5, 0.75, 1.0],
                    depth_clear_value: 0.0,
                    depth_texture_usages: Camera3d::default().depth_texture_usages.0,
                    exposure_ev100: 12.5,
                    transmission_steps: 3,
                    is_active: FfiBool::TRUE,
                    clear_color_mode: CLEAR_COLOR_CUSTOM,
                    invert_culling: FfiBool::FALSE,
                    hdr: FfiBool::TRUE,
                    depth_load_op: DEPTH_LOAD_OP_LOAD,
                    tonemapping: TONEMAPPING_AGX,
                    deband_dither: FfiBool::TRUE,
                    transmission_enabled: FfiBool::TRUE,
                    transmission_quality: TRANSMISSION_QUALITY_HIGH,
                    reserved0: [0; 3],
                },
                ModScoped(ModId(2)),
            ))
            .id();

        app.update();

        let world = app.world();
        let c3d = world.get::<Camera3d>(cam3d_ent).expect("Camera3d spawned");
        assert!(matches!(c3d.depth_load_op, Camera3dDepthLoadOp::Load));
        assert_eq!(world.get::<Camera>(cam3d_ent).unwrap().order, 1);
        assert_eq!(world.get::<Tonemapping>(cam3d_ent), Some(&Tonemapping::AgX));
        assert_eq!(
            world.get::<DebandDither>(cam3d_ent),
            Some(&DebandDither::Enabled)
        );
        assert!((world.get::<Exposure>(cam3d_ent).unwrap().ev100 - 12.5).abs() < 1e-6);
        assert!(world.get::<Hdr>(cam3d_ent).is_some());
        let sst = world
            .get::<ScreenSpaceTransmission>(cam3d_ent)
            .expect("ScreenSpaceTransmission spawned");
        assert_eq!(sst.steps, 3);
        assert_eq!(sst.quality, ScreenSpaceTransmissionQuality::High);

        // Guest mutates Tonemapping, DebandDither, DepthLoadOp, and updates then disables ScreenSpaceTransmission.
        {
            let mut ffi = app.world_mut().get_mut::<FfiCamera3d>(cam3d_ent).unwrap();
            ffi.tonemapping = TONEMAPPING_KHRONOS_PBR_NEUTRAL;
            ffi.deband_dither = FfiBool::FALSE;
            ffi.depth_load_op = DEPTH_LOAD_OP_CLEAR;
            ffi.depth_clear_value = 0.25;
            ffi.transmission_steps = 2;
            ffi.transmission_quality = TRANSMISSION_QUALITY_ULTRA;
        }
        app.update();

        assert_eq!(
            app.world().get::<Tonemapping>(cam3d_ent),
            Some(&Tonemapping::KhronosPbrNeutral)
        );
        assert_eq!(
            app.world().get::<DebandDither>(cam3d_ent),
            Some(&DebandDither::Disabled)
        );
        match &app
            .world()
            .get::<Camera3d>(cam3d_ent)
            .unwrap()
            .depth_load_op
        {
            Camera3dDepthLoadOp::Clear(v) => assert!((*v - 0.25).abs() < 1e-6),
            other => panic!("expected Clear(0.25), got {other:?}"),
        }
        let sst = app
            .world()
            .get::<ScreenSpaceTransmission>(cam3d_ent)
            .unwrap();
        assert_eq!(sst.steps, 2);
        assert_eq!(sst.quality, ScreenSpaceTransmissionQuality::Ultra);

        // Guest disables transmission -> removes ScreenSpaceTransmission component.
        app.world_mut()
            .get_mut::<FfiCamera3d>(cam3d_ent)
            .unwrap()
            .transmission_enabled = FfiBool::FALSE;
        app.update();
        assert!(
            app.world()
                .get::<ScreenSpaceTransmission>(cam3d_ent)
                .is_none()
        );

        // Guest re-enables transmission with Low quality -> re-attaches component.
        {
            let mut ffi = app.world_mut().get_mut::<FfiCamera3d>(cam3d_ent).unwrap();
            ffi.transmission_enabled = FfiBool::TRUE;
            ffi.transmission_steps = 4;
            ffi.transmission_quality = TRANSMISSION_QUALITY_LOW;
        }
        app.update();
        let sst = app
            .world()
            .get::<ScreenSpaceTransmission>(cam3d_ent)
            .expect("ScreenSpaceTransmission re-attached");
        assert_eq!(sst.steps, 4);
        assert_eq!(sst.quality, ScreenSpaceTransmissionQuality::Low);

        // Host mutates Tonemapping, DebandDither, Exposure, and ScreenSpaceTransmission -> pulls back into FfiCamera3d.
        {
            let mut em = app.world_mut().entity_mut(cam3d_ent);
            *em.get_mut::<Tonemapping>().unwrap() = Tonemapping::BlenderFilmic;
            *em.get_mut::<DebandDither>().unwrap() = DebandDither::Enabled;
            em.get_mut::<Exposure>().unwrap().ev100 = 7.0;
            let mut sst = em.get_mut::<ScreenSpaceTransmission>().unwrap();
            sst.steps = 5;
            sst.quality = ScreenSpaceTransmissionQuality::Medium;
        }
        app.update();

        let ffi = *app.world().get::<FfiCamera3d>(cam3d_ent).unwrap();
        assert_eq!(ffi.tonemapping, TONEMAPPING_BLENDER_FILMIC);
        assert!(ffi.deband_dither.get());
        assert!((ffi.exposure_ev100 - 7.0).abs() < 1e-6);
        assert!(ffi.transmission_enabled.get());
        assert_eq!(ffi.transmission_steps, 5);
        assert_eq!(ffi.transmission_quality, TRANSMISSION_QUALITY_MEDIUM);

        // Host removes ScreenSpaceTransmission -> pull detects absence and clears transmission_enabled.
        app.world_mut()
            .entity_mut(cam3d_ent)
            .remove::<ScreenSpaceTransmission>();
        app.update();
        assert!(
            !app.world()
                .get::<FfiCamera3d>(cam3d_ent)
                .unwrap()
                .transmission_enabled
                .get()
        );
    }

    #[test]
    fn lights_point_spot_directional_spawn_mutation_and_bidirectional_sync() {
        let mut app = setup_camera_light_app();

        {
            let reg = app.world().resource::<ModRegistry>();
            for (name, size) in [
                (CANONICAL_POINT_LIGHT_NAME, 44usize),
                (CANONICAL_SPOT_LIGHT_NAME, 52usize),
                (CANONICAL_DIRECTIONAL_LIGHT_NAME, 32usize),
            ] {
                let entry = reg.get_by_name(name).expect("light mirror registered");
                assert!(entry.is_abi_pod);
                assert!(entry.insert_raw_fn.is_some());
                assert!(entry.insert_default_fn.is_some());
                assert_eq!(entry.size, size);
                assert_eq!(entry.align, 4);
            }
        }

        let point_ent = app
            .world_mut()
            .spawn(FfiPointLight {
                color: [1.0, 0.5, 0.25, 1.0],
                intensity: 2500.0,
                range: 35.0,
                radius: 0.4,
                shadow_depth_bias: 0.05,
                shadow_normal_bias: 0.7,
                shadow_map_near_z: 0.2,
                shadow_maps_enabled: FfiBool::TRUE,
                contact_shadows_enabled: FfiBool::TRUE,
                affects_lightmapped_mesh_diffuse: FfiBool::FALSE,
                reserved0: 0,
            })
            .id();

        let spot_ent = app
            .world_mut()
            .spawn(FfiSpotLight {
                color: [0.2, 0.8, 1.0, 1.0],
                intensity: 4000.0,
                range: 50.0,
                radius: 0.25,
                shadow_depth_bias: 0.03,
                shadow_normal_bias: 1.5,
                shadow_map_near_z: 0.15,
                outer_angle: 0.9,
                inner_angle: 0.45,
                shadow_maps_enabled: FfiBool::TRUE,
                contact_shadows_enabled: FfiBool::FALSE,
                affects_lightmapped_mesh_diffuse: FfiBool::TRUE,
                reserved0: 0,
            })
            .id();

        let dir_ent = app
            .world_mut()
            .spawn(FfiDirectionalLight {
                color: [1.0, 0.95, 0.85, 1.0],
                illuminance: 32_000.0,
                shadow_depth_bias: 0.04,
                shadow_normal_bias: 1.2,
                shadow_maps_enabled: FfiBool::TRUE,
                contact_shadows_enabled: FfiBool::TRUE,
                affects_lightmapped_mesh_diffuse: FfiBool::TRUE,
                reserved0: 0,
            })
            .id();

        app.update();

        let pl = app.world().get::<PointLight>(point_ent).unwrap();
        assert!((pl.intensity - 2500.0).abs() < 1e-6);
        assert!((pl.range - 35.0).abs() < 1e-6);
        assert!((pl.radius - 0.4).abs() < 1e-6);
        assert!(pl.shadow_maps_enabled);
        assert!(pl.contact_shadows_enabled);
        assert!(!pl.affects_lightmapped_mesh_diffuse);

        let sl = app.world().get::<SpotLight>(spot_ent).unwrap();
        assert!((sl.intensity - 4000.0).abs() < 1e-6);
        assert!((sl.outer_angle - 0.9).abs() < 1e-6);
        assert!((sl.inner_angle - 0.45).abs() < 1e-6);
        assert!(sl.shadow_maps_enabled);

        let dl = app.world().get::<DirectionalLight>(dir_ent).unwrap();
        assert!((dl.illuminance - 32_000.0).abs() < 1e-6);
        assert!(dl.shadow_maps_enabled);
        assert!(dl.contact_shadows_enabled);

        // Guest mutations push to Bevy; NaN values are safely ignored without corrupting Bevy state.
        {
            let mut ffi_pl = app.world_mut().get_mut::<FfiPointLight>(point_ent).unwrap();
            ffi_pl.intensity = 9000.0;
            ffi_pl.range = f32::NAN; // ignored!
            ffi_pl.shadow_maps_enabled = FfiBool::FALSE;

            let mut ffi_sl = app.world_mut().get_mut::<FfiSpotLight>(spot_ent).unwrap();
            ffi_sl.inner_angle = 0.6;
            ffi_sl.outer_angle = 1.1;

            let mut ffi_dl = app
                .world_mut()
                .get_mut::<FfiDirectionalLight>(dir_ent)
                .unwrap();
            ffi_dl.illuminance = 50_000.0;
            ffi_dl.contact_shadows_enabled = FfiBool::FALSE;
        }
        app.update();

        let pl = app.world().get::<PointLight>(point_ent).unwrap();
        assert!((pl.intensity - 9000.0).abs() < 1e-6);
        assert!((pl.range - 35.0).abs() < 1e-6, "NaN range must be rejected");
        assert!(!pl.shadow_maps_enabled);

        let sl = app.world().get::<SpotLight>(spot_ent).unwrap();
        assert!((sl.inner_angle - 0.6).abs() < 1e-6);
        assert!((sl.outer_angle - 1.1).abs() < 1e-6);

        let dl = app.world().get::<DirectionalLight>(dir_ent).unwrap();
        assert!((dl.illuminance - 50_000.0).abs() < 1e-6);
        assert!(!dl.contact_shadows_enabled);

        // Host mutations pull back into Ffi* mirrors.
        {
            let mut pl = app.world_mut().get_mut::<PointLight>(point_ent).unwrap();
            pl.intensity = 1234.0;
            pl.color = Color::linear_rgb(0.0, 1.0, 0.0);

            let mut sl = app.world_mut().get_mut::<SpotLight>(spot_ent).unwrap();
            sl.range = 80.0;

            let mut dl = app
                .world_mut()
                .get_mut::<DirectionalLight>(dir_ent)
                .unwrap();
            dl.illuminance = 750.0;
            dl.affects_lightmapped_mesh_diffuse = false;
        }
        app.update();

        let ffi_pl = app.world().get::<FfiPointLight>(point_ent).unwrap();
        assert!((ffi_pl.intensity - 1234.0).abs() < 1e-6);
        assert!((ffi_pl.color[1] - 1.0).abs() < 1e-6);
        assert!((app.world().get::<FfiSpotLight>(spot_ent).unwrap().range - 80.0).abs() < 1e-6);
        let ffi_dl = app.world().get::<FfiDirectionalLight>(dir_ent).unwrap();
        assert!((ffi_dl.illuminance - 750.0).abs() < 1e-6);
        assert!(!ffi_dl.affects_lightmapped_mesh_diffuse.get());
    }

    #[test]
    fn guest_ffi_raw_spawn_and_mod_scoped_despawn_on_unload() {
        let mut app = setup_camera_light_app();

        let mod_a = ModId(11);
        let mod_b = ModId(12);
        let point_handle = app
            .world()
            .resource::<ModRegistry>()
            .get_by_name(CANONICAL_POINT_LIGHT_NAME)
            .unwrap()
            .handle;

        // Spawn a `FfiPointLight` through a real `GuestSystem` + `host_cmd_spawn` with raw POD bytes!
        let spawned_out: Arc<Mutex<Option<FfiEntity>>> = Arc::new(Mutex::new(None));
        struct RawSpawnState {
            handle: bevy_abi_mod_sys::BamComponent,
            out: Arc<Mutex<Option<FfiEntity>>>,
            did_spawn: bool,
        }
        let state = Arc::new(Mutex::new(RawSpawnState {
            handle: point_handle,
            out: Arc::clone(&spawned_out),
            did_spawn: false,
        }));

        unsafe extern "C" fn spawn_light_cb(ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
            let mut st = unsafe { &*(ud as *const Mutex<RawSpawnState>) }
                .lock()
                .unwrap();
            if st.did_spawn {
                return FfiResult::OK;
            }
            let mut cmds = core::ptr::null_mut();
            assert_eq!(unsafe { host_sys_commands(ctx, &mut cmds) }, FfiResult::OK);
            let light = FfiPointLight {
                intensity: 7777.0,
                range: 42.0,
                shadow_maps_enabled: FfiBool::TRUE,
                ..Default::default()
            };
            let comps = [st.handle];
            let data_ptrs = [(&light as *const FfiPointLight).cast::<u8>()];
            let mut ent = FfiEntity::NULL;
            let rc = unsafe {
                host_cmd_spawn(
                    cmds,
                    comps.as_ptr(),
                    data_ptrs.as_ptr(),
                    comps.len(),
                    &mut ent,
                )
            };
            assert_eq!(rc, FfiResult::OK);
            *st.out.lock().unwrap() = Some(ent);
            st.did_spawn = true;
            FfiResult::OK
        }

        let reg = app.world().resource::<ModRegistry>().clone();
        let guest_sys = GuestSystem::new(
            "spawn_ffi_point_light",
            vec![BamQuerySpecOwned::new(vec![])],
            spawn_light_cb,
            Arc::as_ptr(&state) as *mut c_void,
            reg,
        )
        .with_mod_id(mod_a);
        app.add_systems(Update, guest_sys);

        // Additional mod_a entities, a mod_b entity, and a host-persistent entity.
        let mod_a_cam2d = app
            .world_mut()
            .spawn((FfiCamera2d::default(), ModScoped(mod_a)))
            .id();
        let mod_a_cam3d = app
            .world_mut()
            .spawn((
                FfiCamera3d {
                    transmission_enabled: FfiBool::TRUE,
                    ..Default::default()
                },
                ModScoped(mod_a),
            ))
            .id();
        let mod_a_spot = app
            .world_mut()
            .spawn((FfiSpotLight::default(), ModScoped(mod_a)))
            .id();
        let mod_a_dir = app
            .world_mut()
            .spawn((FfiDirectionalLight::default(), ModScoped(mod_a)))
            .id();
        let mod_b_light = app
            .world_mut()
            .spawn((FfiPointLight::default(), ModScoped(mod_b)))
            .id();
        let host_cam3d = app.world_mut().spawn(FfiCamera3d::default()).id();

        app.update();

        let ffi_spawned_ent = Entity::from_bits(
            spawned_out
                .lock()
                .unwrap()
                .expect("guest spawned entity")
                .to_bits(),
        );
        assert_eq!(
            app.world().get::<ModScoped>(ffi_spawned_ent),
            Some(&ModScoped(mod_a))
        );
        let synced_pl = app
            .world()
            .get::<PointLight>(ffi_spawned_ent)
            .expect("PointLight synced onto guest-spawned entity");
        assert!((synced_pl.intensity - 7777.0).abs() < 1e-6);
        assert!(synced_pl.shadow_maps_enabled);

        assert!(app.world().get::<Camera2d>(mod_a_cam2d).is_some());
        assert!(app.world().get::<Camera3d>(mod_a_cam3d).is_some());
        assert!(
            app.world()
                .get::<ScreenSpaceTransmission>(mod_a_cam3d)
                .is_some()
        );
        assert!(app.world().get::<SpotLight>(mod_a_spot).is_some());
        assert!(app.world().get::<DirectionalLight>(mod_a_dir).is_some());
        assert!(app.world().get::<PointLight>(mod_b_light).is_some());
        assert!(app.world().get::<Camera3d>(host_cam3d).is_some());

        // Unload mod_a -> all ModScoped(mod_a) entities despawn; mod_b and host survive!
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_a);

        for gone in [
            ffi_spawned_ent,
            mod_a_cam2d,
            mod_a_cam3d,
            mod_a_spot,
            mod_a_dir,
        ] {
            assert!(
                app.world().get_entity(gone).is_err(),
                "ModScoped(mod_a) entity {gone:?} must be despawned on unload"
            );
        }
        assert!(app.world().get::<PointLight>(mod_b_light).is_some());
        assert!(app.world().get::<Camera3d>(host_cam3d).is_some());
    }
}

#[cfg(feature = "bridge-camera-controller")]
mod pan_orbit_camera_tests {
    use bevy_abi_mod_bridges::camera_light::{
        CANONICAL_PAN_ORBIT_CAMERA_NAME, FfiPanOrbitCamera, ModCameraLightBridgePlugin,
        ORBIT_CONSTRAINT_FIXED, ORBIT_CONSTRAINT_FREE,
    };
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::loader::cleanup_orphaned_mod_scoped_entities;
    use bevy_abi_mod_sys::FfiBool;
    use bevy_app::App;
    use bevy_camera_controller::pan_orbit_camera::controller::component::{
        OrbitConstraint, PanOrbitCamera,
    };
    use bevy_math::DVec3;

    #[test]
    fn pan_orbit_camera_spawn_runtime_sync_and_mod_scoped_despawn() {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, ModCameraLightBridgePlugin));

        {
            let reg = app.world().resource::<ModRegistry>();
            let entry = reg
                .get_by_name(CANONICAL_PAN_ORBIT_CAMERA_NAME)
                .expect("FfiPanOrbitCamera registered");
            assert!(entry.is_abi_pod);
            assert!(entry.insert_raw_fn.is_some());
            assert!(entry.insert_default_fn.is_some());
            assert_eq!(entry.size, 136);
            assert_eq!(entry.align, 8);
        }

        let mod_id = ModId(21);
        let mod_cam = app
            .world_mut()
            .spawn((
                FfiPanOrbitCamera {
                    last_anchor_depth: -10.0,
                    orbit_constraint_mode: ORBIT_CONSTRAINT_FIXED,
                    orbit_up: [0.0, 0.0, 1.0],
                    orbit_can_pass_tdc: FfiBool::TRUE,
                    enabled_pan: FfiBool::FALSE,
                    enabled_orbit: FfiBool::TRUE,
                    enabled_zoom: FfiBool::TRUE,
                    zoom_through_objects: FfiBool::TRUE,
                    sensitivity_orbit: [1.5, 2.0],
                    sensitivity_zoom: 0.75,
                    smoothing_orbit_secs: 0.05,
                    momentum_orbit_damping: 200,
                    ..Default::default()
                },
                ModScoped(mod_id),
            ))
            .id();
        let host_cam = app.world_mut().spawn(FfiPanOrbitCamera::default()).id();

        app.update();

        let cam = app
            .world()
            .get::<PanOrbitCamera>(mod_cam)
            .expect("PanOrbitCamera spawned");
        assert!((cam.last_anchor_depth - (-10.0)).abs() < 1e-9);
        assert!(!cam.enabled_motion.pan);
        assert!(cam.enabled_motion.orbit);
        assert!(cam.zoom_limits.zoom_through_objects);
        assert!((cam.sensitivity.orbit.x - 1.5).abs() < 1e-6);
        assert!((cam.sensitivity.orbit.y - 2.0).abs() < 1e-6);
        assert!((cam.sensitivity.zoom - 0.75).abs() < 1e-6);
        assert_eq!(cam.momentum.orbit_damping, 200);
        match cam.orbit_constraint {
            OrbitConstraint::Fixed { up, can_pass_tdc } => {
                assert!((up - DVec3::Z).length() < 1e-9);
                assert!(can_pass_tdc);
            }
            OrbitConstraint::Free => panic!("expected OrbitConstraint::Fixed"),
        }

        // Start an active orbit motion on host, then mutate FfiPanOrbitCamera runtime parameters;
        // verify config updates while active motion state is preserved!
        app.world_mut()
            .get_mut::<PanOrbitCamera>(mod_cam)
            .unwrap()
            .start_orbit(None);
        assert!(
            app.world()
                .get::<PanOrbitCamera>(mod_cam)
                .unwrap()
                .is_actively_controlled()
        );

        {
            let mut ffi = app
                .world_mut()
                .get_mut::<FfiPanOrbitCamera>(mod_cam)
                .unwrap();
            ffi.orbit_constraint_mode = ORBIT_CONSTRAINT_FREE;
            ffi.enabled_pan = FfiBool::TRUE;
            ffi.last_anchor_depth = -4.5;
        }
        app.update();

        let cam = app.world().get::<PanOrbitCamera>(mod_cam).unwrap();
        assert!(matches!(cam.orbit_constraint, OrbitConstraint::Free));
        assert!(cam.enabled_motion.pan);
        assert!((cam.last_anchor_depth - (-4.5)).abs() < 1e-9);
        assert!(
            cam.is_actively_controlled(),
            "runtime parameter sync must preserve PanOrbitCamera::current_motion"
        );

        // Host mutation pulls back into FfiPanOrbitCamera.
        {
            let mut cam = app.world_mut().get_mut::<PanOrbitCamera>(mod_cam).unwrap();
            cam.sensitivity.zoom = 2.5;
            cam.zoom_limits.zoom_through_objects = false;
            cam.last_anchor_depth = -7.25;
        }
        app.update();

        let ffi = app.world().get::<FfiPanOrbitCamera>(mod_cam).unwrap();
        assert!((ffi.sensitivity_zoom - 2.5).abs() < 1e-6);
        assert!(!ffi.zoom_through_objects.get());
        assert!((ffi.last_anchor_depth - (-7.25)).abs() < 1e-9);

        // Unload despawns mod_cam while preserving host_cam.
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_id);
        assert!(app.world().get_entity(mod_cam).is_err());
        assert!(app.world().get::<PanOrbitCamera>(host_cam).is_some());
    }
}

#[cfg(feature = "bridge-solari")]
mod solari_lighting_tests {
    use bevy_abi_mod_bridges::camera_light::{
        CANONICAL_SOLARI_LIGHTING_NAME, FfiSolariLighting, ModCameraLightBridgePlugin,
    };
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::loader::cleanup_orphaned_mod_scoped_entities;
    use bevy_abi_mod_sys::FfiBool;
    use bevy_app::App;
    use bevy_solari::realtime::SolariLighting;

    #[test]
    fn solari_lighting_spawn_runtime_sync_reset_toggle_and_mod_scoped_despawn() {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, ModCameraLightBridgePlugin));

        {
            let reg = app.world().resource::<ModRegistry>();
            let entry = reg
                .get_by_name(CANONICAL_SOLARI_LIGHTING_NAME)
                .expect("FfiSolariLighting registered");
            assert!(entry.is_abi_pod);
            assert!(entry.insert_raw_fn.is_some());
            assert!(entry.insert_default_fn.is_some());
            assert_eq!(entry.size, 44);
            assert_eq!(entry.align, 4);
        }

        let mod_id = ModId(31);
        let mod_solari = app
            .world_mut()
            .spawn((
                FfiSolariLighting {
                    confidence_weight_cap: 16.0,
                    primary_di_samples: 16,
                    secondary_di_samples: 8,
                    max_bounces: 5,
                    world_cache_max_temporal_samples: 64.0,
                    world_cache_direct_light_sample_count: 48,
                    world_cache_max_gi_ray_distance: 120.0,
                    world_cache_cell_updates_soft_target: 80_000,
                    world_cache_position_base_cell_size: 0.25,
                    world_cache_position_lod_scale: 20.0,
                    restir: FfiBool::TRUE,
                    reset: FfiBool::TRUE,
                    reserved0: [0; 2],
                },
                ModScoped(mod_id),
            ))
            .id();
        let host_solari = app.world_mut().spawn(FfiSolariLighting::default()).id();

        app.update();

        let sol = app
            .world()
            .get::<SolariLighting>(mod_solari)
            .expect("SolariLighting spawned");
        assert!(sol.restir);
        assert!(sol.reset);
        assert!((sol.confidence_weight_cap - 16.0).abs() < 1e-6);
        assert_eq!(sol.primary_di_samples, 16);
        assert_eq!(sol.secondary_di_samples, 8);
        assert_eq!(sol.max_bounces, 5);
        assert!((sol.world_cache_max_temporal_samples - 64.0).abs() < 1e-6);
        assert_eq!(sol.world_cache_direct_light_sample_count, 48);
        assert!((sol.world_cache_max_gi_ray_distance - 120.0).abs() < 1e-6);
        assert_eq!(sol.world_cache_cell_updates_soft_target, 80_000);
        assert!((sol.world_cache_position_base_cell_size - 0.25).abs() < 1e-6);
        assert!((sol.world_cache_position_lod_scale - 20.0).abs() < 1e-6);

        // Simulate Bevy clearing `SolariLighting::reset` at end-of-frame -> pulls back to FfiSolariLighting.
        app.world_mut()
            .get_mut::<SolariLighting>(mod_solari)
            .unwrap()
            .reset = false;
        app.update();
        assert!(
            !app.world()
                .get::<FfiSolariLighting>(mod_solari)
                .unwrap()
                .reset
                .get()
        );

        // Guest triggers a camera-cut reset and adjusts ReSTIR / bounce knobs.
        {
            let mut ffi = app
                .world_mut()
                .get_mut::<FfiSolariLighting>(mod_solari)
                .unwrap();
            ffi.reset = FfiBool::TRUE;
            ffi.restir = FfiBool::FALSE;
            ffi.max_bounces = 2;
        }
        app.update();

        let sol = app.world().get::<SolariLighting>(mod_solari).unwrap();
        assert!(sol.reset);
        assert!(!sol.restir);
        assert_eq!(sol.max_bounces, 2);

        // Unload despawns mod_solari while preserving host_solari.
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_id);
        assert!(app.world().get_entity(mod_solari).is_err());
        assert!(app.world().get::<SolariLighting>(host_solari).is_some());
    }
}

#[cfg(feature = "bridge-audio")]
mod audio_bridge_tests {
    use bevy_abi_mod_bridges::audio::{
        AUDIO_SOURCE_ASSET_ALIAS, CANONICAL_AUDIO_PLAYBACK_SETTINGS_NAME,
        CANONICAL_AUDIO_PLAYER_SPEC_NAME, CANONICAL_SPATIAL_AUDIO_SINK_CONTROL_NAME,
        FfiAudioPlaybackSettings, FfiAudioPlayerSpec, FfiSpatialAudioSinkControl, MUTE_CMD_NONE,
        ModAudioBridgePlugin, PLAYBACK_MODE_DESPAWN, PLAYBACK_MODE_LOOP, PLAYBACK_MODE_ONCE,
        PLAYBACK_MODE_REMOVE, SINK_CMD_NONE, VOLUME_DECIBELS, VOLUME_LINEAR,
        audio_source_handle_to_ffi,
    };
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::loader::cleanup_orphaned_mod_scoped_entities;
    use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiBool};
    use bevy_app::App;
    use bevy_asset::Assets;
    use bevy_audio::{
        AudioPlayer, AudioSink, AudioSinkPlayback, AudioSource, PlaybackMode, PlaybackSettings,
        SpatialAudioSink, Volume,
    };
    use core::num::NonZero;
    use core::time::Duration;
    use rodio::{Player, Source, SpatialPlayer, source::SineWave};
    use std::sync::Arc;

    fn setup_audio_app() -> App {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, ModAudioBridgePlugin));
        app
    }

    #[test]
    fn audio_playback_settings_bidirectional_sync() {
        let mut app = setup_audio_app();

        {
            let reg = app.world().resource::<ModRegistry>();
            for (name, size) in [
                (CANONICAL_AUDIO_PLAYBACK_SETTINGS_NAME, 36usize),
                (CANONICAL_SPATIAL_AUDIO_SINK_CONTROL_NAME, 56usize),
                (CANONICAL_AUDIO_PLAYER_SPEC_NAME, 68usize),
            ] {
                let entry = reg.get_by_name(name).expect("audio component registered");
                assert!(entry.is_abi_pod);
                assert!(entry.insert_raw_fn.is_some());
                assert!(entry.insert_default_fn.is_some());
                assert_eq!(entry.size, size);
                assert_eq!(entry.align, 4);
            }
            assert!(reg.get_by_name(AUDIO_SOURCE_ASSET_ALIAS).is_some());
        }

        let ent = app
            .world_mut()
            .spawn(FfiAudioPlaybackSettings {
                volume: -6.0,
                speed: 1.25,
                spatial_scale: [0.5, 0.5, 0.25],
                start_position_secs: 1.5,
                duration_secs: 4.0,
                mode: PLAYBACK_MODE_LOOP,
                volume_mode: VOLUME_DECIBELS,
                paused: FfiBool::TRUE,
                muted: FfiBool::TRUE,
                spatial: FfiBool::TRUE,
                has_spatial_scale: FfiBool::TRUE,
                has_start_position: FfiBool::TRUE,
                has_duration: FfiBool::TRUE,
            })
            .id();

        app.update();

        let ps = app
            .world()
            .get::<PlaybackSettings>(ent)
            .expect("PlaybackSettings spawned");
        assert!(matches!(ps.mode, PlaybackMode::Loop));
        assert_eq!(ps.volume, Volume::Decibels(-6.0));
        assert!((ps.speed - 1.25).abs() < 1e-6);
        assert!(ps.paused);
        assert!(ps.muted);
        assert!(ps.spatial);
        let scale = ps.spatial_scale.expect("spatial_scale set").0;
        assert!((scale.x - 0.5).abs() < 1e-6);
        assert!((scale.z - 0.25).abs() < 1e-6);
        assert!((ps.start_position.unwrap().as_secs_f32() - 1.5).abs() < 1e-4);
        assert!((ps.duration.unwrap().as_secs_f32() - 4.0).abs() < 1e-4);

        // Guest mutates settings -> pushes to Bevy PlaybackSettings.
        {
            let mut ffi = app
                .world_mut()
                .get_mut::<FfiAudioPlaybackSettings>(ent)
                .unwrap();
            ffi.mode = PLAYBACK_MODE_DESPAWN;
            ffi.volume_mode = VOLUME_LINEAR;
            ffi.volume = 0.4;
            ffi.has_spatial_scale = FfiBool::FALSE;
            ffi.paused = FfiBool::FALSE;
        }
        app.update();

        let ps = app.world().get::<PlaybackSettings>(ent).unwrap();
        assert!(matches!(ps.mode, PlaybackMode::Despawn));
        assert_eq!(ps.volume, Volume::Linear(0.4));
        assert!(ps.spatial_scale.is_none());
        assert!(!ps.paused);

        // Host mutates PlaybackSettings -> pulls back into FfiAudioPlaybackSettings.
        *app.world_mut().get_mut::<PlaybackSettings>(ent).unwrap() = PlaybackSettings::REMOVE
            .with_speed(0.8)
            .with_volume(Volume::Linear(0.75));
        app.update();

        let ffi = app.world().get::<FfiAudioPlaybackSettings>(ent).unwrap();
        assert_eq!(ffi.mode, PLAYBACK_MODE_REMOVE);
        assert_eq!(ffi.volume_mode, VOLUME_LINEAR);
        assert!((ffi.volume - 0.75).abs() < 1e-6);
        assert!((ffi.speed - 0.8).abs() < 1e-6);
    }

    #[test]
    fn audio_player_spec_handles_updates_and_mod_scoped_despawn() {
        let mut app = setup_audio_app();

        let audio_type = app
            .world()
            .resource::<ModRegistry>()
            .get_by_name(AUDIO_SOURCE_ASSET_ALIAS)
            .expect("AudioSource registered")
            .handle;

        let (handle_a, handle_b) = {
            let mut assets = app.world_mut().resource_mut::<Assets<AudioSource>>();
            let a = assets.add(AudioSource {
                bytes: Arc::from([1u8, 2, 3, 4]),
            });
            let b = assets.add(AudioSource {
                bytes: Arc::from([5u8, 6, 7, 8]),
            });
            (a, b)
        };
        let ffi_a = audio_source_handle_to_ffi(audio_type, &handle_a);
        let ffi_b = audio_source_handle_to_ffi(audio_type, &handle_b);
        let ffi_uuid = FfiAssetHandle::from_uuid(
            audio_type,
            ComponentUuid::from_canonical_name("audio_test::synthetic_clip"),
        );

        let mod_id = ModId(41);
        let spatial_settings = FfiAudioPlaybackSettings {
            mode: PLAYBACK_MODE_LOOP,
            spatial: FfiBool::TRUE,
            volume: 0.6,
            ..Default::default()
        };

        let mod_audio_ent = app
            .world_mut()
            .spawn((
                FfiAudioPlayerSpec::new(ffi_a, spatial_settings),
                ModScoped(mod_id),
            ))
            .id();
        let uuid_audio_ent = app
            .world_mut()
            .spawn((
                FfiAudioPlayerSpec::new(ffi_uuid, FfiAudioPlaybackSettings::default()),
                ModScoped(mod_id),
            ))
            .id();
        let mismatched_type_ent = app
            .world_mut()
            .spawn(FfiAudioPlayerSpec::new(
                FfiAssetHandle::from_index(BamComponent::from_raw(999), 0, 0),
                FfiAudioPlaybackSettings::default(),
            ))
            .id();
        let host_audio_ent = app
            .world_mut()
            .spawn(FfiAudioPlayerSpec::new(ffi_a, spatial_settings))
            .id();

        app.update();

        let player = app
            .world()
            .get::<AudioPlayer<AudioSource>>(mod_audio_ent)
            .expect("AudioPlayer attached");
        assert_eq!(player.0.id(), handle_a.id());
        let ps = app.world().get::<PlaybackSettings>(mod_audio_ent).unwrap();
        assert!(ps.spatial);
        assert!(matches!(ps.mode, PlaybackMode::Loop));
        assert_eq!(ps.volume, Volume::Linear(0.6));

        // UUID-backed handle also resolves cleanly.
        assert!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(uuid_audio_ent)
                .is_some()
        );
        // Mismatched asset_type is rejected cleanly without attaching AudioPlayer.
        assert!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(mismatched_type_ent)
                .is_none()
        );

        // Guest switches clip handle and updates settings.
        {
            let mut spec = app
                .world_mut()
                .get_mut::<FfiAudioPlayerSpec>(mod_audio_ent)
                .unwrap();
            spec.source = ffi_b;
            spec.settings.mode = PLAYBACK_MODE_ONCE;
            spec.settings.volume = 0.9;
        }
        app.update();

        assert_eq!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(mod_audio_ent)
                .unwrap()
                .0
                .id(),
            handle_b.id()
        );
        let ps = app.world().get::<PlaybackSettings>(mod_audio_ent).unwrap();
        assert!(matches!(ps.mode, PlaybackMode::Once));
        assert_eq!(ps.volume, Volume::Linear(0.9));

        // Clearing source to NONE removes AudioPlayer<AudioSource>.
        app.world_mut()
            .get_mut::<FfiAudioPlayerSpec>(uuid_audio_ent)
            .unwrap()
            .source = FfiAssetHandle::NONE;
        app.update();
        assert!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(uuid_audio_ent)
                .is_none()
        );

        // Unload despawns all ModScoped audio entities; host audio entity survives.
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_id);
        assert!(app.world().get_entity(mod_audio_ent).is_err());
        assert!(app.world().get_entity(uuid_audio_ent).is_err());
        assert!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(host_audio_ent)
                .is_some()
        );
    }

    #[test]
    fn spatial_and_regular_audio_sink_control_headless() {
        let mut app = setup_audio_app();
        let mod_id = ModId(51);

        // Build pure-Rust headless SpatialAudioSink + AudioSink without any OS audio device.
        let (mixer, _mixer_output) =
            rodio::mixer::mixer(NonZero::new(2).unwrap(), NonZero::new(44_100).unwrap());
        let spatial_player =
            SpatialPlayer::connect_new(&mixer, [0.0, 0.0, 0.0], [-2.0, 0.0, 0.0], [2.0, 0.0, 0.0]);
        spatial_player.append(SineWave::new(440.0).take_duration(Duration::from_secs(5)));
        let spatial_sink = SpatialAudioSink::new(spatial_player);

        let (regular_player, mut queue_out) = Player::new();
        regular_player.append(SineWave::new(220.0).take_duration(Duration::from_secs(5)));
        let regular_sink = AudioSink::new(regular_player);

        let spatial_ent = app
            .world_mut()
            .spawn((
                spatial_sink,
                FfiSpatialAudioSinkControl::default(),
                ModScoped(mod_id),
            ))
            .id();
        let regular_ent = app
            .world_mut()
            .spawn((
                regular_sink,
                FfiSpatialAudioSinkControl::default(),
                ModScoped(mod_id),
            ))
            .id();

        // Initial pull populates telemetry (`is_spatial`, `is_empty`, `volume`, `speed`).
        app.update();
        let sc = app
            .world()
            .get::<FfiSpatialAudioSinkControl>(spatial_ent)
            .unwrap();
        assert!(sc.is_spatial.get());
        assert!(!sc.is_paused.get());
        assert!(!sc.is_muted.get());
        assert!(!sc.is_empty.get());
        assert!((sc.volume - 1.0).abs() < 1e-6);
        assert!((sc.speed - 1.0).abs() < 1e-6);

        let rc = app
            .world()
            .get::<FfiSpatialAudioSinkControl>(regular_ent)
            .unwrap();
        assert!(!rc.is_spatial.get());
        assert!(!rc.is_empty.get());

        // Issue pause + volume + speed + mute + spatial emitter/ear updates on SpatialAudioSink.
        {
            let mut ctrl = app
                .world_mut()
                .get_mut::<FfiSpatialAudioSinkControl>(spatial_ent)
                .unwrap();
            ctrl.request_pause();
            ctrl.set_linear_volume(0.35);
            ctrl.set_speed(1.75);
            ctrl.request_mute();
            ctrl.set_emitter_position([3.0, 1.0, -4.0]);
            ctrl.set_ear_positions([-1.5, 0.0, 0.0], [1.5, 0.0, 0.0]);
        }
        app.update();

        let sink = app.world().get::<SpatialAudioSink>(spatial_ent).unwrap();
        assert!(sink.is_paused());
        assert!(sink.is_muted());
        assert!((sink.volume().to_linear() - 0.35).abs() < 1e-5);
        assert!((sink.speed() - 1.75).abs() < 1e-5);

        let ctrl = app
            .world()
            .get::<FfiSpatialAudioSinkControl>(spatial_ent)
            .unwrap();
        assert_eq!(ctrl.playback_command, SINK_CMD_NONE);
        assert_eq!(ctrl.mute_command, MUTE_CMD_NONE);
        assert_eq!(ctrl.apply_flags, 0);
        assert!(ctrl.is_paused.get());
        assert!(ctrl.is_muted.get());
        assert!((ctrl.volume - 0.35).abs() < 1e-5);
        assert!((ctrl.speed - 1.75).abs() < 1e-5);

        // Adjust decibel volume while muted, then unmute & resume playback.
        {
            let mut ctrl = app
                .world_mut()
                .get_mut::<FfiSpatialAudioSinkControl>(spatial_ent)
                .unwrap();
            ctrl.set_decibel_volume(-6.0);
            ctrl.request_unmute();
            ctrl.request_play();
        }
        app.update();

        let sink = app.world().get::<SpatialAudioSink>(spatial_ent).unwrap();
        assert!(!sink.is_paused());
        assert!(!sink.is_muted());
        assert!((sink.volume().to_decibels() - (-6.0)).abs() < 1e-3);

        // Toggle playback & toggle mute + stop on regular AudioSink.
        {
            let mut ctrl = app
                .world_mut()
                .get_mut::<FfiSpatialAudioSinkControl>(regular_ent)
                .unwrap();
            ctrl.request_toggle_playback();
            ctrl.request_toggle_mute();
            ctrl.set_speed(0.5);
        }
        app.update();
        {
            let sink = app.world().get::<AudioSink>(regular_ent).unwrap();
            assert!(sink.is_paused());
            assert!(sink.is_muted());
            assert!((sink.speed() - 0.5).abs() < 1e-5);
        }

        // Host direct mutation pulls back into FfiSpatialAudioSinkControl.
        {
            let mut sink = app.world_mut().get_mut::<AudioSink>(regular_ent).unwrap();
            sink.unmute();
            sink.play();
            sink.set_volume(Volume::Linear(0.25));
        }
        app.update();
        let ctrl = app
            .world()
            .get::<FfiSpatialAudioSinkControl>(regular_ent)
            .unwrap();
        assert!(!ctrl.is_paused.get());
        assert!(!ctrl.is_muted.get());
        assert!((ctrl.volume - 0.25).abs() < 1e-5);

        // Stop command stops regular sink; stepping the headless audio consumer past rodio's
        // 5ms periodic_access window drains the stopped source.
        app.world_mut()
            .get_mut::<FfiSpatialAudioSinkControl>(regular_ent)
            .unwrap()
            .request_stop();
        app.update();
        for _ in 0..512 {
            let _ = queue_out.next();
        }
        app.update();
        assert!(app.world().get::<AudioSink>(regular_ent).unwrap().empty());
        assert!(
            app.world()
                .get::<FfiSpatialAudioSinkControl>(regular_ent)
                .unwrap()
                .is_empty
                .get()
        );

        // Mod unload cleanly despawns both spatial and regular sink entities.
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_id);
        assert!(app.world().get_entity(spatial_ent).is_err());
        assert!(app.world().get_entity(regular_ent).is_err());
    }
}

#[cfg(all(
    feature = "bridge-light",
    feature = "bridge-camera-controller",
    feature = "bridge-solari",
    feature = "bridge-audio"
))]
mod combined_camera_light_audio_tests {
    use bevy_abi_mod_bridges::audio::{
        AUDIO_SOURCE_ASSET_ALIAS, FfiAudioPlaybackSettings, FfiAudioPlayerSpec,
        FfiSpatialAudioSinkControl, ModAudioBridgePlugin, PLAYBACK_MODE_LOOP,
        audio_source_handle_to_ffi,
    };
    use bevy_abi_mod_bridges::camera_light::{
        FfiCamera3d, FfiDirectionalLight, FfiPanOrbitCamera, FfiPointLight, FfiSolariLighting,
        ModCameraLightBridgePlugin, TONEMAPPING_AGX, TRANSMISSION_QUALITY_HIGH,
    };
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::loader::cleanup_orphaned_mod_scoped_entities;
    use bevy_abi_mod_sys::FfiBool;
    use bevy_app::App;
    use bevy_asset::Assets;
    use bevy_audio::{AudioPlayer, AudioSource, PlaybackSettings};
    use bevy_camera::{Camera3d, Hdr};
    use bevy_camera_controller::pan_orbit_camera::controller::component::PanOrbitCamera;
    use bevy_light::{DirectionalLight, PointLight};
    use bevy_pbr::ScreenSpaceTransmission;
    use bevy_render::view::{DebandDither, Tonemapping};
    use bevy_solari::realtime::SolariLighting;
    use std::sync::Arc;

    #[test]
    fn full_mod_scene_camera_solari_lights_and_spatial_audio_lifecycle() {
        let mut app = App::new();
        app.add_plugins((
            AbiModPlugin,
            ModCameraLightBridgePlugin,
            ModAudioBridgePlugin,
        ));

        let audio_type = app
            .world()
            .resource::<ModRegistry>()
            .get_by_name(AUDIO_SOURCE_ASSET_ALIAS)
            .unwrap()
            .handle;
        let clip_handle = app
            .world_mut()
            .resource_mut::<Assets<AudioSource>>()
            .add(AudioSource {
                bytes: Arc::from([0u8; 16]),
            });
        let ffi_clip = audio_source_handle_to_ffi(audio_type, &clip_handle);

        let mod_id = ModId(77);
        let rig_ent = app
            .world_mut()
            .spawn((
                FfiCamera3d {
                    hdr: FfiBool::TRUE,
                    tonemapping: TONEMAPPING_AGX,
                    deband_dither: FfiBool::TRUE,
                    transmission_enabled: FfiBool::TRUE,
                    transmission_steps: 2,
                    transmission_quality: TRANSMISSION_QUALITY_HIGH,
                    ..Default::default()
                },
                FfiPanOrbitCamera {
                    last_anchor_depth: -8.0,
                    ..Default::default()
                },
                FfiSolariLighting {
                    restir: FfiBool::TRUE,
                    max_bounces: 4,
                    ..Default::default()
                },
                ModScoped(mod_id),
            ))
            .id();
        let sun_ent = app
            .world_mut()
            .spawn((FfiDirectionalLight::default(), ModScoped(mod_id)))
            .id();
        let lamp_ent = app
            .world_mut()
            .spawn((FfiPointLight::default(), ModScoped(mod_id)))
            .id();
        let emitter_ent = app
            .world_mut()
            .spawn((
                FfiAudioPlayerSpec::new(
                    ffi_clip,
                    FfiAudioPlaybackSettings {
                        mode: PLAYBACK_MODE_LOOP,
                        spatial: FfiBool::TRUE,
                        ..Default::default()
                    },
                ),
                FfiSpatialAudioSinkControl::default(),
                ModScoped(mod_id),
            ))
            .id();
        let host_camera = app.world_mut().spawn(FfiCamera3d::default()).id();

        app.update();

        // Verify all bridged Bevy components materialized cleanly on the single camera rig + scene entities.
        assert!(app.world().get::<Camera3d>(rig_ent).is_some());
        assert!(app.world().get::<Hdr>(rig_ent).is_some());
        assert_eq!(
            app.world().get::<Tonemapping>(rig_ent),
            Some(&Tonemapping::AgX)
        );
        assert_eq!(
            app.world().get::<DebandDither>(rig_ent),
            Some(&DebandDither::Enabled)
        );
        assert!(
            app.world()
                .get::<ScreenSpaceTransmission>(rig_ent)
                .is_some()
        );
        assert!(app.world().get::<PanOrbitCamera>(rig_ent).is_some());
        assert!(app.world().get::<SolariLighting>(rig_ent).unwrap().restir);
        assert!(app.world().get::<DirectionalLight>(sun_ent).is_some());
        assert!(app.world().get::<PointLight>(lamp_ent).is_some());
        assert!(
            app.world()
                .get::<AudioPlayer<AudioSource>>(emitter_ent)
                .is_some()
        );
        assert!(
            app.world()
                .get::<PlaybackSettings>(emitter_ent)
                .unwrap()
                .spatial
        );

        // Mod unload cleans up the entire mod rig, lights, and audio emitter while keeping host camera.
        cleanup_orphaned_mod_scoped_entities(app.world_mut(), mod_id);
        for gone in [rig_ent, sun_ent, lamp_ent, emitter_ent] {
            assert!(app.world().get_entity(gone).is_err());
        }
        assert!(app.world().get::<Camera3d>(host_camera).is_some());
    }
}
