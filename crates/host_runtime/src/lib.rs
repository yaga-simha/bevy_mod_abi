#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `host_runtime`
//!
//! Bevy host runtime plugin, VTable registry, dynamic system execution,
//! and `libloading` lifecycle manager for `bevy_abi_mod`.

use abi_contract::{
    FfiEntity, FfiResult, HostApi, ModAbiVersionFn, ModInitFn, ModShutdownFn, RawChunkIter,
    SystemDescriptor, ABI_VERSION_PACKED,
};
use core::ffi::{c_char, c_void};
use libloading::{Library, Symbol};
use std::path::Path;

static HOST_API: HostApi = HostApi {
    struct_size: core::mem::size_of::<HostApi>() as u32,
    abi_version: ABI_VERSION_PACKED,
    register_system: stub_register_system,
    iter_next_chunk: stub_iter_next_chunk,
    iter_column_readonly: stub_iter_column_readonly,
    iter_column_mutable: stub_iter_column_mutable,
    iter_mark_changed: stub_iter_mark_changed,
    iter_entities: stub_iter_entities,
    iter_column_stride: stub_iter_column_stride,
};

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
    pub unsafe fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<(), FfiResult> {
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

        // SAFETY: Resolving standard C-ABI entry point `bevy_abi_mod_init` with signature `ModInitFn`.
        let init_fn: Symbol<ModInitFn> = unsafe { lib.get(b"bevy_abi_mod_init\0") }
            .map_err(|_| FfiResult::INTERNAL_ERROR)?;

        // SAFETY: Resolving optional C-ABI teardown point `bevy_abi_mod_shutdown` with signature `ModShutdownFn`.
        let shutdown_fn: Option<ModShutdownFn> = unsafe { lib.get(b"bevy_abi_mod_shutdown\0") }
            .ok()
            .map(|sym| *sym);

        // SAFETY: Passing valid `stub_get_proc_address` function pointer and `'static` `HOST_API` reference.
        let status = unsafe { init_fn(stub_get_proc_address, &HOST_API, std::ptr::null_mut()) };
        if status.is_err() {
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

impl Drop for ModHost {
    fn drop(&mut self) {
        for loaded in self.loaded_libraries.drain(..).rev() {
            if let Some(shutdown) = loaded.shutdown_fn {
                // SAFETY: Invoking the mod's teardown hook prior to dropping `loaded.library` (`dlclose`).
                let _ = unsafe { shutdown(std::ptr::null_mut()) };
            }
        }
    }
}

unsafe extern "C" fn stub_get_proc_address(_name: *const c_char) -> *const c_void {
    std::ptr::null()
}

unsafe extern "C" fn stub_register_system(
    _host_context: *mut c_void,
    _descriptor: *const SystemDescriptor,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_iter_next_chunk(_iter: *mut RawChunkIter) -> usize {
    0
}

unsafe extern "C" fn stub_iter_column_readonly(
    _iter: *const RawChunkIter,
    _index: usize,
) -> *const u8 {
    std::ptr::null()
}

unsafe extern "C" fn stub_iter_column_mutable(
    _iter: *mut RawChunkIter,
    _index: usize,
) -> *mut u8 {
    std::ptr::null_mut()
}

unsafe extern "C" fn stub_iter_mark_changed(
    _iter: *mut RawChunkIter,
    _index: usize,
    _row: usize,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_iter_entities(_iter: *const RawChunkIter) -> *const FfiEntity {
    std::ptr::null()
}

unsafe extern "C" fn stub_iter_column_stride(_iter: *const RawChunkIter, _index: usize) -> usize {
    0
}
