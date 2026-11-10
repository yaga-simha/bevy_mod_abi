#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use crate::dynamic_system::{DynamicCSystem, ModInitContext};
use crate::registry::{host_get_proc_address, ModRegistry};
use crate::HOST_API;
use abi_contract::{FfiResult, ModAbiVersionFn, ModInitFn, ModShutdownFn, ABI_VERSION_PACKED};
use bevy_ecs::prelude::Resource;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use libloading::{Library, Symbol};
use std::path::Path;
use std::sync::Arc;

/// Unique identifier for a loaded mod instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModId(pub u32);

/// A loaded native mod shared library.
pub struct LoadedMod {
    pub id: ModId,
    pub name: String,
    pub library: Library,
    pub shutdown_fn: Option<ModShutdownFn>,
    pub is_active: Arc<AtomicBool>,
}

/// Host manager tracking loaded dynamic libraries and managing their lifecycle.
#[derive(Resource)]
pub struct ModHost {
    loaded_libraries: Vec<LoadedMod>,
    next_mod_id: u32,
}

impl Default for ModHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ModHost {
    pub fn new() -> Self {
        Self {
            loaded_libraries: Vec::new(),
            next_mod_id: 1,
        }
    }

    /// Load a native mod library and perform the initial C-ABI handshake with a default registry.
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<(), FfiResult> {
        let registry = Arc::new(ModRegistry::new());
        // SAFETY: Delegating to load_mod_with_registry with newly constructed registry.
        unsafe { self.load_mod_with_registry(path, &registry).map(|_| ()) }
    }

    /// Load a native mod library with a specific `ModRegistry`, returning the assigned `ModId`
    /// and all registered dynamic systems.
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn load_mod_tracked_with_registry<P: AsRef<Path>>(
        &mut self,
        path: P,
        registry: &Arc<ModRegistry>,
    ) -> Result<(ModId, Vec<DynamicCSystem>), FfiResult> {
        let path = path.as_ref();
        // SAFETY: Caller guarantees `path` points to a valid dynamic library safe to load.
        let lib = unsafe { Library::new(path) }.map_err(|_| FfiResult::INTERNAL_ERROR)?;

        // Optional ABI version handshake check
        // SAFETY: If `bevy_abi_mod_abi_version` is present, it has signature `unsafe extern "C" fn() -> u32`.
        if let Ok(ver_fn) = unsafe { lib.get::<ModAbiVersionFn>(b"bevy_abi_mod_abi_version\0") } {
            // SAFETY: Calling the side-effect-free version query symbol exported by the mod.
            let mod_ver = unsafe { ver_fn() };
            if mod_ver != ABI_VERSION_PACKED {
                return Err(FfiResult::INVALID_VERSION);
            }
        }

        // SAFETY: Resolving standard C-ABI entry point `bevy_abi_mod_init`.
        let init_fn: Symbol<ModInitFn> =
            unsafe { lib.get(b"bevy_abi_mod_init\0") }.map_err(|_| FfiResult::INTERNAL_ERROR)?;

        // SAFETY: Resolving optional C-ABI teardown point `bevy_abi_mod_shutdown`.
        let shutdown_fn: Option<ModShutdownFn> = unsafe { lib.get(b"bevy_abi_mod_shutdown\0") }
            .ok()
            .map(|sym| *sym);

        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx =
            ModInitContext::with_is_active(Arc::clone(registry), Arc::clone(&is_active));

        let _guard = registry.make_current();

        // SAFETY: Passing valid `host_get_proc_address`, static `HOST_API`, and `ModInitContext`.
        let status = unsafe {
            init_fn(
                host_get_proc_address,
                &HOST_API,
                &mut init_ctx as *mut ModInitContext as *mut c_void,
            )
        };

        if status.is_err() {
            return Err(status);
        }

        let mod_id = ModId(self.next_mod_id);
        self.next_mod_id += 1;

        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            name: path.to_string_lossy().to_string(),
            library: lib,
            shutdown_fn,
            is_active,
        });

        Ok((mod_id, init_ctx.registered_systems))
    }

    /// Load a native mod library with a specific `ModRegistry`, returning all registered systems.
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn load_mod_with_registry<P: AsRef<Path>>(
        &mut self,
        path: P,
        registry: &Arc<ModRegistry>,
    ) -> Result<Vec<DynamicCSystem>, FfiResult> {
        // SAFETY: Delegating to load_mod_tracked_with_registry.
        unsafe {
            self.load_mod_tracked_with_registry(path, registry)
                .map(|(_, sys)| sys)
        }
    }

    /// Unload a specific mod by its `ModId`:
    /// Deactivates its `is_active` atomic guard, invokes `shutdown_fn`, and drops its `Library`.
    ///
    /// # Safety
    /// Caller must ensure unloading the dynamic library is safe with respect to external threads.
    pub unsafe fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult> {
        let pos = self
            .loaded_libraries
            .iter()
            .position(|m| m.id == id)
            .ok_or(FfiResult::INTERNAL_ERROR)?;
        let loaded = self.loaded_libraries.remove(pos);
        loaded.is_active.store(false, Ordering::Release);
        if let Some(shutdown) = loaded.shutdown_fn {
            // SAFETY: Invoking mod teardown hook prior to dropping `loaded.library` (`dlclose`).
            let _ = unsafe { shutdown(core::ptr::null_mut()) };
        }
        drop(loaded.library);
        Ok(())
    }

    /// Unload all loaded mods in reverse load order:
    /// Deactivates each mod's `is_active` atomic guard, invokes `shutdown_fn`, and drops `Library`.
    ///
    /// # Safety
    /// Caller must ensure unloading all libraries is safe with respect to external threads.
    pub unsafe fn unload_all(&mut self) {
        for loaded in self.loaded_libraries.drain(..).rev() {
            loaded.is_active.store(false, Ordering::Release);
            if let Some(shutdown) = loaded.shutdown_fn {
                // SAFETY: Invoking mod teardown hook prior to dropping `loaded.library` (`dlclose`).
                let _ = unsafe { shutdown(core::ptr::null_mut()) };
            }
            drop(loaded.library);
        }
    }

    /// Number of loaded mod libraries.
    pub fn len(&self) -> usize {
        self.loaded_libraries.len()
    }

    /// True if no mod libraries are loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded_libraries.is_empty()
    }
}

impl Drop for ModHost {
    fn drop(&mut self) {
        // SAFETY: Unloading all remaining mods and releasing resources.
        unsafe {
            self.unload_all();
        }
    }
}
