use abi_contract::{FfiEntity, FfiResult, RawCommandQueue};
use core::ffi::{c_char, CStr};
use guest_sdk::{CommandBridgeVTable, GuestCommandQueue};

#[derive(Default, Debug)]
struct TestContext {
    call_count: usize,
    last_len: usize,
    last_entity_bits: u64,
    captured_bytes: Vec<u8>,
}

unsafe extern "C" fn mock_insert_ok(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    name: *const c_char,
    data: *const u8,
    len: usize,
) -> FfiResult {
    assert!(!queue.is_null(), "queue must not be null");
    let ctx = unsafe { &mut *(queue as *mut TestContext) };
    ctx.call_count += 1;
    ctx.last_len = len;
    ctx.last_entity_bits = entity.0;

    assert!(!name.is_null(), "component_name must not be null");
    let cstr = unsafe { CStr::from_ptr(name) };
    assert!(cstr.to_str().is_ok(), "component_name must be valid utf8");

    if len > 0 {
        assert!(!data.is_null(), "data must not be null when len > 0");
        let bytes = unsafe { core::slice::from_raw_parts(data, len) };
        ctx.captured_bytes = bytes.to_vec();
    } else {
        ctx.captured_bytes.clear();
    }

    FfiResult::OK
}

unsafe extern "C" fn mock_insert_error(
    _queue: *mut RawCommandQueue,
    _entity: FfiEntity,
    _name: *const c_char,
    _data: *const u8,
    _len: usize,
) -> FfiResult {
    FfiResult::INVALID_COMPONENT
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct EmptyZst;

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
struct SimplePos {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(align(64))]
struct OverAligned {
    tag: u64,
    bytes: [u8; 56],
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
struct PaddedStruct {
    a: u8,
    b: u32,
    c: u16,
    d: u64,
}

#[test]
fn test_insert_null_queue_returns_null_pointer() {
    let mut queue = GuestCommandQueue::new(
        core::ptr::null_mut(),
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let val = 42u32;
    let res_val = queue.insert_value(FfiEntity(1), c"MyComponent", &val);
    assert_eq!(res_val, FfiResult::NULL_POINTER);

    let slice = [1u32, 2, 3];
    let res_slice = queue.insert_slice(FfiEntity(1), c"MyComponent", &slice);
    assert_eq!(res_slice, FfiResult::NULL_POINTER);
}

#[test]
fn test_insert_missing_vtable_function_returns_null_pointer() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: None,
        },
    );

    let val = 42u32;
    let res_val = queue.insert_value(FfiEntity(1), c"MyComponent", &val);
    assert_eq!(res_val, FfiResult::NULL_POINTER);

    let slice = [1u32, 2, 3];
    let res_slice = queue.insert_slice(FfiEntity(1), c"MyComponent", &slice);
    assert_eq!(res_slice, FfiResult::NULL_POINTER);
}

#[test]
fn test_insert_error_propagation() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_error),
        },
    );

    let val = 42u32;
    let res = queue.insert_value(FfiEntity(1), c"MyComponent", &val);
    assert_eq!(res, FfiResult::INVALID_COMPONENT);

    let slice = [10u32, 20];
    let res_slice = queue.insert_slice(FfiEntity(1), c"MyComponent", &slice);
    assert_eq!(res_slice, FfiResult::INVALID_COMPONENT);
}

#[test]
fn test_insert_value_and_slice_valid_data() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let pos = SimplePos {
        x: 10.0,
        y: 20.0,
        z: 30.0,
    };
    let res = queue.insert_value(FfiEntity(42), c"Position", &pos);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.call_count, 1);
    assert_eq!(ctx.last_len, core::mem::size_of::<SimplePos>());
    assert_eq!(ctx.last_entity_bits, 42);

    let poses = [
        SimplePos {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        SimplePos {
            x: 4.0,
            y: 5.0,
            z: 6.0,
        },
        SimplePos {
            x: 7.0,
            y: 8.0,
            z: 9.0,
        },
    ];
    let res_slice = queue.insert_slice(FfiEntity(99), c"Positions", &poses);
    assert_eq!(res_slice, FfiResult::OK);
    assert_eq!(ctx.call_count, 2);
    assert_eq!(ctx.last_len, core::mem::size_of::<SimplePos>() * 3);
    assert_eq!(ctx.last_entity_bits, 99);
}

#[test]
fn test_data_content_fidelity() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let padded = PaddedStruct {
        a: 0x42,
        b: 0xDEADBEEF,
        c: 0x1234,
        d: 0xCAFEBABE_FEEDFACE,
    };

    let res = queue.insert_value(FfiEntity(123), c"Padded", &padded);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.last_len, core::mem::size_of::<PaddedStruct>());

    let recovered: PaddedStruct =
        unsafe { core::ptr::read_unaligned(ctx.captured_bytes.as_ptr() as *const PaddedStruct) };
    assert_eq!(recovered, padded);
}

#[test]
fn test_insert_zero_sized_types_and_empty_slices() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    // 1. ZST value
    let zst = EmptyZst;
    let res = queue.insert_value(FfiEntity(100), c"EmptyZst", &zst);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.call_count, 1);
    assert_eq!(ctx.last_len, 0);
    assert!(ctx.captured_bytes.is_empty());

    // 2. ZST slice
    let zst_slice = [EmptyZst, EmptyZst, EmptyZst];
    let res = queue.insert_slice(FfiEntity(101), c"EmptyZstSlice", &zst_slice);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.call_count, 2);
    assert_eq!(ctx.last_len, 0);
    assert!(ctx.captured_bytes.is_empty());

    // 3. Empty slice of sized type
    let empty_u32: [u32; 0] = [];
    let res = queue.insert_slice(FfiEntity(102), c"EmptyU32Slice", &empty_u32);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.call_count, 3);
    assert_eq!(ctx.last_len, 0);
    assert!(ctx.captured_bytes.is_empty());
}

#[test]
fn test_insert_overaligned_type() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let aligned = OverAligned {
        tag: 0xCAFEBABE,
        bytes: [0x5A; 56],
    };
    let res = queue.insert_value(FfiEntity(777), c"OverAligned", &aligned);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.last_len, core::mem::size_of::<OverAligned>());
    assert_eq!(core::mem::align_of::<OverAligned>(), 64);
}

#[test]
fn test_insert_large_slice_scale() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let large_buffer = vec![123456789u64; 10_000];
    let res = queue.insert_slice(FfiEntity(888), c"LargeU64Array", &large_buffer);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(ctx.last_len, 10_000 * 8);
    assert_eq!(ctx.captured_bytes.len(), 10_000 * 8);
}

#[test]
fn test_multiple_sequential_insertions_stress() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    let iterations = if cfg!(miri) { 100 } else { 5000 };

    for i in 0..iterations {
        let entity = FfiEntity(i as u64);
        let val = (i * 3) as u32;
        let res = queue.insert_value(entity, c"IterComp", &val);
        assert_eq!(res, FfiResult::OK);
    }

    assert_eq!(ctx.call_count, iterations);
    assert_eq!(ctx.last_entity_bits, (iterations - 1) as u64);
}

#[test]
fn test_edge_case_component_names() {
    let mut ctx = TestContext::default();
    let mut queue = GuestCommandQueue::new(
        core::ptr::addr_of_mut!(ctx) as *mut RawCommandQueue,
        CommandBridgeVTable {
            spawn_empty: None,
            despawn: None,
            insert_component: Some(mock_insert_ok),
        },
    );

    // Empty string
    let res = queue.insert_value(FfiEntity(1), c"", &1u8);
    assert_eq!(res, FfiResult::OK);

    // UTF-8 multibyte name
    let res2 = queue.insert_value(FfiEntity(2), c"Mod_🦀_Rocket🚀", &2u8);
    assert_eq!(res2, FfiResult::OK);
}
