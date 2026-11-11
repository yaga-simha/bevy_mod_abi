#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
//! Guest SDK dynamic component and typed slice stress suite.

use bevy_abi_mod_guest::{
    AbiPod, ChunkSlice, ChunkSliceMut, ChunkSliceMutRawExt, ChunkSliceRawExt, CommandBridgeVTable,
    FfiLayout, GuestCommandQueue, GuestCommandQueueRawExt, register_runtime_component,
};
use bevy_abi_mod_sys::{
    FfiEntity, FfiResult, HostApi, ModComponentDescriptor, RawCommandQueue, RegisterComponentFn,
};
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PodComponent {
    x: f32,
    y: u32,
}

unsafe impl FfiLayout for PodComponent {}
unsafe impl AbiPod for PodComponent {}

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
    unsafe extern "C" fn stub_register_system(
        _: *mut c_void,
        _: *const bevy_abi_mod_sys::SystemDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_iter_next_chunk(_: *mut bevy_abi_mod_sys::RawChunkIter) -> usize {
        0
    }
    unsafe extern "C" fn stub_iter_column_readonly(
        _: *const bevy_abi_mod_sys::RawChunkIter,
        _: usize,
    ) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_iter_column_mutable(
        _: *mut bevy_abi_mod_sys::RawChunkIter,
        _: usize,
    ) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn stub_iter_mark_changed(
        _: *mut bevy_abi_mod_sys::RawChunkIter,
        _: usize,
        _: usize,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_iter_entities(
        _: *const bevy_abi_mod_sys::RawChunkIter,
    ) -> *const FfiEntity {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_iter_column_stride(
        _: *const bevy_abi_mod_sys::RawChunkIter,
        _: usize,
    ) -> usize {
        0
    }

    HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: bevy_abi_mod_sys::ABI_VERSION_PACKED,
        register_system: stub_register_system,
        iter_next_chunk: stub_iter_next_chunk,
        iter_column_readonly: stub_iter_column_readonly,
        iter_column_mutable: stub_iter_column_mutable,
        iter_mark_changed: stub_iter_mark_changed,
        iter_entities: stub_iter_entities,
        iter_column_stride: stub_iter_column_stride,
        register_component: reg_fn,
    }
}

#[test]
fn test_register_runtime_component_pod_vs_non_pod() {
    let api = create_mock_host_api(mock_register_component);

    let mut ctx = MockHostContext {
        captured_desc: None,
    };
    let ctx_ptr = core::ptr::addr_of_mut!(ctx) as *mut c_void;

    let res = unsafe { register_runtime_component::<PodComponent>(&api, ctx_ptr, c"PodComponent") };
    assert_eq!(res, FfiResult::OK);

    let desc_pod = ctx.captured_desc.take().expect("Descriptor captured");
    assert_eq!(desc_pod.size, 8);
    assert_eq!(desc_pod.align, 4);
    assert!(desc_pod.drop_fn.is_none());

    let res_non_pod =
        unsafe { register_runtime_component::<NonPodComponent>(&api, ctx_ptr, c"NonPodComponent") };
    assert_eq!(res_non_pod, FfiResult::OK);

    let desc_non_pod = ctx.captured_desc.take().expect("Descriptor captured");
    assert_eq!(desc_non_pod.size, core::mem::size_of::<NonPodComponent>());
    assert_eq!(desc_non_pod.align, core::mem::align_of::<NonPodComponent>());
    assert!(desc_non_pod.drop_fn.is_some());

    NON_POD_DROPPED.store(false, Ordering::SeqCst);
    let instance = Box::new(NonPodComponent {
        _tag: "Hello".to_string(),
    });
    let raw_instance = Box::into_raw(instance) as *mut c_void;

    let drop_fn = desc_non_pod.drop_fn.unwrap();
    unsafe {
        drop_fn(raw_instance);
    }
    assert!(NON_POD_DROPPED.load(Ordering::SeqCst));
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
    let ctx_ptr = core::ptr::addr_of_mut!(ctx) as *mut c_void;

    let res = unsafe {
        register_runtime_component::<PanickingDropComponent>(&api, ctx_ptr, c"PanickingDrop")
    };
    assert_eq!(res, FfiResult::OK);

    let desc = ctx.captured_desc.take().expect("Descriptor captured");
    let drop_fn = desc.drop_fn.expect("drop_fn must be installed");

    let mut item = core::mem::ManuallyDrop::new(PanickingDropComponent);
    let ptr = &mut *item as *mut PanickingDropComponent as *mut c_void;

    unsafe {
        drop_fn(ptr);
    }
}

#[test]
fn test_chunk_slice_as_typed_slice_defensive_checks() {
    let data = [10u32, 20, 30, 40];
    let raw_ptr = data.as_ptr() as *const u8;

    let slice_valid = unsafe { ChunkSlice::new(raw_ptr, 4, 4) };
    let typed = slice_valid.as_typed_slice::<u32>().expect("Valid slice");
    assert_eq!(typed, &[10, 20, 30, 40]);

    let slice_bad_stride = unsafe { ChunkSlice::new(raw_ptr, 8, 2) };
    assert!(slice_bad_stride.as_typed_slice::<u32>().is_none());

    let slice_small_stride = unsafe { ChunkSlice::new(raw_ptr, 2, 8) };
    assert!(slice_small_stride.as_typed_slice::<u32>().is_none());

    let misaligned_ptr = unsafe { raw_ptr.add(1) };
    let slice_misaligned = unsafe { ChunkSlice::new(misaligned_ptr, 4, 3) };
    assert!(slice_misaligned.as_typed_slice::<u32>().is_none());

    let slice_empty_null = unsafe { ChunkSlice::new(core::ptr::null(), 4, 0) };
    assert_eq!(slice_empty_null.as_typed_slice::<u32>(), Some(&[][..]));

    let slice_empty_misaligned = unsafe { ChunkSlice::new(misaligned_ptr, 4, 0) };
    assert_eq!(
        slice_empty_misaligned.as_typed_slice::<u32>(),
        Some(&[][..])
    );

    let slice_null_with_count = unsafe { ChunkSlice::new(core::ptr::null(), 4, 5) };
    assert!(slice_null_with_count.as_typed_slice::<u32>().is_none());
}

#[test]
fn test_chunk_slice_mut_as_typed_slice_defensive_checks() {
    let mut data = [10u32, 20, 30, 40];
    let raw_ptr = data.as_mut_ptr() as *mut u8;

    let mut slice_valid = unsafe { ChunkSliceMut::new(raw_ptr, 4, 4) };
    {
        let typed = slice_valid.as_typed_slice::<u32>().expect("Valid slice");
        typed[1] = 999;
    }
    assert_eq!(data[1], 999);

    let mut slice_bad_stride = unsafe { ChunkSliceMut::new(raw_ptr, 8, 2) };
    assert!(slice_bad_stride.as_typed_slice::<u32>().is_none());

    let misaligned_ptr = unsafe { raw_ptr.add(2) };
    let mut slice_misaligned = unsafe { ChunkSliceMut::new(misaligned_ptr, 4, 3) };
    assert!(slice_misaligned.as_typed_slice::<u32>().is_none());

    let mut slice_empty_null = unsafe { ChunkSliceMut::new(core::ptr::null_mut(), 4, 0) };
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

    let mut queue = unsafe {
        GuestCommandQueue::new(
            core::ptr::addr_of_mut!(q_ctx) as *mut RawCommandQueue,
            vtable,
        )
    };

    let comp = PodComponent { x: 2.5, y: 42 };
    let res = queue.insert_runtime_component(FfiEntity(123), c"PodComponent", &comp);
    assert_eq!(res, FfiResult::OK);

    assert_eq!(q_ctx.last_entity, FfiEntity(123));
    assert_eq!(q_ctx.last_name, "PodComponent");
    assert_eq!(q_ctx.last_bytes.len(), 8);

    let read_comp =
        unsafe { core::ptr::read_unaligned(q_ctx.last_bytes.as_ptr() as *const PodComponent) };
    assert_eq!(read_comp, comp);
}
