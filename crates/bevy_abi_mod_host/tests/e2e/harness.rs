#![allow(dead_code)]

use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    ABI_VERSION, AccessMode, BamColumnInfo, BamCommands, BamComponent, BamHostApi, BamHostCtx,
    BamIter, BamLoadCtx, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid,
    FfiBool, FfiEntity, FfiResult, ModComponent as _,
};
use core::ffi::{CStr, c_char, c_void};

// ============================================================================
// Standard Exposable Test Components (ABI 0.2)
// ============================================================================

pub const POS_HANDLE: BamComponent = BamComponent::from_raw(1);
pub const VEL_HANDLE: BamComponent = BamComponent::from_raw(2);
pub const HEALTH_HANDLE: BamComponent = BamComponent::from_raw(3);
pub const DAMAGE_HANDLE: BamComponent = BamComponent::from_raw(4);

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::KinematicsPos")]
pub struct KinematicsPos {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::KinematicsVel")]
pub struct KinematicsVel {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::CombatHealth")]
pub struct CombatHealth {
    pub current: i32,
    pub max: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::CombatDamage")]
pub struct CombatDamage {
    pub amount: i32,
}

#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::MetadataTag")]
pub struct MetadataTag {
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::BufferScores")]
pub struct BufferScores {
    pub scores: Vec<u32>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::AlignByte")]
pub struct AlignByte {
    pub val: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::AlignWord")]
pub struct AlignWord {
    pub val: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::AlignDWord")]
pub struct AlignDWord {
    pub val: u64,
}

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::AlignSimd")]
pub struct AlignSimd {
    pub val: [f32; 4],
}

#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "e2e_harness::AlignCacheLine")]
pub struct AlignCacheLine {
    pub val: [u64; 8],
}

// ============================================================================
// Symbol Resolver Helper (`BamGetProcAddressFn`)
// ============================================================================

/// Resolves generated component VTable symbols in unit/tier tests.
///
/// # Safety
///
/// `name` must either be null or point to a valid NUL-terminated C string.
pub unsafe extern "C" fn test_symbol_resolver(
    _ctx: *const BamHostCtx,
    name: *const c_char,
) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller guarantees `name` is NUL-terminated when non-null.
    let Ok(sym) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return core::ptr::null();
    };

    if let Some(ptr) = KinematicsPos::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = KinematicsVel::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = CombatHealth::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = CombatDamage::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = MetadataTag::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = BufferScores::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = AlignByte::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = AlignWord::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = AlignDWord::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = AlignSimd::resolve_symbol(sym) {
        return ptr;
    }
    if let Some(ptr) = AlignCacheLine::resolve_symbol(sym) {
        return ptr;
    }

    core::ptr::null()
}

// ============================================================================
// Borrow Conflict Checker (`BamTermDesc`)
// ============================================================================

/// Detects ECS component borrow conflicts between two system query term slices.
/// Conflict occurs if both access the same valid component handle as data/resource
/// (neither is `FILTER_ONLY` / `WITHOUT`) and at least one access is mutable.
pub fn check_borrow_conflict(sys_a: &[BamTermDesc], sys_b: &[BamTermDesc]) -> bool {
    for a in sys_a {
        if !a.component.is_valid() || a.mode.is_filter_only() {
            continue;
        }
        for b in sys_b {
            if !b.component.is_valid() || b.mode.is_filter_only() {
                continue;
            }
            if a.component == b.component && (a.mode.is_mutable() || b.mode.is_mutable()) {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// Mock Chunk Iteration & Table Testbed Fixture (`BamIter` / `BamHostApi`)
// ============================================================================

pub struct MockColumnData {
    pub name: String,
    pub stride: usize,
    pub is_mutable: bool,
    pub bytes: Vec<u8>,
    pub changed_flags: Vec<bool>,
}

pub struct MockTable {
    pub entity_count: usize,
    pub entities: Vec<FfiEntity>,
    pub columns: Vec<MockColumnData>,
}

pub struct MockChunkIterState {
    pub tables: Vec<MockTable>,
    pub current_table_idx: usize,
    pub exhausted: bool,
}

impl MockChunkIterState {
    pub fn new() -> Self {
        Self {
            tables: Vec::new(),
            current_table_idx: 0,
            exhausted: false,
        }
    }

    pub fn add_table(&mut self, table: MockTable) {
        self.tables.push(table);
    }
}

impl Default for MockChunkIterState {
    fn default() -> Self {
        Self::new()
    }
}

/// Advances a mock chunk iterator to the next non-empty chunk.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`, and `out_len`
/// must either be null or writable for one `usize`.
pub unsafe extern "C" fn mock_iter_next_chunk(iter: *mut BamIter, out_len: *mut usize) -> FfiBool {
    if iter.is_null() || out_len.is_null() {
        return FfiBool::FALSE;
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.exhausted {
        // SAFETY: `out_len` is non-null and writable.
        unsafe {
            *out_len = 0;
        }
        return FfiBool::FALSE;
    }
    while state.current_table_idx < state.tables.len() {
        let idx = state.current_table_idx;
        state.current_table_idx += 1;
        let count = state.tables[idx].entity_count;
        if count > 0 {
            // SAFETY: `out_len` is non-null and writable.
            unsafe {
                *out_len = count;
            }
            return FfiBool::TRUE;
        }
    }
    state.exhausted = true;
    // SAFETY: `out_len` is non-null and writable.
    unsafe {
        *out_len = 0;
    }
    FfiBool::FALSE
}

/// Convenience helper advancing `iter` and returning chunk entity count (`0` when exhausted).
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe fn mock_step_chunk(iter: *mut BamIter) -> usize {
    let mut len = 0usize;
    // SAFETY: `len` is a valid stack `usize`, and `iter` satisfies `mock_iter_next_chunk`.
    let has_more = unsafe { mock_iter_next_chunk(iter, &mut len) };
    if has_more.get() { len } else { 0 }
}

/// Resets a mock chunk iterator back to table index 0.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_reset(iter: *mut BamIter) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    state.current_table_idx = 0;
    state.exhausted = false;
    FfiResult::OK
}

/// Returns a read-only column pointer for the active mock chunk.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_column_readonly(
    iter: *const BamIter,
    index: usize,
) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return core::ptr::null();
    }
    let table = &state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 || index >= table.columns.len() {
        return core::ptr::null();
    }
    table.columns[index].bytes.as_ptr()
}

/// Returns a mutable column pointer for the active mock chunk, marking all rows changed.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_column_mutable(iter: *mut BamIter, index: usize) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return core::ptr::null_mut();
    }
    let table = &mut state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 || index >= table.columns.len() || !table.columns[index].is_mutable {
        return core::ptr::null_mut();
    }
    table.columns[index].changed_flags.fill(true);
    table.columns[index].bytes.as_mut_ptr()
}

/// Returns a mutable column pointer for the active mock chunk without marking rows changed.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_column_mutable_untracked(
    iter: *mut BamIter,
    index: usize,
) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return core::ptr::null_mut();
    }
    let table = &mut state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 || index >= table.columns.len() || !table.columns[index].is_mutable {
        return core::ptr::null_mut();
    }
    table.columns[index].bytes.as_mut_ptr()
}

/// Marks a range of rows as changed in the active mock chunk.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_mark_changed_range(
    iter: *mut BamIter,
    index: usize,
    start_row: usize,
    row_count: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return FfiResult::INVALID_STATE;
    }
    let table = &mut state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 {
        return FfiResult::INVALID_STATE;
    }
    if index >= table.columns.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    if !table.columns[index].is_mutable {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(end_row) = start_row.checked_add(row_count) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if end_row > table.entity_count {
        return FfiResult::OUT_OF_RANGE;
    }
    table.columns[index].changed_flags[start_row..end_row].fill(true);
    FfiResult::OK
}

/// Convenience single-row wrapper around `mock_iter_mark_changed_range`.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe fn mock_iter_mark_changed(iter: *mut BamIter, index: usize, row: usize) -> FfiResult {
    // SAFETY: Caller upholds the `mock_iter_mark_changed_range` contract.
    unsafe { mock_iter_mark_changed_range(iter, index, row, 1) }
}

/// Returns the entity ID slice pointer for the active mock chunk.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe extern "C" fn mock_iter_entities(iter: *const BamIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return core::ptr::null();
    }
    let table = &state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 {
        return core::ptr::null();
    }
    table.entities.as_ptr()
}

/// Populates `BamColumnInfo` metadata for `index` in the active mock chunk.
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`, and `out_info`
/// must either be null or writable for one `BamColumnInfo`.
pub unsafe extern "C" fn mock_iter_column_info(
    iter: *const BamIter,
    index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult {
    if iter.is_null() || out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller guarantees `iter` points to a valid `MockChunkIterState`.
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.exhausted
        || state.current_table_idx == 0
        || state.current_table_idx > state.tables.len()
    {
        return FfiResult::INVALID_STATE;
    }
    let table = &state.tables[state.current_table_idx - 1];
    if table.entity_count == 0 {
        return FfiResult::INVALID_STATE;
    }
    if index >= table.columns.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let col = &table.columns[index];
    let info = BamColumnInfo {
        component: BamComponent::from_raw((index as u32) + 1),
        mode: if col.is_mutable {
            AccessMode::WRITE
        } else {
            AccessMode::READ
        },
        uuid: ComponentUuid::from_canonical_name(&col.name),
        size: col.stride,
        align: col.stride.next_power_of_two().clamp(1, 64),
        storage_flags: 0,
        reserved0: 0,
    };
    // SAFETY: `out_info` is non-null and writable.
    unsafe {
        *out_info = info;
    }
    FfiResult::OK
}

/// Convenience helper returning `BamColumnInfo::size` for `index` (`0` when invalid/empty).
///
/// # Safety
///
/// `iter` must either be null or point to a valid `MockChunkIterState`.
pub unsafe fn mock_iter_column_stride(iter: *const BamIter, index: usize) -> usize {
    let mut info = BamColumnInfo {
        component: BamComponent::INVALID,
        mode: AccessMode::READ,
        uuid: ComponentUuid::ZERO,
        size: 0,
        align: 1,
        storage_flags: 0,
        reserved0: 0,
    };
    // SAFETY: `info` is a valid stack `BamColumnInfo`, and `iter` satisfies `mock_iter_column_info`.
    let status = unsafe { mock_iter_column_info(iter, index, &mut info) };
    if status.is_ok() { info.size } else { 0 }
}

unsafe extern "C" fn mock_last_error(
    _ctx: *const BamHostCtx,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if !buf.is_null() && cap > 0 {
        unsafe {
            *buf = 0;
        }
    }
    0
}

unsafe extern "C" fn mock_load_host_ctx(_load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    core::ptr::NonNull::<BamHostCtx>::dangling().as_ptr()
}

/// Validates a `ComponentDesc` and returns a dummy `BamComponent` handle.
///
/// # Safety
///
/// `descriptor` must either be null or point to a valid `ComponentDesc` header, and
/// `out_component` must either be null or writable for one `BamComponent`.
pub unsafe extern "C" fn mock_register_component(
    _load_ctx: *mut BamLoadCtx,
    descriptor: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if descriptor.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller guarantees `descriptor` points to a readable `ComponentDesc`.
    let desc = unsafe { &*descriptor };
    if (desc.struct_size as usize) < ComponentDesc::MIN_V0_2_SIZE {
        return FfiResult::INVALID_VERSION;
    }
    let v = desc.validate();
    if v.is_err() {
        return v;
    }
    // SAFETY: `out_component` is non-null and writable.
    unsafe {
        *out_component = BamComponent::from_raw(1);
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_resolve_component(
    _ctx: *const BamHostCtx,
    name: *const c_char,
    _uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if name.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `out_component` is non-null and writable.
    unsafe {
        *out_component = BamComponent::from_raw(1);
    }
    FfiResult::OK
}

/// Validates a `BamSystemDesc` registration call in unit/tier tests.
///
/// # Safety
///
/// `descriptor` must either be null or point to a valid `BamSystemDesc` header.
pub unsafe extern "C" fn mock_register_system(
    _load_ctx: *mut BamLoadCtx,
    descriptor: *const BamSystemDesc,
) -> FfiResult {
    if descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller guarantees `descriptor` points to a readable `BamSystemDesc`.
    let desc = unsafe { &*descriptor };
    if (desc.struct_size as usize) < BamSystemDesc::MIN_V0_2_SIZE {
        return FfiResult::INVALID_VERSION;
    }
    desc.validate()
}

unsafe extern "C" fn mock_sys_host_ctx(_sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    core::ptr::NonNull::<BamHostCtx>::dangling().as_ptr()
}

unsafe extern "C" fn mock_sys_query(
    _sys_ctx: *mut BamSystemCtx,
    _query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult {
    if out_iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe {
        *out_iter = core::ptr::null_mut();
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_sys_commands(
    _sys_ctx: *mut BamSystemCtx,
    out_cmds: *mut *mut BamCommands,
) -> FfiResult {
    if out_cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe {
        *out_cmds = core::ptr::null_mut();
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_iter_rows_ro(
    _iter: *mut BamIter,
    _term_index: usize,
) -> *const *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn mock_iter_rows_mut(_iter: *mut BamIter, _term_index: usize) -> *const *mut u8 {
    core::ptr::null()
}

unsafe extern "C" fn mock_res_ptr(_iter: *const BamIter, _term_index: usize) -> *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn mock_res_mut_ptr(_iter: *mut BamIter, _term_index: usize) -> *mut u8 {
    core::ptr::null_mut()
}

unsafe extern "C" fn mock_cmd_spawn_empty(
    _cmds: *mut BamCommands,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe {
        *out_entity = FfiEntity::from_bits(1);
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_spawn(
    _cmds: *mut BamCommands,
    _comps: *const BamComponent,
    _data_ptrs: *const *const u8,
    _count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe {
        *out_entity = FfiEntity::from_bits(1);
    }
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_despawn(_cmds: *mut BamCommands, _entity: FfiEntity) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_insert(
    _cmds: *mut BamCommands,
    _entity: FfiEntity,
    _comp: BamComponent,
    _data: *const u8,
    _len: usize,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_insert_default(
    _cmds: *mut BamCommands,
    _entity: FfiEntity,
    _comp: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_remove(
    _cmds: *mut BamCommands,
    _entity: FfiEntity,
    _comp: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

pub static MOCK_HOST_API: BamHostApi = BamHostApi {
    struct_size: core::mem::size_of::<BamHostApi>() as u32,
    abi_version: ABI_VERSION,
    get_proc_address: Some(test_symbol_resolver),
    last_error: Some(mock_last_error),
    load_host_ctx: Some(mock_load_host_ctx),
    register_component: Some(mock_register_component),
    resolve_component: Some(mock_resolve_component),
    register_system: Some(mock_register_system),
    sys_host_ctx: Some(mock_sys_host_ctx),
    sys_query: Some(mock_sys_query),
    sys_commands: Some(mock_sys_commands),
    iter_next_chunk: Some(mock_iter_next_chunk),
    iter_reset: Some(mock_iter_reset),
    iter_entities: Some(mock_iter_entities),
    iter_column_readonly: Some(mock_iter_column_readonly),
    iter_column_mutable: Some(mock_iter_column_mutable),
    iter_column_mutable_untracked: Some(mock_iter_column_mutable_untracked),
    iter_mark_changed_range: Some(mock_iter_mark_changed_range),
    iter_column_rows_readonly: Some(mock_iter_rows_ro),
    iter_column_rows_mutable: Some(mock_iter_rows_mut),
    iter_column_info: Some(mock_iter_column_info),
    res_ptr: Some(mock_res_ptr),
    res_mut_ptr: Some(mock_res_mut_ptr),
    cmd_spawn_empty: Some(mock_cmd_spawn_empty),
    cmd_spawn: Some(mock_cmd_spawn),
    cmd_despawn: Some(mock_cmd_despawn),
    cmd_insert: Some(mock_cmd_insert),
    cmd_insert_default: Some(mock_cmd_insert_default),
    cmd_remove: Some(mock_cmd_remove),
    par_for_each_chunk: None,
    sys_entity_get: None,
    sys_entity_get_mut: None,
    msg_read_batch: None,
    msg_send: None,
};

// ============================================================================
// Mock Command Queue Fixture
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum MockCommand {
    Spawn(FfiEntity),
    Despawn(FfiEntity),
    InsertComponent(FfiEntity, BamComponent, Vec<u8>),
}

pub struct MockCommandQueueState {
    pub commands: Vec<MockCommand>,
}

impl MockCommandQueueState {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: MockCommand) {
        self.commands.push(cmd);
    }

    pub fn flush(&mut self) -> Vec<MockCommand> {
        std::mem::take(&mut self.commands)
    }
}

impl Default for MockCommandQueueState {
    fn default() -> Self {
        Self::new()
    }
}

/// No-op guest system callback used by descriptor tests.
///
/// # Safety
///
/// Safe to invoke with any `_sys_ctx` and `_user_data` pointers (neither is dereferenced).
pub unsafe extern "C" fn dummy_system_callback(
    _sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}
