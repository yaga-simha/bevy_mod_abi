//! Empirical Adversarial Stress Test Suite for Milestone 1:
//! C-ABI Dynamic Component Contract & C Headers (`ModComponentDescriptor`, `HostApi`).

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentUuid, FfiResult, HostApi, ModComponentDescriptor};
use core::ffi::c_void;
use host_runtime::{host_get_proc_address, host_register_component, ModRegistry, HOST_API};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ============================================================================
// 1. Zero-Sized Types (ZST) Stress Tests
// ============================================================================

#[test]
fn test_zst_descriptors_boundary() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"ZstMarkerTag".as_ptr();
    let uuid = ComponentUuid::from_name("ZstMarkerTag");

    let zst_alignments = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 4096];

    for &align in &zst_alignments {
        let desc = ModComponentDescriptor::new(name, uuid, 0, align);

        assert!(desc.is_zst(), "size: 0 must report is_zst == true");
        assert!(
            desc.is_valid(),
            "ZST with valid align {align} must be valid"
        );
        assert_eq!(desc.validate(), FfiResult::OK);

        let layout = desc
            .layout()
            .expect("ZST with power-of-two align must produce layout");
        assert_eq!(layout.size(), 0);
        assert_eq!(layout.align(), align);

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::OK,
            "host_register_component must accept ZST with align {align}"
        );
    }
}

// ============================================================================
// 2. Massive Structs Stress Tests
// ============================================================================

#[test]
fn test_massive_struct_descriptors() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"MassiveComponent".as_ptr();
    let uuid = ComponentUuid::from_name("MassiveComponent");

    // 1MB struct with 16-byte alignment
    let size_1mb = 1024 * 1024;
    let desc_1mb = ModComponentDescriptor::new(name, uuid, size_1mb, 16);
    assert!(!desc_1mb.is_zst());
    assert!(desc_1mb.is_valid());
    assert_eq!(desc_1mb.validate(), FfiResult::OK);
    let layout = desc_1mb.layout().expect("1MB layout must be valid");
    assert_eq!(layout.size(), size_1mb);
    assert_eq!(layout.align(), 16);
    let res = unsafe { host_register_component(dummy_ctx, &desc_1mb) };
    assert_eq!(res, FfiResult::OK);

    // 64MB struct with 64-byte alignment
    let size_64mb = 64 * 1024 * 1024;
    let desc_64mb = ModComponentDescriptor::new(name, uuid, size_64mb, 64);
    assert!(!desc_64mb.is_zst());
    assert!(desc_64mb.is_valid());
    assert_eq!(desc_64mb.validate(), FfiResult::OK);
    let res = unsafe { host_register_component(dummy_ctx, &desc_64mb) };
    assert_eq!(res, FfiResult::OK);
}

// ============================================================================
// 3. Invalid Alignments Fuzzing & Boundary Tests
// ============================================================================

#[test]
fn test_invalid_alignments_fuzzing() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"InvalidAlignComp".as_ptr();
    let uuid = ComponentUuid::from_name("InvalidAlignComp");

    // Align = 0
    let desc_zero = ModComponentDescriptor::new(name, uuid, 0, 0);
    assert!(!desc_zero.is_valid());
    assert_ne!(desc_zero.validate(), FfiResult::OK);
    assert!(desc_zero.layout().is_none());
    let res = unsafe { host_register_component(dummy_ctx, &desc_zero) };
    assert_eq!(res, FfiResult::INTERNAL_ERROR);

    // Non-power-of-two alignments
    let invalid_alignments = [
        3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 21, 23, 25, 27, 29, 31, 33, 40, 50, 60,
        63, 65, 70, 80, 90, 100, 127, 129, 255, 257, 500, 1000, 1023, 1025, 4095, 4097, 65535,
        65537,
    ];

    for &bad_align in &invalid_alignments {
        let size = bad_align * 2; // ensure size is a multiple of bad_align to isolate alignment check
        let desc = ModComponentDescriptor::new(name, uuid, size, bad_align);

        assert!(
            !desc.is_valid(),
            "Non-power-of-two align {bad_align} must be invalid"
        );
        assert_ne!(desc.validate(), FfiResult::OK);
        assert!(
            desc.layout().is_none(),
            "Layout must reject non-power-of-two align {bad_align}"
        );

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INTERNAL_ERROR,
            "host_register_component must reject non-power-of-two align {bad_align}"
        );
    }
}

// ============================================================================
// 4. Misaligned Sizes Matrix Tests (size % align != 0)
// ============================================================================

#[test]
fn test_misaligned_sizes_matrix() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"MisalignedComp".as_ptr();
    let uuid = ComponentUuid::from_name("MisalignedComp");

    let test_cases = [
        // (size, align)
        (1, 2),
        (3, 2),
        (5, 2),
        (101, 2),
        (1, 4),
        (2, 4),
        (3, 4),
        (5, 4),
        (6, 4),
        (7, 4),
        (10, 4),
        (1, 8),
        (2, 8),
        (4, 8),
        (7, 8),
        (9, 8),
        (15, 8),
        (1, 16),
        (8, 16),
        (15, 16),
        (17, 16),
        (31, 16),
        (1, 32),
        (16, 32),
        (31, 32),
        (33, 32),
        (1, 64),
        (32, 64),
        (63, 64),
        (65, 64),
        (100, 64),
    ];

    for &(size, align) in &test_cases {
        let desc = ModComponentDescriptor::new(name, uuid, size, align);

        assert!(
            !desc.is_valid(),
            "size {size} not divisible by align {align} must be invalid"
        );
        assert_ne!(desc.validate(), FfiResult::OK);
        assert!(desc.layout().is_none());

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INTERNAL_ERROR,
            "host_register_component must reject misaligned size {size} align {align}"
        );
    }
}

// ============================================================================
// 5. Struct Size Negotiation Boundary Tests
// ============================================================================

#[test]
fn test_struct_size_negotiation_boundary() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"NegotiationComp".as_ptr();
    let uuid = ComponentUuid::from_name("NegotiationComp");
    let valid_base = ModComponentDescriptor::new(name, uuid, 16, 8);

    // Truncated struct sizes (< 64 bytes)
    let truncated_sizes: [u32; 15] = [0, 1, 2, 4, 8, 16, 24, 31, 32, 40, 48, 56, 60, 62, 63];

    for &trunc in &truncated_sizes {
        let mut desc = valid_base;
        desc.struct_size = trunc;

        assert_eq!(
            desc.validate(),
            FfiResult::INVALID_VERSION,
            "struct_size {trunc} < 64 must fail validation with INVALID_VERSION"
        );
        assert!(!desc.is_valid());

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INVALID_VERSION,
            "host_register_component must reject struct_size {trunc} with INVALID_VERSION"
        );
    }

    // Extended struct sizes (>= 64 bytes for forward compatibility)
    let extended_sizes: [u32; 10] = [64, 65, 72, 80, 128, 256, 512, 1024, 4096, 65536];

    for &ext in &extended_sizes {
        let mut desc = valid_base;
        desc.struct_size = ext;

        assert_eq!(
            desc.validate(),
            FfiResult::OK,
            "struct_size {ext} >= 64 must pass validation"
        );
        assert!(desc.is_valid());

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::OK,
            "host_register_component must accept forward-compatible struct_size {ext}"
        );
    }
}

// ============================================================================
// 6. Null Pointers Boundary Tests
// ============================================================================

#[test]
fn test_null_pointers_boundary() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"NullTestComp".as_ptr();
    let uuid = ComponentUuid::from_name("NullTestComp");
    let valid_desc = ModComponentDescriptor::new(name, uuid, 8, 8);

    // 1. Null host context
    let res = unsafe { host_register_component(core::ptr::null_mut(), &valid_desc) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 2. Null descriptor pointer
    let res = unsafe { host_register_component(dummy_ctx, core::ptr::null()) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 3. Both null
    let res = unsafe { host_register_component(core::ptr::null_mut(), core::ptr::null()) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 4. Null component name
    let null_name_desc = ModComponentDescriptor::new(core::ptr::null(), uuid, 8, 8);
    assert_eq!(null_name_desc.validate(), FfiResult::NULL_POINTER);
    assert!(!null_name_desc.is_valid());
    let res = unsafe { host_register_component(dummy_ctx, &null_name_desc) };
    assert_eq!(res, FfiResult::NULL_POINTER);

    // 5. Null drop_fn and null vtable_ptr are VALID (POD component)
    assert!(valid_desc.is_pod());
    assert!(valid_desc.drop_fn.is_none());
    assert!(valid_desc.vtable_ptr.is_null());
    let res = unsafe { host_register_component(dummy_ctx, &valid_desc) };
    assert_eq!(res, FfiResult::OK);
}

// ============================================================================
// 7. Malformed UTF-8 Component Names
// ============================================================================

#[test]
fn test_malformed_utf8_names() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let uuid = ComponentUuid::from_name("BadUtf8Comp");

    let bad_utf8_sequences: [&[u8]; 6] = [
        &[0xFF, 0x00],                   // Invalid starting byte
        &[0xFE, 0x00],                   // Invalid starting byte
        &[0xC0, 0xAF, 0x00],             // Overlong ASCII slash
        &[0xED, 0xA0, 0x80, 0x00],       // UTF-16 surrogate half
        &[0xF4, 0x90, 0x80, 0x80, 0x00], // Beyond Unicode maximum (U+10FFFF)
        &[0xE2, 0x82, 0x00],             // Truncated multi-byte sequence
    ];

    for (idx, &bad_bytes) in bad_utf8_sequences.iter().enumerate() {
        let desc =
            ModComponentDescriptor::new(bad_bytes.as_ptr() as *const core::ffi::c_char, uuid, 8, 8);

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INVALID_UTF8,
            "Sequence {idx} must be rejected with INVALID_UTF8"
        );
    }
}

// ============================================================================
// 8. HostApi Layout Stability & Byte Offsets 0..64
// ============================================================================

#[test]
fn test_host_api_layout_stability_and_byte_offsets() {
    // Assert 64-bit layout
    assert_eq!(core::mem::size_of::<HostApi>(), 72);
    assert_eq!(core::mem::align_of::<HostApi>(), 8);

    // Verify historical offsets 0..64 are completely preserved:
    assert_eq!(core::mem::offset_of!(HostApi, struct_size), 0);
    assert_eq!(core::mem::offset_of!(HostApi, abi_version), 4);
    assert_eq!(core::mem::offset_of!(HostApi, register_system), 8);
    assert_eq!(core::mem::offset_of!(HostApi, iter_next_chunk), 16);
    assert_eq!(core::mem::offset_of!(HostApi, iter_column_readonly), 24);
    assert_eq!(core::mem::offset_of!(HostApi, iter_column_mutable), 32);
    assert_eq!(core::mem::offset_of!(HostApi, iter_mark_changed), 40);
    assert_eq!(core::mem::offset_of!(HostApi, iter_entities), 48);
    assert_eq!(core::mem::offset_of!(HostApi, iter_column_stride), 56);

    // Offset 64 is the new register_component field:
    assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);

    // Global static HOST_API instance
    assert_eq!(HOST_API.struct_size, 72);
    assert_ne!(HOST_API.register_component as *const () as usize, 0);
    assert_eq!(
        HOST_API.register_component as *const () as usize,
        host_register_component as *const () as usize
    );
}

// ============================================================================
// 9. Callable Invocation Parity Matrix Across All Resolution Paths
// ============================================================================

#[test]
fn test_callable_invocation_parity_matrix() {
    let dummy_ctx = 0x9988_7766_usize as *mut c_void;
    let registry = ModRegistry::new();
    let reg_arc = Arc::new(registry);
    let _guard = reg_arc.make_current();

    let name = c"ParityComponent".as_ptr();
    let uuid = ComponentUuid::from_name("ParityComponent");

    // Build a matrix of descriptors (both valid and invalid)
    let valid_desc = ModComponentDescriptor::new(name, uuid, 32, 8);

    let mut truncated_desc = valid_desc;
    truncated_desc.struct_size = 32;

    let mut misaligned_desc = valid_desc;
    misaligned_desc.size = 15;

    let bad_utf8 = [0xFF, 0x00];
    let bad_utf8_desc =
        ModComponentDescriptor::new(bad_utf8.as_ptr() as *const core::ffi::c_char, uuid, 32, 8);

    let descriptors: [(&str, ModComponentDescriptor, FfiResult); 4] = [
        ("valid", valid_desc, FfiResult::OK),
        ("truncated", truncated_desc, FfiResult::INVALID_VERSION),
        ("misaligned", misaligned_desc, FfiResult::INTERNAL_ERROR),
        ("bad_utf8", bad_utf8_desc, FfiResult::INVALID_UTF8),
    ];

    // Resolve pointers through all 5 mechanisms
    let direct_fn = host_register_component;
    let host_api_fn = HOST_API.register_component;
    let reg_sym_fn = reg_arc
        .resolve_symbol("register_component")
        .expect("symbol register_component must resolve");
    let reg_sym_fn: abi_contract::RegisterComponentFn = unsafe { core::mem::transmute(reg_sym_fn) };

    let reg_host_sym_fn = reg_arc
        .resolve_symbol("host_register_component")
        .expect("symbol host_register_component must resolve");
    let reg_host_sym_fn: abi_contract::RegisterComponentFn =
        unsafe { core::mem::transmute(reg_host_sym_fn) };

    let gpa_sym_ptr = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
    assert!(!gpa_sym_ptr.is_null());
    let gpa_sym_fn: abi_contract::RegisterComponentFn =
        unsafe { core::mem::transmute(gpa_sym_ptr) };

    let gpa_host_sym_ptr = unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
    assert!(!gpa_host_sym_ptr.is_null());
    let gpa_host_sym_fn: abi_contract::RegisterComponentFn =
        unsafe { core::mem::transmute(gpa_host_sym_ptr) };

    // Verify all function pointer addresses are bitwise identical
    let target_addr = direct_fn as *const () as usize;
    assert_eq!(host_api_fn as *const () as usize, target_addr);
    assert_eq!(reg_sym_fn as *const () as usize, target_addr);
    assert_eq!(reg_host_sym_fn as *const () as usize, target_addr);
    assert_eq!(gpa_sym_fn as *const () as usize, target_addr);
    assert_eq!(gpa_host_sym_fn as *const () as usize, target_addr);

    // Execute each descriptor across all 6 invocation paths
    for (label, desc, expected_res) in descriptors {
        let r1 = unsafe { direct_fn(dummy_ctx, &desc) };
        let r2 = unsafe { host_api_fn(dummy_ctx, &desc) };
        let r3 = unsafe { reg_sym_fn(dummy_ctx, &desc) };
        let r4 = unsafe { reg_host_sym_fn(dummy_ctx, &desc) };
        let r5 = unsafe { gpa_sym_fn(dummy_ctx, &desc) };
        let r6 = unsafe { gpa_host_sym_fn(dummy_ctx, &desc) };

        assert_eq!(r1, expected_res, "{label} via direct_fn");
        assert_eq!(r2, expected_res, "{label} via host_api_fn");
        assert_eq!(r3, expected_res, "{label} via reg_sym_fn");
        assert_eq!(r4, expected_res, "{label} via reg_host_sym_fn");
        assert_eq!(r5, expected_res, "{label} via gpa_sym_fn");
        assert_eq!(r6, expected_res, "{label} via gpa_host_sym_fn");
    }
}

// ============================================================================
// 10. Lifecycle Hooks & Custom Drop Callback Invocation
// ============================================================================

static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn test_custom_drop(ptr: *mut c_void) {
    if !ptr.is_null() {
        DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn test_descriptor_with_drop_and_vtable() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"CustomDropComp".as_ptr();
    let uuid = ComponentUuid::from_name("CustomDropComp");

    let fake_vtable: usize = 0xcafe_babe;
    let desc = ModComponentDescriptor::new(name, uuid, 16, 8)
        .with_drop(test_custom_drop)
        .with_vtable(fake_vtable as *const c_void);

    assert!(!desc.is_pod());
    assert!(desc.drop_fn.is_some());
    assert_eq!(
        desc.drop_fn.unwrap() as *const () as usize,
        test_custom_drop as *const () as usize
    );
    assert_eq!(desc.vtable_ptr, fake_vtable as *const c_void);

    let res = unsafe { host_register_component(dummy_ctx, &desc) };
    assert_eq!(res, FfiResult::OK);

    // Exercise calling the drop callback safely
    let mut payload = [42u8; 16];
    let before_count = DROP_CALL_COUNT.load(Ordering::SeqCst);
    if let Some(drop_fn) = desc.drop_fn {
        unsafe { drop_fn(payload.as_mut_ptr() as *mut c_void) };
    }
    assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), before_count + 1);
}

// ============================================================================
// 11. Extreme Limits & Edge Cases (usize::MAX and Maximum Alignments)
// ============================================================================

#[test]
fn test_extreme_size_and_alignment_limits() {
    let dummy_ctx = 0x1234_5678_usize as *mut c_void;
    let name = c"ExtremeComp".as_ptr();
    let uuid = ComponentUuid::from_name("ExtremeComp");

    // Rust Layout limit on 64-bit platforms:
    // Layout requires size <= isize::MAX as usize
    // If size == usize::MAX, Layout::from_size_align returns Err(LayoutError).
    let desc_usize_max = ModComponentDescriptor::new(name, uuid, usize::MAX, 1);
    assert!(
        desc_usize_max.layout().is_none(),
        "usize::MAX layout must be None"
    );
    let res_max = unsafe { host_register_component(dummy_ctx, &desc_usize_max) };
    println!("host_register_component(usize::MAX, 1): {:?}", res_max);

    // Alignment 1 << 63
    let layout_res = core::alloc::Layout::from_size_align(0, 1usize << 63);
    println!("Layout for (0, 1<<63): {:?}", layout_res);
    let desc_high_align = ModComponentDescriptor::new(name, uuid, 0, 1usize << 63);
    assert!(desc_high_align.align.is_power_of_two());

    // Alignment 1 << 29 (536,870,912) is the maximum supported Rust alignment
    let desc_max_rust_align = ModComponentDescriptor::new(name, uuid, 0, 1usize << 29);
    assert!(desc_max_rust_align.align.is_power_of_two());
    assert!(desc_max_rust_align.layout().is_some());
    let res = unsafe { host_register_component(dummy_ctx, &desc_max_rust_align) };
    assert_eq!(res, FfiResult::OK);
}
