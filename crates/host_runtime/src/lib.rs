#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `host_runtime`
//!
//! Bevy host runtime plugin, VTable registry, dynamic system execution,
//! and `libloading` lifecycle manager for `bevy_abi_mod`.

use abi_contract::{HostApi, ModInitFn, ModShutdownFn, ModStatus};
use libloading::{Library, Symbol};
use std::path::Path;

pub struct ModHost {
    loaded_libraries: Vec<LoadedMod>,
}

pub struct LoadedMod {
    pub name: String,
    pub library: Library,
    pub shutdown_fn: Option<ModShutdownFn>,
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
        }
    }

    /// Load a native mod library and perform the initial C-ABI handshake.
    ///
    /// # Safety
    /// Loading a dynamic library executes foreign initialization code (`bevy_abi_mod_init`).
    /// The caller must ensure `path` points to a trusted, valid mod shared library.
    pub unsafe fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<(), ModStatus> {
        let path = path.as_ref();
        // SAFETY: Caller guarantees `path` points to a valid dynamic library safe to load.
        let lib = unsafe { Library::new(path) }.map_err(|_| ModStatus::ErrInternal)?;

        // SAFETY: Resolving standard C-ABI entry point `bevy_abi_mod_init` with signature `ModInitFn`.
        let init_fn: Symbol<ModInitFn> = unsafe { lib.get(b"bevy_abi_mod_init\0") }
            .map_err(|_| ModStatus::ErrInternal)?;

        // SAFETY: Resolving optional C-ABI teardown point `bevy_abi_mod_shutdown` with signature `ModShutdownFn`.
        let shutdown_fn: Option<ModShutdownFn> = unsafe { lib.get(b"bevy_abi_mod_shutdown\0") }
            .ok()
            .map(|sym| *sym);

        let host_api = HostApi {
            register_system: stub_register_system,
        };

        let status = init_fn(stub_get_proc_address, &host_api, std::ptr::null_mut());
        if status != ModStatus::Success {
            return Err(status);
        }

        self.loaded_libraries.push(LoadedMod {
            name: path.to_string_lossy().to_string(),
            library: lib,
            shutdown_fn,
        });

        Ok(())
    }
}

unsafe extern "C-unwind" fn stub_get_proc_address(
    _name: *const core::ffi::c_char,
) -> *const core::ffi::c_void {
    std::ptr::null()
}

extern "C" fn stub_register_system(
    _host_context: *mut core::ffi::c_void,
    _descriptor: *const abi_contract::SystemAccessDescriptor,
    _callback: abi_contract::SystemRunCallback,
    _user_data: *mut core::ffi::c_void,
) -> ModStatus {
    ModStatus::Success
}
