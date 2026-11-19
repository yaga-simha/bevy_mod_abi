#![allow(dead_code)]

use bevy_abi_mod_host::prelude::*;
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use libloading::{Library, Symbol};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// 1. Canonical Namespaced `ModComponent` & `AbiPod` Definitions
// ============================================================================

/// Canonical dynamic component name registered by `asymmetric_rust_mod`.
pub const RUST_MOD_CUSTOM_DATA_NAME: &str = "asymmetric_rust_mod::RustModCustomData";

/// Canonical dynamic component name registered by `asymmetric_c_mod`.
pub const C_MOD_CUSTOM_DATA_NAME: &str = "asymmetric_c_mod::CModCustomData";

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Velocity")]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CombatHealth")]
pub struct CombatHealth {
    pub current: i32,
    pub max: i32,
}

#[derive(Component, Debug, Clone, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::NameTag")]
pub struct NameTag {
    pub label: String,
}

#[derive(Component, Debug, Clone, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::MetadataTag")]
pub struct MetadataTag {
    pub label: String,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Mass")]
pub struct Mass {
    pub value: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Armor")]
pub struct Armor {
    pub defense: i32,
}

/// Host-side mirror of `asymmetric_rust_mod::RustModCustomData`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, AbiPod)]
pub struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

/// Host-side mirror of `asymmetric_c_mod::CModCustomData`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, AbiPod)]
pub struct CModCustomData {
    pub mana: f32,
    pub level: i32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::FilteredTransform")]
pub struct FilteredTransform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFilteredTransform")]
pub struct CFilteredTransform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::ChangeSensor")]
pub struct ChangeSensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CChangeSensor")]
pub struct CChangeSensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::FreshSpawnTag")]
pub struct FreshSpawnTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFreshSpawnTag")]
pub struct CFreshSpawnTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::TransientMarker")]
pub struct TransientMarker {
    pub dummy: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CTransientMarker")]
pub struct CTransientMarker {
    pub dummy: u32,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::RustFilterTelemetry")]
pub struct RustFilterTelemetry {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub _pad0: u32,
    pub spawned_default_entity: u64,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFilterTelemetry")]
pub struct CFilterTelemetry {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub _pad0: u32,
    pub spawned_default_entity: u64,
}

// ============================================================================
// 2. Hermetic Fixture Resolution (`BAM_FIXTURE_DIR` / `target/fixtures`)
// ============================================================================

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ directory")
        .parent()
        .expect("workspace root directory")
        .to_path_buf()
}

pub fn fixture_dir() -> PathBuf {
    std::env::var_os("BAM_FIXTURE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target").join("fixtures"))
}

pub fn dylib_filename(stem: &str) -> String {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    format!("{prefix}{stem}.{ext}")
}

pub fn fixture_dylib(stem: &str) -> PathBuf {
    let path = fixture_dir().join(dylib_filename(stem));
    if !path.is_file() {
        panic!(
            "Fixture missing at {} — run 'cargo xtask fixtures' before 'cargo test'",
            path.display()
        );
    }
    path
}

pub fn ensure_guest_mods_built() -> (PathBuf, PathBuf) {
    (
        fixture_dylib("asymmetric_rust_mod"),
        fixture_dylib("asymmetric_c_mod"),
    )
}

/// RAII temporary copy of a shared library with a unique path/inode so tests that mutate static
/// test-harness flags via `libloading::Library::new` remain 100% isolated under parallel `cargo test`.
pub struct TempDylibCopy {
    path: PathBuf,
    dir: PathBuf,
}

impl TempDylibCopy {
    pub fn new(original: &Path, tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ext = original
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dylib".to_owned());
        let dir =
            std::env::temp_dir().join(format!("bevy_abi_mod_shadow_{}_{}", std::process::id(), id));
        fs::create_dir_all(&dir).expect("create shadow dir");
        let path = dir.join(format!("{tag}_{id}.{ext}"));
        fs::copy(original, &path).expect("copy shadow dylib");
        Self { path, dir }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDylibCopy {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDylibCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn create_shadow_copy(original: &Path, tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ext = original
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dylib".to_owned());
    let shadow_dir =
        std::env::temp_dir().join(format!("bevy_abi_mod_shadow_{}_{}", std::process::id(), id));
    fs::create_dir_all(&shadow_dir).expect("create shadow dir");
    let shadow_file = shadow_dir.join(format!("{tag}_{id}.{ext}"));
    fs::copy(original, &shadow_file).expect("copy shadow dylib");
    shadow_file
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

/// Spawns `total` entities distributed evenly across `num_archetypes` distinct topologies.
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
// 4. Testbed App and Builder (`ABI 0.2.0`)
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

        app.register_mod_component::<Position>(ComponentOptions::new().with_default())
            .expect("register Position");
        app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
            .expect("register Velocity");
        app.register_mod_component::<CombatHealth>(ComponentOptions::new().with_default())
            .expect("register CombatHealth");
        app.register_mod_component::<NameTag>(ComponentOptions::new().with_default())
            .expect("register NameTag");
        app.register_mod_component::<MetadataTag>(ComponentOptions::new().with_default())
            .expect("register MetadataTag");
        app.register_mod_component::<Mass>(ComponentOptions::new().with_default())
            .expect("register Mass");
        app.register_mod_component::<Armor>(ComponentOptions::new().with_default())
            .expect("register Armor");

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
            .load_mod_with(&self.rust_dylib, LoadOptions::new().with_shadow_copy(true))
            .expect("Failed to load asymmetric_rust_mod");
        let c_id = self
            .app
            .load_mod_with(&self.c_dylib, LoadOptions::new().with_shadow_copy(true))
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
