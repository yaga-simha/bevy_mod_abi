#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Bevy Plugin & Application Integration
//!
//! This module provides [`AbiModPlugin`] and the [`ModAppExt`] extension trait to
//! seamlessly integrate dynamic C-ABI native mods into Bevy's application lifecycle
//! and schedule.
//!
//! ## Architecture
//! - **Resource Initialization**: Automatically inserts [`ModRegistry`] and [`ModHost`]
//!   into the Bevy [`World`].
//! - **Component Registration**: Extends [`App`] with [`ModAppExt::register_mod_component`]
//!   and [`ModAppExt::register_mod_type`] to expose component VTables and accessors.
//! - **Dynamic Mod Loading**: Loads native shared libraries (`.so`, `.dylib`, `.dll`),
//!   executes the C-ABI handshake, and injects foreign dynamic systems into Bevy's
//!   [`Update`] schedule.
//! - **Safe Unloading**: Deactivates dynamic systems via atomic tombstone flags and
//!   calls guest teardown hooks before dropping library handles.

use crate::host::{ModHost, ModId};
use crate::registry::ModRegistry;
use abi_contract::{ExposableComponent, FfiResult};
use bevy_app::{App, Plugin, Update};
use bevy_ecs::component::Component;
use std::path::Path;
use std::sync::Arc;

/// Bevy Plugin initializing the native ABI modding framework.
///
/// When added to a Bevy [`App`], this plugin ensures that the necessary resources
/// ([`ModRegistry`] and [`ModHost`]) are initialized in the ECS world if they have
/// not already been inserted.
///
/// Implements [`Default`], [`Debug`], [`Clone`], and [`Copy`], allowing idiomatic
/// usage in Bevy apps:
///
/// ```rust,no_run
/// use bevy_app::App;
/// use host_runtime::AbiModPlugin;
///
/// let mut app = App::new();
/// app.add_plugins(AbiModPlugin::default());
/// ```
#[derive(Default, Debug, Clone, Copy)]
pub struct AbiModPlugin;

impl Plugin for AbiModPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ModRegistry>() {
            app.insert_resource(ModRegistry::new());
        }
        if !app.world().contains_resource::<ModHost>() {
            app.insert_resource(ModHost::new());
        }
    }
}

/// Extension methods on Bevy [`App`] for registering mod-exposable components and loading native mods.
pub trait ModAppExt {
    /// Register an exposable component type with Bevy ECS and the native mod registry.
    ///
    /// Assigns a Bevy `ComponentId` and registers the component with [`ModRegistry`].
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self;

    /// Register a nested/compound `ModExposable` helper type (without requiring `Component`)
    /// so its VTable and field accessors are exposed via `GetProcAddressFn`.
    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self;

    /// Load a native shared library mod into the Bevy App's update schedule.
    ///
    /// Convenience chaining method returning `&mut Self`.
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, FfiResult>;

    /// Load a native shared library mod into the Bevy App's update schedule, returning its tracked `ModId`.
    ///
    /// Snapshots the current [`ModRegistry`], executes the C-ABI handshake, extracts generated
    /// dynamic systems, and injects them into Bevy's [`Update`] schedule.
    fn load_mod_tracked<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, FfiResult>;

    /// Unload a loaded mod by its [`ModId`]: deactivates systems via atomic tombstone flags,
    /// calls guest teardown hook while memory is still mapped, and drops the library handle.
    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult>;
}

impl ModAppExt for App {
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self {
        let comp_id = self.world_mut().register_component::<T>();
        if let Some(mut registry) = self.world_mut().get_resource_mut::<ModRegistry>() {
            registry.register_component::<T>(comp_id);
        } else {
            let mut registry = ModRegistry::new();
            registry.register_component::<T>(comp_id);
            self.insert_resource(registry);
        }
        self
    }

    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self {
        if let Some(mut registry) = self.world_mut().get_resource_mut::<ModRegistry>() {
            registry.register_type::<T>();
        } else {
            let mut registry = ModRegistry::new();
            registry.register_type::<T>();
            self.insert_resource(registry);
        }
        self
    }

    fn load_mod_tracked<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, FfiResult> {
        let registry = self
            .world()
            .get_resource::<ModRegistry>()
            .cloned()
            .unwrap_or_default();
        let registry_arc = Arc::new(registry);

        let (mod_id, systems) = {
            let mut mod_host = self.world_mut().get_resource_or_init::<ModHost>();
            // SAFETY: Loading dynamic library specified by path.
            unsafe { mod_host.load_mod_tracked_with_registry(path, &registry_arc)? }
        };

        for sys in systems {
            self.add_systems(Update, sys);
        }

        Ok(mod_id)
    }

    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, FfiResult> {
        self.load_mod_tracked(path)?;
        Ok(self)
    }

    fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult> {
        let mut mod_host = self
            .world_mut()
            .get_resource_mut::<ModHost>()
            .ok_or(FfiResult::INTERNAL_ERROR)?;
        // SAFETY: Delegating to ModHost::unload_mod with specified ModId.
        unsafe { mod_host.unload_mod(id) }
    }
}
