//! Non-POD message witness round-trip tests (moved out of `src/iter.rs`, reusing shared mock stubs).

mod common;

use core::ffi::{CStr, c_char, c_void};

use bevy_abi_mod_guest_sys::{Host, UuidFilter, ValidatedIter};
use bevy_abi_mod_sys::{
    AccessMode, BAM_STORAGE_FLAG_MESSAGE, BamColumnInfo, BamComponent, BamHostCtx, BamIter,
    ComponentUuid, FfiResult, FfiSlice, ModComponent, StrView, SymbolEntry,
};
use common::make_mock_api;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct MockNonPodMsg {
    label: String,
    nums: Vec<u32>,
}

unsafe extern "C" fn mock_export_fields(
    src_ptr: *const c_void,
    sink: Option<
        unsafe extern "C" fn(
            sink_ctx: *mut c_void,
            field_name: StrView,
            type_tag: StrView,
            payload: FfiSlice<u8>,
        ) -> FfiResult,
    >,
    sink_ctx: *mut c_void,
) -> FfiResult {
    // SAFETY: `src_ptr` points to a live `MockNonPodMsg`.
    let msg = unsafe { &*src_ptr.cast::<MockNonPodMsg>() };
    let Some(sink_fn) = sink else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: Forwarding live UTF-8 field slice into `sink_fn`.
    let rc = unsafe {
        sink_fn(
            sink_ctx,
            StrView::from_str("label"),
            StrView::from_str("String"),
            FfiSlice::from_slice(msg.label.as_bytes()),
        )
    };
    if !rc.is_ok() {
        return rc;
    }
    let mut raw_nums = Vec::with_capacity(4 + msg.nums.len() * 4);
    raw_nums.extend_from_slice(&(msg.nums.len() as u32).to_le_bytes());
    for &n in &msg.nums {
        raw_nums.extend_from_slice(&n.to_le_bytes());
    }
    // SAFETY: Forwarding encoded `Vec<u32>` slice into `sink_fn`.
    unsafe {
        sink_fn(
            sink_ctx,
            StrView::from_str("nums"),
            StrView::from_str("Vec<u32>"),
            FfiSlice::from_slice(&raw_nums),
        )
    }
}

unsafe extern "C" fn mock_import_field(
    dst_ptr: *mut c_void,
    field_name: StrView,
    _type_tag: StrView,
    payload: FfiSlice<u8>,
) -> FfiResult {
    // SAFETY: `dst_ptr` points to a valid `MockNonPodMsg`.
    let dst = unsafe { &mut *dst_ptr.cast::<MockNonPodMsg>() };
    // SAFETY: `field_name` and `payload` borrow live slices for this synchronous callback.
    let Ok(name) = (unsafe { field_name.try_as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    // SAFETY: Same as above for `payload`.
    let Ok(bytes) = (unsafe { payload.try_as_slice() }) else {
        return FfiResult::NULL_POINTER;
    };
    match name {
        "label" => {
            let Ok(s) = core::str::from_utf8(bytes) else {
                return FfiResult::INVALID_UTF8;
            };
            dst.label = s.to_owned();
        }
        "nums" => {
            if bytes.len() < 4 {
                return FfiResult::INVALID_ARGUMENT;
            }
            let cnt = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            if bytes.len() != 4 + cnt * 4 {
                return FfiResult::INVALID_ARGUMENT;
            }
            dst.nums.clear();
            for i in 0..cnt {
                let off = 4 + i * 4;
                dst.nums.push(u32::from_le_bytes([
                    bytes[off],
                    bytes[off + 1],
                    bytes[off + 2],
                    bytes[off + 3],
                ]));
            }
        }
        _ => return FfiResult::INVALID_ARGUMENT,
    }
    FfiResult::OK
}

static MOCK_SYMS: [SymbolEntry; 2] = [
    SymbolEntry {
        name: "bam.v0.mock::NonPodChat.export_fields",
        ptr: mock_export_fields as *const c_void,
    },
    SymbolEntry {
        name: "bam.v0.mock::NonPodChat.import_field",
        ptr: mock_import_field as *const c_void,
    },
];

impl ModComponent for MockNonPodMsg {
    const CANONICAL_NAME: &'static str = "mock::NonPodChat";
    const COMPONENT_CSTR: &'static CStr = c"mock::NonPodChat";
    const UUID: ComponentUuid = ComponentUuid::from_parts("mock::NonPodChat", 48, 8, 1);
    type VTable = ();

    fn vtable() -> &'static Self::VTable {
        &()
    }

    fn symbols() -> &'static [SymbolEntry] {
        &MOCK_SYMS
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct MockPodWithoutMarker;

impl ModComponent for MockPodWithoutMarker {
    const CANONICAL_NAME: &'static str = "mock::PodWithoutMarker";
    const COMPONENT_CSTR: &'static CStr = c"mock::PodWithoutMarker";
    const UUID: ComponentUuid = ComponentUuid::from_parts("mock::PodWithoutMarker", 8, 8, 2);
    type VTable = ();

    fn vtable() -> &'static Self::VTable {
        &()
    }

    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

struct MockMsgWorld {
    stored: Vec<MockNonPodMsg>,
    sent_packets: Vec<Vec<u8>>,
}

unsafe extern "C" fn mock_get_proc(_ctx: *const BamHostCtx, sym: *const c_char) -> *const c_void {
    // SAFETY: Caller passes valid NUL-terminated CStr.
    let name = unsafe { CStr::from_ptr(sym) };
    match name.to_bytes() {
        b"bam.v0.mock::NonPodChat.export_fields" => mock_export_fields as *const c_void,
        b"bam.v0.mock::NonPodChat.non_pod_message" => mock_msg_read_batch as *const c_void,
        _ => core::ptr::null(),
    }
}

unsafe extern "C" fn mock_col_info(
    _iter: *const BamIter,
    term_index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult {
    if term_index > 1 {
        return FfiResult::OUT_OF_RANGE;
    }
    let uuid = if term_index == 0 {
        MockNonPodMsg::UUID
    } else {
        MockPodWithoutMarker::UUID
    };
    // SAFETY: `out_info` is writable.
    unsafe {
        *out_info = BamColumnInfo {
            component: BamComponent::from_raw((term_index + 1) as u32),
            uuid,
            size: core::mem::size_of::<*const c_void>(),
            align: core::mem::align_of::<*const c_void>(),
            mode: AccessMode::MESSAGE_WRITE,
            storage_flags: BAM_STORAGE_FLAG_MESSAGE,
            reserved0: 0,
        };
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_msg_read_batch(
    iter: *mut BamIter,
    _term_index: usize,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: `iter` points to `MockMsgWorld`.
    let world = unsafe { &*iter.cast::<MockMsgWorld>() };
    let take = world.stored.len().min(max_count);
    for (i, stored_msg) in world.stored.iter().enumerate().take(take) {
        let msg_ptr = core::ptr::from_ref(stored_msg).cast::<c_void>();
        // SAFETY: `out_buf` is writable for `take * size_of::<*const c_void>()` bytes;
        // `write_unaligned` preserves full pointer provenance.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            core::ptr::write_unaligned(
                out_buf
                    .add(i * core::mem::size_of::<*const c_void>())
                    .cast::<*const c_void>(),
                msg_ptr,
            );
        }
    }
    // SAFETY: `out_count` is writable.
    unsafe {
        *out_count = take;
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_msg_send(
    iter: *mut BamIter,
    _term_index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: `iter` points to `MockMsgWorld`, `data_ptr` is readable for `data_len`.
    let world = unsafe { &mut *iter.cast::<MockMsgWorld>() };
    // SAFETY: `data_ptr` covers `data_len` bytes.
    let pkt = unsafe { core::slice::from_raw_parts(data_ptr, data_len) }.to_vec();
    world.sent_packets.push(pkt);
    FfiResult::OK
}

#[test]
fn validated_iter_non_pod_message_read_and_send_encode_witness_packets() {
    let mut api = make_mock_api();
    api.get_proc_address = Some(mock_get_proc);
    api.iter_column_info = Some(mock_col_info);
    api.msg_read_batch = Some(mock_msg_read_batch);
    api.msg_send = Some(mock_msg_send);

    let mut world = MockMsgWorld {
        stored: vec![MockNonPodMsg {
            label: "hello".to_owned(),
            nums: vec![10, 20, 30],
        }],
        sent_packets: Vec::new(),
    };
    let host_ctx = (&raw mut world).cast::<BamHostCtx>();
    // SAFETY: `api` and `host_ctx` live for the test duration.
    let host = unsafe { Host::from_raw(&api, host_ctx) }.unwrap();
    // SAFETY: `raw_iter` points to `MockMsgWorld` used by mock callbacks.
    let mut iter = unsafe { ValidatedIter::from_raw((&raw mut world).cast::<BamIter>(), &host) };

    let got = iter
        .read_non_pod_messages_checked::<MockNonPodMsg>(
            0,
            Some(UuidFilter::exact(MockNonPodMsg::UUID)),
            8,
        )
        .unwrap();
    assert_eq!(got, world.stored);

    let outgoing = MockNonPodMsg {
        label: "reply".to_owned(),
        nums: vec![7, 8],
    };
    assert_eq!(
        iter.send_non_pod_message_checked::<MockNonPodMsg>(
            0,
            Some(UuidFilter::exact(MockNonPodMsg::UUID)),
            &outgoing,
        ),
        FfiResult::OK
    );
    assert_eq!(world.sent_packets.len(), 1);
    assert_eq!(
        u32::from_le_bytes(world.sent_packets[0][0..4].try_into().unwrap()),
        2
    );

    // 8-byte POD message without `non_pod_message` marker must be rejected with TYPE_MISMATCH.
    assert_eq!(
        iter.read_non_pod_messages_checked::<MockPodWithoutMarker>(
            1,
            Some(UuidFilter::exact(MockPodWithoutMarker::UUID)),
            4,
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        iter.send_non_pod_message_checked::<MockPodWithoutMarker>(
            1,
            Some(UuidFilter::exact(MockPodWithoutMarker::UUID)),
            &MockPodWithoutMarker,
        ),
        FfiResult::TYPE_MISMATCH
    );
}
