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

impl ModHost {
    pub fn new() -> Self {
        Self {
            loaded_libraries: Vec::new(),
        }
    }

    /// Load a native mod library and perform the initial C-ABI handshake.
    pub unsafe fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<(), ModStatus> {
        let path = path.as_ref();
        let lib = Library::new(path).map_err(|_| ModStatus::ErrInternal)?;

        let init_fn: Symbol<ModInitFn> = lib
            .get(b"bevy_abi_mod_init\0")
            .map_err(|_| ModStatus::ErrInternal)?;

        let shutdown_fn: Option<ModShutdownFn> = lib
            .get(b"bevy_abi_mod_shutdown\0")
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

extern "C" fn stub_get_proc_address(_name: *const core::ffi::c_char) -> *const core::ffi::c_void {
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
