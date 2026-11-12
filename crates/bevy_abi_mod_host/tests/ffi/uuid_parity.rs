//! Cross-language FNV-1a 128-bit `ComponentUuid` parity between Rust and C99 header.

use bevy_abi_mod_sys::ComponentUuid;
use libloading::{Library, Symbol};

type CUuidFn = unsafe extern "C" fn(*const core::ffi::c_char, *mut u8);

#[test]
fn test_cross_language_uuid_parity_matrix() {
    let dylib_path = crate::support::fixture_dylib("c_uuid_oracle");
    let lib = unsafe { Library::new(&dylib_path) }.expect("Failed to load c_uuid_oracle fixture");
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
