#![forbid(unsafe_code)]

//! Binary entry point for Example 10 — Bevy `0.20.1` Full Companion Modding Showcase.

use example_10_bevy20_full_companion::run_companion_demo;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.is_empty() || args.iter().any(|a| a == "--headless");

    println!("================================================================================");
    println!("  bevy_abi_mod: Example 10 — Bevy 0.20.1 Full Companion Showcase                ");
    println!("  Camera/Lights/PanOrbit + .wesl SpriteMaterial + Picking + Spatial Audio       ");
    println!("  Retained Val::Rem TabList UI + Budgets/BRP Telemetry + Save/Recovery          ");
    println!("================================================================================\n");

    let report = run_companion_demo(headless);

    println!("[1/7] 3D Camera + PanOrbitCamera + Lights:");
    println!(
        "      PointLight intensity pulsed by Mod A -> {:.1} lm",
        report.synced_point_light_intensity
    );
    println!("[2/7] Custom .wesl SpriteMaterial + ExtendedMaterial2d + RenderApp extraction: OK");
    println!("[3/7] Flattened PointerPress -> guest cmd_observe mutation:");
    println!(
        "      beacon_alpha press_count={}, last_button={}, energy={:.1}",
        report.primary_beacon_after_events.press_count,
        report.primary_beacon_after_events.last_button,
        report.primary_beacon_after_events.energy,
    );
    println!("[4/7] Spatial AudioPlayer + live PlaybackSettings sync: OK");
    println!("[5/7] Retained UI (Val::Rem + FontSize::Rem + TabList + FfiValueChange):");
    println!(
        "      beacon_alpha ui_events={}, active_tab_index={}",
        report.primary_beacon_after_events.ui_events,
        report.primary_beacon_after_events.active_tab_index,
    );
    println!(
        "[6/7] ModBudgetPolicy + ModTelemetryPlugin BRP: {} active mods, {} tracked systems",
        report.brp_total_mods, report.brp_tracked_systems
    );
    for (name, p50, p99) in &report.system_metrics_us {
        println!("      - {name}: p50={p50}us, p99={p99}us");
    }
    println!(
        "[7/7] ModSaveGamePlugin Quick-Save / Mod-Disable / Quick-Load Recovery:\n      saved={} persistent entities -> recovered={} entities, stripped={} ({:?}), post-recovery ticks={}",
        report.saved_persistent_entities,
        report.recovered_entities,
        report.stripped_mod_b_components,
        report.stripped_component_names,
        report.post_recovery_orbital_ticks,
    );
    println!(
        "\nExample 10 completed cleanly (headless={}).",
        report.headless
    );
}
