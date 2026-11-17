//! Cross-language SipHash-2-4-128 `ComponentUuid` parity between Rust and ISO C99.

use bevy_abi_mod_sys::ComponentUuid;
use core::ffi::c_char;
use libloading::{Library, Symbol};
use std::collections::HashSet;
use std::ffi::CString;

type CUuidFromPartsFn = unsafe extern "C" fn(
    canonical_name: *const c_char,
    size: usize,
    align: usize,
    field_schema_hash: u64,
    out_bytes: *mut u8,
);

type CUuidFromCanonicalNameFn =
    unsafe extern "C" fn(canonical_name: *const c_char, out_bytes: *mut u8);

#[test]
fn test_cross_language_uuid_parity_matrix() {
    let orig = crate::support::fixture_dylib("c_uuid_oracle");
    let dylib_copy = crate::support::TempDylibCopy::new(&orig, "uuid_oracle");
    let lib =
        unsafe { Library::new(dylib_copy.path()) }.expect("Failed to load c_uuid_oracle fixture");

    let c_from_parts: Symbol<CUuidFromPartsFn> = unsafe {
        lib.get(b"c_uuid_from_parts\0")
            .expect("Failed to resolve c_uuid_from_parts")
    };
    let c_from_canonical_name: Symbol<CUuidFromCanonicalNameFn> = unsafe {
        lib.get(b"c_uuid_from_canonical_name\0")
            .expect("Failed to resolve c_uuid_from_canonical_name")
    };

    let test_cases = [
        // 1. Standard namespaced ECS names
        "testbed::Position",
        "testbed::Velocity",
        "testbed::CombatHealth",
        "testbed::MetadataTag",
        "asymmetric_rust_mod::RustModCustomData",
        "asymmetric_c_mod::CModCustomData",
        "game::PlayerTag",
        "game::DroneTag",
        "bevy::Transform",
        "bevy::GlobalTransform",
        // 2. Boundary & short names (covering SipHash 8-byte block tail lengths 0..=16)
        "",
        "a",
        "A",
        "0",
        "_",
        "ab",
        "abc",
        "abcd",
        "abcde",
        "abcdef",
        "abcdefg",
        "12345678",
        "123456789",
        "123456789012345",
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

    // A. `from_canonical_name` parity (`size = 0, align = 0, field_schema_hash = 0`)
    for name in &test_cases {
        let rust_uuid = ComponentUuid::from_canonical_name(name);
        let c_str = CString::new(*name).unwrap();
        let mut c_bytes = [0u8; 16];
        unsafe {
            c_from_canonical_name(c_str.as_ptr(), c_bytes.as_mut_ptr());
        }

        assert_eq!(
            rust_uuid.bytes, c_bytes,
            "from_canonical_name mismatch for '{name}': Rust={:?} vs C={:?}",
            rust_uuid.bytes, c_bytes
        );
    }

    // B. Full `from_parts` `(canonical_name, size, align, field_schema_hash)` parity matrix
    let layout_matrix: [(usize, usize, u64); 8] = [
        (0, 1, 0),
        (1, 1, 1),
        (4, 4, 0x0123_4567_89AB_CDEF),
        (12, 4, 0xDEAD_BEEF_CAFE_BABE),
        (16, 8, u64::MAX),
        (64, 64, 0x8000_0000_0000_0001),
        (4096, 256, 42),
        (usize::MAX, 1, 0x55AA_55AA_55AA_55AA),
    ];

    for name in &test_cases {
        let c_str = CString::new(*name).unwrap();
        for &(size, align, schema_hash) in &layout_matrix {
            let rust_uuid = ComponentUuid::from_parts(name, size, align, schema_hash);
            let mut c_bytes = [0u8; 16];
            unsafe {
                c_from_parts(
                    c_str.as_ptr(),
                    size,
                    align,
                    schema_hash,
                    c_bytes.as_mut_ptr(),
                );
            }
            assert_eq!(
                rust_uuid.bytes, c_bytes,
                "from_parts mismatch for ('{name}', {size}, {align}, {schema_hash:#x})"
            );
        }
    }

    // C. Long names (up to 4096 bytes)
    for len in [63, 64, 65, 128, 256, 512, 1024, 2048, 4096] {
        let long_name: String = (0..len)
            .map(|i| {
                let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_:";
                chars[i % chars.len()] as char
            })
            .collect();

        let rust_canon = ComponentUuid::from_canonical_name(&long_name);
        let rust_parts = ComponentUuid::from_parts(&long_name, 24, 8, 0xFEED_FACE_1234_5678);
        let c_str = CString::new(long_name).unwrap();

        let mut c_canon = [0u8; 16];
        let mut c_parts = [0u8; 16];
        unsafe {
            c_from_canonical_name(c_str.as_ptr(), c_canon.as_mut_ptr());
            c_from_parts(
                c_str.as_ptr(),
                24,
                8,
                0xFEED_FACE_1234_5678,
                c_parts.as_mut_ptr(),
            );
        }
        assert_eq!(
            rust_canon.bytes, c_canon,
            "canonical mismatch at length {len}"
        );
        assert_eq!(rust_parts.bytes, c_parts, "parts mismatch at length {len}");
    }

    // D. Null pointer behavior in C (`bam_uuid_from_canonical_name(NULL)` hashes `("", 0, 0, 0)`)
    let empty_rust = ComponentUuid::from_canonical_name("");
    assert_ne!(empty_rust.bytes, [0u8; 16]);

    let mut c_null_canon = [0u8; 16];
    let mut c_null_parts = [0u8; 16];
    unsafe {
        c_from_canonical_name(core::ptr::null(), c_null_canon.as_mut_ptr());
        c_from_parts(core::ptr::null(), 16, 8, 99, c_null_parts.as_mut_ptr());
    }
    assert_eq!(
        c_null_canon, empty_rust.bytes,
        "bam_uuid_from_canonical_name(NULL) must hash (\"\", 0, 0, 0)"
    );
    assert_eq!(
        c_null_parts,
        ComponentUuid::from_parts("", 16, 8, 99).bytes,
        "bam_uuid_from_parts(NULL, ...) must treat NULL name as empty string"
    );

    // E. Collision resistance & layout-drift sensitivity across distinct names and layouts
    let mut seen = HashSet::new();
    for name in &test_cases {
        let u0 = ComponentUuid::from_canonical_name(name);
        let u_sz = ComponentUuid::from_parts(name, 8, 4, 0);
        let u_al = ComponentUuid::from_parts(name, 8, 8, 0);
        let u_sh = ComponentUuid::from_parts(name, 8, 8, 1);

        assert!(seen.insert(u0.bytes), "collision on canonical '{name}'");
        assert!(seen.insert(u_sz.bytes), "collision on size '{name}'");
        assert!(seen.insert(u_al.bytes), "collision on align '{name}'");
        assert!(seen.insert(u_sh.bytes), "collision on schema_hash '{name}'");
    }
}
