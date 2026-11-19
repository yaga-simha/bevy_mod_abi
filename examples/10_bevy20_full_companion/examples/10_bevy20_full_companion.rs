#![forbid(unsafe_code)]

//! Runnable & CI-testable example target (`10_bevy20_full_companion`) showcasing all Bevy `0.20.1`
//! Phase 3 companion modding capabilities headlessly.
//!
//! Run directly:
//! ```bash
//! cargo run --manifest-path examples/10_bevy20_full_companion/Cargo.toml --example 10_bevy20_full_companion -- --headless
//! ```
//!
//! Run deterministic headless tests:
//! ```bash
//! cargo test --manifest-path examples/10_bevy20_full_companion/Cargo.toml --example 10_bevy20_full_companion
//! ```

use example_10_bevy20_full_companion::run_companion_demo;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.is_empty() || args.iter().any(|a| a == "--headless");

    let report = run_companion_demo(headless);
    println!(
        "10_bevy20_full_companion OK (headless={}, mods={}, systems={}, saved={}, recovered={}, stripped={})",
        report.headless,
        report.brp_total_mods,
        report.brp_tracked_systems,
        report.saved_persistent_entities,
        report.recovered_entities,
        report.stripped_mod_b_components,
    );
}

#[cfg(test)]
mod tests {
    use example_10_bevy20_full_companion::{MOD_B_PLASMA_AURA_NAME, run_companion_demo};

    #[test]
    fn full_companion_demo_runs_all_seven_phase3_capabilities_headlessly() {
        let report = run_companion_demo(true);

        assert!(report.headless);
        assert!(report.synced_point_light_intensity >= 2_775.0);
        assert_eq!(report.primary_beacon_after_events.press_count, 2);
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
}
