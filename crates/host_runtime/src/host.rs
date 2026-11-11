#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Dynamic Mod Host & Lifecycle Management
//!
//! This module manages the lifecycle of dynamic shared library mods (`.so`, `.dylib`, `.dll`)
//! loaded at runtime via [`libloading`].
//!
//! ## Dynamic Lifecycle Architecture
//! 1. **Loading (`dlopen`) & Semver Handshake**:
//!    - Dynamically loads the shared library from the specified path.
//!    - Resolves optional version handshake `bevy_abi_mod_abi_version` using semver compatibility
//!      ([`is_abi_version_compatible`]: matching major version and `guest_minor <= host_minor`).
//!    - Resolves mandatory initialization entry point `bevy_abi_mod_init` and optional
//!      teardown hook `bevy_abi_mod_shutdown`.
//! 2. **Execution & The `is_active: Arc<AtomicBool>` Tombstone Guard**:
//!    - Every [`DynamicCSystem`](crate::dynamic_system::DynamicCSystem) shares an `Arc<AtomicBool>`
//!      tombstone guard with its [`LoadedMod`] and belongs to [`ModSystemSet::Mod`](crate::dynamic_system::ModSystemSet::Mod).
//!    - On unload, `is_active` becomes `false` and [`ModAppExt::unload_mod`](crate::plugin::ModAppExt::unload_mod)
//!      excises the mod's systems from Bevy's [`Schedules`](bevy_ecs::schedule::Schedules).
//! 3. **4-Phase Shutdown Sequence**:
//!    - **Phase 1: Deactivation**: `is_active.store(false, Ordering::Release)` prevents any further system runs.
//!    - **Phase 2: Guest Teardown Callback**: `shutdown_fn(null_mut())` is called while library memory
//!      is still mapped when the final active instance of that library image unloads.
//!    - **Phase 3: Dynamic Unmapping**: Dropping `loaded.library` unmaps the shared object from process memory.
//!    - **Phase 4: Reverse-Order Unloading (LIFO)**: `unload_all` unloads libraries in reverse load order.

use crate::dynamic_system::{DynamicCSystem, ModInitContext};
use crate::registry::{host_get_proc_address, ModRegistry};
use crate::HOST_API;
use abi_contract::{FfiResult, ModAbiVersionFn, ModInitFn, ModShutdownFn, ABI_VERSION_PACKED};
use bevy_ecs::prelude::Resource;
use core::ffi::c_void;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use libloading::{Library, Symbol};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// Checks semver compatibility between packed `(major << 24) | (minor << 16) | patch` ABI versions.
///
/// Delegates directly to [`abi_contract::abi_version_is_compatible`].
#[inline]
pub const fn is_abi_version_compatible(host_packed: u32, guest_packed: u32) -> bool {
    abi_contract::abi_version_is_compatible(host_packed, guest_packed)
}

/// Rich, typed diagnostic error returned when loading or unloading a native mod fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModLoadError {
    /// Failed to open the shared library via OS dynamic linker (`dlopen` / `LoadLibrary`).
    DlOpen {
        /// Path passed to the loader.
        path: PathBuf,
        /// Underlying OS linker error message.
        message: String,
    },
    /// Required symbol (`bevy_abi_mod_init`) was not exported by the shared library.
    MissingEntryPoint {
        /// Path of the loaded library.
        path: PathBuf,
        /// Missing symbol name.
        symbol: &'static str,
        /// Underlying symbol lookup error message.
        message: String,
    },
    /// Guest `bevy_abi_mod_abi_version` reported an incompatible packed version.
    IncompatibleAbiVersion {
        /// Path of the loaded library.
        path: PathBuf,
        /// Host's `ABI_VERSION_PACKED`.
        host_version: u32,
        /// Guest's reported packed ABI version.
        guest_version: u32,
    },
    /// Guest `bevy_abi_mod_init` returned a non-OK [`FfiResult`].
    InitFailed {
        /// Path of the loaded library.
        path: PathBuf,
        /// Error status returned by `bevy_abi_mod_init`.
        status: FfiResult,
    },
    /// Guest `bevy_abi_mod_init` unwound across the initialization boundary.
    InitPanicked {
        /// Path of the loaded library.
        path: PathBuf,
    },
    /// Strict validation failed because a guest system queried unregistered components.
    MissingComponents {
        /// Path of the loaded library.
        path: PathBuf,
        /// Guest system name requesting unknown components.
        system_name: String,
        /// Unregistered component names.
        missing: Vec<String>,
    },
    /// Attempted to unload a [`ModId`] that is not currently loaded in [`ModHost`].
    ModNotFound {
        /// Unknown mod identifier.
        id: ModId,
    },
}

impl ModLoadError {
    /// Maps this rich [`ModLoadError`] onto its C-ABI [`FfiResult`] status equivalent.
    #[inline]
    pub const fn to_ffi_result(&self) -> FfiResult {
        match self {
            Self::DlOpen { .. } | Self::MissingEntryPoint { .. } | Self::ModNotFound { .. } => {
                FfiResult::INTERNAL_ERROR
            }
            Self::IncompatibleAbiVersion { .. } => FfiResult::INVALID_VERSION,
            Self::InitFailed { status, .. } => *status,
            Self::InitPanicked { .. } => FfiResult::PANIC_CAUGHT,
            Self::MissingComponents { .. } => FfiResult::INVALID_COMPONENT,
        }
    }
}

impl From<ModLoadError> for FfiResult {
    #[inline]
    fn from(err: ModLoadError) -> Self {
        err.to_ffi_result()
    }
}

impl fmt::Display for ModLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DlOpen { path, message } => {
                write!(f, "failed to load mod '{}': {}", path.display(), message)
            }
            Self::MissingEntryPoint {
                path,
                symbol,
                message,
            } => write!(
                f,
                "mod '{}' is missing required symbol '{}': {}",
                path.display(),
                symbol,
                message
            ),
            Self::IncompatibleAbiVersion {
                path,
                host_version,
                guest_version,
            } => write!(
                f,
                "mod '{}' reported incompatible ABI version 0x{:08x} (host expects compatible with 0x{:08x})",
                path.display(),
                guest_version,
                host_version
            ),
            Self::InitFailed { path, status } => write!(
                f,
                "mod '{}' initialization failed with {:?}",
                path.display(),
                status
            ),
            Self::InitPanicked { path } => {
                write!(f, "mod '{}' panicked during initialization", path.display())
            }
            Self::MissingComponents {
                path,
                system_name,
                missing,
            } => write!(
                f,
                "mod '{}' system '{}' references unregistered component(s): {}",
                path.display(),
                system_name,
                missing.join(", ")
            ),
            Self::ModNotFound { id } => write!(f, "mod {:?} is not loaded", id),
        }
    }
}

impl std::error::Error for ModLoadError {}

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
    /// Filesystem path from which the library was loaded.
    pub path: PathBuf,
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

    /// Core deduplicated loader shared by both registry-only and `World`-backed mod loading.
    ///
    /// # Safety
    /// - `path` must point to a trusted, valid mod shared library.
    /// - If `world_ptr` is non-null, it must point to a valid mutable [`bevy_ecs::world::World`].
    unsafe fn load_mod_core(
        &mut self,
        path: &Path,
        registry: Arc<ModRegistry>,
        world_ptr: *mut bevy_ecs::world::World,
    ) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
        let lib_state = shared_lib_state_for_path(path);
        let mut state_guard = lib_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // SAFETY: Caller guarantees `path` points to a valid dynamic library safe to load.
        let lib = unsafe { Library::new(path) }.map_err(|err| ModLoadError::DlOpen {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;

        // Optional ABI version handshake check (semver-compatible: matching major, guest_minor <= host_minor)
        // SAFETY: If `bevy_abi_mod_abi_version` is present, it has signature `unsafe extern "C" fn() -> u32`.
        if let Ok(ver_fn) = unsafe { lib.get::<ModAbiVersionFn>(b"bevy_abi_mod_abi_version\0") } {
            // SAFETY: Calling side-effect-free version query symbol exported by the mod.
            let guest_version = unsafe { ver_fn() };
            if !is_abi_version_compatible(ABI_VERSION_PACKED, guest_version) {
                return Err(ModLoadError::IncompatibleAbiVersion {
                    path: path.to_path_buf(),
                    host_version: ABI_VERSION_PACKED,
                    guest_version,
                });
            }
        }

        // SAFETY: Resolving standard C-ABI entry point `bevy_abi_mod_init`.
        let init_fn: Symbol<ModInitFn> =
            unsafe { lib.get(b"bevy_abi_mod_init\0") }.map_err(|err| {
                ModLoadError::MissingEntryPoint {
                    path: path.to_path_buf(),
                    symbol: "bevy_abi_mod_init",
                    message: err.to_string(),
                }
            })?;

        // SAFETY: Resolving optional C-ABI teardown point `bevy_abi_mod_shutdown`.
        let shutdown_fn: Option<ModShutdownFn> = unsafe { lib.get(b"bevy_abi_mod_shutdown\0") }
            .ok()
            .map(|sym| *sym);

        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx =
            ModInitContext::with_world(Arc::clone(&registry), Arc::clone(&is_active), world_ptr);

        let _guard = registry.make_current();
        let _init_guard = init_ctx.make_active();

        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();
        // SAFETY: Passing valid `host_get_proc_address`, static `HOST_API`, and `ModInitContext`.
        let init_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            init_fn(host_get_proc_address, &HOST_API, ctx_ptr)
        }));

        let status = match init_res {
            Ok(status) => status,
            Err(_) => {
                return Err(ModLoadError::InitPanicked {
                    path: path.to_path_buf(),
                });
            }
        };

        if status.is_err() {
            return Err(ModLoadError::InitFailed {
                path: path.to_path_buf(),
                status,
            });
        }

        let mod_id = ModId(self.next_mod_id);
        self.next_mod_id += 1;

        // Ensure all systems registered during init share the final ModRegistry snapshot
        // (including any dynamic components registered after an earlier register_system call)
        // and carry `mod_id` for `ModSystemSet::Mod(mod_id)`.
        for sys in &mut init_ctx.registered_systems {
            sys.set_registry(Arc::clone(&init_ctx.registry));
            sys.set_mod_id(mod_id);
        }

        state_guard.active_count += 1;
        drop(state_guard);

        self.loaded_libraries.push(LoadedMod {
            id: mod_id,
            name: path.to_string_lossy().to_string(),
            path: path.to_path_buf(),
            library: lib,
            shutdown_fn,
            is_active,
            lib_state,
        });

        Ok((mod_id, init_ctx.registered_systems))
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

    /// Typed variant of [`Self::load_mod_tracked_with_registry`] returning [`ModLoadError`].
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn try_load_mod_tracked_with_registry<P: AsRef<Path>>(
        &mut self,
        path: P,
        registry: &Arc<ModRegistry>,
    ) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
        // SAFETY: Caller upholds library validity; `world_ptr` is null for registry-only loads.
        unsafe {
            self.load_mod_core(
                path.as_ref(),
                Arc::clone(registry),
                core::ptr::null_mut(),
            )
        }
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
        // SAFETY: Delegating to typed `try_load_mod_tracked_with_registry`.
        unsafe {
            self.try_load_mod_tracked_with_registry(path, registry)
                .map_err(ModLoadError::into)
        }
    }

    /// Typed variant of [`Self::load_mod_tracked_with_world`] returning [`ModLoadError`].
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn try_load_mod_tracked_with_world<P: AsRef<Path>>(
        &mut self,
        path: P,
        world: &mut bevy_ecs::world::World,
    ) -> Result<(ModId, Vec<DynamicCSystem>), ModLoadError> {
        if !world.contains_resource::<ModRegistry>() {
            world.insert_resource(ModRegistry::new());
        }
        let registry = Arc::new(world.get_resource::<ModRegistry>().unwrap().clone());
        let world_ptr = world as *mut bevy_ecs::world::World;
        // SAFETY: Caller upholds library validity; `world_ptr` points to valid `&mut World`.
        unsafe { self.load_mod_core(path.as_ref(), registry, world_ptr) }
    }

    /// Load a native mod library with a mutable reference to the Bevy [`World`](bevy_ecs::world::World),
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
        // SAFETY: Delegating to typed `try_load_mod_tracked_with_world`.
        unsafe {
            self.try_load_mod_tracked_with_world(path, world)
                .map_err(ModLoadError::into)
        }
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

    /// Returns `true` if a mod with the given [`ModId`] is currently loaded.
    pub fn contains_mod(&self, id: ModId) -> bool {
        self.loaded_libraries.iter().any(|m| m.id == id)
    }

    /// Borrows the [`LoadedMod`] entry for `id`, if currently loaded.
    pub fn get_mod(&self, id: ModId) -> Option<&LoadedMod> {
        self.loaded_libraries.iter().find(|m| m.id == id)
    }

    /// Returns a slice of all currently loaded mod records.
    pub fn loaded_mods(&self) -> &[LoadedMod] {
        &self.loaded_libraries
    }

    /// Atomically sets `is_active = false` for `id` prior to schedule graph cleanup.
    pub(crate) fn deactivate_mod(&self, id: ModId) {
        if let Some(m) = self.loaded_libraries.iter().find(|m| m.id == id) {
            m.is_active.store(false, Ordering::Release);
        }
    }

    /// Typed unload returning the unloaded mod's [`PathBuf`] or [`ModLoadError::ModNotFound`].
    ///
    /// # Safety
    /// Caller must ensure unloading the dynamic library is safe with respect to external threads.
    pub unsafe fn try_unload_mod(&mut self, id: ModId) -> Result<PathBuf, ModLoadError> {
        let pos = self
            .loaded_libraries
            .iter()
            .position(|m| m.id == id)
            .ok_or(ModLoadError::ModNotFound { id })?;
        let loaded = self.loaded_libraries.remove(pos);
        let path = loaded.path.clone();
        // SAFETY: Caller guarantees unloading this mod is safe with respect to external threads.
        unsafe {
            unload_loaded_mod(loaded);
        }
        Ok(path)
    }

    /// Unload a specific mod by its [`ModId`]:
    /// Deactivates its `is_active` atomic guard, invokes `shutdown_fn` if it is the last active
    /// instance of that shared library image, and drops its `Library`.
    ///
    /// # Safety
    /// Caller must ensure unloading the dynamic library is safe with respect to external threads.
    pub unsafe fn unload_mod(&mut self, id: ModId) -> Result<(), FfiResult> {
        // SAFETY: Delegating to typed `try_unload_mod`.
        unsafe { self.try_unload_mod(id).map(|_| ()).map_err(ModLoadError::into) }
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
