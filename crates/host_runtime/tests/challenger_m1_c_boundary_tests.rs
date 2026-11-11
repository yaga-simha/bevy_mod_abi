#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentUuid, FfiResult, HostApi, ModComponentDescriptor, ABI_VERSION_PACKED};
use core::ffi::{c_char, c_void};
use host_runtime::{
    host_get_proc_address, host_iter_column_stride, host_register_component, ModRegistry, HOST_API,
};
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

// ============================================================================
// 1. ABI Boundary & Monotonic Version Compatibility Tests
// ============================================================================

/// Memory layout mirroring the older 64-byte HostApi prior to register_component.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct LegacyHostApi64 {
    struct_size: u32,
    abi_version: u32,
    register_system: usize,
    iter_next_chunk: usize,
    iter_column_readonly: usize,
    iter_column_mutable: usize,
    iter_mark_changed: usize,
    iter_entities: usize,
    iter_column_stride: usize,
}

#[repr(C)]
struct LegacyHostMemoryBuffer {
    legacy_api: LegacyHostApi64,
    canary_bytes: [u8; 8],
}

#[test]
fn test_legacy_host_api_layout_assumptions() {
    assert_eq!(core::mem::size_of::<LegacyHostApi64>(), 64);
    assert_eq!(core::mem::align_of::<LegacyHostApi64>(), 8);
    assert_eq!(core::mem::size_of::<HostApi>(), 72);
    assert_eq!(core::mem::align_of::<HostApi>(), 8);
    assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);
}

#[test]
fn test_legacy_mod_on_newer_72_byte_host() {
    // Host provides modern 72-byte HOST_API
    assert_eq!(HOST_API.struct_size, 72);
    assert_eq!(HOST_API.abi_version, ABI_VERSION_PACKED);

    // Older mod compiled against LegacyHostApi64 receives HOST_API pointer
    let host_ptr = &HOST_API as *const HostApi as *const LegacyHostApi64;
    let legacy_view = unsafe { &*host_ptr };

    // Legacy mod performs monotonic negotiation: host_size >= legacy_size (72 >= 64)
    assert!(legacy_view.struct_size >= core::mem::size_of::<LegacyHostApi64>() as u32);
    assert_eq!(legacy_view.struct_size, 72);
    assert_eq!(legacy_view.abi_version, ABI_VERSION_PACKED);

    // Verify all legacy function pointers point to the exact expected host functions
    assert_eq!(
        legacy_view.iter_column_stride,
        host_iter_column_stride as *const () as usize
    );
}

#[test]
fn test_newer_mod_on_older_64_byte_host_boundary_guard() {
    // Older host allocates exactly 64 bytes for LegacyHostApi64.
    // We place a canary immediately following it in memory to simulate unmapped/foreign data.
    let buffer = LegacyHostMemoryBuffer {
        legacy_api: LegacyHostApi64 {
            struct_size: 64,
            abi_version: ABI_VERSION_PACKED,
            register_system: 0x1111,
            iter_next_chunk: 0x2222,
            iter_column_readonly: 0x3333,
            iter_column_mutable: 0x4444,
            iter_mark_changed: 0x5555,
            iter_entities: 0x6666,
            iter_column_stride: host_iter_column_stride as *const () as usize,
        },
        canary_bytes: [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE],
    };

    let host_api_ptr = &buffer.legacy_api as *const LegacyHostApi64 as *const HostApi;
    let modern_view = unsafe { &*host_api_ptr };

    // Modern mod must inspect struct_size before touching register_component
    let host_struct_size = modern_view.struct_size;
    assert_eq!(host_struct_size, 64);

    let has_register_component = host_struct_size as usize >= core::mem::size_of::<HostApi>();
    assert!(
        !has_register_component,
        "Modern mod must detect that older host lacks register_component"
    );

    // Older functionality remains fully operational
    assert_eq!(
        modern_view.iter_column_stride as *const () as usize,
        host_iter_column_stride as *const () as usize
    );

    // Canary bytes at offset 64 remain untouched
    let canary_read =
        unsafe { core::ptr::read_unaligned((host_api_ptr as *const u8).add(64) as *const [u8; 8]) };
    assert_eq!(
        canary_read,
        [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]
    );
}

// ============================================================================
// 2. Dynamic Symbol Resolution Stress Tests
// ============================================================================

#[test]
fn test_host_get_proc_address_stress_and_adversarial_queries() {
    let registry = Arc::new(ModRegistry::new());
    let _guard = registry.make_current();

    // 1. Valid registered symbols
    let sym_reg = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
    assert_eq!(sym_reg, host_register_component as *const c_void);

    let sym_host_reg = unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
    assert_eq!(sym_host_reg, host_register_component as *const c_void);

    // 2. Null pointer input
    let null_res = unsafe { host_get_proc_address(core::ptr::null()) };
    assert_eq!(null_res, core::ptr::null());

    // 3. Empty string
    let empty_res = unsafe { host_get_proc_address(c"".as_ptr()) };
    assert_eq!(empty_res, core::ptr::null());

    // 4. Non-existent symbol
    let missing_res = unsafe { host_get_proc_address(c"some_non_existent_symbol".as_ptr()) };
    assert_eq!(missing_res, core::ptr::null());

    // 5. Near matches / Off-by-one prefix queries
    let near_misses = [
        c"register_componen".as_ptr(),
        c"register_components".as_ptr(),
        c"register_component_".as_ptr(),
        c"host_register_comp".as_ptr(),
        c"host_register_components".as_ptr(),
        c"Register_Component".as_ptr(), // case sensitivity check
        c"REGISTER_COMPONENT".as_ptr(),
    ];
    for &query in &near_misses {
        let res = unsafe { host_get_proc_address(query) };
        assert_eq!(
            res,
            core::ptr::null(),
            "Query {:?} must resolve to NULL",
            unsafe { core::ffi::CStr::from_ptr(query) }
        );
    }

    // 6. Whitespace and formatting variations
    let whitespace_queries = [
        c" register_component".as_ptr(),
        c"register_component ".as_ptr(),
        c"register_component\n".as_ptr(),
        c"register_component\t".as_ptr(),
    ];
    for &query in &whitespace_queries {
        let res = unsafe { host_get_proc_address(query) };
        assert_eq!(res, core::ptr::null());
    }

    // 7. Invalid UTF-8 sequence
    let bad_utf8_bytes = [0xff, 0xfe, 0xfd, 0x00];
    let bad_utf8_ptr = bad_utf8_bytes.as_ptr() as *const c_char;
    let res = unsafe { host_get_proc_address(bad_utf8_ptr) };
    assert_eq!(res, core::ptr::null());

    // 8. Extremely long query string (10,000 bytes)
    let long_string = "A".repeat(10_000);
    let c_long = CString::new(long_string).unwrap();
    let res = unsafe { host_get_proc_address(c_long.as_ptr()) };
    assert_eq!(res, core::ptr::null());
}

#[test]
fn test_host_get_proc_address_high_concurrency() {
    let registry = Arc::new(ModRegistry::new());
    let _guard = registry.make_current();

    const NUM_THREADS: usize = 8;
    const QUERIES_PER_THREAD: usize = 5_000;

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|_| {
            thread::spawn(|| {
                for i in 0..QUERIES_PER_THREAD {
                    let sym1 = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
                    assert_eq!(sym1, host_register_component as *const c_void);

                    let sym2 =
                        unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
                    assert_eq!(sym2, host_register_component as *const c_void);

                    if i % 10 == 0 {
                        let bad = unsafe {
                            host_get_proc_address(c"non_existent_concurrent_query".as_ptr())
                        };
                        assert_eq!(bad, core::ptr::null());
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ============================================================================
// 3. Dynamic Component Descriptor Stress Matrix
// ============================================================================

static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn custom_test_drop_hook(ptr: *mut c_void) {
    if !ptr.is_null() {
        DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn test_host_register_component_extensive_stress_matrix() {
    let dummy_ctx = 0xbeef_cafe_usize as *mut c_void;
    let name_ascii = c"StressComponent".as_ptr();
    let uuid = ComponentUuid::from_name("StressComponent");

    // 1. ZST matrix with various power-of-two alignments
    let zst_alignments = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
    for &align in &zst_alignments {
        let desc = ModComponentDescriptor::new(name_ascii, uuid, 0, align);
        assert!(desc.is_valid());
        assert_eq!(desc.validate(), FfiResult::OK);
        assert!(desc.is_zst());
        assert!(desc.is_pod());
        assert!(desc.layout().is_some());

        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::OK,
            "Failed registering ZST with align {}",
            align
        );
    }

    // 2. Over-aligned non-ZST types
    let overaligned_configs = [
        (16, 16),
        (32, 16),
        (64, 64),
        (128, 64),
        (256, 256),
        (1024, 512),
        (4096, 4096),
    ];
    for &(size, align) in &overaligned_configs {
        let desc = ModComponentDescriptor::new(name_ascii, uuid, size, align);
        assert!(desc.is_valid());
        assert_eq!(desc.validate(), FfiResult::OK);
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(res, FfiResult::OK);
    }

    // 3. Invalid non-power-of-two alignments
    let invalid_alignments = [0, 3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 24, 100, 333];
    for &align in &invalid_alignments {
        let desc = ModComponentDescriptor {
            struct_size: 64,
            name: name_ascii,
            uuid,
            size: align * 2,
            align,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        };
        assert!(!desc.is_valid());
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INTERNAL_ERROR,
            "Alignment {} must be rejected with INTERNAL_ERROR",
            align
        );
    }

    // 4. Misaligned sizes (size % align != 0)
    let misaligned_configs = [
        (1, 2),
        (3, 4),
        (7, 8),
        (15, 8),
        (17, 16),
        (31, 32),
        (65, 64),
    ];
    for &(size, align) in &misaligned_configs {
        let desc = ModComponentDescriptor {
            struct_size: 64,
            name: name_ascii,
            uuid,
            size,
            align,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        };
        assert!(!desc.is_valid());
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(
            res,
            FfiResult::INTERNAL_ERROR,
            "Size {} with align {} must be rejected with INTERNAL_ERROR",
            size,
            align
        );
    }

    // 5. Valid forward-compatible struct_size
    let extended_sizes = [64, 72, 80, 96, 128, 256, 1024, 65536];
    for &sz in &extended_sizes {
        let mut desc = ModComponentDescriptor::new(name_ascii, uuid, 16, 8);
        desc.struct_size = sz;
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(res, FfiResult::OK);
    }

    // 6. Truncated struct_size
    let truncated_sizes = [0, 1, 4, 8, 16, 32, 48, 63];
    for &sz in &truncated_sizes {
        let mut desc = ModComponentDescriptor::new(name_ascii, uuid, 16, 8);
        desc.struct_size = sz;
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(res, FfiResult::INVALID_VERSION);
    }

    // 7. Non-ASCII UTF-8 component names
    let utf8_names = [
        CString::new("生命值").unwrap(),
        CString::new("Компонент").unwrap(),
        CString::new("CaféAuLaitTag").unwrap(),
    ];
    for c_name in &utf8_names {
        let desc = ModComponentDescriptor::new(c_name.as_ptr(), uuid, 8, 8);
        let res = unsafe { host_register_component(dummy_ctx, &desc) };
        assert_eq!(res, FfiResult::OK);
    }

    // 8. Lifecycle drop hook execution
    let drop_desc =
        ModComponentDescriptor::new(name_ascii, uuid, 16, 8).with_drop(custom_test_drop_hook);
    assert!(!drop_desc.is_pod());
    assert!(drop_desc.drop_fn.is_some());

    let res = unsafe { host_register_component(dummy_ctx, &drop_desc) };
    assert_eq!(res, FfiResult::OK);

    DROP_CALL_COUNT.store(0, Ordering::SeqCst);
    let mut dummy_val = 0x1234_u64;
    unsafe {
        (drop_desc.drop_fn.unwrap())(&mut dummy_val as *mut u64 as *mut c_void);
    }
    assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), 1);

    // 9. Static VTable pointer preservation
    let dummy_vtable = 0x9999_usize as *const c_void;
    let vtable_desc = ModComponentDescriptor::new(name_ascii, uuid, 8, 8).with_vtable(dummy_vtable);
    assert_eq!(vtable_desc.vtable_ptr, dummy_vtable);
    let res = unsafe { host_register_component(dummy_ctx, &vtable_desc) };
    assert_eq!(res, FfiResult::OK);
}
