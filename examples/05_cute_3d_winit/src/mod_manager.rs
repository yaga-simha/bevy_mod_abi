//! Runtime Dynamic Mod Manager for Example 5: Cute 3D Slime Meadow (`winit` + `wgpu`).
//!
//! Supports zero-serialization dynamic loading, hot-reloading via shadow copies,
//! and clean unloading of both Rust (`.cdylib`) and pure ISO C99 (`.dylib`/`.so`/`.dll`) mods!

use bevy_app::App;
use bevy_ecs::prelude::Resource;
use host_runtime::{ModAppExt, ModId};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use crate::components::{ColorRgba, CuteSpirit, HomeStyle, Scale3D};

/// Active 3D mod selection in the Cute Slime Meadow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ActiveMod {
    #[default]
    None,
    Boing,
    Carousel,
    CuddleC99,
}

impl ActiveMod {
    pub fn name(&self) -> &'static str {
        match self {
            ActiveMod::None => "Meadow Stroll (Baseline)",
            ActiveMod::Boing => "Jelly Boing Hop (Rust cdylib)",
            ActiveMod::Carousel => "Pastel Carousel Waltz (Rust cdylib)",
            ActiveMod::CuddleC99 => "Cozy Cuddle Huddle (Pure C99)",
        }
    }

    pub fn short_badge(&self) -> &'static str {
        match self {
            ActiveMod::None => "IDLE STROLL",
            ActiveMod::Boing => "MOD 1: JELLY BOING (RUST)",
            ActiveMod::Carousel => "MOD 2: PASTEL CAROUSEL (RUST)",
            ActiveMod::CuddleC99 => "MOD 3: COZY CUDDLE (C99)",
        }
    }

    pub fn accent_rgb(&self) -> [f32; 3] {
        match self {
            ActiveMod::None => [0.58, 0.86, 0.72],
            ActiveMod::Boing => [1.00, 0.62, 0.76],
            ActiveMod::Carousel => [0.76, 0.68, 1.00],
            ActiveMod::CuddleC99 => [1.00, 0.78, 0.52],
        }
    }

    pub fn binary_stem(&self) -> Option<&'static str> {
        match self {
            ActiveMod::None => None,
            ActiveMod::Boing => Some("mod_boing"),
            ActiveMod::Carousel => Some("mod_carousel"),
            ActiveMod::CuddleC99 => Some("mod_cuddle"),
        }
    }
}

/// ECS Resource mirroring current mod state for 3D HUD & window title rendering.
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
            status_message: "Slimes are strolling peacefully. Press [1] [2] [3] to load 3D mods!"
                .to_string(),
            last_event_time: Instant::now(),
        }
    }
}

/// Host Mod Manager handling on-demand compilation, shadow-copying, hot-reload, and unload.
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
    pub fn new() -> Result<Self, std::io::Error> {
        let shadow_dir =
            std::env::temp_dir().join(format!("bevy_abi_mod_cute3d_{}", std::process::id()));
        fs::create_dir_all(&shadow_dir)?;

        let mut mgr = Self {
            shadow_dir,
            original_paths: HashMap::new(),
            active_mod: ActiveMod::None,
            current_mod_id: None,
            current_shadow_path: None,
            generation: 0,
            reload_count: 0,
            status_message: "Ready! Press 1: Boing, 2: Carousel, 3: C99 Cuddle".to_string(),
        };

        mgr.discover_mods();
        Ok(mgr)
    }

    fn discover_mods(&mut self) {
        for kind in [ActiveMod::Boing, ActiveMod::Carousel, ActiveMod::CuddleC99] {
            if let Some(path) = Self::resolve_binary(kind) {
                self.original_paths.insert(kind, path);
            }
        }
    }

    pub fn resolve_binary(kind: ActiveMod) -> Option<PathBuf> {
        let stem = kind.binary_stem()?;
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };

        let filename = format!("{prefix}{stem}.{ext}");
        let candidates = match kind {
            ActiveMod::None => return None,
            ActiveMod::Boing | ActiveMod::Carousel => vec![
                PathBuf::from(format!(
                    "examples/05_cute_3d_winit/mods/{stem}/target/release/{filename}"
                )),
                PathBuf::from(format!("mods/{stem}/target/release/{filename}")),
                PathBuf::from(format!(
                    "examples/05_cute_3d_winit/mods/{stem}/target/debug/{filename}"
                )),
                PathBuf::from(format!("mods/{stem}/target/debug/{filename}")),
            ],
            ActiveMod::CuddleC99 => vec![
                PathBuf::from(format!(
                    "examples/05_cute_3d_winit/mods/mod_cuddle_c99/{filename}"
                )),
                PathBuf::from(format!("mods/mod_cuddle_c99/{filename}")),
            ],
        };

        candidates.into_iter().find(|p| p.exists())
    }

    fn ensure_binary_built(&mut self, kind: ActiveMod) -> Result<PathBuf, String> {
        if let Some(existing) = Self::resolve_binary(kind) {
            self.original_paths.insert(kind, existing.clone());
            return Ok(existing);
        }

        match kind {
            ActiveMod::None => return Err("Cannot build None mod".to_string()),
            ActiveMod::Boing | ActiveMod::Carousel => {
                let stem = kind.binary_stem().unwrap();
                let manifest_candidates = [
                    PathBuf::from(format!("examples/05_cute_3d_winit/mods/{stem}/Cargo.toml")),
                    PathBuf::from(format!("mods/{stem}/Cargo.toml")),
                ];
                let manifest = manifest_candidates
                    .into_iter()
                    .find(|p| p.exists())
                    .ok_or_else(|| format!("Cargo.toml not found for {stem}"))?;

                println!("[ModManager] Building {stem} Rust cdylib on demand...");
                let status = Command::new("cargo")
                    .args(["build", "--manifest-path"])
                    .arg(&manifest)
                    .arg("--release")
                    .status()
                    .map_err(|e| format!("Failed to invoke cargo build for {stem}: {e}"))?;

                if !status.success() {
                    return Err(format!("cargo build failed for {stem}"));
                }
            }
            ActiveMod::CuddleC99 => {
                let dir_candidates = [
                    PathBuf::from("examples/05_cute_3d_winit/mods/mod_cuddle_c99"),
                    PathBuf::from("mods/mod_cuddle_c99"),
                ];
                let c_dir = dir_candidates
                    .into_iter()
                    .find(|p| p.join("Makefile").exists())
                    .ok_or_else(|| "Makefile not found for mod_cuddle_c99".to_string())?;

                println!("[ModManager] Compiling mod_cuddle_c99 (ISO C99) on demand...");
                let status = Command::new("make")
                    .arg("-C")
                    .arg(&c_dir)
                    .status()
                    .map_err(|e| format!("Failed to invoke make for mod_cuddle_c99: {e}"))?;

                if !status.success() {
                    return Err("make failed for mod_cuddle_c99".to_string());
                }
            }
        }

        let resolved = Self::resolve_binary(kind)
            .ok_or_else(|| format!("Binary for {:?} still missing after build", kind))?;
        self.original_paths.insert(kind, resolved.clone());
        Ok(resolved)
    }

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
        let stem = target.binary_stem().unwrap_or("mod");
        let shadow_file = format!("{stem}_gen{}_{}.{ext}", self.generation, std::process::id());
        let shadow_path = self.shadow_dir.join(shadow_file);

        fs::copy(&original_path, &shadow_path)
            .map_err(|e| format!("Shadow copy failed ({}): {e}", shadow_path.display()))?;

        let mod_id = app
            .load_mod_tracked(&shadow_path)
            .map_err(|err| format!("Failed to load 3D mod {}: {:?}", target.name(), err))?;

        self.active_mod = target;
        self.current_mod_id = Some(mod_id);
        self.current_shadow_path = Some(shadow_path);
        self.reload_count = 0;
        self.status_message = format!("Loaded {} (ModId: {:?})", target.name(), mod_id);
        self.sync_resource(app);

        Ok(())
    }

    pub fn unload_mod(&mut self, app: &mut App) -> Result<(), String> {
        if self.active_mod == ActiveMod::None {
            self.status_message = "Already in peaceful Meadow Stroll!".to_string();
            self.sync_resource(app);
            return Ok(());
        }

        if let Some(mod_id) = self.current_mod_id.take() {
            app.unload_mod(mod_id)
                .map_err(|err| format!("Failed to unload {:?}: {:?}", self.active_mod, err))?;
        }

        if let Some(shadow) = self.current_shadow_path.take() {
            let _ = fs::remove_file(shadow);
        }

        // Restore each slime's signature pastel coat and relaxed proportions
        {
            let mut query =
                app.world_mut()
                    .query::<(&mut ColorRgba, &mut Scale3D, &mut CuteSpirit, &HomeStyle)>();
            for (mut col, mut scale, mut spirit, home) in query.iter_mut(app.world_mut()) {
                *col = home.base_color;
                *scale = Scale3D::uniform(1.0);
                spirit.blush = 0.42;
                spirit.ear_wiggle = 0.0;
            }
        }

        let prev_name = self.active_mod.name();
        self.active_mod = ActiveMod::None;
        self.reload_count = 0;
        self.status_message = format!("Unloaded {prev_name}. Slimes back to cozy stroll.");
        self.sync_resource(app);

        Ok(())
    }

    pub fn reload_mod(&mut self, app: &mut App) -> Result<(), String> {
        if self.active_mod == ActiveMod::None {
            self.status_message = "No active mod to hot-reload! Press 1, 2, or 3.".to_string();
            self.sync_resource(app);
            return Ok(());
        }

        let target = self.active_mod;
        let prev_reloads = self.reload_count;

        // Re-check disk binary in case the developer recompiled the mod while the 3D window is open!
        if let Some(fresh_disk_path) = Self::resolve_binary(target) {
            self.original_paths.insert(target, fresh_disk_path);
        }

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
}

impl Drop for ModManager {
    fn drop(&mut self) {
        if let Some(shadow) = self.current_shadow_path.take() {
            let _ = fs::remove_file(shadow);
        }
        let _ = fs::remove_dir_all(&self.shadow_dir);
    }
}
