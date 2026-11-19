#![forbid(unsafe_code)]

//! Deterministic headless smoke and lifecycle integration tests for `10_bevy20_full_companion`.

use bevy_abi_mod::prelude::*;
use bevy_asset::Assets;
use bevy_render::storage::ShaderBuffer;
use bevy_shader::Shader;
use bevy_state::state::{NextState, State, States};
use example_10_bevy20_full_companion::{
    CompanionBeacon, MOD_B_PLASMA_AURA_NAME, build_companion_app,
    ensure_companion_guest_mods_built, run_companion_demo, spawn_companion_scene,
};

#[derive(States, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
enum CompanionPhase {
    #[default]
    Observing,
    Inspecting,
}

#[test]
fn companion_full_seven_subsystem_headless_showcase() {
    let report = run_companion_demo(true);

    assert!(report.headless);
    assert!(report.synced_point_light_intensity >= 2_775.0);
    assert_eq!(report.primary_beacon_after_events.press_count, 2);
    assert_eq!(report.primary_beacon_after_events.last_button, 1);
    assert_eq!(report.primary_beacon_after_events.ui_events, 1);
    assert_eq!(report.primary_beacon_after_events.active_tab_index, 1);
    assert_eq!(report.brp_total_mods, 2);
    assert!(report.brp_tracked_systems >= 5);
    assert_eq!(report.saved_persistent_entities, 3);
    assert_eq!(report.recovered_entities, 3);
    assert_eq!(report.stripped_mod_b_components, 3);
    assert_eq!(
        report.stripped_component_names,
        vec![MOD_B_PLASMA_AURA_NAME.to_owned()]
    );
    assert!(report.post_recovery_orbital_ticks >= 4);
}

#[test]
fn unloading_mod_a_cleans_up_mod_scoped_scene_and_wesl_assets_while_preserving_persistent_beacons()
{
    let mods = ensure_companion_guest_mods_built();
    let mut app = build_companion_app(true);

    let mod_a = app
        .load_mod_with(&mods.mod_a_path, LoadOptions::new().with_shadow_copy(true))
        .expect("load Mod A");
    let handles = spawn_companion_scene(&mut app, mod_a);

    app.update();
    app.update();

    // Unload Mod A: all ModScoped entities and mod-owned .wesl/buffer/material assets must purge,
    // while the 3 persistent host beacons remain alive.
    app.unload_mod(mod_a).expect("unload Mod A");
    app.update();

    for scoped_ent in [
        handles.camera_rig,
        handles.directional_light,
        handles.point_light,
        handles.spatial_audio_emitter,
        handles.hud_root,
        handles.hud_title,
        handles.hud_tablist,
        handles.tab_overview,
        handles.tab_telemetry,
        handles.unified_sprite,
        handles.extended_sprite,
    ] {
        assert!(
            app.world().get_entity(scoped_ent).is_err(),
            "ModScoped entity {scoped_ent:?} must despawn on mod unload"
        );
    }

    assert!(
        app.world()
            .resource::<Assets<Shader>>()
            .get(&handles.wesl_shader)
            .is_none(),
        ".wesl shader owned by Mod A must be removed from Assets<Shader>"
    );
    assert!(
        app.world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&handles.shader_buffer)
            .is_none(),
        "ShaderBuffer owned by Mod A must be removed from Assets<ShaderBuffer>"
    );
    assert!(
        app.world()
            .resource::<Assets<ModWeslSpriteExtension2d>>()
            .get(&handles.sprite_extension)
            .is_none(),
        "ModWeslSpriteExtension2d owned by Mod A must be removed"
    );
    assert!(
        app.world()
            .resource::<Assets<ModExtendedSpriteMaterial2d>>()
            .get(&handles.extended_material)
            .is_none(),
        "ModExtendedSpriteMaterial2d owned by Mod A must be removed"
    );

    for persistent_ent in handles.persistent_beacons {
        assert!(
            app.world().get::<CompanionBeacon>(persistent_ent).is_some(),
            "persistent host beacon {persistent_ent:?} must survive mod unload"
        );
    }
}

#[test]
fn companion_app_supports_exposed_mod_states_and_isolated_mod_assets() {
    let mut app = build_companion_app(true);
    let spec = app
        .expose_mod_state::<CompanionPhase>(
            ScheduleId::custom(1024),
            &[
                (0, "observing", CompanionPhase::Observing),
                (1, "inspecting", CompanionPhase::Inspecting),
            ],
        )
        .expect("expose CompanionPhase state");

    assert_eq!(spec.variant_count(), 2);
    app.update();
    assert_eq!(
        *app.world().resource::<State<CompanionPhase>>().get(),
        CompanionPhase::Observing
    );

    app.world_mut()
        .resource_mut::<NextState<CompanionPhase>>()
        .set(CompanionPhase::Inspecting);
    app.update();
    assert_eq!(
        *app.world().resource::<State<CompanionPhase>>().get(),
        CompanionPhase::Inspecting
    );
}
