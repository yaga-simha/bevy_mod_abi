#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Top-level `crates.io` umbrella crate for the zero-serialization, asymmetric C-ABI (`ABI 0.3.0`)
//! native modding framework for Bevy ECS (`v0.20+`).
//!
//! `bevy_mod_abi` matches the repository name (`https://github.com/yaga-simha/bevy_mod_abi`) so
//! Bevy host applications can run `cargo add bevy_mod_abi` and access the complete
//! [`bevy_abi_mod`] API surface, [`prelude`], procedural derives (`#[derive(ModComponent, AbiPod)]`),
//! and every optional Bevy subsystem bridge feature flag while enforcing `#![forbid(unsafe_code)]`.
//!
//! # Quickstart
//!
//! ```no_run
//! use bevy_app::App;
//! use bevy_ecs::component::Component;
//! use bevy_mod_abi::prelude::*;
//!
//! #[repr(C)]
//! #[derive(Component, ModComponent, Clone, Copy, Default)]
//! #[mod_component(name = "my_game::Health")]
//! struct Health {
//!     current: f32,
//!     max: f32,
//! }
//!
//! fn main() {
//!     let mut app = App::new();
//!     app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
//!     app.register_mod_component::<Health>(
//!         ComponentOptions::new().with_raw_insert().with_default(),
//!     )
//!     .expect("register Health");
//!     app.load_mod_with("mods/libcombat_mod.dylib", LoadOptions::default())
//!         .expect("load mod");
//!     app.run();
//! }
//! ```
//!
//! # Feature Flags
//!
//! Every optional feature flag on [`bevy_abi_mod`] is forwarded 1:1:
//!
//! - `strict-ffi-checks`: Enables extra runtime FFI pointer, alignment, and handle-generation validation.
//! - `diagnostics`: Publishes per-mod and aggregate `bevy_diagnostic` metrics.
//! - `signature-verification`: Enables pure-Rust OpenSSH `SSHSIG`, Ed25519 CA certificate, `SSHKRL`,
//!   `.bam_sig`, and `.bampack` pre-load cryptographic verification (`ModTrustPolicy`).
//! - `hot-reload-watcher`: Enables debounced filesystem hot-reload watching (`ModHotReloadPlugin`).
//! - `bridge-std-types`: Bridges `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Disabled`,
//!   `Time<*>`, and `ButtonInput<*>`.
//! - `bridge-asset`: Sandboxed `mod://<namespace>/...` `AssetSource` & `AssetOverlayPolicy` (`ModAssetPlugin`).
//! - `bridge-state`: Exposes Bevy `States`, `SubStates`, and `ComputedStates` slabs (`ModStateExt`).
//! - `bridge-reflect`: Dynamic `bevy_reflect`, `bevy_remote` BRP, `bevy_settings` TOML, `ModSaveGamePlugin`,
//!   and `ModTelemetryPlugin`.
//! - `bridge-bsn`: Bevy Scene Notation (`bevy_scene`) spawning (`ModBsnPlugin`).
//! - `bridge-picking`: Bevy `0.20` flattened `Pointer*` observers, `EntityCursor`, and `Gamepad` bridges.
//! - `bridge-audio`: Spatial `AudioPlayer<AudioSource>` and `PlaybackSettings` bridges (`ModAudioBridgePlugin`).
//! - `bridge-light`: `Camera2d`/`Camera3d` and `PointLight`/`SpotLight`/`DirectionalLight` bridges.
//! - `bridge-ui`: Retained Bevy `0.20` UI, `EditableText`/`TextInput`, `InlineBox`/`InlineImage`, and
//!   headless widgets (`ModUiBridgePlugin`).
//! - `bridge-feathers`: `bevy_feathers` controls and `ThemeContext` bridges (implies `bridge-ui`).
//! - `bridge-camera-controller`: `PanOrbitCamera` controller bridge.
//! - `bridge-solari`: `SolariLighting` realtime global illumination bridge.
//! - `sub-app-extract`: `SubApp` (`RenderApp`) extraction, `.wesl` 2D/3D materials (`ModWeslMaterialPlugin`,
//!   `ModSpriteMaterialPlugin`), and opt-in mesh-shader pipelines (`ModMeshShaderPlugin`).

pub use bevy_abi_mod;
pub use bevy_abi_mod::*;

/// Convenient unified prelude re-exporting all core host APIs plus all enabled bridge, render,
/// reflection, and hot-reload watcher extensions from [`bevy_abi_mod::prelude`].
pub mod prelude {
    pub use bevy_abi_mod::prelude::*;
}
