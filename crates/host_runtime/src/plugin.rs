#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use crate::host::{ModHost, ModId};
use crate::registry::ModRegistry;
use abi_contract::{ExposableComponent, FfiResult};
use bevy_app::{App, Plugin, Update};
use bevy_ecs::component::Component;
use std::path::Path;
use std::sync::Arc;

/// Bevy Plugin initializing the native ABI modding framework.
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

/// Extension methods on Bevy `App` for registering mod-exposable components and loading native mods.
pub trait ModAppExt {
    /// Register an exposable component type with Bevy ECS and the native mod registry.
    fn register_mod_component<T: Component + ExposableComponent>(&mut self) -> &mut Self;

    /// Register a nested/compound `ModExposable` helper type (without requiring `Component`)
    /// so its VTable and field accessors are exposed via `GetProcAddressFn`.
    fn register_mod_type<T: ExposableComponent>(&mut self) -> &mut Self;

    /// Load a native shared library mod into the Bevy App's update schedule.
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, FfiResult>;

    /// Load a native shared library mod into the Bevy App's update schedule, returning its tracked `ModId`.
    fn load_mod_tracked<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, FfiResult>;

    /// Unload a loaded mod by its `ModId`: deactivates systems, calls teardown hook, and drops library.
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
