use abi_contract::ComponentUuid;
use libloading::{Library, Symbol};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

type CUuidFn = unsafe extern "C" fn(*const core::ffi::c_char, *mut u8);

fn compile_and_load_c_uuid_oracle() -> (Library, PathBuf) {
    let root = get_workspace_root();
    let target_dir = root.join("target").join("challenger_test");
    fs::create_dir_all(&target_dir).expect("Failed to create target/challenger_test directory");

    let c_source_path = target_dir.join("c_uuid_oracle.c");
    let dylib_name = if cfg!(target_os = "windows") {
        "c_uuid_oracle.dll"
    } else if cfg!(target_os = "macos") {
        "libc_uuid_oracle.dylib"
    } else {
        "libc_uuid_oracle.so"
    };
    let dylib_path = target_dir.join(dylib_name);

    let c_code = r#"
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "../../include/bevy_abi_mod.h"

BEVY_ABI_MOD_EXPORT void c_uuid_from_name(const char* name, uint8_t* out_bytes) {
    ComponentUuid id = bevy_abi_uuid_from_name(name);
    memcpy(out_bytes, id.bytes, 16);
}
"#;

    fs::write(&c_source_path, c_code).expect("Failed to write c_uuid_oracle.c");

    let compiler = if cfg!(target_os = "windows") {
        "gcc"
    } else {
        "cc"
    };
    let mut cmd = Command::new(compiler);
    cmd.args([
        "-std=c99",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-I",
        root.join("include").to_str().unwrap(),
        "-shared",
    ]);
    if !cfg!(target_os = "windows") {
        cmd.arg("-fPIC");
    }
    let status = cmd
        .args([
            "-O3",
            "-o",
            dylib_path.to_str().unwrap(),
            c_source_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to compile c_uuid_oracle.c");

    assert!(status.success(), "{compiler} failed to build {dylib_name}");

    let lib = unsafe { Library::new(&dylib_path) }.expect("Failed to load c_uuid_oracle library");
    (lib, dylib_path)
}

#[test]
fn test_cross_language_uuid_parity_matrix() {
    let (lib, _dylib_path) = compile_and_load_c_uuid_oracle();
    let c_uuid_fn: Symbol<CUuidFn> = unsafe {
        lib.get(b"c_uuid_from_name\0")
            .expect("Failed to resolve c_uuid_from_name")
    };

    let test_cases = [
        // 1. Standard ECS names
        "Position",
        "Velocity",
        "CombatHealth",
        "MetadataTag",
        "RustModCustomData",
        "CModCustomData",
        "PlayerTag",
        "DroneTag",
        "Transform",
        "GlobalTransform",
        // 2. Boundary & short names
        "",
        "a",
        "A",
        "0",
        "_",
        "ab",
        "abc",
        "abcd",
        "12345678",
        "1234567890123456",
        // 3. Names with special characters & whitespace
        "Comp With Space",
        "Comp::With::Colons",
        "path/to/component",
        "hyphen-separated-component",
        "under_score_component",
        "!@#$%^&*()_+-=[]{}|;':,.<>?~`",
        // 4. Unicode & multi-byte UTF-8
        "Position_🚀",
        "Fire_🔥_Shield",
        "你好_Component",
        "한국어_컴포넌트",
        "日本語_コンポーネント",
        "русский_компонент",
        "München",
        "café_au_lait",
        "Größe_und_Länge",
    ];

    for name in &test_cases {
        let rust_uuid = ComponentUuid::from_name(name);

        let c_str = std::ffi::CString::new(*name).unwrap();
        let mut c_bytes = [0u8; 16];
        unsafe {
            c_uuid_fn(c_str.as_ptr(), c_bytes.as_mut_ptr());
        }

        assert_eq!(
            rust_uuid.bytes, c_bytes,
            "UUID mismatch for name '{name}': Rust={:?} vs C={:?}",
            rust_uuid.bytes, c_bytes
        );
    }

    // 5. Long names (up to 4096 bytes)
    for len in [64, 128, 256, 512, 1024, 2048, 4096] {
        let long_name: String = (0..len)
            .map(|i| {
                let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_";
                chars[i % chars.len()] as char
            })
            .collect();

        let rust_uuid = ComponentUuid::from_name(&long_name);
        let c_str = std::ffi::CString::new(long_name.clone()).unwrap();
        let mut c_bytes = [0u8; 16];
        unsafe {
            c_uuid_fn(c_str.as_ptr(), c_bytes.as_mut_ptr());
        }

        assert_eq!(
            rust_uuid.bytes, c_bytes,
            "UUID mismatch for long name of length {len}"
        );
    }

    // 6. Null pointer behavior in C
    let mut c_null_bytes = [0u8; 16];
    unsafe {
        c_uuid_fn(core::ptr::null(), c_null_bytes.as_mut_ptr());
    }
    assert_eq!(
        c_null_bytes, [0u8; 16],
        "C bevy_abi_uuid_from_name(NULL) must return all-zeros UUID"
    );

    // 7. Empty string comparison
    // In C, bevy_abi_uuid_from_name("") should match ComponentUuid::from_name("")
    let empty_rust = ComponentUuid::from_name("");
    let c_empty_str = std::ffi::CString::new("").unwrap();
    let mut c_empty_bytes = [0u8; 16];
    unsafe {
        c_uuid_fn(c_empty_str.as_ptr(), c_empty_bytes.as_mut_ptr());
    }
    assert_eq!(
        empty_rust.bytes, c_empty_bytes,
        "Empty string UUID must match byte-for-byte between C and Rust"
    );
    assert_ne!(
        c_empty_bytes, [0u8; 16],
        "Empty string UUID must be non-zero (hash basis value)"
    );

    // 8. Collision resistance check across test set
    let mut set = std::collections::HashSet::new();
    for name in &test_cases {
        let uuid = ComponentUuid::from_name(name);
        assert!(
            set.insert(uuid.bytes),
            "Hash collision detected for distinct name '{name}'"
        );
    }
}
