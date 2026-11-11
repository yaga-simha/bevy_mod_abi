#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
//! Unit and regression tests for `bevy_abi_mod_guest` (`S-01`, `S-02`, `S-03`, `C-02`, `G-06`).

use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use bevy_abi_mod_guest::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
struct PosData {
    x: f32,
    y: f32,
}

unsafe impl FfiLayout for PosData {}
unsafe impl AbiPod for PosData {}

impl GuestComponent for PosData {
    const NAME: &'static CStr = c"PosData";
}

unsafe fn panicking_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    panic!("mod crash");
}

unsafe fn healthy_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

wrap_system_callback!(fn c_panicking_system calls panicking_system);
wrap_system_callback!(fn c_healthy_system calls healthy_system);

#[test]
fn test_guest_panic_containment_returns_panic_caught() {
    let status = unsafe {
        c_panicking_system(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(status, FfiResult::PANIC_CAUGHT);

    let ok_status = unsafe {
        c_healthy_system(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(ok_status, FfiResult::OK);
}

#[test]
fn regression_s01_chunk_slice_over_stack_buffer() {
    let mut data = [10u32, 20u32, 30u32, 40u32];
    let stride = core::mem::size_of::<u32>();
    let count = data.len();

    let slice = unsafe { ChunkSlice::new(data.as_ptr() as *const u8, stride, count) };
    assert_eq!(slice.count(), 4);
    assert_eq!(slice.stride(), 4);
    assert!(!slice.is_empty());
    assert_eq!(slice.as_typed_slice::<u32>(), Some(&[10, 20, 30, 40][..]));

    let p0 = slice.get_ptr(0).unwrap();
    assert_eq!(unsafe { *(p0 as *const u32) }, 10);

    let p2 = slice.get_ptr(2).unwrap();
    assert_eq!(unsafe { *(p2 as *const u32) }, 30);

    assert!(slice.get_ptr(4).is_none());
    assert!(slice.get_ptr(100).is_none());

    let massive_slice = unsafe { ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX) };
    assert!(massive_slice.get_ptr(usize::MAX / 2).is_none());

    let mut slice_mut = unsafe { ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, count) };
    let p1 = slice_mut.get_ptr_mut(1).unwrap();
    unsafe { *(p1 as *mut u32) = 99 };
    assert_eq!(data[1], 99);
    assert!(slice_mut.get_ptr_mut(4).is_none());

    let mut massive_slice_mut =
        unsafe { ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, usize::MAX) };
    assert!(massive_slice_mut.get_ptr_mut(usize::MAX / 2).is_none());
}

#[test]
fn regression_s01_chunk_slice_as_typed_slice_pod() {
    let mut data = [PosData { x: 1.0, y: 2.0 }, PosData { x: 3.0, y: 4.0 }];
    let stride = core::mem::size_of::<PosData>();
    let slice = unsafe { ChunkSlice::new(data.as_ptr() as *const u8, stride, 2) };

    let typed = slice
        .as_typed_slice::<PosData>()
        .expect("valid typed slice");
    assert_eq!(typed.len(), 2);
    assert_eq!(typed[0], PosData { x: 1.0, y: 2.0 });
    assert_eq!(typed[1], PosData { x: 3.0, y: 4.0 });

    let wrong_stride = unsafe { ChunkSlice::new(data.as_ptr() as *const u8, stride + 4, 2) };
    assert!(wrong_stride.as_typed_slice::<PosData>().is_none());

    let misaligned_ptr = unsafe { (data.as_ptr() as *const u8).add(1) };
    let misaligned = unsafe { ChunkSlice::new(misaligned_ptr, stride, 1) };
    assert!(misaligned.as_typed_slice::<PosData>().is_none());

    let empty = unsafe { ChunkSlice::new(core::ptr::null(), stride, 0) };
    assert_eq!(empty.as_typed_slice::<PosData>(), Some(&[][..]));

    let empty_bad_stride = unsafe { ChunkSlice::new(core::ptr::null(), stride + 1, 0) };
    assert!(empty_bad_stride.as_typed_slice::<PosData>().is_none());

    let massive = unsafe { ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX) };
    assert!(massive.as_typed_slice::<PosData>().is_none());

    let mut slice_mut = unsafe { ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, 2) };
    {
        let typed_mut = slice_mut
            .as_typed_slice::<PosData>()
            .expect("valid typed slice mut");
        typed_mut[0].x = 10.0;
        typed_mut[1].y = 40.0;
    }
    assert_eq!(data[0], PosData { x: 10.0, y: 2.0 });
    assert_eq!(data[1], PosData { x: 3.0, y: 40.0 });
}

#[test]
fn regression_s01_guest_command_queue_over_stack_context() {
    static CAPTURED_LEN: AtomicUsize = AtomicUsize::new(0);
    static CAPTURED_ENTITY: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn mock_insert(
        _queue: *mut RawCommandQueue,
        entity: FfiEntity,
        _name: *const c_char,
        _data: *const u8,
        len: usize,
    ) -> FfiResult {
        CAPTURED_LEN.store(len, Ordering::SeqCst);
        CAPTURED_ENTITY.store(entity.0, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut queue = unsafe {
        GuestCommandQueue::new(
            core::ptr::without_provenance_mut(0x1234),
            CommandBridgeVTable {
                spawn_empty: None,
                despawn: None,
                insert_component: Some(mock_insert),
            },
        )
    };

    let pos = PosData { x: 5.0, y: 7.0 };
    let res = queue.insert_runtime_component(FfiEntity(42), c"PosData", &pos);
    assert_eq!(res, FfiResult::OK);
    assert_eq!(
        CAPTURED_LEN.load(Ordering::SeqCst),
        core::mem::size_of::<PosData>()
    );
    assert_eq!(CAPTURED_ENTITY.load(Ordering::SeqCst), 42);
}

#[test]
fn regression_s02_count_cannot_exceed_chunk() {
    struct TwoChunkMock {
        cursor: usize,
        entities: [[FfiEntity; 3]; 2],
        col_ro: [[PosData; 3]; 2],
        col_rw: [[PosData; 3]; 2],
    }

    unsafe extern "C" fn next_chunk(iter: *mut RawChunkIter) -> usize {
        let m = unsafe { &mut *(iter as *mut TwoChunkMock) };
        let len = match m.cursor {
            0 => 2,
            1 => 3,
            _ => 0,
        };
        if m.cursor < 2 {
            m.cursor += 1;
        }
        len
    }

    unsafe extern "C" fn reset_iter(iter: *mut RawChunkIter) -> FfiResult {
        let m = unsafe { &mut *(iter as *mut TwoChunkMock) };
        m.cursor = 0;
        FfiResult::OK
    }

    unsafe extern "C" fn col_ro(iter: *const RawChunkIter, index: usize) -> *const u8 {
        let m = unsafe { &*(iter as *const TwoChunkMock) };
        let Some(c) = m.cursor.checked_sub(1) else {
            return core::ptr::null();
        };
        match index {
            0 => m.col_ro[c].as_ptr() as *const u8,
            _ => core::ptr::null(),
        }
    }

    unsafe extern "C" fn col_rw(iter: *mut RawChunkIter, index: usize) -> *mut u8 {
        let m = unsafe { &mut *(iter as *mut TwoChunkMock) };
        let Some(c) = m.cursor.checked_sub(1) else {
            return core::ptr::null_mut();
        };
        match index {
            1 => m.col_rw[c].as_mut_ptr() as *mut u8,
            _ => core::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn mark_changed(
        _iter: *mut RawChunkIter,
        _index: usize,
        _row: usize,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn entities(iter: *const RawChunkIter) -> *const FfiEntity {
        let m = unsafe { &*(iter as *const TwoChunkMock) };
        let Some(c) = m.cursor.checked_sub(1) else {
            return core::ptr::null();
        };
        m.entities[c].as_ptr()
    }

    unsafe extern "C" fn stride(_iter: *const RawChunkIter, _index: usize) -> usize {
        core::mem::size_of::<PosData>()
    }

    unsafe extern "C" fn stub_reg_sys(_: *mut c_void, _: *const SystemDescriptor) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_reg_comp(
        _: *mut c_void,
        _: *const ModComponentDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }

    let api = HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: ABI_VERSION_PACKED,
        register_system: stub_reg_sys,
        iter_next_chunk: next_chunk,
        iter_column_readonly: col_ro,
        iter_column_mutable: col_rw,
        iter_mark_changed: mark_changed,
        iter_entities: entities,
        iter_column_stride: stride,
        register_component: stub_reg_comp,
    };

    let mut mock = TwoChunkMock {
        cursor: 0,
        entities: [
            [FfiEntity(1), FfiEntity(2), FfiEntity(999)],
            [FfiEntity(10), FfiEntity(20), FfiEntity(30)],
        ],
        col_ro: [
            [
                PosData { x: 1.0, y: 1.0 },
                PosData { x: 2.0, y: 2.0 },
                PosData { x: 99.0, y: 99.0 },
            ],
            [
                PosData { x: 10.0, y: 10.0 },
                PosData { x: 20.0, y: 20.0 },
                PosData { x: 30.0, y: 30.0 },
            ],
        ],
        col_rw: [[PosData { x: 0.0, y: 0.0 }; 3]; 2],
    };

    let mut iter = unsafe {
        GuestChunkIter::with_reset(
            core::ptr::addr_of_mut!(mock) as *mut RawChunkIter,
            &api,
            Some(reset_iter),
        )
    };

    // 1. Before next_chunk: current_len == 0, all slices empty
    assert_eq!(iter.current_len(), 0);
    assert!(iter.entities_slice().is_empty());
    assert_eq!(iter.readonly_slice(0).count(), 0);
    assert_eq!(iter.mutable_slice(1).count(), 0);

    // 2. Chunk 0 has exactly 2 rows (even though backing array has 3 elements)
    assert_eq!(iter.next_chunk(), 2);
    assert_eq!(iter.current_len(), 2);
    assert_eq!(iter.entities_slice(), &[FfiEntity(1), FfiEntity(2)]);
    assert_eq!(iter.readonly_slice(0).count(), 2);
    assert_eq!(iter.mutable_slice(1).count(), 2);
    {
        let (ro, rw) = iter.read_write::<PosData, PosData>(0, 1).unwrap();
        assert_eq!(ro.len(), 2);
        assert_eq!(rw.len(), 2);
        rw.copy_from_slice(ro);
    }

    // 3. Reset zeroes current_len immediately before next_chunk is called again
    assert_eq!(iter.reset(), FfiResult::OK);
    assert_eq!(iter.current_len(), 0);
    assert!(iter.entities_slice().is_empty());
    assert_eq!(iter.readonly_slice(0).count(), 0);

    // 4. Re-iterate both chunks to exhaustion; exhaustion zeroes current_len
    assert_eq!(iter.next_chunk(), 2);
    assert_eq!(iter.current_len(), 2);
    assert_eq!(iter.next_chunk(), 3);
    assert_eq!(iter.current_len(), 3);
    assert_eq!(
        iter.entities_slice(),
        &[FfiEntity(10), FfiEntity(20), FfiEntity(30)]
    );
    assert_eq!(iter.next_chunk(), 0);
    assert_eq!(iter.current_len(), 0);
    assert!(iter.entities_slice().is_empty());
    assert_eq!(iter.readonly_slice(0).count(), 0);
    assert_eq!(iter.mutable_slice(1).count(), 0);
}

#[test]
fn test_register_runtime_component_mock() {
    static CAPTURED_NAME: Mutex<Option<std::string::String>> = Mutex::new(None);
    static CAPTURED_SIZE: AtomicUsize = AtomicUsize::new(0);
    static CAPTURED_ALIGN: AtomicUsize = AtomicUsize::new(0);
    static CAPTURED_DROP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "C" fn mock_register_component(
        _ctx: *mut c_void,
        desc: *const ModComponentDescriptor,
    ) -> FfiResult {
        assert!(!desc.is_null());
        let d = unsafe { &*desc };
        *CAPTURED_NAME.lock().unwrap() = Some(
            unsafe { CStr::from_ptr(d.name) }
                .to_str()
                .unwrap()
                .to_string(),
        );
        CAPTURED_SIZE.store(d.size, Ordering::SeqCst);
        CAPTURED_ALIGN.store(d.align, Ordering::SeqCst);
        CAPTURED_DROP.store(d.drop_fn.is_some(), Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe extern "C" fn stub_register_system(
        _: *mut c_void,
        _: *const SystemDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_iter_next_chunk(_: *mut RawChunkIter) -> usize {
        0
    }
    unsafe extern "C" fn stub_iter_column_readonly(_: *const RawChunkIter, _: usize) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_iter_column_mutable(_: *mut RawChunkIter, _: usize) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn stub_iter_mark_changed(
        _: *mut RawChunkIter,
        _: usize,
        _: usize,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_iter_entities(_: *const RawChunkIter) -> *const FfiEntity {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_iter_column_stride(_: *const RawChunkIter, _: usize) -> usize {
        0
    }

    let dummy_host_api = HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: ABI_VERSION_PACKED,
        register_system: stub_register_system,
        iter_next_chunk: stub_iter_next_chunk,
        iter_column_readonly: stub_iter_column_readonly,
        iter_column_mutable: stub_iter_column_mutable,
        iter_mark_changed: stub_iter_mark_changed,
        iter_entities: stub_iter_entities,
        iter_column_stride: stub_iter_column_stride,
        register_component: mock_register_component,
    };

    let dummy_ctx = core::ptr::without_provenance_mut(0x1234);

    let res =
        unsafe { register_runtime_component::<PosData>(&dummy_host_api, dummy_ctx, c"PosData") };
    assert_eq!(res, FfiResult::OK);
    assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("PosData"));
    assert_eq!(
        CAPTURED_SIZE.load(Ordering::SeqCst),
        core::mem::size_of::<PosData>()
    );
    assert_eq!(
        CAPTURED_ALIGN.load(Ordering::SeqCst),
        core::mem::align_of::<PosData>()
    );
    assert!(!CAPTURED_DROP.load(Ordering::SeqCst));

    #[allow(dead_code)]
    struct HasDrop(std::string::String);
    let res_drop =
        unsafe { register_runtime_component::<HasDrop>(&dummy_host_api, dummy_ctx, c"HasDrop") };
    assert_eq!(res_drop, FfiResult::OK);
    assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("HasDrop"));
    assert!(CAPTURED_DROP.load(Ordering::SeqCst));
}

#[test]
fn test_guest_component_descriptors_and_commands() {
    assert_eq!(
        PosData::optional_read_desc().mode,
        ComponentAccessMode::OPTIONAL_READ
    );
    assert_eq!(
        PosData::optional_write_desc().mode,
        ComponentAccessMode::OPTIONAL_WRITE
    );
    assert_eq!(PosData::changed_desc().mode, ComponentAccessMode::CHANGED);
    assert_eq!(PosData::added_desc().mode, ComponentAccessMode::ADDED);
    assert_eq!(
        PosData::resource_read_desc().mode,
        ComponentAccessMode::RESOURCE_READ
    );
    assert_eq!(
        PosData::resource_write_desc().mode,
        ComponentAccessMode::RESOURCE_WRITE
    );

    static REMOVED_ENT: AtomicU64 = AtomicU64::new(0);
    static DEFAULT_ENT: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn mock_remove(
        _q: *mut RawCommandQueue,
        entity: FfiEntity,
        _name: *const c_char,
    ) -> FfiResult {
        REMOVED_ENT.store(entity.0, Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe extern "C" fn mock_insert_default(
        _q: *mut RawCommandQueue,
        entity: FfiEntity,
        _name: *const c_char,
    ) -> FfiResult {
        DEFAULT_ENT.store(entity.0, Ordering::SeqCst);
        FfiResult::OK
    }

    let ext = CommandBridgeVTableExt {
        base: CommandBridgeVTable::default(),
        remove_component: Some(mock_remove),
        insert_default: Some(mock_insert_default),
    };
    let mut queue =
        unsafe { GuestCommandQueue::with_ext(core::ptr::without_provenance_mut(0x9999), ext) };
    assert_eq!(
        queue.remove_guest_component::<PosData>(FfiEntity(77)),
        FfiResult::OK
    );
    assert_eq!(REMOVED_ENT.load(Ordering::SeqCst), 77);
    assert_eq!(
        queue.insert_guest_default::<PosData>(FfiEntity(88)),
        FfiResult::OK
    );
    assert_eq!(DEFAULT_ENT.load(Ordering::SeqCst), 88);
}

unsafe fn panicking_cond(_ud: *mut c_void) -> bool {
    panic!("cond panic");
}
unsafe fn true_cond(_ud: *mut c_void) -> bool {
    true
}
wrap_run_condition!(fn c_panicking_cond calls panicking_cond);
wrap_run_condition!(fn c_true_cond calls true_cond);

#[test]
fn test_wrap_run_condition_panic_containment() {
    assert_eq!(
        unsafe { c_panicking_cond(core::ptr::null_mut()) },
        FFI_BOOL_FALSE
    );
    assert_eq!(unsafe { c_true_cond(core::ptr::null_mut()) }, FFI_BOOL_TRUE);
}

#[test]
fn test_multi_query_context_blocks_iter_resource_aliasing_and_resolves_group1_resource() {
    static SHARED_RES: Mutex<PosData> = Mutex::new(PosData { x: 10.0, y: 20.0 });
    static CAPTURED_CB: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    static CAPTURED_UD: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    const GROUP0_SENTINEL: usize = 0xA000;
    const GROUP1_SENTINEL: usize = 0xB000;

    unsafe extern "C" fn mock_res_readonly(
        iter: *const RawChunkIter,
        _name: *const c_char,
    ) -> *const u8 {
        if iter as usize == GROUP1_SENTINEL {
            let guard = SHARED_RES.lock().unwrap();
            (&*guard as *const PosData).cast::<u8>()
        } else {
            core::ptr::null()
        }
    }

    unsafe extern "C" fn mock_res_mutable(
        iter: *mut RawChunkIter,
        _name: *const c_char,
    ) -> *mut u8 {
        if iter as usize == GROUP1_SENTINEL {
            let mut guard = SHARED_RES.lock().unwrap();
            (&mut *guard as *mut PosData).cast::<u8>()
        } else {
            core::ptr::null_mut()
        }
    }

    unsafe extern "C" fn mock_reg_multi(
        _ctx: *mut c_void,
        desc: *const MultiQuerySystemDescriptor,
    ) -> FfiResult {
        let d = unsafe { &*desc };
        CAPTURED_CB.store(d.callback as *mut c_void, Ordering::SeqCst);
        CAPTURED_UD.store(d.user_data, Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe extern "C" fn mock_get_proc_address(symbol_name: *const c_char) -> *const c_void {
        let name = unsafe { CStr::from_ptr(symbol_name) }.to_str().unwrap();
        match name {
            "register_multi_query_system" => mock_reg_multi as *const c_void,
            "res_readonly" => mock_res_readonly as *const c_void,
            "res_mutable" => mock_res_mutable as *const c_void,
            _ => core::ptr::null(),
        }
    }

    unsafe extern "C" fn stub_reg_sys(_: *mut c_void, _: *const SystemDescriptor) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_next(_: *mut RawChunkIter) -> usize {
        0
    }
    unsafe extern "C" fn stub_ro(_: *const RawChunkIter, _: usize) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_rw(_: *mut RawChunkIter, _: usize) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn stub_mark(_: *mut RawChunkIter, _: usize, _: usize) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_ents(_: *const RawChunkIter) -> *const FfiEntity {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_stride(_: *const RawChunkIter, _: usize) -> usize {
        0
    }
    unsafe extern "C" fn stub_reg_comp(
        _: *mut c_void,
        _: *const ModComponentDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }

    let host_api = HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: ABI_VERSION_PACKED,
        register_system: stub_reg_sys,
        iter_next_chunk: stub_next,
        iter_column_readonly: stub_ro,
        iter_column_mutable: stub_rw,
        iter_mark_changed: stub_mark,
        iter_entities: stub_ents,
        iter_column_stride: stub_stride,
        register_component: stub_reg_comp,
    };

    *SHARED_RES.lock().unwrap() = PosData { x: 10.0, y: 20.0 };

    let mut app = unsafe {
        GuestApp::from_raw(
            mock_get_proc_address,
            &host_api as *const HostApi,
            core::ptr::without_provenance_mut(0x5555),
        )
        .expect("GuestApp::from_raw")
    };
    let captured_res_vt = app.resource_vtable();

    let q0_terms = [PosData::read_desc()];
    let q1_terms = [PosData::resource_write_desc()];
    let groups: [&[ComponentAccessDesc]; 2] = [&q0_terms, &q1_terms];

    assert_eq!(
        app.add_multi_query_system(c"mq_res_safety", &groups, move |ctx| {
            {
                let (q0, q1) = ctx.iters_2_mut(0, 1).unwrap();
                unsafe {
                    q0.set_resource_vtable(captured_res_vt);
                    q1.set_resource_vtable(captured_res_vt);
                }
                assert!(q0.resource::<PosData>(c"PosData").is_none());
                assert!(q1.resource::<PosData>(c"PosData").is_none());
                assert!(q0.resource_mut::<PosData>(c"PosData").is_none());
                assert!(q1.resource_mut::<PosData>(c"PosData").is_none());
            }

            assert_eq!(ctx.guest_resource::<PosData>().unwrap().x, 10.0);
            let res_mut = ctx.guest_resource_mut::<PosData>().unwrap();
            res_mut.x += 5.0;
            res_mut.y += 7.0;
            FfiResult::OK
        }),
        FfiResult::OK
    );
    app.commit();

    let cb_ptr = CAPTURED_CB.load(Ordering::SeqCst);
    let ud_ptr = CAPTURED_UD.load(Ordering::SeqCst);
    assert!(!cb_ptr.is_null());
    let cb: MultiQuerySystemCallback = unsafe { core::mem::transmute(cb_ptr) };
    let raw_iters = [
        core::ptr::without_provenance_mut(GROUP0_SENTINEL),
        core::ptr::without_provenance_mut(GROUP1_SENTINEL),
    ];
    let status = unsafe { cb(raw_iters.as_ptr(), 2, core::ptr::null_mut(), ud_ptr) };
    assert_eq!(status, FfiResult::OK);
    assert_eq!(*SHARED_RES.lock().unwrap(), PosData { x: 15.0, y: 27.0 });

    cleanup_registered_guest_state();
}
