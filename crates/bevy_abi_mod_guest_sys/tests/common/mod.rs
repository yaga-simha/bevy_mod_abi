//! Shared mock `BamHostApi` stubs for `bevy_abi_mod_guest_sys` integration and Miri tests.

#![allow(dead_code, unreachable_pub, clippy::undocumented_unsafe_blocks)]

use core::ffi::{CStr, c_char, c_void};
use core::mem::MaybeUninit;

use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, BamColumnInfo, BamCommands, BamComponent, BamHostApi, BamHostCtx, BamIter,
    BamLayoutInfo, BamLoadCtx, BamSystemCtx, BamSystemDesc, ComponentDesc, ComponentUuid, FfiBool,
    FfiEntity, FfiResult, RawInsertable, VTableHeader, pack_abi_version,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaddedPod {
    pub tag: u8,
    // 3 bytes of natural C alignment padding (uninitialized on stack if constructed by literal)
    pub val: u32,
}
// SAFETY: `#[repr(C)]` struct of fixed-width `AbiPod` primitives with no drop glue.
unsafe impl RawInsertable for PaddedPod {}
// SAFETY: Deterministic C layout and all fields are `AbiPod`.
unsafe impl AbiPod for PaddedPod {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DummyVTable {
    pub header: VTableHeader,
    pub getter: Option<unsafe extern "C" fn(*const c_void) -> u32>,
}

pub unsafe extern "C" fn dummy_getter(_ptr: *const c_void) -> u32 {
    42
}

pub static VALID_VTABLE_UUID: ComponentUuid = ComponentUuid::from_parts("mock::Dummy", 8, 4, 0);

pub static VALID_VTABLE: DummyVTable = DummyVTable {
    header: VTableHeader::from_raw_parts(
        core::mem::size_of::<DummyVTable>() as u32,
        VALID_VTABLE_UUID,
        8,
        4,
    ),
    getter: Some(dummy_getter),
};

pub static TRUNCATED_HEADER: VTableHeader = VTableHeader {
    struct_size: VTableHeader::MIN_V0_2_SIZE as u32,
    abi_version: ABI_VERSION,
    uuid: VALID_VTABLE_UUID,
    size: 8,
    align: 4,
};

pub static WRONG_VERSION_VTABLE: DummyVTable = DummyVTable {
    header: VTableHeader {
        struct_size: core::mem::size_of::<DummyVTable>() as u32,
        abi_version: pack_abi_version(0, 1, 0),
        uuid: VALID_VTABLE_UUID,
        size: 8,
        align: 4,
    },
    getter: Some(dummy_getter),
};

pub unsafe extern "C" fn custom_math_fn(x: u32) -> u32 {
    x + 10
}

pub struct MockWorldCtx {
    pub error_msg: &'static [u8],
    pub spawned: usize,
    pub last_inserted_val: u32,
}

pub unsafe extern "C" fn stub_get_proc_address(
    _ctx: *const BamHostCtx,
    name: *const c_char,
) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    match bytes {
        b"bam.v0.mock::Dummy.vtable" => (&raw const VALID_VTABLE).cast::<c_void>(),
        b"bam.v0.mock::Truncated.vtable" => (&raw const TRUNCATED_HEADER).cast::<c_void>(),
        b"bam.v0.mock::WrongVer.vtable" => (&raw const WRONG_VERSION_VTABLE).cast::<c_void>(),
        b"bam.v0.math.add10" => custom_math_fn as *const c_void,
        _ => core::ptr::null(),
    }
}

pub unsafe extern "C" fn stub_last_error(
    ctx: *const BamHostCtx,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if ctx.is_null() {
        return 0;
    }
    let world = unsafe { &*ctx.cast::<MockWorldCtx>() };
    let msg = world.error_msg;
    if !buf.is_null() && cap > 0 {
        let copy_len = msg.len().min(cap - 1);
        unsafe {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), buf.cast::<u8>(), copy_len);
            *buf.add(copy_len) = 0;
        }
    }
    msg.len()
}

pub unsafe extern "C" fn stub_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    load_ctx.cast::<BamHostCtx>()
}

pub unsafe extern "C" fn stub_register_component(
    _load_ctx: *mut BamLoadCtx,
    desc: *const ComponentDesc,
    out: *mut BamComponent,
) -> FfiResult {
    if desc.is_null() || out.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe { *out = BamComponent::from_raw(1) };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_resolve_component(
    _ctx: *const BamHostCtx,
    _name: *const c_char,
    _uuid: *const ComponentUuid,
    out: *mut BamComponent,
) -> FfiResult {
    unsafe { *out = BamComponent::from_raw(2) };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_register_system(
    _load_ctx: *mut BamLoadCtx,
    desc: *const BamSystemDesc,
) -> FfiResult {
    if desc.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe { (*desc).validate() }
}

pub unsafe extern "C" fn stub_sys_host_ctx(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    sys_ctx.cast::<BamHostCtx>()
}

pub unsafe extern "C" fn stub_sys_query(
    _sys_ctx: *mut BamSystemCtx,
    _query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult {
    unsafe { *out_iter = core::ptr::null_mut() };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_sys_commands(
    sys_ctx: *mut BamSystemCtx,
    out_commands: *mut *mut BamCommands,
) -> FfiResult {
    unsafe { *out_commands = sys_ctx.cast::<BamCommands>() };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_iter_next_chunk(
    _iter: *mut BamIter,
    _out_len: *mut usize,
) -> FfiBool {
    FfiBool::FALSE
}

pub unsafe extern "C" fn stub_iter_reset(_iter: *mut BamIter) -> FfiResult {
    FfiResult::OK
}

pub unsafe extern "C" fn stub_iter_entities(_iter: *const BamIter) -> *const FfiEntity {
    core::ptr::null()
}

pub unsafe extern "C" fn stub_iter_col_ro(_iter: *const BamIter, _idx: usize) -> *const u8 {
    core::ptr::null()
}

pub unsafe extern "C" fn stub_iter_col_mut(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
    core::ptr::null_mut()
}

pub unsafe extern "C" fn stub_iter_mark_changed(
    _iter: *mut BamIter,
    _idx: usize,
    _start: usize,
    _count: usize,
) -> FfiResult {
    FfiResult::OK
}

pub unsafe extern "C" fn stub_iter_rows_ro(_iter: *mut BamIter, _idx: usize) -> *const *const u8 {
    core::ptr::null()
}

pub unsafe extern "C" fn stub_iter_rows_mut(_iter: *mut BamIter, _idx: usize) -> *const *mut u8 {
    core::ptr::null()
}

pub unsafe extern "C" fn stub_iter_col_info(
    _iter: *const BamIter,
    _idx: usize,
    _out: *mut BamColumnInfo,
) -> FfiResult {
    FfiResult::OUT_OF_RANGE
}

pub unsafe extern "C" fn stub_res_ptr(_iter: *const BamIter, _idx: usize) -> *const u8 {
    core::ptr::null()
}

pub unsafe extern "C" fn stub_res_mut_ptr(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
    core::ptr::null_mut()
}

pub unsafe extern "C" fn stub_cmd_spawn_empty(
    cmds: *mut BamCommands,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockWorldCtx>() };
    world.spawned += 1;
    unsafe { *out_entity = FfiEntity::from_bits(world.spawned as u64) };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_cmd_spawn(
    cmds: *mut BamCommands,
    _comps: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockWorldCtx>() };
    if count > 0 && !data_ptrs.is_null() {
        let mut slot = MaybeUninit::<PaddedPod>::uninit();
        unsafe {
            let first_ptr = *data_ptrs;
            core::ptr::copy_nonoverlapping(
                first_ptr,
                slot.as_mut_ptr().cast::<u8>(),
                core::mem::size_of::<PaddedPod>(),
            );
            world.last_inserted_val = slot.assume_init().val;
        }
    }
    world.spawned += 1;
    unsafe { *out_entity = FfiEntity::from_bits(world.spawned as u64) };
    FfiResult::OK
}

pub unsafe extern "C" fn stub_cmd_despawn(_cmds: *mut BamCommands, _e: FfiEntity) -> FfiResult {
    FfiResult::OK
}

pub unsafe extern "C" fn stub_cmd_insert(
    cmds: *mut BamCommands,
    _e: FfiEntity,
    _comp: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len == core::mem::size_of::<PaddedPod>() {
        let mut slot = MaybeUninit::<PaddedPod>::uninit();
        unsafe {
            core::ptr::copy_nonoverlapping(data_ptr, slot.as_mut_ptr().cast::<u8>(), data_len);
            (*cmds.cast::<MockWorldCtx>()).last_inserted_val = slot.assume_init().val;
        }
    }
    FfiResult::OK
}

pub unsafe extern "C" fn stub_cmd_insert_default(
    _cmds: *mut BamCommands,
    _e: FfiEntity,
    _c: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

pub unsafe extern "C" fn stub_cmd_remove(
    _cmds: *mut BamCommands,
    _e: FfiEntity,
    _c: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

pub unsafe extern "C" fn stub_component_layout_info_padded(
    _ctx: *const BamHostCtx,
    comp: BamComponent,
    out_info: *mut BamLayoutInfo,
) -> FfiResult {
    if out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if comp != BamComponent::from_raw(1) {
        return FfiResult::INVALID_COMPONENT;
    }
    unsafe {
        *out_info = BamLayoutInfo {
            size: core::mem::size_of::<PaddedPod>(),
            align: core::mem::align_of::<PaddedPod>(),
            uuid: ComponentUuid::from_parts(
                "mock::PaddedPod",
                core::mem::size_of::<PaddedPod>(),
                core::mem::align_of::<PaddedPod>(),
                0,
            ),
            ..BamLayoutInfo::empty()
        };
    }
    FfiResult::OK
}

/// Builds a baseline valid ABI 0.3 `BamHostApi` with all 27 core slots populated and extended
/// slots left `None` for per-test customization.
pub fn make_mock_api() -> BamHostApi {
    // SAFETY: `BamHostApi` is `#[repr(C)]` consisting solely of `u32` fields and `Option<extern "C" fn>`.
    let mut api: BamHostApi = unsafe { MaybeUninit::zeroed().assume_init() };
    api.struct_size = core::mem::size_of::<BamHostApi>() as u32;
    api.abi_version = ABI_VERSION;
    api.get_proc_address = Some(stub_get_proc_address);
    api.last_error = Some(stub_last_error);
    api.load_host_ctx = Some(stub_load_host_ctx);
    api.register_component = Some(stub_register_component);
    api.resolve_component = Some(stub_resolve_component);
    api.register_system = Some(stub_register_system);
    api.sys_host_ctx = Some(stub_sys_host_ctx);
    api.sys_query = Some(stub_sys_query);
    api.sys_commands = Some(stub_sys_commands);
    api.iter_next_chunk = Some(stub_iter_next_chunk);
    api.iter_reset = Some(stub_iter_reset);
    api.iter_entities = Some(stub_iter_entities);
    api.iter_column_readonly = Some(stub_iter_col_ro);
    api.iter_column_mutable = Some(stub_iter_col_mut);
    api.iter_column_mutable_untracked = Some(stub_iter_col_mut);
    api.iter_mark_changed_range = Some(stub_iter_mark_changed);
    api.iter_column_rows_readonly = Some(stub_iter_rows_ro);
    api.iter_column_rows_mutable = Some(stub_iter_rows_mut);
    api.iter_column_info = Some(stub_iter_col_info);
    api.res_ptr = Some(stub_res_ptr);
    api.res_mut_ptr = Some(stub_res_mut_ptr);
    api.cmd_spawn_empty = Some(stub_cmd_spawn_empty);
    api.cmd_spawn = Some(stub_cmd_spawn);
    api.cmd_despawn = Some(stub_cmd_despawn);
    api.cmd_insert = Some(stub_cmd_insert);
    api.cmd_insert_default = Some(stub_cmd_insert_default);
    api.cmd_remove = Some(stub_cmd_remove);
    api
}
