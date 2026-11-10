#![allow(dead_code)]

use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::world::World;
use host_runtime::{AbiModPlugin, ModAppExt, ModId};
use libloading::{Library, Symbol};
use mod_derive::ModExposable;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

// ============================================================================
// 1. ModExposable Component Definitions
// ============================================================================

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct CombatHealth {
    pub current: i32,
    pub max: i32,
}

#[derive(Component, Debug, Clone, PartialEq, ModExposable)]
pub struct NameTag {
    pub label: String,
}

#[derive(Component, Debug, Clone, PartialEq, ModExposable)]
pub struct MetadataTag {
    pub label: String,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Mass {
    pub value: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Armor {
    pub defense: i32,
}

// ============================================================================
// 2. Compilation and Dylib Path Helpers
// ============================================================================

static BUILD_ONCE: Once = Once::new();

pub fn get_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();

    let rust_mod_dir = root_dir.join("tests/asymmetric_rust_mod");
    let c_mod_dir = root_dir.join("tests/asymmetric_c_mod");

    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let rust_dylib = rust_mod_dir
        .join("target/release")
        .join(format!("{prefix}asymmetric_rust_mod.{ext}"));
    let c_dylib = c_mod_dir.join(format!("{prefix}asymmetric_c_mod.{ext}"));

    (rust_dylib, c_dylib)
}

pub fn ensure_guest_mods_built() -> (PathBuf, PathBuf) {
    let (rust_dylib, c_dylib) = get_dylib_paths();
    if rust_dylib.exists() && c_dylib.exists() {
        return (rust_dylib, c_dylib);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();

    BUILD_ONCE.call_once(|| {
        let status_rust = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(root_dir.join("tests/asymmetric_rust_mod/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for asymmetric_rust_mod");
        assert!(
            status_rust.success(),
            "cargo build for asymmetric_rust_mod failed"
        );

        let status_c = Command::new("make")
            .arg("-C")
            .arg(root_dir.join("tests/asymmetric_c_mod"))
            .status()
            .expect("Failed to execute make for asymmetric_c_mod");
        assert!(status_c.success(), "make for asymmetric_c_mod failed");
    });

    let (rust_dylib, c_dylib) = get_dylib_paths();
    assert!(
        rust_dylib.exists(),
        "Rust mod dylib missing: {:?}",
        rust_dylib
    );
    assert!(c_dylib.exists(), "C mod dylib missing: {:?}", c_dylib);

    (rust_dylib, c_dylib)
}

pub fn reset_rust_mod_panic(rust_dylib: &Path) {
    // SAFETY: Dynamic library load and symbol invocation of thread-safe atomic reset.
    unsafe {
        let lib = Library::new(rust_dylib).expect("Failed to open rust_dylib for reset");
        let reset: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_reset_panic\0")
            .expect("Failed to resolve asymmetric_rust_mod_reset_panic");
        reset();
    }
}

// ============================================================================
// 3. Entity Generators for Heterogeneous Archetypes
// ============================================================================

pub fn spawn_kinematics_entities(world: &mut World, count: usize) {
    for i in 0..count {
        world.spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ));
    }
}

pub fn spawn_combat_entities(world: &mut World, count: usize) {
    for _ in 0..count {
        world.spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            CombatHealth {
                current: 100,
                max: 100,
            },
        ));
    }
}

pub fn spawn_metadata_entities(world: &mut World, count: usize) {
    for i in 0..count {
        world.spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            NameTag {
                label: format!("entity_{i}"),
            },
        ));
    }
}

/// Spawns `total` entities distributed evenly across `archetypes` distinct topologies.
pub fn spawn_heterogeneous_archetypes(world: &mut World, total: usize, num_archetypes: usize) {
    let archetypes = num_archetypes.max(1);
    let per_arch = total / archetypes;
    let remainder = total % archetypes;

    for arch_idx in 0..archetypes {
        let count = per_arch + if arch_idx == 0 { remainder } else { 0 };
        for i in 0..count {
            let id = arch_idx * per_arch + i;
            match arch_idx % 8 {
                0 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                    ));
                }
                1 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        NameTag {
                            label: format!("kin_{id}"),
                        },
                    ));
                }
                2 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        CombatHealth {
                            current: 100,
                            max: 100,
                        },
                    ));
                }
                3 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        CombatHealth {
                            current: 100,
                            max: 100,
                        },
                        NameTag {
                            label: format!("hero_{id}"),
                        },
                    ));
                }
                4 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Mass { value: 5.0 },
                    ));
                }
                5 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        Mass { value: 10.0 },
                    ));
                }
                6 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        CombatHealth {
                            current: 100,
                            max: 100,
                        },
                        Armor { defense: 25 },
                    ));
                }
                7 => {
                    world.spawn((
                        Position {
                            x: id as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        CombatHealth {
                            current: 100,
                            max: 100,
                        },
                        NameTag {
                            label: format!("boss_{id}"),
                        },
                        Mass { value: 50.0 },
                        Armor { defense: 100 },
                    ));
                }
                _ => unreachable!(),
            }
        }
    }
}

// ============================================================================
// 4. Testbed App and Builder
// ============================================================================

pub struct TestbedBuilder {
    pub app: App,
    pub rust_dylib: PathBuf,
    pub c_dylib: PathBuf,
}

impl TestbedBuilder {
    pub fn new() -> Self {
        let (rust_dylib, c_dylib) = ensure_guest_mods_built();
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        // Register host components with fallback/alternative names
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<NameTag>();
        app.register_mod_component::<MetadataTag>();
        app.register_mod_component::<Mass>();
        app.register_mod_component::<Armor>();

        Self {
            app,
            rust_dylib,
            c_dylib,
        }
    }

    pub fn with_entities_1k(mut self) -> Self {
        spawn_heterogeneous_archetypes(self.app.world_mut(), 1_000, 4);
        self
    }

    pub fn with_entities_10k(mut self) -> Self {
        spawn_heterogeneous_archetypes(self.app.world_mut(), 10_000, 4);
        self
    }

    pub fn with_entities_100k(mut self) -> Self {
        spawn_heterogeneous_archetypes(self.app.world_mut(), 100_000, 4);
        self
    }

    pub fn with_fragmented_archetypes(mut self, total: usize, num_archetypes: usize) -> Self {
        spawn_heterogeneous_archetypes(self.app.world_mut(), total, num_archetypes);
        self
    }

    pub fn build_and_load_mods(mut self) -> (TestbedApp, ModId, ModId) {
        let r_id = self
            .app
            .load_mod_tracked(&self.rust_dylib)
            .expect("Failed to load asymmetric_rust_mod");
        let c_id = self
            .app
            .load_mod_tracked(&self.c_dylib)
            .expect("Failed to load asymmetric_c_mod");

        (TestbedApp { app: self.app }, r_id, c_id)
    }

    pub fn build_app(self) -> App {
        self.app
    }
}

pub struct TestbedApp {
    pub app: App,
}

impl TestbedApp {
    pub fn tick(&mut self, count: usize) {
        for _ in 0..count {
            self.app.update();
        }
    }

    pub fn unload_mod(&mut self, id: ModId) {
        self.app
            .unload_mod(id)
            .expect("Failed to unload mod in TestbedApp");
    }

    pub fn world(&self) -> &World {
        self.app.world()
    }

    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }
}
