#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Dynamic Mod Host & Lifecycle Management
//!
//! This module manages the lifecycle of dynamic shared library mods (`.so`, `.dylib`, `.dll`)
//! loaded at runtime via [`libloading`].
//!
//! ## Dynamic Lifecycle Architecture
//! 1. **Loading (`dlopen`)**:
//!    - Dynamically loads the shared library from the specified path.
//!    - Resolves optional version handshake: `bevy_abi_mod_abi_version`.
//!    - Resolves mandatory initialization entry point: `bevy_abi_mod_init`.
//!    - Resolves optional teardown hook: `bevy_abi_mod_shutdown`.
//! 2. **Execution & The `is_active: Arc<AtomicBool>` Tombstone Guard**:
//!    - In Bevy ECS, systems added to a schedule graph cannot be easily excised mid-run.
//!    - When a mod is unloaded, dropping [`libloading::Library`] issues `dlclose()`, unmapping
//!      the executable `.text` pages of the shared library from memory.
//!    - If the scheduler subsequently invoked the foreign function pointer, a fatal SIGSEGV
//!      or access violation would crash the process.
//!    - To prevent this, every [`DynamicCSystem`](crate::dynamic_system::DynamicCSystem) shares
//!      an `Arc<AtomicBool>` tombstone guard with its [`LoadedMod`].
//!    - When unloaded, `is_active.store(false, Ordering::Release)` is executed.
//!    - On subsequent frames, `DynamicCSystem::run_unsafe` checks `is_active.load(Ordering::Acquire)`
//!      and instantly returns `Ok(())` without executing foreign instructions.
//! 3. **4-Phase Shutdown Sequence**:
//!    - **Phase 1: Deactivation**: `is_active.store(false, Ordering::Release)` prevents any further system runs.
//!    - **Phase 2: Guest Teardown Callback**: `shutdown_fn(null_mut())` is called while library memory
//!      is still mapped, allowing the guest to cleanly free heap allocations, stop threads, etc.
//!    - **Phase 3: Dynamic Unmapping**: Dropping `loaded.library` unmaps the shared object from process memory.
//!    - **Phase 4: Reverse-Order Unloading (LIFO)**: `unload_all` unloads libraries in reverse load order,
//!      preventing cascading dangling pointer issues between interdependent mods.

use crate::dynamic_system::{DynamicCSystem, ModInitContext};
use crate::registry::{host_get_proc_address, ModRegistry};
use crate::HOST_API;
use abi_contract::{FfiResult, ModAbiVersionFn, ModInitFn, ModShutdownFn, ABI_VERSION_PACKED};
use bevy_ecs::prelude::Resource;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use libloading::{Library, Symbol};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// OS-level identity of a shared library file so concurrent `ModHost` instances
/// loading the same `.so`/`.dylib`/`.dll` coordinate `bevy_abi_mod_init` and
/// `bevy_abi_mod_shutdown` across their shared `.data`/`.bss` segment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SharedLibKey {
    #[cfg(unix)]
    UnixInode { dev: u64, ino: u64 },
    CanonicalPath(PathBuf),
}

impl SharedLibKey {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(path) {
                return Self::UnixInode {
                    dev: meta.dev(),
                    ino: meta.ino(),
                };
            }
        }
        Self::CanonicalPath(
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        )
    }
}

#[derive(Default)]
struct SharedLibState {
    active_count: usize,
}

static ACTIVE_SHARED_LIBS: LazyLock<Mutex<HashMap<SharedLibKey, Arc<Mutex<SharedLibState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn shared_lib_state_for_path(path: &Path) -> Arc<Mutex<SharedLibState>> {
    let key = SharedLibKey::from_path(path);
    let mut map = ACTIVE_SHARED_LIBS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(SharedLibState::default()))),
    )
}

/// Unique generational identifier for a loaded mod instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModId(pub u32);

/// Internal record representing a loaded native mod shared library.
pub struct LoadedMod {
    /// Unique identifier assigned to this mod.
    pub id: ModId,
    /// Path or human-readable name of the loaded library.
    pub name: String,
    /// Handle to the dynamically loaded shared library (`dlopen`).
    pub library: Library,
    /// Optional guest shutdown callback resolved from the library.
    pub shutdown_fn: Option<ModShutdownFn>,
    /// Atomic tombstone flag shared with all systems spawned by this mod.
    pub is_active: Arc<AtomicBool>,
    /// Process-wide per-image lifecycle synchronization state.
    lib_state: Arc<Mutex<SharedLibState>>,
}

/// Deactivates a [`LoadedMod`], decrements its process-wide shared-library reference count,
/// invokes `bevy_abi_mod_shutdown` only when the final active instance unloads, and drops
/// the OS library handle under the per-image lock.
///
/// # Safety
/// Caller must ensure unloading the dynamic library is safe with respect to external threads.
unsafe fn unload_loaded_mod(loaded: LoadedMod) {
    loaded.is_active.store(false, Ordering::Release);
    let mut state_guard = loaded
        .lib_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state_guard.active_count = state_guard.active_count.saturating_sub(1);
    if state_guard.active_count == 0 {
        if let Some(shutdown) = loaded.shutdown_fn {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: Invoking mod teardown hook while the shared library remains mapped
                // and only after the final active ModHost reference is being released.
                unsafe { shutdown(core::ptr::null_mut()) }
            }));
        }
    }
    drop(loaded.library);
}

/// Host resource tracking loaded dynamic libraries and managing their lifecycle.
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
    /// Creates a new, empty mod host manager.
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
        let lib_state = shared_lib_state_for_path(path);
        let mut state_guard = lib_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

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
        let _init_guard = init_ctx.make_active();

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

        state_guard.active_count += 1;
        drop(state_guard);

        let mod_id = ModId(self.next_mod_id);
        self.next_mod_id += 1;

        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            name: path.to_string_lossy().to_string(),
            library: lib,
            shutdown_fn,
            is_active,
            lib_state,
        });

        Ok((mod_id, init_ctx.registered_systems))
    }

    /// Load a native mod library with a mutable reference to the Bevy [`World`],
    /// allowing dynamic components declared at runtime by the mod to be registered
    /// into the engine's ECS world and [`ModRegistry`].
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn load_mod_tracked_with_world<P: AsRef<Path>>(
        &mut self,
        path: P,
        world: &mut bevy_ecs::world::World,
    ) -> Result<(ModId, Vec<DynamicCSystem>), FfiResult> {
        let path = path.as_ref();
        let lib_state = shared_lib_state_for_path(path);
        let mut state_guard = lib_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // SAFETY: Caller guarantees `path` points to a valid dynamic library safe to load.
        let lib = unsafe { Library::new(path) }.map_err(|_| FfiResult::INTERNAL_ERROR)?;

        // Optional ABI version handshake check
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

        if !world.contains_resource::<ModRegistry>() {
            world.insert_resource(ModRegistry::new());
        }
        let registry = Arc::new(world.get_resource::<ModRegistry>().unwrap().clone());

        let mut init_ctx = ModInitContext::with_world(
            Arc::clone(&registry),
            Arc::clone(&is_active),
            world as *mut bevy_ecs::world::World,
        );

        let _guard = registry.make_current();
        let _init_guard = init_ctx.make_active();

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

        // Ensure all systems registered during init share the final ModRegistry snapshot
        // (including any dynamic components registered after an earlier register_system call).
        for sys in &mut init_ctx.registered_systems {
            sys.set_registry(Arc::clone(&init_ctx.registry));
        }

        state_guard.active_count += 1;
        drop(state_guard);

        let mod_id = ModId(self.next_mod_id);
        self.next_mod_id += 1;

        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            name: path.to_string_lossy().to_string(),
            library: lib,
            shutdown_fn,
            is_active,
            lib_state,
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

    /// Unload a specific mod by its [`ModId`]:
    /// Deactivates its `is_active` atomic guard, invokes `shutdown_fn` if it is the last active
    /// instance of that shared library image, and drops its `Library`.
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
        // SAFETY: Caller guarantees unloading this mod is safe with respect to external threads.
        unsafe {
            unload_loaded_mod(loaded);
        }
        Ok(())
    }

    /// Unload all loaded mods in reverse load order (LIFO):
    /// Deactivates each mod's `is_active` atomic guard, invokes `shutdown_fn` when the final
    /// active reference to each shared library image is released, and drops `Library`.
    ///
    /// # Safety
    /// Caller must ensure unloading all libraries is safe with respect to external threads.
    pub unsafe fn unload_all(&mut self) {
        for loaded in self.loaded_libraries.drain(..).rev() {
            // SAFETY: Caller guarantees unloading all mods is safe with respect to external threads.
            unsafe {
                unload_loaded_mod(loaded);
            }
        }
    }

    /// Returns the number of currently loaded mod libraries.
    pub fn len(&self) -> usize {
        self.loaded_libraries.len()
    }

    /// Returns `true` if no mod libraries are currently loaded.
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
