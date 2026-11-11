//! # Challenger 2 Guest SDK Empirical Stress Suite
//!
//! Empirical tests for Milestone 3 Guest SDK dynamic components:
//! 1. `register_runtime_component`:
//!    - POD types set `drop_fn` to `None`.
//!    - Non-POD types set `drop_fn` to `Some(generic_drop_trampoline::<T>)`.
//!    - Panic containment: `generic_drop_trampoline` intercepts panics using `catch_unwind`.
//! 2. `ChunkSlice` & `ChunkSliceMut::as_typed_slice`:
//!    - Stride mismatch returns `None`.
//!    - Misaligned pointer returns `None`.
//!    - Null pointer with count > 0 returns `None`.
//!    - Zero-count slice returns `Some(&[])` without pointer dereference.
//! 3. `GuestCommandQueue::insert_runtime_component`:
//!    - Validates correct name, size, data forwarding.

use abi_contract::{
    FfiEntity, FfiResult, HostApi, ModComponentDescriptor, RawCommandQueue, RegisterComponentFn,
};
use core::ffi::{c_char, c_void, CStr};
use core::sync::atomic::{AtomicBool, Ordering};
use guest_sdk::{
    register_runtime_component, ChunkSlice, ChunkSliceMut, CommandBridgeVTable, GuestCommandQueue,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PodComponent {
    x: f32,
    y: u32,
}

static NON_POD_DROPPED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct NonPodComponent {
    _tag: String,
}

impl Drop for NonPodComponent {
    fn drop(&mut self) {
        NON_POD_DROPPED.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct PanickingDropComponent;

impl Drop for PanickingDropComponent {
    fn drop(&mut self) {
        panic!("Intentional panic inside Non-POD drop hook");
    }
}

struct MockHostContext {
    captured_desc: Option<ModComponentDescriptor>,
}

unsafe extern "C" fn mock_register_component(
    host_context: *mut c_void,
    descriptor: *const ModComponentDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let ctx = unsafe { &mut *(host_context as *mut MockHostContext) };
    ctx.captured_desc = Some(unsafe { *descriptor });
    FfiResult::OK
}

fn create_mock_host_api(reg_fn: RegisterComponentFn) -> HostApi {
    HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: abi_contract::ABI_VERSION_PACKED,
        register_system: unsafe { core::mem::transmute(1usize) },
        iter_next_chunk: unsafe { core::mem::transmute(1usize) },
        iter_column_readonly: unsafe { core::mem::transmute(1usize) },
        iter_column_mutable: unsafe { core::mem::transmute(1usize) },
        iter_mark_changed: unsafe { core::mem::transmute(1usize) },
        iter_entities: unsafe { core::mem::transmute(1usize) },
        iter_column_stride: unsafe { core::mem::transmute(1usize) },
        register_component: reg_fn,
    }
}

#[test]
fn test_register_runtime_component_pod_vs_non_pod() {
    let api = create_mock_host_api(mock_register_component);

    let mut ctx = MockHostContext {
        captured_desc: None,
    };
    let ctx_ptr = &mut ctx as *mut MockHostContext as *mut c_void;

    // 1. POD component registration: drop_fn MUST be None!
    let res = unsafe { register_runtime_component::<PodComponent>(&api, ctx_ptr, c"PodComponent") };
    assert_eq!(res, FfiResult::OK);

    let desc_pod = ctx.captured_desc.take().expect("Descriptor captured");
    assert_eq!(desc_pod.size, 8);
    assert_eq!(desc_pod.align, 4);
    assert!(
        desc_pod.drop_fn.is_none(),
        "POD components MUST set drop_fn to None, consuming 0 drop trampoline slots"
    );

    // 2. Non-POD component registration: drop_fn MUST be Some!
    let res_non_pod =
        unsafe { register_runtime_component::<NonPodComponent>(&api, ctx_ptr, c"NonPodComponent") };
    assert_eq!(res_non_pod, FfiResult::OK);

    let desc_non_pod = ctx.captured_desc.take().expect("Descriptor captured");
    assert_eq!(desc_non_pod.size, core::mem::size_of::<NonPodComponent>());
    assert_eq!(desc_non_pod.align, core::mem::align_of::<NonPodComponent>());
    assert!(
        desc_non_pod.drop_fn.is_some(),
        "Non-POD components MUST set drop_fn to generic_drop_trampoline"
    );

    // Verify drop_fn works properly
    NON_POD_DROPPED.store(false, Ordering::SeqCst);
    let instance = Box::new(NonPodComponent {
        _tag: "Hello".to_string(),
    });
    let raw_instance = Box::into_raw(instance) as *mut c_void;

    let drop_fn = desc_non_pod.drop_fn.unwrap();
    unsafe {
        drop_fn(raw_instance);
    }
    assert!(
        NON_POD_DROPPED.load(Ordering::SeqCst),
        "generic_drop_trampoline must execute drop_in_place"
    );
    // Deallocate the memory buffer
    unsafe {
        std::alloc::dealloc(
            raw_instance as *mut u8,
            std::alloc::Layout::new::<NonPodComponent>(),
        );
    }
}

#[test]
fn test_generic_drop_trampoline_panic_containment() {
    let api = create_mock_host_api(mock_register_component);

    let mut ctx = MockHostContext {
        captured_desc: None,
    };
    let ctx_ptr = &mut ctx as *mut MockHostContext as *mut c_void;

    let res = unsafe {
        register_runtime_component::<PanickingDropComponent>(&api, ctx_ptr, c"PanickingDrop")
    };
    assert_eq!(res, FfiResult::OK);

    let desc = ctx.captured_desc.take().expect("Descriptor captured");
    let drop_fn = desc.drop_fn.expect("drop_fn must be installed");

    let mut item = core::mem::ManuallyDrop::new(PanickingDropComponent);
    let ptr = &mut *item as *mut PanickingDropComponent as *mut c_void;

    // Invoking drop_fn on panicking component must NOT unwind into caller!
    // generic_drop_trampoline catches the unwind using std::panic::catch_unwind
    unsafe {
        drop_fn(ptr);
    }
    // Execution continues without crashing!
}

#[test]
fn test_chunk_slice_as_typed_slice_defensive_checks() {
    let data = [10u32, 20, 30, 40];
    let raw_ptr = data.as_ptr() as *const u8;

    // 1. Valid typed slice
    let slice_valid = ChunkSlice::new(raw_ptr, 4, 4);
    let typed = slice_valid.as_typed_slice::<u32>().expect("Valid slice");
    assert_eq!(typed, &[10, 20, 30, 40]);

    // 2. Stride mismatch: stride is 8, but u32 is 4
    let slice_bad_stride = ChunkSlice::new(raw_ptr, 8, 2);
    assert!(
        slice_bad_stride.as_typed_slice::<u32>().is_none(),
        "Stride mismatch must return None"
    );

    // 3. Stride mismatch: stride is 2, but u32 is 4
    let slice_small_stride = ChunkSlice::new(raw_ptr, 2, 8);
    assert!(
        slice_small_stride.as_typed_slice::<u32>().is_none(),
        "Stride mismatch must return None"
    );

    // 4. Misaligned pointer: raw_ptr + 1 is misaligned for u32
    let misaligned_ptr = unsafe { raw_ptr.add(1) };
    let slice_misaligned = ChunkSlice::new(misaligned_ptr, 4, 3);
    assert!(
        slice_misaligned.as_typed_slice::<u32>().is_none(),
        "Misaligned pointer must return None"
    );

    // 5. Zero elements: count == 0 returns Some(&[]) safely even with null or misaligned pointer
    let slice_empty_null = ChunkSlice::new(core::ptr::null(), 4, 0);
    assert_eq!(
        slice_empty_null.as_typed_slice::<u32>(),
        Some(&[][..]),
        "Empty slice must return Some(&[])"
    );

    let slice_empty_misaligned = ChunkSlice::new(misaligned_ptr, 4, 0);
    assert_eq!(
        slice_empty_misaligned.as_typed_slice::<u32>(),
        Some(&[][..]),
        "Empty slice with misaligned pointer must return Some(&[]) without dereference"
    );

    // 6. Null pointer with count > 0 returns None
    let slice_null_with_count = ChunkSlice::new(core::ptr::null(), 4, 5);
    assert!(
        slice_null_with_count.as_typed_slice::<u32>().is_none(),
        "Null pointer with count > 0 must return None"
    );
}

#[test]
fn test_chunk_slice_mut_as_typed_slice_defensive_checks() {
    let mut data = [10u32, 20, 30, 40];
    let raw_ptr = data.as_mut_ptr() as *mut u8;

    // 1. Valid mutable slice
    let mut slice_valid = ChunkSliceMut::new(raw_ptr, 4, 4);
    {
        let typed = slice_valid.as_typed_slice::<u32>().expect("Valid slice");
        typed[1] = 999;
    }
    assert_eq!(data[1], 999);

    // 2. Stride mismatch
    let mut slice_bad_stride = ChunkSliceMut::new(raw_ptr, 8, 2);
    assert!(slice_bad_stride.as_typed_slice::<u32>().is_none());

    // 3. Misaligned pointer
    let misaligned_ptr = unsafe { raw_ptr.add(2) };
    let mut slice_misaligned = ChunkSliceMut::new(misaligned_ptr, 4, 3);
    assert!(slice_misaligned.as_typed_slice::<u32>().is_none());

    // 4. Zero elements
    let mut slice_empty_null = ChunkSliceMut::new(core::ptr::null_mut(), 4, 0);
    assert_eq!(slice_empty_null.as_typed_slice::<u32>(), Some(&mut [][..]));
}

#[test]
fn test_guest_command_queue_insert_runtime_component_contract() {
    #[derive(Default)]
    struct QueueContext {
        last_entity: FfiEntity,
        last_name: String,
        last_bytes: Vec<u8>,
    }

    unsafe extern "C" fn mock_insert(
        queue: *mut RawCommandQueue,
        entity: FfiEntity,
        name: *const c_char,
        data: *const u8,
        len: usize,
    ) -> FfiResult {
        let ctx = unsafe { &mut *(queue as *mut QueueContext) };
        ctx.last_entity = entity;
        ctx.last_name = unsafe { CStr::from_ptr(name) }
            .to_str()
            .unwrap()
            .to_string();
        ctx.last_bytes = unsafe { core::slice::from_raw_parts(data, len) }.to_vec();
        FfiResult::OK
    }

    let mut q_ctx = QueueContext::default();
    let vtable = CommandBridgeVTable {
        spawn_empty: None,
        despawn: None,
        insert_component: Some(mock_insert),
    };

    let mut queue = GuestCommandQueue::new(
        &mut q_ctx as *mut QueueContext as *mut RawCommandQueue,
        vtable,
    );

    let comp = PodComponent { x: 3.14, y: 42 };
    let res = queue.insert_runtime_component(FfiEntity(123), c"PodComponent", &comp);
    assert_eq!(res, FfiResult::OK);

    assert_eq!(q_ctx.last_entity, FfiEntity(123));
    assert_eq!(q_ctx.last_name, "PodComponent");
    assert_eq!(q_ctx.last_bytes.len(), 8);

    let read_comp = unsafe { *(q_ctx.last_bytes.as_ptr() as *const PodComponent) };
    assert_eq!(read_comp, comp);
}
