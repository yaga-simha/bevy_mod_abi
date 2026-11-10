#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ExposableComponent, FfiResult, GetProcAddressFn};
use core::ffi::{c_char, c_void, CStr};
use host_runtime::ModHost;
use libloading::{Library, Symbol};
use mod_derive::ModExposable;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct CInteropComponent {
    pub health: i32,
    pub pos: Vec3,
    pub label: String,
}

unsafe extern "C" fn test_get_proc_address(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller passes a valid NUL-terminated C string name.
    let Ok(sym_str) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return core::ptr::null();
    };
    if let Some(ptr) = CInteropComponent::resolve_symbol(sym_str) {
        return ptr;
    }
    if let Some(ptr) = Vec3::resolve_symbol(sym_str) {
        return ptr;
    }
    core::ptr::null()
}

type CModTestVTableMutationFn = unsafe extern "C" fn(
    get_proc_address: GetProcAddressFn,
    column_base_ptr: *mut c_void,
    count: usize,
) -> FfiResult;

#[test]
fn test_c99_shared_library_mutates_rust_components_via_vtable() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let c_mod_dir = manifest_dir.join("../../tests/asymmetric_c_mod");

    let dylib_name = if cfg!(target_os = "macos") {
        "libasymmetric_c_mod.dylib"
    } else if cfg!(target_os = "windows") {
        "asymmetric_c_mod.dll"
    } else {
        "libasymmetric_c_mod.so"
    };
    let dylib_path = c_mod_dir.join(dylib_name);

    // Compile the C99 shared library via `make` if missing
    if !dylib_path.exists() {
        let status = Command::new("make")
            .arg("-C")
            .arg(&c_mod_dir)
            .status()
            .expect("failed to invoke `make` for asymmetric_c_mod");
        assert!(status.success(), "`make -C tests/asymmetric_c_mod` failed");
    }

    // 1. Verify `ModHost::load_mod` handshake and ABI version check succeed
    let mut host = ModHost::new();
    // SAFETY: `dylib_path` was just compiled from `tests/asymmetric_c_mod/mod.c`.
    unsafe {
        host.load_mod(&dylib_path)
            .expect("ModHost failed to load C99 mod");
    }

    // 2. Invoke `bevy_c_mod_test_vtable_mutation` exported by the C99 shared library
    let mut column: Vec<CInteropComponent> = (0..16)
        .map(|i| CInteropComponent {
            health: i * 10,
            pos: Vec3 {
                x: i as f32,
                y: (i * 2) as f32,
                z: (i * 3) as f32,
            },
            label: format!("rust_entity_{i}"),
        })
        .collect();

    // SAFETY: Loading the compiled C99 shared library and invoking its exported test entry point.
    unsafe {
        let lib = Library::new(&dylib_path).expect("failed to open C99 shared library");
        let test_fn: Symbol<CModTestVTableMutationFn> = lib
            .get(b"bevy_c_mod_test_vtable_mutation\0")
            .expect("missing bevy_c_mod_test_vtable_mutation symbol");

        let res = test_fn(
            test_get_proc_address,
            column.as_mut_ptr() as *mut c_void,
            column.len(),
        );
        assert_eq!(res, FfiResult::OK);
    }

    for (i, item) in column.iter().enumerate() {
        assert_eq!(item.health, (i as i32) * 10 + 1000);
        assert_eq!(item.pos.x, (i as f32) + 10.5);
        assert_eq!(item.pos.y, (i as f32) * 2.0 + 20.5);
        assert_eq!(item.pos.z, (i as f32) * 3.0 + 30.5);
        assert_eq!(item.label, "mutated_from_c99");
    }

    drop(host);
}
