//! Host Runtime Mod Manager for Example 4: Visual Kinetic Arena Game.
//!
//! Provides zero-serialization dynamic library loading, atomic tombstone unloading,
//! and safe hot-reloading without OS file locking via shadow copying.

use bevy_app::App;
use bevy_ecs::prelude::Resource;
use host_runtime::{ModAppExt, ModId};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use crate::components::ColorRgba;

/// Active mod selection in the Kinetic Arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ActiveMod {
    #[default]
    None,
    Gravity,
    Rainbow,
    Vortex,
}

impl ActiveMod {
    pub fn name(&self) -> &'static str {
        match self {
            ActiveMod::None => "None (Baseline Drift)",
            ActiveMod::Gravity => "Gravity (Kinetic Bouncer)",
            ActiveMod::Rainbow => "Rainbow (Color Spectrum)",
            ActiveMod::Vortex => "Vortex (Orbital Swirl)",
        }
    }

    #[allow(dead_code)]
    pub fn key_hint(&self) -> &'static str {
        match self {
            ActiveMod::None => "[u] Unload",
            ActiveMod::Gravity => "[1] Gravity",
            ActiveMod::Rainbow => "[2] Rainbow",
            ActiveMod::Vortex => "[3] Vortex",
        }
    }

    pub fn crate_name(&self) -> Option<&'static str> {
        match self {
            ActiveMod::None => None,
            ActiveMod::Gravity => Some("mod_gravity"),
            ActiveMod::Rainbow => Some("mod_rainbow"),
            ActiveMod::Vortex => Some("mod_vortex"),
        }
    }
}

/// ECS Resource mirroring current mod state for HUD rendering and Bevy queries.
#[derive(Resource, Debug, Clone)]
pub struct ModStatusResource {
    pub active_mod: ActiveMod,
    #[allow(dead_code)]
    pub mod_id: Option<ModId>,
    pub reload_count: u32,
    pub status_message: String,
    #[allow(dead_code)]
    pub last_event_time: Instant,
}

impl Default for ModStatusResource {
    fn default() -> Self {
        Self {
            active_mod: ActiveMod::None,
            mod_id: None,
            reload_count: 0,
            status_message: "Ready. Entities in baseline drift.".to_string(),
            last_event_time: Instant::now(),
        }
    }
}

/// Host Mod Manager responsible for lifecycle tracking, shadow-copying, and dynamic reloading.
pub struct ModManager {
    shadow_dir: PathBuf,
    original_paths: HashMap<ActiveMod, PathBuf>,
    active_mod: ActiveMod,
    current_mod_id: Option<ModId>,
    current_shadow_path: Option<PathBuf>,
    generation: u64,
    reload_count: u32,
    status_message: String,
}

impl ModManager {
    /// Initialize ModManager with designated temporary shadow cache directory.
    pub fn new() -> Result<Self, std::io::Error> {
        let shadow_dir =
            std::env::temp_dir().join(format!("bevy_abi_mod_visual_{}", std::process::id()));
        fs::create_dir_all(&shadow_dir)?;

        let mut mgr = Self {
            shadow_dir,
            original_paths: HashMap::new(),
            active_mod: ActiveMod::None,
            current_mod_id: None,
            current_shadow_path: None,
            generation: 0,
            reload_count: 0,
            status_message: "Initialized. Baseline drift active.".to_string(),
        };

        mgr.discover_mods();
        Ok(mgr)
    }

    fn discover_mods(&mut self) {
        for kind in [ActiveMod::Gravity, ActiveMod::Rainbow, ActiveMod::Vortex] {
            if let Some(crate_name) = kind.crate_name() {
                if let Some(path) = Self::resolve_binary(crate_name) {
                    self.original_paths.insert(kind, path);
                }
            }
        }
    }

    /// Resolve candidate binary path across platforms and working directories.
    pub fn resolve_binary(crate_name: &str) -> Option<PathBuf> {
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };

        let filename = format!("{prefix}{crate_name}.{ext}");
        let candidates = [
            PathBuf::from(format!(
                "examples/04_visual_game/mods/{crate_name}/target/release/{filename}"
            )),
            PathBuf::from(format!("mods/{crate_name}/target/release/{filename}")),
            PathBuf::from(format!("target/release/{filename}")),
            PathBuf::from(format!(
                "examples/04_visual_game/mods/{crate_name}/target/debug/{filename}"
            )),
            PathBuf::from(format!("mods/{crate_name}/target/debug/{filename}")),
        ];

        candidates.into_iter().find(|p| p.exists())
    }

    /// Ensure binary is compiled; if absent, invokes `cargo build --release` automatically.
    fn ensure_binary_built(&mut self, kind: ActiveMod) -> Result<PathBuf, String> {
        let crate_name = kind.crate_name().ok_or("Cannot build None mod")?;

        if let Some(existing) = Self::resolve_binary(crate_name) {
            self.original_paths.insert(kind, existing.clone());
            return Ok(existing);
        }

        let manifest_candidates = [
            PathBuf::from(format!(
                "examples/04_visual_game/mods/{crate_name}/Cargo.toml"
            )),
            PathBuf::from(format!("mods/{crate_name}/Cargo.toml")),
        ];
        let manifest = manifest_candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| format!("Cargo.toml manifest not found for {crate_name}"))?;

        println!("[ModManager] Compiling {crate_name} cdylib on demand...");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--release")
            .status()
            .map_err(|e| format!("Failed to spawn cargo build for {crate_name}: {e}"))?;

        if !status.success() {
            return Err(format!("cargo build for {crate_name} exited with error"));
        }

        let resolved = Self::resolve_binary(crate_name).ok_or_else(|| {
            format!("Binary {crate_name} not found even after successful cargo build")
        })?;
        self.original_paths.insert(kind, resolved.clone());
        Ok(resolved)
    }

    /// Load or switch to the specified mod.
    pub fn load_mod(&mut self, app: &mut App, target: ActiveMod) -> Result<(), String> {
        if target == ActiveMod::None {
            return self.unload_mod(app);
        }

        if self.active_mod == target {
            return self.reload_mod(app);
        }

        if self.active_mod != ActiveMod::None {
            self.unload_mod(app)?;
        }

        let original_path = self.ensure_binary_built(target)?;
        self.generation += 1;

        let ext = original_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy();
        let crate_name = target.crate_name().unwrap_or("mod");
        let shadow_file = format!(
            "{crate_name}_gen{}_{}.{ext}",
            self.generation,
            std::process::id()
        );
        let shadow_path = self.shadow_dir.join(shadow_file);

        fs::copy(&original_path, &shadow_path).map_err(|e| {
            format!(
                "Failed to shadow-copy mod to {}: {e}",
                shadow_path.display()
            )
        })?;

        let mod_id = app
            .load_mod_tracked(&shadow_path)
            .map_err(|err| format!("Failed to load dynamic mod {}: {:?}", target.name(), err))?;

        self.active_mod = target;
        self.current_mod_id = Some(mod_id);
        self.current_shadow_path = Some(shadow_path);
        self.reload_count = 0;
        self.status_message = format!("Loaded {} (ModId: {:?})", target.name(), mod_id);
        self.sync_resource(app);

        Ok(())
    }

    /// Unload the currently active mod. Entities revert to baseline drift.
    pub fn unload_mod(&mut self, app: &mut App) -> Result<(), String> {
        if self.active_mod == ActiveMod::None {
            self.status_message = "No active mod to unload. Baseline drift active.".to_string();
            self.sync_resource(app);
            return Ok(());
        }

        if let Some(mod_id) = self.current_mod_id.take() {
            app.unload_mod(mod_id)
                .map_err(|err| format!("Failed to unload mod {:?}: {:?}", self.active_mod, err))?;
        }

        if let Some(shadow_path) = self.current_shadow_path.take() {
            let _ = fs::remove_file(shadow_path);
        }

        // Reset particle colors to neutral baseline when leaving mods
        {
            let mut query = app.world_mut().query::<&mut ColorRgba>();
            let mut toggle = false;
            for mut col in query.iter_mut(app.world_mut()) {
                if toggle {
                    *col = ColorRgba::cyan();
                } else {
                    *col = ColorRgba::white();
                }
                toggle = !toggle;
            }
        }

        let prev_name = self.active_mod.name();
        self.active_mod = ActiveMod::None;
        self.reload_count = 0;
        self.status_message = format!("Unloaded {prev_name}. Reverted to baseline drift.");
        self.sync_resource(app);

        Ok(())
    }

    /// Hot-reload the active mod from disk via fresh shadow copy.
    pub fn reload_mod(&mut self, app: &mut App) -> Result<(), String> {
        if self.active_mod == ActiveMod::None {
            self.status_message = "No mod active to reload. Press 1, 2, or 3 to load.".to_string();
            self.sync_resource(app);
            return Ok(());
        }

        let target = self.active_mod;
        let prev_reloads = self.reload_count;

        self.unload_mod(app)?;
        self.load_mod(app, target)?;

        self.reload_count = prev_reloads + 1;
        self.status_message = format!(
            "Hot-reloaded {} (Reload #{})",
            target.name(),
            self.reload_count
        );
        self.sync_resource(app);

        Ok(())
    }

    /// Synchronize state into Bevy ECS resource.
    pub fn sync_resource(&self, app: &mut App) {
        app.insert_resource(ModStatusResource {
            active_mod: self.active_mod,
            mod_id: self.current_mod_id,
            reload_count: self.reload_count,
            status_message: self.status_message.clone(),
            last_event_time: Instant::now(),
        });
    }

    pub fn active_mod(&self) -> ActiveMod {
        self.active_mod
    }

    pub fn reload_count(&self) -> u32 {
        self.reload_count
    }

    #[allow(dead_code)]
    pub fn status_message(&self) -> &str {
        &self.status_message
    }
}

impl Drop for ModManager {
    fn drop(&mut self) {
        if let Some(shadow) = self.current_shadow_path.take() {
            let _ = fs::remove_file(shadow);
        }
        let _ = fs::remove_dir_all(&self.shadow_dir);
    }
}
