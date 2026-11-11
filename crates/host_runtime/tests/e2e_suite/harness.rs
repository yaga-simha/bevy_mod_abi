#![allow(dead_code)]

use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ExposableComponent, FfiEntity, FfiResult, HostApi,
    RawChunkIter, SystemDescriptor, ABI_VERSION_PACKED,
};
use core::ffi::{c_char, c_void, CStr};
use mod_derive::ModExposable;

// ============================================================================
// Standard Exposable Test Components
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct KinematicsPos {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct KinematicsVel {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct CombatHealth {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct CombatDamage {
    pub amount: i32,
}

#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct MetadataTag {
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct BufferScores {
    pub scores: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AlignByte {
    pub val: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AlignWord {
    pub val: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AlignDWord {
    pub val: u64,
}

#[repr(align(16))]
#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AlignSimd {
    pub val: [f32; 4],
}

#[repr(align(64))]
#[derive(Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AlignCacheLine {
    pub val: [u64; 8],
}

// ============================================================================
// Symbol Resolver Helper
// ============================================================================

pub unsafe extern "C" fn test_symbol_resolver(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
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
// Borrow Conflict Checker
// ============================================================================

/// Detects ECS component borrow conflicts between two system query descriptor sets.
/// Conflict occurs if both access the same component and at least one is WRITE.
pub fn check_borrow_conflict(sys_a: &[ComponentAccessDesc], sys_b: &[ComponentAccessDesc]) -> bool {
    for a in sys_a {
        if a.name.is_null() || a.mode == ComponentAccessMode::FILTER_ONLY {
            continue;
        }
        let a_name = unsafe { CStr::from_ptr(a.name) }.to_bytes();

        for b in sys_b {
            if b.name.is_null() || b.mode == ComponentAccessMode::FILTER_ONLY {
                continue;
            }
            let b_name = unsafe { CStr::from_ptr(b.name) }.to_bytes();

            if a_name == b_name {
                if a.mode == ComponentAccessMode::WRITE || b.mode == ComponentAccessMode::WRITE {
                    return true;
                }
            }
        }
    }
    false
}

// ============================================================================
// Mock Chunk Iteration & Table Testbed Fixture
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

// Global host API implementation for mock testing
pub unsafe extern "C" fn mock_iter_next_chunk(iter: *mut RawChunkIter) -> usize {
    if iter.is_null() {
        return 0;
    }
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.exhausted || state.current_table_idx >= state.tables.len() {
        state.exhausted = true;
        return 0;
    }
    let count = state.tables[state.current_table_idx].entity_count;
    state.current_table_idx += 1;
    count
}

pub unsafe extern "C" fn mock_iter_column_readonly(
    iter: *const RawChunkIter,
    index: usize,
) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.current_table_idx == 0 || state.current_table_idx > state.tables.len() {
        return core::ptr::null();
    }
    let table = &state.tables[state.current_table_idx - 1];
    if index >= table.columns.len() {
        return core::ptr::null();
    }
    table.columns[index].bytes.as_ptr()
}

pub unsafe extern "C" fn mock_iter_column_mutable(
    iter: *mut RawChunkIter,
    index: usize,
) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.current_table_idx == 0 || state.current_table_idx > state.tables.len() {
        return core::ptr::null_mut();
    }
    let table = &mut state.tables[state.current_table_idx - 1];
    if index >= table.columns.len() || !table.columns[index].is_mutable {
        return core::ptr::null_mut();
    }
    // Mark all changed
    for flag in &mut table.columns[index].changed_flags {
        *flag = true;
    }
    table.columns[index].bytes.as_mut_ptr()
}

pub unsafe extern "C" fn mock_iter_mark_changed(
    iter: *mut RawChunkIter,
    index: usize,
    row: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &mut *(iter as *mut MockChunkIterState) };
    if state.current_table_idx == 0 || state.current_table_idx > state.tables.len() {
        return FfiResult::INTERNAL_ERROR;
    }
    let table = &mut state.tables[state.current_table_idx - 1];
    if index >= table.columns.len() {
        return FfiResult::INVALID_COMPONENT;
    }
    if !table.columns[index].is_mutable {
        return FfiResult::PERMISSION_DENIED;
    }
    if row >= table.entity_count {
        return FfiResult::INTERNAL_ERROR;
    }
    table.columns[index].changed_flags[row] = true;
    FfiResult::OK
}

pub unsafe extern "C" fn mock_iter_entities(iter: *const RawChunkIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.current_table_idx == 0 || state.current_table_idx > state.tables.len() {
        return core::ptr::null();
    }
    let table = &state.tables[state.current_table_idx - 1];
    table.entities.as_ptr()
}

pub unsafe extern "C" fn mock_iter_column_stride(iter: *const RawChunkIter, index: usize) -> usize {
    if iter.is_null() {
        return 0;
    }
    let state = unsafe { &*(iter as *const MockChunkIterState) };
    if state.current_table_idx == 0 || state.current_table_idx > state.tables.len() {
        return 0;
    }
    let table = &state.tables[state.current_table_idx - 1];
    if index >= table.columns.len() {
        return 0;
    }
    table.columns[index].stride
}

pub static MOCK_HOST_API: HostApi = HostApi {
    struct_size: core::mem::size_of::<HostApi>() as u32,
    abi_version: ABI_VERSION_PACKED,
    register_system: mock_register_system,
    iter_next_chunk: mock_iter_next_chunk,
    iter_column_readonly: mock_iter_column_readonly,
    iter_column_mutable: mock_iter_column_mutable,
    iter_mark_changed: mock_iter_mark_changed,
    iter_entities: mock_iter_entities,
    iter_column_stride: mock_iter_column_stride,
    register_component: mock_register_component,
};

pub unsafe extern "C" fn mock_register_component(
    _host_context: *mut c_void,
    descriptor: *const abi_contract::ModComponentDescriptor,
) -> FfiResult {
    if descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let desc = unsafe { &*descriptor };
    if desc.struct_size < core::mem::size_of::<abi_contract::ModComponentDescriptor>() as u32 {
        return FfiResult::INVALID_VERSION;
    }
    FfiResult::OK
}

pub unsafe extern "C" fn mock_register_system(
    _host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult {
    if descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let desc = unsafe { &*descriptor };
    if desc.struct_size < core::mem::size_of::<SystemDescriptor>() as u32 {
        return FfiResult::INVALID_VERSION;
    }
    FfiResult::OK
}

// ============================================================================
// Mock Command Queue Fixture
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum MockCommand {
    Spawn(FfiEntity),
    Despawn(FfiEntity),
    InsertComponent(FfiEntity, String, Vec<u8>),
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
