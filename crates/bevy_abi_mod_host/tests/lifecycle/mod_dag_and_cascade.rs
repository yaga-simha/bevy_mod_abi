//! `mod.toml` SemVer validation, Kahn topological load ordering, cycle quarantine
//! (`CycleQuarantined`), and reverse-topological cascading `unload_mod` / `reload_mod`.

use crate::support::{TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::mod_dag::{
    ManifestError, ModDagError, ModDependencyGraph, ModTomlManifest, resolve_dependency_dag,
};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_sys::{ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH};
use bevy_app::{App, FixedUpdate, PostUpdate};
use bevy_ecs::component::Component;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::ReloadStateProbe")]
struct ReloadStateProbe {
    reload_epoch: u64,
    accumulated_ticks: u64,
    fail_next_save: u32,
    fail_next_restore: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::EnableInSetTest")]
struct EnableInSetTest;

struct TempModsDir {
    path: PathBuf,
}

impl TempModsDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bevy_abi_mod_dag_{}_{}_{}",
            tag,
            std::process::id(),
            id
        ));
        fs::create_dir_all(&path).expect("create temp mods dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write_mod(
        &self,
        dir_name: &str,
        toml_body: &str,
        source_dylib: Option<&Path>,
        lib_filename: &str,
    ) {
        let mod_dir = self.path.join(dir_name);
        fs::create_dir_all(&mod_dir).expect("create mod subdir");
        fs::write(mod_dir.join("mod.toml"), toml_body).expect("write mod.toml");
        if let Some(src) = source_dylib {
            fs::copy(src, mod_dir.join(lib_filename)).expect("copy dylib into mod dir");
        }
    }
}

impl Drop for TempModsDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn mod_toml_parses_fields_and_rejects_malformed_semver() {
    let valid = format!(
        r#"
[mod]
id = "gameplay_core"
name = "Gameplay Core"
version = "1.4.2"
abi_version = "^{ABI_VERSION_MAJOR}.{ABI_VERSION_MINOR}.{ABI_VERSION_PATCH}"
library = "libgameplay_core.dylib"

[dependencies]
math_lib = "^2.0"

[optional_dependencies]
debug_hud = ">=0.1.0, <1.0.0"
"#
    );

    let parsed = ModTomlManifest::parse_str(&valid, "mods/gameplay_core/mod.toml").unwrap();
    assert_eq!(parsed.id(), "gameplay_core");
    assert_eq!(parsed.name, "gameplay_core");
    assert_eq!(parsed.display_name.as_deref(), Some("Gameplay Core"));
    assert_eq!(parsed.version, semver::Version::new(1, 4, 2));
    assert_eq!(parsed.entry.as_deref(), Some("libgameplay_core.dylib"));
    assert_eq!(parsed.dependencies.len(), 1);
    assert_eq!(parsed.optional_dependencies.len(), 1);

    // Missing [mod].id and [mod].name
    let missing_name = r#"
[mod]
version = "1.0.0"
"#;
    assert!(matches!(
        ModTomlManifest::parse_str(missing_name, "bad/mod.toml"),
        Err(ManifestError::MissingModName { .. })
    ));

    // Invalid SemVer version
    let bad_ver = r#"
[mod]
name = "bad_ver"
version = "not-a-semver"
"#;
    assert!(matches!(
        ModTomlManifest::parse_str(bad_ver, "bad/mod.toml"),
        Err(ManifestError::InvalidVersion { .. })
    ));

    // Invalid dependency SemVer requirement
    let bad_dep_req = r#"
[mod]
name = "bad_dep"
version = "1.0.0"

[dependencies]
core = "===broken"
"#;
    assert!(matches!(
        ModTomlManifest::parse_str(bad_dep_req, "bad/mod.toml"),
        Err(ManifestError::InvalidDependencyReq { .. })
    ));
}

#[test]
fn kahn_topological_sort_validates_semver_and_quarantines_cycles_deterministically() {
    let host_abi = format!("^{ABI_VERSION_MAJOR}.{ABI_VERSION_MINOR}.{ABI_VERSION_PATCH}");
    let m_a = ModTomlManifest::parse_str(
        &format!(
            r#"
[mod]
name = "mod_a"
version = "1.2.0"
abi_version = "{host_abi}"
"#
        ),
        "a/mod.toml",
    )
    .unwrap();

    let m_b = ModTomlManifest::parse_str(
        r#"
[mod]
name = "mod_b"
version = "1.0.0"

[dependencies]
mod_a = "^1.0"

[optional_dependencies]
missing_opt = "^9.9"
"#,
        "b/mod.toml",
    )
    .unwrap();

    let m_c = ModTomlManifest::parse_str(
        r#"
[mod]
name = "mod_c"
version = "1.0.0"

[dependencies]
mod_a = "^1.0"
mod_b = "^1.0"
"#,
        "c/mod.toml",
    )
    .unwrap();

    let m_bad_ver = ModTomlManifest::parse_str(
        r#"
[mod]
name = "mod_bad_ver"
version = "0.1.0"

[dependencies]
mod_a = "^2.0"
"#,
        "bad_ver/mod.toml",
    )
    .unwrap();

    let m_missing_dep = ModTomlManifest::parse_str(
        r#"
[mod]
name = "mod_missing"
version = "0.1.0"

[dependencies]
nonexistent = "^1.0"
"#,
        "missing/mod.toml",
    )
    .unwrap();

    // Cycle: cyc_x <-> cyc_y, plus cyc_downstream depending on cyc_x.
    let m_cyc_x = ModTomlManifest::parse_str(
        r#"
[mod]
name = "cyc_x"
version = "1.0.0"

[dependencies]
cyc_y = "^1.0"
"#,
        "cyc_x/mod.toml",
    )
    .unwrap();

    let m_cyc_y = ModTomlManifest::parse_str(
        r#"
[mod]
name = "cyc_y"
version = "1.0.0"

[dependencies]
cyc_x = "^1.0"
"#,
        "cyc_y/mod.toml",
    )
    .unwrap();

    let m_cyc_downstream = ModTomlManifest::parse_str(
        r#"
[mod]
name = "cyc_downstream"
version = "1.0.0"

[dependencies]
cyc_x = "^1.0"
"#,
        "cyc_downstream/mod.toml",
    )
    .unwrap();

    let res = resolve_dependency_dag(
        vec![
            m_c,
            m_cyc_downstream,
            m_b,
            m_cyc_y,
            m_bad_ver,
            m_a,
            m_missing_dep,
            m_cyc_x,
        ],
        &BTreeMap::new(),
    );

    let ordered_names: Vec<&str> = res.load_order.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(ordered_names, vec!["mod_a", "mod_b", "mod_c"]);
    assert_eq!(
        res.quarantined_cycles,
        vec![
            "cyc_downstream".to_string(),
            "cyc_x".to_string(),
            "cyc_y".to_string()
        ]
    );
    assert!(res.diagnostics.iter().any(|d| matches!(
        d,
        ModDagError::IncompatibleDependencyVersion {
            mod_name,
            dependency,
            ..
        } if mod_name == "mod_bad_ver" && dependency == "mod_a"
    )));
    assert!(res.diagnostics.iter().any(|d| matches!(
        d,
        ModDagError::MissingDependency {
            mod_name,
            dependency,
            ..
        } if mod_name == "mod_missing" && dependency == "nonexistent"
    )));
    assert!(
        res.diagnostics
            .iter()
            .any(|d| matches!(d, ModDagError::CycleQuarantined { .. }))
    );
}

#[test]
fn load_mods_dir_orders_topologically_quarantines_cycles_and_cascades_reload_and_unload() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let state_dylib = TempDylibCopy::new(
        &fixture_dylib("state_hooks_and_observers_mod"),
        "dag_state_mod",
    );
    let ordered_dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "dag_ordered_mod");
    let c_dylib = TempDylibCopy::new(&fixture_dylib("asymmetric_c_mod"), "dag_c_mod");

    let mods_dir = TempModsDir::new("cascade");
    // Chain: mod_a (state_hooks_and_observers_mod) <- mod_b (ordered_sets_mod) <- mod_c (asymmetric_c_mod)
    mods_dir.write_mod(
        "03_mod_c",
        r#"
[mod]
id = "mod_c"
version = "1.0.0"
entry = "mod_c.dylib"

[dependencies]
mod_b = "^1.0"
"#,
        Some(c_dylib.path()),
        "mod_c.dylib",
    );
    mods_dir.write_mod(
        "02_mod_b",
        r#"
[mod]
id = "mod_b"
version = "1.2.0"
entry = "mod_b.dylib"

[dependencies]
mod_a = "^1.0"
"#,
        Some(ordered_dylib.path()),
        "mod_b.dylib",
    );
    mods_dir.write_mod(
        "01_mod_a",
        r#"
[mod]
id = "mod_a"
version = "1.0.0"
entry = "mod_a.dylib"
"#,
        Some(state_dylib.path()),
        "mod_a.dylib",
    );
    // Cyclic pair that must be quarantined without blocking `mod_a -> mod_b -> mod_c`!
    mods_dir.write_mod(
        "04_cyc_1",
        r#"
[mod]
id = "cyc_1"
version = "1.0.0"
entry = "cyc_1.dylib"

[dependencies]
cyc_2 = "^1.0"
"#,
        Some(ordered_dylib.path()),
        "cyc_1.dylib",
    );
    mods_dir.write_mod(
        "05_cyc_2",
        r#"
[mod]
id = "cyc_2"
version = "1.0.0"
entry = "cyc_2.dylib"

[dependencies]
cyc_1 = "^1.0"
"#,
        Some(ordered_dylib.path()),
        "cyc_2.dylib",
    );

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);
    app.insert_resource(ReloadStateProbe::default());
    app.register_mod_resource::<ReloadStateProbe>().unwrap();
    app.register_mod_component::<EnableInSetTest>(ComponentOptions::new().with_default())
        .unwrap();

    let report = app
        .load_mods_dir_with(mods_dir.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load_mods_dir_with succeeds");

    let loaded_names: Vec<&str> = report.loaded.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(loaded_names, vec!["mod_a", "mod_b", "mod_c"]);
    assert_eq!(
        report.quarantined_cycles,
        vec!["cyc_1".to_string(), "cyc_2".to_string()]
    );
    assert!(
        report
            .errors
            .iter()
            .any(|e| matches!(e, ModDagError::CycleQuarantined { .. }))
    );

    let id_a = report.loaded[0].1;
    let id_b = report.loaded[1].1;
    let id_c = report.loaded[2].1;

    // Run 2 ticks so `state_hooks_and_observers_mod` (`mod_a`) accumulates state.
    app.update();
    app.update();
    assert_eq!(app.world().resource::<ReloadStateProbe>().reload_epoch, 0);
    assert_eq!(
        app.world().resource::<ReloadStateProbe>().accumulated_ticks,
        2
    );

    // Clear message queues to observe exact ordering of cascading `reload_mod(id_a)`!
    let mut unloaded_cursor = app.world().resource::<Messages<ModUnloaded>>().get_cursor();
    let mut loaded_cursor = app.world().resource::<Messages<ModLoaded>>().get_cursor();
    let _ = unloaded_cursor
        .read(app.world().resource::<Messages<ModUnloaded>>())
        .count();
    let _ = loaded_cursor
        .read(app.world().resource::<Messages<ModLoaded>>())
        .count();

    // Cascading reload of `mod_a`:
    // 1. Saves state & unloads in reverse topological order: `mod_c -> mod_b -> mod_a`.
    // 2. Reloads & restores state in forward topological order: `mod_a -> mod_b -> mod_c`.
    let new_id_a = app.reload_mod(id_a).expect("cascading reload_mod succeeds");
    assert_ne!(new_id_a, id_a);

    let unloaded_ids: Vec<ModId> = unloaded_cursor
        .read(app.world().resource::<Messages<ModUnloaded>>())
        .map(|m| m.id)
        .collect();
    assert_eq!(
        unloaded_ids,
        vec![id_c, id_b, id_a],
        "cascading reload must unload dependents in reverse topological order C -> B -> A"
    );

    let reloaded_events: Vec<(ModId, String)> = loaded_cursor
        .read(app.world().resource::<Messages<ModLoaded>>())
        .map(|m| (m.id, m.name.clone()))
        .collect();
    assert_eq!(reloaded_events.len(), 3);
    assert_eq!(reloaded_events[0].0, new_id_a);
    assert_eq!(reloaded_events[0].1, "state_hooks_and_observers_mod");
    assert_eq!(reloaded_events[1].1, "ordered_sets_mod");
    assert_eq!(reloaded_events[2].1, "asymmetric_c_mod");

    // Verify `save_state` / `restore_state` ran on `mod_a` across cascading reload!
    app.update();
    assert_eq!(app.world().resource::<ReloadStateProbe>().reload_epoch, 1);
    assert_eq!(
        app.world().resource::<ReloadStateProbe>().accumulated_ticks,
        3
    );

    let new_id_b = reloaded_events[1].0;
    let new_id_c = reloaded_events[2].0;
    assert!(
        app.world()
            .resource::<ModDependencyGraph>()
            .get_node(new_id_a)
            .is_some()
    );
    assert!(
        app.world()
            .resource::<ModDependencyGraph>()
            .get_node(new_id_b)
            .is_some()
    );
    assert!(
        app.world()
            .resource::<ModDependencyGraph>()
            .get_node(new_id_c)
            .is_some()
    );

    // Cascading unload of `new_id_a` must unload `C -> B -> A` in reverse topological order!
    app.unload_mod(new_id_a)
        .expect("cascading unload_mod succeeds");
    let cascade_unloaded: Vec<ModId> = unloaded_cursor
        .read(app.world().resource::<Messages<ModUnloaded>>())
        .map(|m| m.id)
        .collect();
    assert_eq!(
        cascade_unloaded,
        vec![new_id_c, new_id_b, new_id_a],
        "cascading unload must unload dependents in reverse topological order C -> B -> A"
    );
    assert!(app.world().resource::<ModHost>().is_empty());
}
