//! Comprehensive ABI `0.3.0` mock-host unit and integration tests for `bevy_abi_mod_guest` and
//! `bevy_abi_mod_guest_sys` (also executed under Miri Tree Borrows).

#![allow(
    dead_code,
    unsafe_code,
    clippy::undocumented_unsafe_blocks,
    clippy::vec_box
)]

use core::{
    ffi::{CStr, c_char, c_void},
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};
use std::collections::{HashMap, HashSet};

use bevy_abi_mod_guest::{
    build_guest_fn,
    prelude::*,
    raw::{
        BamChunkTaskFn, BamCommands, BamHostApi, BamHostCtx, BamInstance, BamIter, BamLoadCtx,
        BamRunIfFn, BamSystemCtx, BamSystemDesc, BamSystemFn, BamTermDesc, ComponentDesc,
        SymbolEntry, init_guest_instance, shutdown_guest_instance,
    },
};

// ============================================================================
// Mock ABI 0.3.0 Host World
// ============================================================================

#[derive(Clone)]
struct RegisteredComp {
    handle: BamComponent,
    name: String,
    uuid: ComponentUuid,
    schema_fingerprint: u64,
    size: usize,
    align: usize,
    flags: u32,
    fields: Vec<BamFieldDesc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisteredRelation {
    handle: BamComponent,
    name: String,
    uuid: ComponentUuid,
    flags: u32,
    data_size: usize,
    data_align: usize,
    max_degree: u32,
}

#[derive(Clone)]
struct RegisteredSystem {
    handle: BamSystem,
    name: String,
    queries: Vec<Vec<BamTermDesc>>,
    schedule: ScheduleId,
    callback: BamSystemFn,
    user_data: *mut c_void,
    run_if: Option<BamRunIfFn>,
    run_if_user_data: *mut c_void,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisteredScheduleSet {
    handle: BamSystemSet,
    name: String,
    schedule: ScheduleId,
    flags: u32,
    before_sets: Vec<BamSystemSet>,
    after_sets: Vec<BamSystemSet>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedCmd {
    SpawnEmpty(FfiEntity),
    Despawn(FfiEntity),
    Insert {
        entity: FfiEntity,
        component: BamComponent,
        bytes: Vec<u8>,
    },
    Remove {
        entity: FfiEntity,
        component: BamComponent,
    },
    InsertRelation {
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
        payload: Vec<u8>,
    },
    RemoveRelation {
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
    },
}

struct MockChunk {
    entities: Vec<FfiEntity>,
    columns: HashMap<BamComponent, Vec<u64>>,
}

impl MockChunk {
    fn new(entities: Vec<FfiEntity>) -> Self {
        Self {
            entities,
            columns: HashMap::new(),
        }
    }

    fn insert_dense<T: Copy>(&mut self, comp: BamComponent, values: &[T]) {
        assert_eq!(values.len(), self.entities.len());
        let byte_len = core::mem::size_of_val(values);
        let word_len = byte_len.div_ceil(8).max(1);
        let mut words = vec![0u64; word_len];
        if byte_len > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    values.as_ptr().cast::<u8>(),
                    words.as_mut_ptr().cast::<u8>(),
                    byte_len,
                );
            }
        }
        self.columns.insert(comp, words);
    }

    fn read_dense<T: Copy>(&self, comp: BamComponent) -> Vec<T> {
        let count = self.entities.len();
        if core::mem::size_of::<T>() == 0 {
            return (0..count).map(|_| unsafe { core::mem::zeroed() }).collect();
        }
        let words = self.columns.get(&comp).expect("missing dense column");
        let mut out = Vec::<T>::with_capacity(count);
        unsafe {
            core::ptr::copy_nonoverlapping(words.as_ptr().cast::<T>(), out.as_mut_ptr(), count);
            out.set_len(count);
        }
        out
    }
}

struct MockIterState {
    world: *mut MockV03World,
    terms: Vec<BamTermDesc>,
    next_chunk_idx: usize,
    end_chunk_idx: Option<usize>,
    active_chunk_idx: Option<usize>,
}

struct MockSystemFrame {
    world: *mut MockV03World,
    iters: Vec<Box<MockIterState>>,
}

struct MockV03World {
    api: BamHostApi,
    components: Vec<RegisteredComp>,
    name_to_comp: HashMap<String, BamComponent>,
    uuid_to_comp: HashMap<ComponentUuid, BamComponent>,
    relations: HashMap<BamComponent, RegisteredRelation>,
    /// Active relation edges: `(relation, source) -> Vec<target>` (used for cycle detection & `up_cascade`).
    relation_edges: HashMap<(BamComponent, FfiEntity), Vec<FfiEntity>>,
    /// Per-entity component storage for `up_cascade` ancestor lookup.
    entity_components: HashMap<(FfiEntity, BamComponent), Vec<u64>>,
    /// When `true`, missing `up_cascade` returns `OK` with null `out_owner`/`out_ptr` instead of `NOT_FOUND`.
    up_cascade_missing_as_ok_null: bool,
    chunks: Vec<MockChunk>,
    systems: Vec<RegisteredSystem>,
    schedule_sets: Vec<RegisteredScheduleSet>,
    mounted_systems: Vec<(BamSystem, ScheduleId, BamSystemSet)>,
    commands: Vec<RecordedCmd>,
    rollback_calls: usize,
    next_entity_bits: u64,
    logs: Vec<(u32, String)>,
    faults: Vec<String>,
    last_error: String,
    symbols: Vec<String>,
    symbol_to_id: HashMap<String, BamSymbol>,
    named_objects: HashMap<String, FfiEntity>,
}

struct V03TestHost {
    raw: *mut MockV03World,
}

impl Drop for V03TestHost {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { drop(Box::from_raw(self.raw)) };
        }
    }
}

impl core::ops::Deref for V03TestHost {
    type Target = MockV03World;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.raw }
    }
}

impl core::ops::DerefMut for V03TestHost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.raw }
    }
}

impl V03TestHost {
    fn new() -> Self {
        Self::with_api(make_v03_host_api())
    }

    fn with_api(api: BamHostApi) -> Self {
        Self {
            raw: Box::into_raw(Box::new(MockV03World {
                api,
                components: Vec::new(),
                name_to_comp: HashMap::new(),
                uuid_to_comp: HashMap::new(),
                relations: HashMap::new(),
                relation_edges: HashMap::new(),
                entity_components: HashMap::new(),
                up_cascade_missing_as_ok_null: false,
                chunks: Vec::new(),
                systems: Vec::new(),
                schedule_sets: Vec::new(),
                mounted_systems: Vec::new(),
                commands: Vec::new(),
                rollback_calls: 0,
                next_entity_bits: 1000,
                logs: Vec::new(),
                faults: Vec::new(),
                last_error: "initial_host_error".to_owned(),
                symbols: Vec::new(),
                symbol_to_id: HashMap::new(),
                named_objects: HashMap::new(),
            })),
        }
    }

    const fn api_ptr(&self) -> *const BamHostApi {
        unsafe { &raw const (*self.raw).api }
    }

    fn load_fn<F>(&self, mod_name: &CStr, build_fn: F) -> Result<*mut BamInstance, FfiResult>
    where
        F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
    {
        let manifest = BamManifest::new(mod_name.as_ptr(), c"0.3.0".as_ptr());
        let mut instance_out: *mut BamInstance = core::ptr::null_mut();
        let status = unsafe {
            init_guest_instance(
                self.api_ptr(),
                self.raw.cast::<BamLoadCtx>(),
                &mut instance_out,
                &manifest,
                |inst, load| build_guest_fn(inst, load, build_fn),
            )
        };
        if status.is_ok() {
            Ok(instance_out)
        } else {
            Err(status)
        }
    }

    fn unload(&self, instance: *mut BamInstance) {
        unsafe { shutdown_guest_instance(instance) };
    }

    fn run_system(&self, sys_idx: usize) -> FfiResult {
        let (run_if, run_if_data, callback, user_data, queries) = unsafe {
            let world = &*self.raw;
            let sys = &world.systems[sys_idx];
            (
                sys.run_if,
                sys.run_if_user_data,
                sys.callback,
                sys.user_data,
                sys.queries.clone(),
            )
        };
        if let Some(pred) = run_if {
            if !unsafe { pred(run_if_data) }.get() {
                return FfiResult::OK;
            }
        }
        let world_ptr = self.raw;
        let iters = queries
            .into_iter()
            .map(|terms| {
                Box::new(MockIterState {
                    world: world_ptr,
                    terms,
                    next_chunk_idx: 0,
                    end_chunk_idx: None,
                    active_chunk_idx: None,
                })
            })
            .collect();
        let mut frame = MockSystemFrame {
            world: world_ptr,
            iters,
        };
        unsafe { callback((&raw mut frame).cast::<BamSystemCtx>(), user_data) }
    }
}

impl MockV03World {
    fn expose_component<T: 'static>(
        &mut self,
        name: &str,
        uuid: ComponentUuid,
        schema_fingerprint: u64,
        fields: &[BamFieldDesc],
    ) -> BamComponent {
        let handle = BamComponent::from_raw((self.components.len() + 1) as u32);
        self.components.push(RegisteredComp {
            handle,
            name: name.to_owned(),
            uuid,
            schema_fingerprint,
            size: core::mem::size_of::<T>(),
            align: core::mem::align_of::<T>(),
            flags: 0,
            fields: fields.to_vec(),
        });
        self.name_to_comp.insert(name.to_owned(), handle);
        if !uuid.is_zero() {
            self.uuid_to_comp.insert(uuid, handle);
        }
        handle
    }

    fn set_entity_component<T: Copy>(&mut self, entity: FfiEntity, comp: BamComponent, value: T) {
        let byte_len = core::mem::size_of::<T>();
        let word_len = byte_len.div_ceil(8).max(1);
        let mut words = vec![0u64; word_len];
        if byte_len > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (&raw const value).cast::<u8>(),
                    words.as_mut_ptr().cast::<u8>(),
                    byte_len,
                );
            }
        }
        self.entity_components.insert((entity, comp), words);
    }

    fn comp_meta(&self, handle: BamComponent) -> Option<&RegisteredComp> {
        let idx = (handle.to_raw() as usize).checked_sub(1)?;
        self.components.get(idx)
    }

    fn comp_meta_mut(&mut self, handle: BamComponent) -> Option<&mut RegisteredComp> {
        let idx = (handle.to_raw() as usize).checked_sub(1)?;
        self.components.get_mut(idx)
    }

    fn would_create_cycle(
        &self,
        relation: BamComponent,
        source: FfiEntity,
        target: FfiEntity,
    ) -> bool {
        if source == target {
            return true;
        }
        let mut stack = vec![target];
        let mut visited = HashSet::new();
        while let Some(cur) = stack.pop() {
            if cur == source {
                return true;
            }
            if !visited.insert(cur) {
                continue;
            }
            if let Some(nexts) = self.relation_edges.get(&(relation, cur)) {
                stack.extend_from_slice(nexts);
            }
        }
        false
    }
}

// ============================================================================
// Mock ABI 0.2 + 0.3 Host FFI Implementations
// ============================================================================

unsafe extern "C" fn host_get_proc_address(
    _ctx: *const BamHostCtx,
    _name: *const c_char,
) -> *const c_void {
    core::ptr::null()
}

unsafe extern "C" fn host_last_error(
    ctx: *const BamHostCtx,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    let world = unsafe { &*ctx.cast::<MockV03World>() };
    let bytes = world.last_error.as_bytes();
    if !buf.is_null() && cap > 0 {
        let n = bytes.len().min(cap - 1);
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
            *buf.add(n) = 0;
        }
    }
    bytes.len()
}

unsafe extern "C" fn host_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    load_ctx.cast::<BamHostCtx>()
}

unsafe extern "C" fn host_register_component(
    load_ctx: *mut BamLoadCtx,
    desc: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    let desc_ref = unsafe { &*desc };
    let st = desc_ref.validate();
    if !st.is_ok() {
        return st;
    }
    let Ok(name) = unsafe { CStr::from_ptr(desc_ref.name) }.to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    if let Some(&existing) = world.name_to_comp.get(name) {
        unsafe { *out_component = existing };
        return FfiResult::OK;
    }
    let fields = if !desc_ref.fields.is_null() && desc_ref.field_count > 0 {
        unsafe { core::slice::from_raw_parts(desc_ref.fields, desc_ref.field_count) }.to_vec()
    } else {
        Vec::new()
    };
    let handle = BamComponent::from_raw((world.components.len() + 1) as u32);
    world.components.push(RegisteredComp {
        handle,
        name: name.to_owned(),
        uuid: desc_ref.uuid,
        schema_fingerprint: desc_ref.schema_fingerprint,
        size: desc_ref.size,
        align: desc_ref.align,
        flags: desc_ref.flags,
        fields,
    });
    world.name_to_comp.insert(name.to_owned(), handle);
    if !desc_ref.uuid.is_zero() {
        world.uuid_to_comp.insert(desc_ref.uuid, handle);
    }
    unsafe { *out_component = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_resolve_component(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if ctx.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &*ctx.cast::<MockV03World>() };
    let mut found = None;
    if !name.is_null() {
        let Ok(name_str) = unsafe { CStr::from_ptr(name) }.to_str() else {
            return FfiResult::INVALID_UTF8;
        };
        if let Some(&h) = world.name_to_comp.get(name_str) {
            found = Some(h);
        }
    } else if !uuid.is_null() {
        let u = unsafe { *uuid };
        if let Some(&h) = world.uuid_to_comp.get(&u) {
            found = Some(h);
        }
    } else {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Some(handle) = found else {
        return FfiResult::NOT_FOUND;
    };
    if !uuid.is_null() {
        let u = unsafe { *uuid };
        if !u.is_zero() && world.comp_meta(handle).is_some_and(|c| c.uuid != u) {
            return FfiResult::TYPE_MISMATCH;
        }
    }
    unsafe { *out_component = handle };
    FfiResult::OK
}

fn push_system(world: &mut MockV03World, desc: &BamSystemDesc) -> Result<BamSystem, FfiResult> {
    let st = desc.validate();
    if !st.is_ok() {
        return Err(st);
    }
    let Ok(name) = unsafe { CStr::from_ptr(desc.name) }.to_str() else {
        return Err(FfiResult::INVALID_UTF8);
    };
    let mut queries = Vec::new();
    if desc.query_count > 0 && !desc.queries.is_null() {
        let q_slice = unsafe { core::slice::from_raw_parts(desc.queries, desc.query_count) };
        for q in q_slice {
            let q_st = q.validate();
            if !q_st.is_ok() {
                return Err(q_st);
            }
            let terms = if q.term_count > 0 && !q.terms.is_null() {
                unsafe { core::slice::from_raw_parts(q.terms, q.term_count) }.to_vec()
            } else {
                Vec::new()
            };
            queries.push(terms);
        }
    }
    let handle = BamSystem::from_raw(world.systems.len() as u32);
    world.systems.push(RegisteredSystem {
        handle,
        name: name.to_owned(),
        queries,
        schedule: desc.schedule,
        callback: desc.callback.ok_or(FfiResult::NULL_POINTER)?,
        user_data: desc.user_data,
        run_if: desc.run_if,
        run_if_user_data: desc.run_if_user_data,
    });
    Ok(handle)
}

unsafe extern "C" fn host_register_system(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamSystemDesc,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    let desc_ref = unsafe { &*desc };
    match push_system(world, desc_ref) {
        Ok(_) => FfiResult::OK,
        Err(err) => err,
    }
}

unsafe extern "C" fn host_sys_host_ctx(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    let frame = unsafe { &*sys_ctx.cast::<MockSystemFrame>() };
    frame.world.cast::<BamHostCtx>()
}

unsafe extern "C" fn host_sys_query(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult {
    let frame = unsafe { &mut *sys_ctx.cast::<MockSystemFrame>() };
    let Some(iter_box) = frame.iters.get_mut(query_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    unsafe { *out_iter = (&raw mut **iter_box).cast::<BamIter>() };
    FfiResult::OK
}

unsafe extern "C" fn host_sys_commands(
    sys_ctx: *mut BamSystemCtx,
    out_commands: *mut *mut BamCommands,
) -> FfiResult {
    let frame = unsafe { &*sys_ctx.cast::<MockSystemFrame>() };
    unsafe { *out_commands = frame.world.cast::<BamCommands>() };
    FfiResult::OK
}

unsafe extern "C" fn host_iter_next_chunk(iter: *mut BamIter, out_len: *mut usize) -> FfiBool {
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let world = unsafe { &*state.world };
    let limit = state
        .end_chunk_idx
        .unwrap_or(world.chunks.len())
        .min(world.chunks.len());
    while state.next_chunk_idx < limit {
        let idx = state.next_chunk_idx;
        state.next_chunk_idx += 1;
        let len = world.chunks[idx].entities.len();
        if len > 0 {
            state.active_chunk_idx = Some(idx);
            unsafe { *out_len = len };
            return FfiBool::TRUE;
        }
    }
    state.active_chunk_idx = None;
    unsafe { *out_len = 0 };
    FfiBool::FALSE
}

unsafe extern "C" fn host_iter_reset(iter: *mut BamIter) -> FfiResult {
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    state.next_chunk_idx = 0;
    state.active_chunk_idx = None;
    FfiResult::OK
}

unsafe extern "C" fn host_iter_entities(iter: *const BamIter) -> *const FfiEntity {
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let world = unsafe { &*state.world };
    world.chunks[idx].entities.as_ptr()
}

unsafe extern "C" fn host_iter_col_ro(iter: *const BamIter, term_index: usize) -> *const u8 {
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null();
    };
    let world = unsafe { &*state.world };
    world.chunks[idx]
        .columns
        .get(&term.component)
        .map_or(core::ptr::null(), |w| w.as_ptr().cast::<u8>())
}

unsafe extern "C" fn host_iter_col_mut(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(idx) = state.active_chunk_idx else {
        return core::ptr::null_mut();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null_mut();
    };
    let world = unsafe { &mut *state.world };
    world.chunks[idx]
        .columns
        .get_mut(&term.component)
        .map_or(core::ptr::null_mut(), |w| w.as_mut_ptr().cast::<u8>())
}

unsafe extern "C" fn host_iter_col_mut_untracked(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    unsafe { host_iter_col_mut(iter, term_index) }
}

unsafe extern "C" fn host_iter_mark_changed(
    _iter: *mut BamIter,
    _term_index: usize,
    _start: usize,
    _count: usize,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn host_iter_rows_ro(_iter: *mut BamIter, _idx: usize) -> *const *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn host_iter_rows_mut(_iter: *mut BamIter, _idx: usize) -> *const *mut u8 {
    core::ptr::null()
}

unsafe extern "C" fn host_iter_col_info(
    iter: *const BamIter,
    term_index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult {
    if iter.is_null() || out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    let world = unsafe { &*state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return FfiResult::NOT_FOUND;
    };
    let info = BamColumnInfo {
        component: term.component,
        mode: term.mode,
        uuid: meta.uuid,
        size: meta.size,
        align: meta.align,
        storage_flags: 0,
        reserved0: 0,
    };
    unsafe { *out_info = info };
    FfiResult::OK
}

unsafe extern "C" fn host_res_ptr(_iter: *const BamIter, _idx: usize) -> *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn host_res_mut_ptr(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
    core::ptr::null_mut()
}

unsafe extern "C" fn host_cmd_spawn_empty(
    cmds: *mut BamCommands,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    world.next_entity_bits += 1;
    let entity = FfiEntity::from_bits(world.next_entity_bits);
    world.commands.push(RecordedCmd::SpawnEmpty(entity));
    unsafe { *out_entity = entity };
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_spawn(
    cmds: *mut BamCommands,
    _comps: *const BamComponent,
    _ptrs: *const *const u8,
    _count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    unsafe { host_cmd_spawn_empty(cmds, out_entity) }
}

unsafe extern "C" fn host_cmd_despawn(cmds: *mut BamCommands, entity: FfiEntity) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    world.commands.push(RecordedCmd::Despawn(entity));
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_insert(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    comp: BamComponent,
    data: *const u8,
    len: usize,
) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    let bytes = if len > 0 && !data.is_null() {
        unsafe { core::slice::from_raw_parts(data, len) }.to_vec()
    } else {
        Vec::new()
    };
    world.commands.push(RecordedCmd::Insert {
        entity,
        component: comp,
        bytes,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_insert_default(
    _cmds: *mut BamCommands,
    _entity: FfiEntity,
    _comp: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_remove(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    comp: BamComponent,
) -> FfiResult {
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    world.commands.push(RecordedCmd::Remove {
        entity,
        component: comp,
    });
    FfiResult::OK
}

// --- All 18 ABI 0.3.0 Host Slots (50..=67) ---

unsafe extern "C" fn host_log_diagnostic(
    ctx: *const BamHostCtx,
    level: u32,
    message: StrView,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if level > BAM_LOG_ERROR {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(text) = (unsafe { message.as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    // NOTE: Must NOT overwrite `world.last_error`.
    let world = unsafe { &mut *ctx.cast::<MockV03World>().cast_mut() };
    world.logs.push((level, text.to_owned()));
    FfiResult::OK
}

unsafe extern "C" fn host_report_guest_fault(
    ctx: *const BamHostCtx,
    message: StrView,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Ok(text) = (unsafe { message.as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    if text.is_empty() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *ctx.cast::<MockV03World>().cast_mut() };
    world.faults.push(text.to_owned());
    world.last_error = text.to_owned();
    FfiResult::OK
}

unsafe extern "C" fn host_resolve_component_by_uuid(
    ctx: *const BamHostCtx,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if ctx.is_null() || uuid.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let u = unsafe { *uuid };
    if u.is_zero() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &*ctx.cast::<MockV03World>() };
    let Some(&handle) = world.uuid_to_comp.get(&u) else {
        return FfiResult::NOT_FOUND;
    };
    unsafe { *out_component = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_component_layout_info(
    ctx: *const BamHostCtx,
    component: BamComponent,
    out_info: *mut BamLayoutInfo,
) -> FfiResult {
    if ctx.is_null() || out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !component.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &*ctx.cast::<MockV03World>() };
    let Some(meta) = world.comp_meta(component) else {
        return FfiResult::NOT_FOUND;
    };
    let info = BamLayoutInfo {
        struct_size: core::mem::size_of::<BamLayoutInfo>() as u32,
        flags: meta.flags,
        uuid: meta.uuid,
        schema_fingerprint: meta.schema_fingerprint,
        size: meta.size,
        align: meta.align,
        fields: if meta.fields.is_empty() {
            core::ptr::null()
        } else {
            meta.fields.as_ptr()
        },
        field_count: meta.fields.len(),
    };
    unsafe { *out_info = info };
    FfiResult::OK
}

unsafe extern "C" fn host_register_relation(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamRelationDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() || out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let desc_ref = unsafe { &*desc };
    let st = desc_ref.validate();
    if !st.is_ok() {
        return st;
    }
    let Ok(name) = unsafe { CStr::from_ptr(desc_ref.name) }.to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    if let Some(&existing) = world.name_to_comp.get(name) {
        unsafe { *out_component = existing };
        return FfiResult::OK;
    }
    let handle = BamComponent::from_raw((world.components.len() + 1) as u32);
    world.components.push(RegisteredComp {
        handle,
        name: name.to_owned(),
        uuid: desc_ref.uuid,
        schema_fingerprint: 0,
        size: desc_ref.data_size,
        align: desc_ref.data_align,
        flags: 0,
        fields: Vec::new(),
    });
    world.name_to_comp.insert(name.to_owned(), handle);
    if !desc_ref.uuid.is_zero() {
        world.uuid_to_comp.insert(desc_ref.uuid, handle);
    }
    world.relations.insert(
        handle,
        RegisteredRelation {
            handle,
            name: name.to_owned(),
            uuid: desc_ref.uuid,
            flags: desc_ref.flags,
            data_size: desc_ref.data_size,
            data_align: desc_ref.data_align,
            max_degree: desc_ref.max_degree,
        },
    );
    unsafe { *out_component = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_intern_symbol(
    ctx: *const BamHostCtx,
    symbol: StrView,
    out_symbol: *mut BamSymbol,
) -> FfiResult {
    if ctx.is_null() || out_symbol.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Ok(text) = (unsafe { symbol.as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    if text.is_empty() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *ctx.cast::<MockV03World>().cast_mut() };
    if let Some(&sym) = world.symbol_to_id.get(text) {
        unsafe { *out_symbol = sym };
        return FfiResult::OK;
    }
    let sym = BamSymbol::from_raw(world.symbols.len() as u32);
    world.symbols.push(text.to_owned());
    world.symbol_to_id.insert(text.to_owned(), sym);
    unsafe { *out_symbol = sym };
    FfiResult::OK
}

unsafe extern "C" fn host_bind_named_object(
    ctx: *const BamHostCtx,
    name: StrView,
    entity: FfiEntity,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Ok(text) = (unsafe { name.as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    if text.is_empty() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *ctx.cast::<MockV03World>().cast_mut() };
    world.named_objects.insert(text.to_owned(), entity);
    FfiResult::OK
}

unsafe extern "C" fn host_iter_count(iter: *mut BamIter, out_count: *mut usize) -> FfiResult {
    if iter.is_null() || out_count.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let world = unsafe { &*state.world };
    let total: usize = world.chunks.iter().map(|c| c.entities.len()).sum();
    unsafe { *out_count = total };
    FfiResult::OK
}

unsafe extern "C" fn host_iter_up_cascade(
    iter: *mut BamIter,
    relation: BamComponent,
    entity: FfiEntity,
    term_index: usize,
    out_ptr: *mut *const c_void,
    out_owner: *mut FfiEntity,
) -> FfiResult {
    if iter.is_null() || out_ptr.is_null() || out_owner.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if matches!(term.mode, AccessMode::WRITE | AccessMode::OPTIONAL_WRITE) {
        return FfiResult::PERMISSION_DENIED;
    }
    let world = unsafe { &*state.world };
    let mut visited = HashSet::new();
    visited.insert(entity);
    let mut cur = entity;
    while let Some(targets) = world.relation_edges.get(&(relation, cur)) {
        let Some(&parent) = targets.first() else {
            break;
        };
        if !visited.insert(parent) {
            return FfiResult::CYCLE_DETECTED;
        }
        if let Some(words) = world.entity_components.get(&(parent, term.component)) {
            unsafe {
                *out_ptr = words.as_ptr().cast::<c_void>();
                *out_owner = parent;
            }
            return FfiResult::OK;
        }
        cur = parent;
    }
    unsafe {
        *out_ptr = core::ptr::null();
        *out_owner = FfiEntity::NULL;
    }
    if world.up_cascade_missing_as_ok_null {
        FfiResult::OK
    } else {
        FfiResult::NOT_FOUND
    }
}

unsafe extern "C" fn host_par_for_each_chunk_readonly(
    iter: *mut BamIter,
    task_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(cb) = task_fn else {
        return FfiResult::NULL_POINTER;
    };
    let state = unsafe { &*iter.cast::<MockIterState>() };
    for term in &state.terms {
        if matches!(term.mode, AccessMode::WRITE | AccessMode::OPTIONAL_WRITE) {
            return FfiResult::PERMISSION_DENIED;
        }
    }
    let world_ptr = state.world;
    let chunk_count = unsafe { &*world_ptr }.chunks.len();
    for chunk_idx in 0..chunk_count {
        let chunk_len = unsafe { &*world_ptr }.chunks[chunk_idx].entities.len();
        if chunk_len == 0 {
            continue;
        }
        let mut task_state = MockIterState {
            world: world_ptr,
            terms: state.terms.clone(),
            next_chunk_idx: chunk_idx,
            end_chunk_idx: Some(chunk_idx + 1),
            active_chunk_idx: None,
        };
        let status = unsafe { cb((&raw mut task_state).cast::<BamIter>(), user_data) };
        if !status.is_ok() {
            return status;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn host_register_system_handle(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamSystemDesc,
    out_system: *mut BamSystem,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() || out_system.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    let desc_ref = unsafe { &*desc };
    match push_system(world, desc_ref) {
        Ok(handle) => {
            unsafe { *out_system = handle };
            FfiResult::OK
        }
        Err(err) => err,
    }
}

unsafe extern "C" fn host_configure_schedule_set(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamScheduleSetDesc,
    out_set: *mut BamSystemSet,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() || out_set.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let desc_ref = unsafe { &*desc };
    let st = desc_ref.validate();
    if !st.is_ok() {
        return st;
    }
    let Ok(name) = unsafe { CStr::from_ptr(desc_ref.name) }.to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    let before_sets = if desc_ref.before_count > 0 && !desc_ref.before_sets.is_null() {
        unsafe { core::slice::from_raw_parts(desc_ref.before_sets, desc_ref.before_count as usize) }
            .to_vec()
    } else {
        Vec::new()
    };
    let after_sets = if desc_ref.after_count > 0 && !desc_ref.after_sets.is_null() {
        unsafe { core::slice::from_raw_parts(desc_ref.after_sets, desc_ref.after_count as usize) }
            .to_vec()
    } else {
        Vec::new()
    };
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    let handle = BamSystemSet::from_raw(world.schedule_sets.len() as u32);
    world.schedule_sets.push(RegisteredScheduleSet {
        handle,
        name: name.to_owned(),
        schedule: desc_ref.schedule,
        flags: desc_ref.flags,
        before_sets,
        after_sets,
    });
    unsafe { *out_set = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_mount_system_in_schedule(
    load_ctx: *mut BamLoadCtx,
    system: BamSystem,
    schedule: ScheduleId,
    set: BamSystemSet,
) -> FfiResult {
    if load_ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !system.is_valid() || !schedule.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    if (system.to_raw() as usize) >= world.systems.len() {
        return FfiResult::NOT_FOUND;
    }
    if set.is_valid() && (set.to_raw() as usize) >= world.schedule_sets.len() {
        return FfiResult::NOT_FOUND;
    }
    world.mounted_systems.push((system, schedule, set));
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_checkpoint(
    cmds: *mut BamCommands,
    out_checkpoint: *mut usize,
) -> FfiResult {
    if cmds.is_null() || out_checkpoint.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &*cmds.cast::<MockV03World>() };
    unsafe { *out_checkpoint = world.commands.len() };
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_rollback(cmds: *mut BamCommands, checkpoint: usize) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    if checkpoint > world.commands.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    world.commands.truncate(checkpoint);
    world.rollback_calls += 1;
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_insert_relation(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
    data: *const c_void,
    data_size: usize,
) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() || target.is_null() || !relation.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    let Some(rel) = world.relations.get(&relation).cloned() else {
        return FfiResult::NOT_FOUND;
    };
    if data_size != rel.data_size {
        return FfiResult::TYPE_MISMATCH;
    }
    if data_size > 0 && data.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if (rel.flags & BAM_RELATION_FLAG_ACYCLIC) != 0
        && world.would_create_cycle(relation, entity, target)
    {
        return FfiResult::CYCLE_DETECTED;
    }
    let payload = if data_size > 0 {
        unsafe { core::slice::from_raw_parts(data.cast::<u8>(), data_size) }.to_vec()
    } else {
        Vec::new()
    };
    world
        .relation_edges
        .entry((relation, entity))
        .or_default()
        .push(target);
    world.commands.push(RecordedCmd::InsertRelation {
        entity,
        relation,
        target,
        payload,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_remove_relation(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() || target.is_null() || !relation.is_valid() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let world = unsafe { &mut *cmds.cast::<MockV03World>() };
    if !world.relations.contains_key(&relation) {
        return FfiResult::NOT_FOUND;
    }
    if let Some(edges) = world.relation_edges.get_mut(&(relation, entity)) {
        edges.retain(|&t| t != target);
    }
    world.commands.push(RecordedCmd::RemoveRelation {
        entity,
        relation,
        target,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_migrate_component_column(
    load_ctx: *mut BamLoadCtx,
    component: BamComponent,
    old_fingerprint: u64,
    new_desc: *const ComponentDesc,
    migrate_fn: Option<BamRowMigrateFn>,
    user_data: *mut c_void,
) -> FfiResult {
    if load_ctx.is_null() || new_desc.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(cb) = migrate_fn else {
        return FfiResult::NULL_POINTER;
    };
    let desc_ref = unsafe { &*new_desc };
    let st = desc_ref.validate();
    if !st.is_ok() {
        return st;
    }
    let world = unsafe { &mut *load_ctx.cast::<MockV03World>() };
    let Some(old_meta) = world.comp_meta(component).cloned() else {
        return FfiResult::NOT_FOUND;
    };
    if old_fingerprint != 0
        && old_meta.schema_fingerprint != 0
        && old_meta.schema_fingerprint != old_fingerprint
    {
        return FfiResult::TYPE_MISMATCH;
    }

    let mut migrated_chunks: Vec<(usize, Vec<u64>)> = Vec::new();
    for (chunk_idx, chunk) in world.chunks.iter().enumerate() {
        let Some(old_words) = chunk.columns.get(&component) else {
            continue;
        };
        let row_count = chunk.entities.len();
        let new_byte_len = row_count * desc_ref.size;
        let new_word_len = new_byte_len.div_ceil(8).max(1);
        let mut new_words = vec![0u64; new_word_len];
        let old_base = old_words.as_ptr().cast::<u8>();
        let new_base = new_words.as_mut_ptr().cast::<u8>();
        for row in 0..row_count {
            let old_row_ptr = if old_meta.size == 0 {
                core::ptr::NonNull::<u8>::dangling()
                    .as_ptr()
                    .cast::<c_void>()
            } else {
                unsafe { old_base.add(row * old_meta.size).cast::<c_void>() }
            };
            let new_row_ptr = if desc_ref.size == 0 {
                core::ptr::NonNull::<u8>::dangling()
                    .as_ptr()
                    .cast::<c_void>()
            } else {
                unsafe { new_base.add(row * desc_ref.size).cast::<c_void>() }
            };
            let row_status = unsafe { cb(old_row_ptr, new_row_ptr, user_data) };
            if !row_status.is_ok() {
                return row_status;
            }
        }
        migrated_chunks.push((chunk_idx, new_words));
    }

    for (chunk_idx, new_words) in migrated_chunks {
        world.chunks[chunk_idx].columns.insert(component, new_words);
    }
    let new_fields = if !desc_ref.fields.is_null() && desc_ref.field_count > 0 {
        unsafe { core::slice::from_raw_parts(desc_ref.fields, desc_ref.field_count) }.to_vec()
    } else {
        Vec::new()
    };
    if let Some(meta) = world.comp_meta_mut(component) {
        meta.uuid = desc_ref.uuid;
        meta.schema_fingerprint = desc_ref.schema_fingerprint;
        meta.size = desc_ref.size;
        meta.align = desc_ref.align;
        meta.flags = desc_ref.flags;
        meta.fields = new_fields;
    }
    if !desc_ref.uuid.is_zero() {
        world.uuid_to_comp.insert(desc_ref.uuid, component);
    }
    FfiResult::OK
}

fn make_v02_only_host_api(truncated_struct_size: bool) -> BamHostApi {
    BamHostApi {
        struct_size: if truncated_struct_size {
            BamHostApi::MIN_V0_2_SIZE as u32
        } else {
            core::mem::size_of::<BamHostApi>() as u32
        },
        abi_version: ABI_VERSION,
        get_proc_address: Some(host_get_proc_address),
        last_error: Some(host_last_error),
        load_host_ctx: Some(host_load_host_ctx),
        register_component: Some(host_register_component),
        resolve_component: Some(host_resolve_component),
        register_system: Some(host_register_system),
        sys_host_ctx: Some(host_sys_host_ctx),
        sys_query: Some(host_sys_query),
        sys_commands: Some(host_sys_commands),
        iter_next_chunk: Some(host_iter_next_chunk),
        iter_reset: Some(host_iter_reset),
        iter_entities: Some(host_iter_entities),
        iter_column_readonly: Some(host_iter_col_ro),
        iter_column_mutable: Some(host_iter_col_mut),
        iter_column_mutable_untracked: Some(host_iter_col_mut_untracked),
        iter_mark_changed_range: Some(host_iter_mark_changed),
        iter_column_rows_readonly: Some(host_iter_rows_ro),
        iter_column_rows_mutable: Some(host_iter_rows_mut),
        iter_column_info: Some(host_iter_col_info),
        res_ptr: Some(host_res_ptr),
        res_mut_ptr: Some(host_res_mut_ptr),
        cmd_spawn_empty: Some(host_cmd_spawn_empty),
        cmd_spawn: Some(host_cmd_spawn),
        cmd_despawn: Some(host_cmd_despawn),
        cmd_insert: Some(host_cmd_insert),
        cmd_insert_default: Some(host_cmd_insert_default),
        cmd_remove: Some(host_cmd_remove),
        par_for_each_chunk: None,
        sys_entity_get: None,
        sys_entity_get_mut: None,
        msg_read_batch: None,
        msg_send: None,
        msg_write_batch: None,
        res_ticks: None,
        sys_res_ptr: None,
        sys_res_mut_ptr: None,
        register_observer: None,
        cmd_trigger: None,
        cmd_spawn_persistent: None,
        cmd_spawn_batch: None,
        cmd_despawn_all: None,
        cmd_insert_if_new: None,
        cmd_trigger_ex: None,
        cmd_observe: None,
        iter_read_removed: None,
        sys_entity_children: None,
        asset_load: None,
        asset_get_load_state: None,
        asset_release: None,
        cmd_spawn_scene: None,
        log_diagnostic: None,
        report_guest_fault: None,
        resolve_component_by_uuid: None,
        component_layout_info: None,
        register_relation: None,
        intern_symbol: None,
        bind_named_object: None,
        iter_count: None,
        iter_up_cascade: None,
        par_for_each_chunk_readonly: None,
        register_system_handle: None,
        configure_schedule_set: None,
        mount_system_in_schedule: None,
        cmd_checkpoint: None,
        cmd_rollback: None,
        cmd_insert_relation: None,
        cmd_remove_relation: None,
        migrate_component_column: None,
    }
}

fn make_v03_host_api() -> BamHostApi {
    let mut api = make_v02_only_host_api(false);
    api.log_diagnostic = Some(host_log_diagnostic);
    api.report_guest_fault = Some(host_report_guest_fault);
    api.resolve_component_by_uuid = Some(host_resolve_component_by_uuid);
    api.component_layout_info = Some(host_component_layout_info);
    api.register_relation = Some(host_register_relation);
    api.intern_symbol = Some(host_intern_symbol);
    api.bind_named_object = Some(host_bind_named_object);
    api.iter_count = Some(host_iter_count);
    api.iter_up_cascade = Some(host_iter_up_cascade);
    api.par_for_each_chunk_readonly = Some(host_par_for_each_chunk_readonly);
    api.register_system_handle = Some(host_register_system_handle);
    api.configure_schedule_set = Some(host_configure_schedule_set);
    api.mount_system_in_schedule = Some(host_mount_system_in_schedule);
    api.cmd_checkpoint = Some(host_cmd_checkpoint);
    api.cmd_rollback = Some(host_cmd_rollback);
    api.cmd_insert_relation = Some(host_cmd_insert_relation);
    api.cmd_remove_relation = Some(host_cmd_remove_relation);
    api.migrate_component_column = Some(host_migrate_component_column);
    api
}

// ============================================================================
// Test Component Types (works with both default features and `--all-features`)
// ============================================================================

macro_rules! define_pod_component {
    ($vis:vis struct $name:ident { $($field:ident : $ty:ty),* $(,)? } => $cname:literal) => {
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        $vis struct $name {
            $(pub $field: $ty,)*
        }
        unsafe impl RawInsertable for $name {}
        unsafe impl AbiPod for $name {}
        impl GuestComponent for $name {
            const NAME: &'static CStr = $cname;
        }
    };
}

define_pod_component!(struct Faction { id: u32 } => c"v03::Faction");
define_pod_component!(struct Counter { value: u32 } => c"v03::Counter");
define_pod_component!(struct EdgeWeight { weight: f32 } => c"v03::EdgeWeight");
define_pod_component!(struct ParentRel {} => c"v03::ParentRel");

macro_rules! impl_manual_mod_component {
    ($ty:ty, $name:literal, $cstr:literal, $fp:expr, $fields:expr) => {
        impl ModComponent for $ty {
            const CANONICAL_NAME: &'static str = $name;
            const COMPONENT_CSTR: &'static CStr = $cstr;
            const UUID: ComponentUuid = ComponentUuid::from_parts(
                $name,
                core::mem::size_of::<$ty>(),
                core::mem::align_of::<$ty>(),
                $fp,
            );
            const IS_ABI_POD: bool = true;
            const IS_RAW_INSERTABLE: bool = true;
            const SCHEMA_FINGERPRINT: u64 = $fp;
            const BAM_FIELDS: &'static [BamFieldDesc] = $fields;
            type VTable = VTableHeader;
            fn vtable() -> &'static Self::VTable {
                static VT: VTableHeader = VTableHeader::from_raw_parts(
                    core::mem::size_of::<VTableHeader>() as u32,
                    <$ty as ModComponent>::UUID,
                    core::mem::size_of::<$ty>(),
                    core::mem::align_of::<$ty>(),
                );
                &VT
            }
            fn symbols() -> &'static [SymbolEntry] {
                &[]
            }
        }
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct OldStats {
    hp: u32,
}
unsafe impl RawInsertable for OldStats {}
unsafe impl AbiPod for OldStats {}
impl_manual_mod_component!(
    OldStats,
    "v03::Stats",
    c"v03::Stats",
    0x1111_2222_3333_4444,
    &[BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("hp"),
        StrView::from_str("u32"),
        0,
        4,
        4,
    )]
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct NewStats {
    hp: f64,
    armor: u32,
    pad: u32,
}
unsafe impl RawInsertable for NewStats {}
unsafe impl AbiPod for NewStats {}
impl_manual_mod_component!(
    NewStats,
    "v03::Stats",
    c"v03::Stats",
    0x5555_6666_7777_8888,
    &[
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("hp"),
            StrView::from_str("f64"),
            0,
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("armor"),
            StrView::from_str("u32"),
            8,
            4,
            4,
        ),
    ]
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct BadFingerprintStats {
    hp: u32,
}
unsafe impl RawInsertable for BadFingerprintStats {}
unsafe impl AbiPod for BadFingerprintStats {}
impl_manual_mod_component!(
    BadFingerprintStats,
    "v03::Stats",
    c"v03::Stats",
    0xDEAD_BEEF_CAFE_BABE,
    &[]
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct BadSizeStats {
    hp: u64,
}
unsafe impl RawInsertable for BadSizeStats {}
unsafe impl AbiPod for BadSizeStats {}
impl_manual_mod_component!(
    BadSizeStats,
    "v03::Stats",
    c"v03::Stats",
    0x1111_2222_3333_4444,
    &[]
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct BadAlignStats {
    bytes: [u8; 4],
}
unsafe impl RawInsertable for BadAlignStats {}
unsafe impl AbiPod for BadAlignStats {}
impl_manual_mod_component!(
    BadAlignStats,
    "v03::Stats",
    c"v03::Stats",
    0x1111_2222_3333_4444,
    &[]
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct OldTag;
unsafe impl RawInsertable for OldTag {}
unsafe impl AbiPod for OldTag {}
impl_manual_mod_component!(OldTag, "v03::Tag", c"v03::Tag", 0x0000_0000_0000_0001, &[]);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct NewTag;
unsafe impl RawInsertable for NewTag {}
unsafe impl AbiPod for NewTag {}
impl_manual_mod_component!(NewTag, "v03::Tag", c"v03::Tag", 0x0000_0000_0000_0002, &[]);

define_pod_component!(struct C0 { v: u32 } => c"q16::C0");
define_pod_component!(struct C1 { v: u32 } => c"q16::C1");
define_pod_component!(struct C2 { v: u32 } => c"q16::C2");
define_pod_component!(struct C3 { v: u32 } => c"q16::C3");
define_pod_component!(struct C4 { v: u32 } => c"q16::C4");
define_pod_component!(struct C5 { v: u32 } => c"q16::C5");
define_pod_component!(struct C6 { v: u32 } => c"q16::C6");
define_pod_component!(struct C7 { v: u32 } => c"q16::C7");
define_pod_component!(struct C8 { v: u32 } => c"q16::C8");
define_pod_component!(struct C9 { v: u32 } => c"q16::C9");
define_pod_component!(struct C10 { v: u32 } => c"q16::C10");
define_pod_component!(struct C11 { v: u32 } => c"q16::C11");
define_pod_component!(struct C12 { v: u32 } => c"q16::C12");
define_pod_component!(struct C13 { v: u32 } => c"q16::C13");
define_pod_component!(struct C14 { v: u32 } => c"q16::C14");
define_pod_component!(struct C15 { v: u32 } => c"q16::C15");

// ============================================================================
// 1. ABI 0.2 Truncated Table & Missing Slot `UNSUPPORTED` Fallbacks
// ============================================================================

#[test]
fn abi_v02_truncated_and_missing_slots_return_unsupported() {
    for truncated in [true, false] {
        let mut host = V03TestHost::with_api(make_v02_only_host_api(truncated));
        let comp_h = host.expose_component::<Faction>("v03::Faction", Faction::UUID, 0, &[]);
        let rel_h = host.expose_component::<ParentRel>("v03::ParentRel", ParentRel::UUID, 0, &[]);
        let mut chunk = MockChunk::new(vec![FfiEntity::from_bits(1)]);
        chunk.insert_dense(comp_h, &[Faction { id: 1 }]);
        host.chunks.push(chunk);

        let inst = host
            .load_fn(c"fallback_mod", |app| {
                // Slots 50..=56 & 60..=62 & 67 during init
                assert_eq!(
                    app.log_diagnostic(BAM_LOG_INFO, "hello"),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(guest_log!(app, info, "macro {}", 1), FfiResult::UNSUPPORTED);
                assert_eq!(app.report_fault("fault"), FfiResult::UNSUPPORTED);
                assert_eq!(
                    app.resolve_component_by_uuid(Faction::UUID),
                    Err(FfiResult::UNSUPPORTED)
                );
                assert_eq!(
                    app.component_layout_info(comp_h).unwrap_err(),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(
                    app.verify_component_layout_handle::<OldStats>(comp_h)
                        .unwrap_err(),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(
                    app.register_relation(RelationDescBuilder::new("likes")),
                    Err(FfiResult::UNSUPPORTED)
                );
                assert_eq!(app.intern_symbol("sym"), Err(FfiResult::UNSUPPORTED));
                assert_eq!(
                    app.bind_named_object("hero", FfiEntity::from_bits(1)),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(
                    app.system(c"decoupled")
                        .query_typed::<&Faction>()
                        .register_handle(|_| FfiResult::OK),
                    Err(FfiResult::UNSUPPORTED)
                );
                assert_eq!(
                    app.configure_schedule_set(ScheduleSetBuilder::new("set_a")),
                    Err(FfiResult::UNSUPPORTED)
                );
                assert_eq!(
                    app.mount_system(
                        BamSystem::from_raw(0),
                        ScheduleId::UPDATE,
                        BamSystemSet::INVALID
                    ),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(
                    app.migrate_component_column::<OldStats, NewStats, _>(comp_h, |old| {
                        Ok(NewStats {
                            hp: old.hp as f64,
                            armor: 0,
                            pad: 0,
                        })
                    }),
                    FfiResult::UNSUPPORTED
                );
                assert_eq!(
                    app.migrate_pod_component_column::<Faction, Counter, _>(comp_h, 0, 0, |f| Ok(
                        Counter { value: f.id }
                    )),
                    FfiResult::UNSUPPORTED
                );

                // Register an ABI 0.2 system to verify runtime slots 57..=59 & 63..=66
                app.system(c"check_runtime_fallbacks")
                    .query_typed::<&Faction>()
                    .build(move |ctx| {
                        assert_eq!(
                            ctx.log_diagnostic(BAM_LOG_WARN, "w"),
                            FfiResult::UNSUPPORTED
                        );
                        assert_eq!(ctx.report_fault("rf"), FfiResult::UNSUPPORTED);
                        assert_eq!(ctx.intern_symbol("x"), Err(FfiResult::UNSUPPORTED));
                        assert_eq!(
                            ctx.bind_named_object("x", FfiEntity::from_bits(1)),
                            FfiResult::UNSUPPORTED
                        );
                        assert_eq!(
                            ctx.resolve_component_by_uuid(Faction::UUID),
                            Err(FfiResult::UNSUPPORTED)
                        );
                        assert_eq!(
                            ctx.component_layout_info(comp_h).unwrap_err(),
                            FfiResult::UNSUPPORTED
                        );

                        assert_eq!(ctx.iter().count(), Err(FfiResult::UNSUPPORTED));
                        assert_eq!(
                            ctx.single_query::<&Faction>().count(),
                            Err(FfiResult::UNSUPPORTED)
                        );
                        assert_eq!(
                            ctx.iter()
                                .up_cascade::<Faction>(rel_h, FfiEntity::from_bits(1), 0),
                            Err(FfiResult::UNSUPPORTED)
                        );
                        assert_eq!(
                            ctx.single_query::<&Faction>()
                                .up_cascade::<ParentRel, Faction>(FfiEntity::from_bits(1), 0),
                            Err(FfiResult::UNSUPPORTED)
                        );
                        assert_eq!(ctx.par_chunks_readonly(|_| {}), FfiResult::UNSUPPORTED);
                        assert_eq!(
                            ctx.single_query::<&Faction>()
                                .par_for_each_chunk_readonly(|_| FfiResult::OK),
                            FfiResult::UNSUPPORTED
                        );

                        let cmds = ctx.commands();
                        assert_eq!(cmds.checkpoint(), Err(FfiResult::UNSUPPORTED));
                        assert_eq!(cmds.rollback(0), FfiResult::UNSUPPORTED);
                        assert_eq!(cmds.transaction(|_| Ok(())), Err(FfiResult::UNSUPPORTED));
                        assert_eq!(
                            cmds.insert_relation(
                                FfiEntity::from_bits(1),
                                rel_h,
                                FfiEntity::from_bits(2)
                            ),
                            FfiResult::UNSUPPORTED
                        );
                        assert_eq!(
                            cmds.insert_relation_with_payload(
                                FfiEntity::from_bits(1),
                                rel_h,
                                FfiEntity::from_bits(2),
                                &EdgeWeight { weight: 1.0 }
                            ),
                            FfiResult::UNSUPPORTED
                        );
                        assert_eq!(
                            cmds.remove_relation(
                                FfiEntity::from_bits(1),
                                rel_h,
                                FfiEntity::from_bits(2)
                            ),
                            FfiResult::UNSUPPORTED
                        );
                        FfiResult::OK
                    })
            })
            .expect("init must succeed even when v0.3 slots are unsupported");

        assert_eq!(host.run_system(0), FfiResult::OK);
        host.unload(inst);
    }
}

// ============================================================================
// 2. Diagnostics (`log_diagnostic`, `guest_log!`) & `report_fault`
// ============================================================================

#[test]
fn diagnostics_guest_log_and_report_fault() {
    let host = V03TestHost::new();
    let inst = host
        .load_fn(c"diag_mod", |app| {
            assert_eq!(guest_log!(app, trace, "boot trace"), FfiResult::OK);
            assert_eq!(guest_log!(app, debug, "boot debug {}", 7), FfiResult::OK);
            assert_eq!(
                app.log_diagnostic(BAM_LOG_INFO, "direct info"),
                FfiResult::OK
            );
            assert_eq!(
                app.log_diagnostic(99, "bad level"),
                FfiResult::INVALID_ARGUMENT
            );
            // `log_diagnostic` must NOT clobber `last_error`.
            assert_eq!(app.host().last_error(), "initial_host_error");

            app.system(c"diag_sys")
                .run_if(|rctx| {
                    assert_eq!(guest_log!(rctx, trace, "run_if check"), FfiResult::OK);
                    true
                })
                .build(|ctx| {
                    let count = 42u32;
                    assert_eq!(guest_log!(ctx, info, "static info"), FfiResult::OK);
                    assert_eq!(guest_log!(ctx, warn, "count={count}"), FfiResult::OK);
                    assert_eq!(guest_log!(ctx, error, "err: {}", "disk"), FfiResult::OK);
                    assert_eq!(
                        guest_log!(ctx, BAM_LOG_DEBUG, "explicit level"),
                        FfiResult::OK
                    );
                    assert_eq!(ctx.host().last_error(), "initial_host_error");

                    assert_eq!(ctx.report_fault(""), FfiResult::INVALID_ARGUMENT);
                    assert_eq!(ctx.report_fault("tick fault recorded"), FfiResult::OK);
                    assert_eq!(ctx.host().last_error(), "tick fault recorded");
                    FfiResult::OK
                })
        })
        .unwrap();

    assert_eq!(host.run_system(0), FfiResult::OK);
    assert_eq!(
        host.logs,
        vec![
            (BAM_LOG_TRACE, "boot trace".to_owned()),
            (BAM_LOG_DEBUG, "boot debug 7".to_owned()),
            (BAM_LOG_INFO, "direct info".to_owned()),
            (BAM_LOG_TRACE, "run_if check".to_owned()),
            (BAM_LOG_INFO, "static info".to_owned()),
            (BAM_LOG_WARN, "count=42".to_owned()),
            (BAM_LOG_ERROR, "err: disk".to_owned()),
            (BAM_LOG_DEBUG, "explicit level".to_owned()),
        ]
    );
    assert_eq!(host.faults, vec!["tick fault recorded".to_owned()]);
    host.unload(inst);
}

// ============================================================================
// 3. UUID Resolution, `component_layout_info`, & `verify_component_layout`
// ============================================================================

#[test]
fn resolve_by_uuid_layout_info_and_verify_layout() {
    let mut host = V03TestHost::new();
    let stats_h = host.expose_component::<OldStats>(
        OldStats::CANONICAL_NAME,
        OldStats::UUID,
        OldStats::SCHEMA_FINGERPRINT,
        OldStats::BAM_FIELDS,
    );

    let inst = host
        .load_fn(c"layout_mod", |app| {
            assert_eq!(app.resolve_component_by_uuid(OldStats::UUID), Ok(stats_h));
            assert_eq!(
                app.resolve_component_by_uuid(ComponentUuid::ZERO),
                Err(FfiResult::INVALID_ARGUMENT)
            );
            assert_eq!(
                app.resolve_component_by_uuid(ComponentUuid::from_u128(0x1234)),
                Err(FfiResult::NOT_FOUND)
            );

            let info = app.component_layout_info(stats_h).unwrap();
            assert_eq!(info.uuid, OldStats::UUID);
            assert_eq!(info.schema_fingerprint, OldStats::SCHEMA_FINGERPRINT);
            assert_eq!(info.size, core::mem::size_of::<OldStats>());
            assert_eq!(info.align, core::mem::align_of::<OldStats>());
            assert_eq!(info.field_count, 1);

            // Matching layout + schema fingerprint passes.
            let verified = app.verify_component_layout::<OldStats>().unwrap();
            assert_eq!(verified.schema_fingerprint, OldStats::SCHEMA_FINGERPRINT);

            // Mismatched fingerprint, size, or align all return TYPE_MISMATCH.
            assert_eq!(
                app.verify_component_layout_handle::<BadFingerprintStats>(stats_h)
                    .unwrap_err(),
                FfiResult::TYPE_MISMATCH
            );
            assert_eq!(
                app.verify_component_layout_handle::<BadSizeStats>(stats_h)
                    .unwrap_err(),
                FfiResult::TYPE_MISMATCH
            );
            assert_eq!(
                app.verify_component_layout_handle::<BadAlignStats>(stats_h)
                    .unwrap_err(),
                FfiResult::TYPE_MISMATCH
            );

            app.system(c"check_layout_in_sys").build(move |ctx| {
                assert_eq!(ctx.resolve_component_by_uuid(OldStats::UUID), Ok(stats_h));
                assert!(ctx.verify_component_layout::<OldStats>().is_ok());
                assert_eq!(
                    ctx.verify_component_layout_handle::<BadFingerprintStats>(stats_h)
                        .unwrap_err(),
                    FfiResult::TYPE_MISMATCH
                );
                FfiResult::OK
            })
        })
        .unwrap();

    assert_eq!(host.run_system(0), FfiResult::OK);
    host.unload(inst);
}

#[cfg(feature = "derive")]
#[test]
fn derived_mod_component_schema_fingerprint_and_fields_verified() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
    #[mod_component(name = "v03::DerivedHero")]
    struct DerivedHero {
        hp: f32,
        mana: f32,
        level: u32,
    }

    let host = V03TestHost::new();
    let inst = host
        .load_fn(c"derive_v03_mod", |app| {
            let handle = app.register_dynamic_component::<DerivedHero>().unwrap();
            let info = app.verify_component_layout::<DerivedHero>().unwrap();
            assert_eq!(info.size, core::mem::size_of::<DerivedHero>());
            assert_eq!(info.align, core::mem::align_of::<DerivedHero>());
            assert_eq!(info.schema_fingerprint, DerivedHero::SCHEMA_FINGERPRINT);
            assert_ne!(info.schema_fingerprint, 0);
            assert_eq!(info.field_count, 3);
            assert_eq!(app.resolve_component_by_uuid(DerivedHero::UUID), Ok(handle));
            FfiResult::OK
        })
        .unwrap();
    host.unload(inst);
}

// ============================================================================
// 4. Typed Relationships (`RelationDescBuilder`, `insert_relation`, Cycles)
// ============================================================================

#[test]
fn relations_builder_commands_and_cycle_detection() {
    let host = V03TestHost::new();
    let mut likes_handle = BamComponent::INVALID;
    let mut weighted_handle = BamComponent::INVALID;
    let mut parent_handle = BamComponent::INVALID;

    let inst = host
        .load_fn(c"rel_mod", |app| {
            let likes_desc = RelationDescBuilder::new("likes")
                .exclusive()
                .acyclic()
                .cascade_despawn()
                .max_degree(2);
            assert_eq!(
                likes_desc.raw_flags(),
                BAM_RELATION_FLAG_EXCLUSIVE
                    | BAM_RELATION_FLAG_ACYCLIC
                    | BAM_RELATION_FLAG_CASCADE_DESPAWN
            );
            assert_eq!(likes_desc.configured_max_degree(), 2);
            likes_handle = app.register_relation(likes_desc).unwrap();

            let weighted_desc = RelationDescBuilder::new("v03::WeightedLink")
                .symmetric()
                .cleanup_remove()
                .cascade_delete()
                .with_payload::<EdgeWeight>();
            assert_eq!(
                weighted_desc.data_size(),
                core::mem::size_of::<EdgeWeight>()
            );
            assert_eq!(
                weighted_desc.data_align(),
                core::mem::align_of::<EdgeWeight>()
            );
            weighted_handle = app.register_relation(weighted_desc).unwrap();

            parent_handle = app
                .register_relation(RelationDescBuilder::for_component::<ParentRel>().acyclic())
                .unwrap();

            // Empty relation name rejected
            assert_eq!(
                app.register_relation(RelationDescBuilder::new("")),
                Err(FfiResult::INVALID_ARGUMENT)
            );

            let likes = likes_handle;
            let weighted = weighted_handle;
            app.system(c"rel_ops").build(move |ctx| {
                let cmds = ctx.commands();
                let a = FfiEntity::from_bits(10);
                let b = FfiEntity::from_bits(20);
                let c = FfiEntity::from_bits(30);

                // Marker relation chain A -> B -> C succeeds
                assert_eq!(cmds.insert_relation(a, likes, b), FfiResult::OK);
                assert_eq!(cmds.insert_relation(b, likes, c), FfiResult::OK);
                // Cycle C -> A rejected with CYCLE_DETECTED
                assert_eq!(cmds.insert_relation(c, likes, a), FfiResult::CYCLE_DETECTED);
                // Self-loop rejected with CYCLE_DETECTED
                assert_eq!(cmds.insert_relation(a, likes, a), FfiResult::CYCLE_DETECTED);

                // Payload relation
                assert_eq!(
                    cmds.insert_relation_with_payload(a, weighted, b, &EdgeWeight { weight: 7.5 }),
                    FfiResult::OK
                );
                // Wrong payload size rejected
                assert_eq!(
                    cmds.insert_relation_with_payload(a, likes, c, &EdgeWeight { weight: 1.0 }),
                    FfiResult::TYPE_MISMATCH
                );
                // Null entity rejected before FFI
                assert_eq!(
                    cmds.insert_relation(FfiEntity::NULL, likes, b),
                    FfiResult::INVALID_ENTITY
                );
                assert_eq!(cmds.remove_relation(b, likes, c), FfiResult::OK);
                FfiResult::OK
            })
        })
        .unwrap();

    // Verify auto-qualification of "likes" -> "rel_mod::likes"
    let registered_likes = host.relations.get(&likes_handle).unwrap();
    assert_eq!(registered_likes.name, "rel_mod::likes");
    assert_eq!(registered_likes.max_degree, 2);
    assert_eq!(
        registered_likes.flags,
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_ACYCLIC | BAM_RELATION_FLAG_CASCADE_DESPAWN
    );
    let registered_weighted = host.relations.get(&weighted_handle).unwrap();
    assert_eq!(registered_weighted.name, "v03::WeightedLink");
    assert_eq!(registered_weighted.data_size, 4);
    assert_eq!(
        host.relations.get(&parent_handle).unwrap().name,
        "v03::ParentRel"
    );

    assert_eq!(host.run_system(0), FfiResult::OK);
    assert_eq!(host.commands.len(), 4);
    assert_eq!(
        host.commands[2],
        RecordedCmd::InsertRelation {
            entity: FfiEntity::from_bits(10),
            relation: weighted_handle,
            target: FfiEntity::from_bits(20),
            payload: 7.5f32.to_ne_bytes().to_vec(),
        }
    );
    assert_eq!(
        host.commands[3],
        RecordedCmd::RemoveRelation {
            entity: FfiEntity::from_bits(20),
            relation: likes_handle,
            target: FfiEntity::from_bits(30),
        }
    );
    host.unload(inst);
}

// ============================================================================
// 5. Symbol Interning & Named Object Binding
// ============================================================================

#[test]
fn symbol_interning_and_named_object_binding() {
    let host = V03TestHost::new();
    let e_hero = FfiEntity::from_bits(501);
    let e_boss = FfiEntity::from_bits(502);

    let inst = host
        .load_fn(c"sym_mod", |app| {
            let hero_a = app.intern_symbol("hero").unwrap();
            let villain = app.intern_symbol("villain").unwrap();
            let hero_b = app.intern_symbol("hero").unwrap();
            assert_eq!(hero_a, hero_b);
            assert_ne!(hero_a, villain);
            assert!(hero_a.is_valid());
            assert_eq!(app.intern_symbol(""), Err(FfiResult::INVALID_ARGUMENT));

            assert_eq!(app.bind_named_object("player_one", e_hero), FfiResult::OK);
            assert_eq!(
                app.bind_named_object("", e_hero),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                app.bind_named_object("null_obj", FfiEntity::NULL),
                FfiResult::INVALID_ENTITY
            );

            app.system(c"sym_sys").build(move |ctx| {
                assert_eq!(ctx.intern_symbol("hero"), Ok(hero_a));
                assert_eq!(ctx.bind_named_object("final_boss", e_boss), FfiResult::OK);
                FfiResult::OK
            })
        })
        .unwrap();

    assert_eq!(host.run_system(0), FfiResult::OK);
    assert_eq!(host.named_objects.get("player_one"), Some(&e_hero));
    assert_eq!(host.named_objects.get("final_boss"), Some(&e_boss));
    host.unload(inst);
}

// ============================================================================
// 6. `iter_count`, `iter_up_cascade`, & `par_for_each_chunk_readonly`
// ============================================================================

#[test]
fn iter_count_up_cascade_and_par_readonly() {
    let mut host = V03TestHost::new();
    let faction_h = host.expose_component::<Faction>("v03::Faction", Faction::UUID, 0, &[]);
    let counter_h = host.expose_component::<Counter>("v03::Counter", Counter::UUID, 0, &[]);
    let rel_h = host.expose_component::<ParentRel>("v03::ParentRel", ParentRel::UUID, 0, &[]);

    let root = FfiEntity::from_bits(1);
    let mid = FfiEntity::from_bits(2);
    let leaf = FfiEntity::from_bits(3);
    let orphan = FfiEntity::from_bits(4);
    let cyc_a = FfiEntity::from_bits(5);
    let cyc_b = FfiEntity::from_bits(6);

    host.relation_edges.insert((rel_h, leaf), vec![mid]);
    host.relation_edges.insert((rel_h, mid), vec![root]);
    // Default `ChildOf` cascade (`BamComponent::INVALID`) also walks `leaf -> mid -> root`
    host.relation_edges
        .insert((BamComponent::INVALID, leaf), vec![mid]);
    host.relation_edges
        .insert((BamComponent::INVALID, mid), vec![root]);
    host.relation_edges.insert((rel_h, cyc_a), vec![cyc_b]);
    host.relation_edges.insert((rel_h, cyc_b), vec![cyc_a]);
    host.set_entity_component(root, faction_h, Faction { id: 77 });

    let mut c1 = MockChunk::new(vec![root, mid]);
    c1.insert_dense(faction_h, &[Faction { id: 77 }, Faction { id: 0 }]);
    c1.insert_dense(counter_h, &[Counter { value: 10 }, Counter { value: 20 }]);
    let mut c2 = MockChunk::new(vec![leaf, orphan, cyc_a]);
    c2.insert_dense(
        faction_h,
        &[Faction { id: 0 }, Faction { id: 0 }, Faction { id: 0 }],
    );
    c2.insert_dense(
        counter_h,
        &[
            Counter { value: 30 },
            Counter { value: 40 },
            Counter { value: 50 },
        ],
    );
    host.chunks.push(c1);
    host.chunks.push(c2);

    let par_sum_raw = std::sync::Arc::new(AtomicU32::new(0));
    let par_sum_typed = std::sync::Arc::new(AtomicU32::new(0));
    let sum_raw_clone = std::sync::Arc::clone(&par_sum_raw);
    let sum_typed_clone = std::sync::Arc::clone(&par_sum_typed);

    let inst = host
        .load_fn(c"iter_v03_mod", |app| {
            // System 0: read-only query testing count, up_cascade, par_for_each_chunk_readonly
            let s0 = app
                .system(c"ro_query")
                .query_typed::<(&Faction, &Counter)>()
                .build(move |ctx| {
                    // Non-destructive count before, during, and after chunk traversal
                    assert_eq!(ctx.iter().count(), Ok(5));
                    {
                        let mut typed = ctx.single_query::<(&Faction, &Counter)>();
                        assert_eq!(typed.count(), Ok(5));
                        let (f0, c0) = typed.next_chunk().unwrap();
                        assert_eq!(f0.len(), 2);
                        assert_eq!(c0[1].value, 20);
                        // Cursor unaffected by count()!
                        assert_eq!(typed.count(), Ok(5));
                        let (f1, c1) = typed.next_chunk().unwrap();
                        assert_eq!(f1.len(), 3);
                        assert_eq!(c1[2].value, 50);
                        assert!(typed.next_chunk().is_none());
                        assert_eq!(typed.count(), Ok(5));

                        // Typed up_cascade via Rel type & pre-resolved handle
                        let hit = typed
                            .up_cascade::<ParentRel, Faction>(leaf, 0)
                            .unwrap()
                            .unwrap();
                        assert_eq!(hit.0, root);
                        assert_eq!(hit.1.id, 77);

                        let hit2 = typed
                            .up_cascade_handle::<Faction>(rel_h, leaf, 0)
                            .unwrap()
                            .unwrap();
                        assert_eq!(hit2.0, root);
                        assert_eq!(hit2.1.id, 77);

                        // Missing ancestor returns Ok(None)
                        assert_eq!(typed.up_cascade::<ParentRel, Faction>(orphan, 0), Ok(None));
                        // Ancestor cycle returns CYCLE_DETECTED
                        assert_eq!(
                            typed.up_cascade::<ParentRel, Faction>(cyc_a, 0),
                            Err(FfiResult::CYCLE_DETECTED)
                        );
                        // Wrong type at term 0 (asking for Counter instead of Faction) -> TYPE_MISMATCH
                        assert_eq!(
                            typed.up_cascade::<ParentRel, Counter>(leaf, 0),
                            Err(FfiResult::TYPE_MISMATCH)
                        );
                    }

                    // Untyped ChunkIter::up_cascade_pod and default-ChildOf (`BamComponent::INVALID`)
                    let pod_hit = ctx
                        .iter()
                        .up_cascade_pod::<Faction>(rel_h, leaf, 0)
                        .unwrap()
                        .unwrap();
                    assert_eq!(pod_hit.0, root);
                    assert_eq!(pod_hit.1.id, 77);

                    let default_child_of_hit = ctx
                        .iter()
                        .up_cascade::<Faction>(BamComponent::INVALID, leaf, 0)
                        .unwrap()
                        .unwrap();
                    assert_eq!(default_child_of_hit.0, root);
                    assert_eq!(default_child_of_hit.1.id, 77);

                    // SystemContext::par_chunks_readonly (&Chunk cannot borrow mutably)
                    let s_raw = &sum_raw_clone;
                    assert_eq!(
                        ctx.par_chunks_readonly(|chunk| {
                            let counters = chunk.component::<Counter>(1).unwrap();
                            for c in counters {
                                s_raw.fetch_add(c.value, Ordering::Relaxed);
                            }
                        }),
                        FfiResult::OK
                    );

                    // Fallible par_try_chunks_readonly propagates error
                    assert_eq!(
                        ctx.par_try_chunks_readonly(|_| FfiResult::OUT_OF_RANGE),
                        FfiResult::OUT_OF_RANGE
                    );

                    // TypedChunkIter::par_for_each_chunk_readonly
                    let s_typed = &sum_typed_clone;
                    assert_eq!(
                        ctx.single_query::<(&Faction, &Counter)>()
                            .par_for_each_chunk_readonly(|(_factions, counters)| {
                                for c in counters {
                                    s_typed.fetch_add(c.value, Ordering::Relaxed);
                                }
                            }),
                        FfiResult::OK
                    );
                    FfiResult::OK
                });
            assert_eq!(s0, FfiResult::OK);

            // System 1: panic inside par_for_each_chunk_readonly caught as PANIC_CAUGHT
            let s1 = app
                .system(c"ro_par_panic")
                .query_typed::<&Counter>()
                .build(|ctx| {
                    let res = ctx.par_chunks_readonly(|_chunk| {
                        panic!("simulated par readonly panic");
                    });
                    assert_eq!(res, FfiResult::PANIC_CAUGHT);
                    FfiResult::OK
                });
            assert_eq!(s1, FfiResult::OK);

            // System 2: writable query rejected by both TypedChunkIter and host
            app.system(c"rw_rejected")
                .query_typed::<(&mut Counter, &Faction)>()
                .build(move |ctx| {
                    // TypedChunkIter rejects `!Q::IS_READ_ONLY` before FFI
                    assert_eq!(
                        ctx.single_query::<(&mut Counter, &Faction)>()
                            .par_for_each_chunk_readonly(|_| FfiResult::OK),
                        FfiResult::PERMISSION_DENIED
                    );
                    // Host also rejects writable query via ChunkIter::par_for_each_chunk_readonly
                    assert_eq!(
                        ctx.par_chunks_readonly(|_| {}),
                        FfiResult::PERMISSION_DENIED
                    );
                    // Host rejects up_cascade targeting writable term 0
                    assert_eq!(
                        ctx.iter().up_cascade::<Counter>(rel_h, leaf, 0),
                        Err(FfiResult::PERMISSION_DENIED)
                    );
                    FfiResult::OK
                })
        })
        .unwrap();

    assert_eq!(host.run_system(0), FfiResult::OK);
    assert_eq!(par_sum_raw.load(Ordering::Relaxed), 150);
    assert_eq!(par_sum_typed.load(Ordering::Relaxed), 150);

    // Also test host convention returning OK + null owner/ptr for missing up_cascade
    host.up_cascade_missing_as_ok_null = true;
    assert_eq!(host.run_system(0), FfiResult::OK);

    assert_eq!(host.run_system(1), FfiResult::OK);
    assert_eq!(host.run_system(2), FfiResult::OK);
    host.unload(inst);
}

// ============================================================================
// 7. Decoupled Scheduling (`register_handle`, `ScheduleSetBuilder`, `mount_system`)
// ============================================================================

#[test]
fn decoupled_scheduling_and_schedule_sets() {
    let mut host = V03TestHost::new();
    let _c = host.expose_component::<Counter>("v03::Counter", Counter::UUID, 0, &[]);
    let ran_count = std::sync::Arc::new(AtomicUsize::new(0));
    let ran_a = std::sync::Arc::clone(&ran_count);
    let ran_b = std::sync::Arc::clone(&ran_count);

    let inst = host
        .load_fn(c"sched_mod", |app| {
            let sys_a = app
                .system(c"physics_step")
                .query_typed::<&Counter>()
                .register_handle(move |_ctx| {
                    ran_a.fetch_add(1, Ordering::Relaxed);
                    FfiResult::OK
                })
                .unwrap();
            let sys_b = app
                .system(c"render_sync")
                .register_handle(move |_ctx| {
                    ran_b.fetch_add(10, Ordering::Relaxed);
                    FfiResult::OK
                })
                .unwrap();
            assert_eq!(sys_a, BamSystem::from_raw(0));
            assert_eq!(sys_b, BamSystem::from_raw(1));

            let set_pre = app
                .configure_schedule_set(
                    ScheduleSetBuilder::new("pre_physics")
                        .schedule(ScheduleId::FIXED_UPDATE)
                        .parallel(),
                )
                .unwrap();
            let set_post = app
                .configure_schedule_set(
                    ScheduleSetBuilder::new("post_physics")
                        .schedule(ScheduleId::FIXED_UPDATE)
                        .chain_weak(),
                )
                .unwrap();

            // Sequence overwrites parallel cleanly; weak/strong before & after flags preserved
            let set_main_builder = ScheduleSetBuilder::new("physics_core")
                .schedule(ScheduleId::FIXED_UPDATE)
                .parallel()
                .sequence()
                .after(set_pre)
                .after_weak(set_pre)
                .before(set_post)
                .before_weak(set_post);
            assert_eq!(
                set_main_builder.raw_flags(),
                BAM_SCHEDULE_SET_FLAG_SEQUENCE
                    | BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK
                    | BAM_SCHEDULE_SET_FLAG_AFTER_WEAK
            );
            let set_main = app.configure_schedule_set(set_main_builder).unwrap();

            // Mutually exclusive raw exec-mode flags rejected by `BamScheduleSetDesc::validate()`
            assert_eq!(
                app.configure_schedule_set(
                    ScheduleSetBuilder::new("invalid_exec")
                        .flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE | BAM_SCHEDULE_SET_FLAG_PARALLEL)
                ),
                Err(FfiResult::INVALID_ARGUMENT)
            );
            assert_eq!(
                app.configure_schedule_set(ScheduleSetBuilder::new("")),
                Err(FfiResult::INVALID_ARGUMENT)
            );

            // Mount into set and ungrouped (`BamSystemSet::INVALID`)
            assert_eq!(
                app.mount_system(sys_a, ScheduleId::FIXED_UPDATE, set_main),
                FfiResult::OK
            );
            assert_eq!(
                app.mount_system(sys_b, ScheduleId::UPDATE, BamSystemSet::INVALID),
                FfiResult::OK
            );
            assert_eq!(
                app.mount_system(
                    BamSystem::INVALID,
                    ScheduleId::UPDATE,
                    BamSystemSet::INVALID
                ),
                FfiResult::INVALID_ARGUMENT
            );
            FfiResult::OK
        })
        .unwrap();

    assert_eq!(host.schedule_sets.len(), 3);
    assert_eq!(host.schedule_sets[0].flags, BAM_SCHEDULE_SET_FLAG_PARALLEL);
    assert_eq!(
        host.schedule_sets[1].flags,
        BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK
    );
    assert_eq!(
        host.schedule_sets[2].before_sets,
        vec![BamSystemSet::from_raw(1), BamSystemSet::from_raw(1)]
    );
    assert_eq!(
        host.schedule_sets[2].after_sets,
        vec![BamSystemSet::from_raw(0), BamSystemSet::from_raw(0)]
    );
    assert_eq!(
        host.mounted_systems,
        vec![
            (
                BamSystem::from_raw(0),
                ScheduleId::FIXED_UPDATE,
                BamSystemSet::from_raw(2)
            ),
            (
                BamSystem::from_raw(1),
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
        ]
    );

    assert_eq!(host.run_system(0), FfiResult::OK);
    assert_eq!(host.run_system(1), FfiResult::OK);
    assert_eq!(ran_count.load(Ordering::Relaxed), 11);
    host.unload(inst);
}

// ============================================================================
// 8. Command Transactions (`GuestTransaction<'a>` & `GuestCommands::transaction`)
// ============================================================================

#[test]
fn command_transactions_commit_rollback_drop_and_nested() {
    let host = V03TestHost::new();
    let mut rel_h = BamComponent::INVALID;

    let inst = host
        .load_fn(c"tx_mod", |app| {
            rel_h = app
                .register_relation(RelationDescBuilder::new("tx_edge").acyclic())
                .unwrap();
            let rel = rel_h;

            // System 0: commit, rollback-on-Err, rollback-on-Drop, explicit rollback, nested tx
            let s0 = app.system(c"tx_lifecycle").build(move |ctx| {
                let cmds = ctx.commands();
                // Pre-transaction command (index 0)
                let base = cmds.spawn_empty().unwrap();

                // 1. Committed transaction via closure
                let committed = cmds
                    .transaction(|tx| -> Result<FfiEntity, FfiResult> {
                        let e = tx.spawn_empty()?;
                        let st = tx.insert_relation(base, rel, e);
                        if !st.is_ok() {
                            return Err(st);
                        }
                        Ok(e)
                    })
                    .unwrap();

                // 2. Rolled-back transaction via Err(...)
                let err_res: Result<(), FfiResult> = cmds.transaction(|tx| {
                    let temp = tx.spawn_empty()?;
                    let _ = tx.despawn(temp);
                    Err(FfiResult::CYCLE_DETECTED)
                });
                assert_eq!(err_res, Err(FfiResult::CYCLE_DETECTED));

                // 3. Uncommitted Drop rolls back automatically
                {
                    let mut tx = cmds.begin_transaction().unwrap();
                    let _ = tx.spawn_empty().unwrap();
                    let _ = tx.remove_relation(base, rel, committed);
                    // dropped without commit()
                }

                // 4. Explicit rollback does NOT double-rollback on drop
                {
                    let mut tx = cmds.begin_transaction().unwrap();
                    let _ = tx.spawn_empty().unwrap();
                    assert_eq!(tx.rollback(), FfiResult::OK);
                }

                // 5. Explicit commit keeps commands and does NOT roll back on drop
                {
                    let mut tx = cmds.begin_transaction().unwrap();
                    let _ = tx.despawn(committed);
                    tx.commit();
                }

                // 6. Nested transactions: outer commits while inner rolls back
                cmds.transaction(|outer| -> Result<(), FfiResult> {
                    let outer_e = outer.spawn_empty()?;
                    let inner_res: Result<(), FfiResult> = outer.transaction(|inner| {
                        let _ = inner.spawn_empty()?;
                        Err(FfiResult::PERMISSION_DENIED)
                    });
                    assert_eq!(inner_res, Err(FfiResult::PERMISSION_DENIED));
                    let _ = outer.despawn(outer_e);
                    Ok(())
                })
                .unwrap();

                FfiResult::OK
            });
            assert_eq!(s0, FfiResult::OK);

            // System 1: panic inside transaction rolls back on unwind before PANIC_CAUGHT
            app.system(c"tx_panic_rollback").build(|ctx| {
                let cmds = ctx.commands();
                let _ = cmds.transaction(|tx| -> Result<(), FfiResult> {
                    let _ = tx.spawn_empty()?;
                    let _ = tx.spawn_empty()?;
                    panic!("abort inside transaction");
                });
                FfiResult::OK
            })
        })
        .unwrap();

    assert_eq!(host.run_system(0), FfiResult::OK);
    // Expected surviving commands:
    // 0: base SpawnEmpty
    // 1: committed SpawnEmpty
    // 2: InsertRelation(base -> committed)
    // 3: Despawn(committed)
    // 4: outer_e SpawnEmpty
    // 5: Despawn(outer_e)
    assert_eq!(host.commands.len(), 6);
    assert!(matches!(host.commands[0], RecordedCmd::SpawnEmpty(_)));
    assert!(matches!(host.commands[1], RecordedCmd::SpawnEmpty(_)));
    assert!(matches!(
        host.commands[2],
        RecordedCmd::InsertRelation { .. }
    ));
    assert!(matches!(host.commands[3], RecordedCmd::Despawn(_)));
    assert!(matches!(host.commands[4], RecordedCmd::SpawnEmpty(_)));
    assert!(matches!(host.commands[5], RecordedCmd::Despawn(_)));
    // Exactly 4 rollbacks: Err tx, dropped tx, explicit rollback (no double rollback), nested inner tx
    assert_eq!(host.rollback_calls, 4);

    // Run panicking transaction: rollback runs during guest unwind, caught as PANIC_CAUGHT!
    assert_eq!(host.run_system(1), FfiResult::PANIC_CAUGHT);
    assert_eq!(host.commands.len(), 6);
    assert_eq!(host.rollback_calls, 5);
    host.unload(inst);
}

// ============================================================================
// 9. Live Component Column Migration (`migrate_component_column`)
// ============================================================================

#[test]
fn component_column_migration_success_error_panic_and_zst() {
    let mut host = V03TestHost::new();
    let stats_h = host.expose_component::<OldStats>(
        OldStats::CANONICAL_NAME,
        OldStats::UUID,
        OldStats::SCHEMA_FINGERPRINT,
        OldStats::BAM_FIELDS,
    );
    let tag_h = host.expose_component::<OldTag>(
        OldTag::CANONICAL_NAME,
        OldTag::UUID,
        OldTag::SCHEMA_FINGERPRINT,
        &[],
    );
    let mut chunk = MockChunk::new(vec![
        FfiEntity::from_bits(1),
        FfiEntity::from_bits(2),
        FfiEntity::from_bits(3),
    ]);
    chunk.insert_dense(
        stats_h,
        &[
            OldStats { hp: 10 },
            OldStats { hp: 25 },
            OldStats { hp: 40 },
        ],
    );
    chunk.insert_dense(tag_h, &[OldTag, OldTag, OldTag]);
    host.chunks.push(chunk);

    let inst = host
        .load_fn(c"migrate_mod", |app| {
            // 1. Mismatched old fingerprint rejected before mutating rows
            assert_eq!(
                app.migrate_component_column::<BadFingerprintStats, NewStats, _>(stats_h, |o| {
                    Ok(NewStats {
                        hp: o.hp as f64,
                        armor: 0,
                        pad: 0,
                    })
                }),
                FfiResult::TYPE_MISMATCH
            );

            // 2. Closure returning Err aborts migration without mutating rows
            assert_eq!(
                app.migrate_component_column::<OldStats, NewStats, _>(stats_h, |o| {
                    if o.hp == 25 {
                        return Err(FfiResult::INVALID_ARGUMENT);
                    }
                    Ok(NewStats {
                        hp: o.hp as f64,
                        armor: 1,
                        pad: 0,
                    })
                }),
                FfiResult::INVALID_ARGUMENT
            );

            // 3. Panic inside migration closure is caught cleanly as PANIC_CAUGHT
            assert_eq!(
                app.migrate_component_column::<OldStats, NewStats, _>(stats_h, |o| {
                    if o.hp == 25 {
                        panic!("row migration panic");
                    }
                    Ok(NewStats {
                        hp: o.hp as f64,
                        armor: 1,
                        pad: 0,
                    })
                }),
                FfiResult::PANIC_CAUGHT
            );

            // 4. Successful migration OldStats (4B, align 4) -> NewStats (16B, align 8)
            assert_eq!(
                app.migrate_component_column::<OldStats, NewStats, _>(stats_h, |old| {
                    Ok(NewStats {
                        hp: (old.hp as f64) * 2.5,
                        armor: 50,
                        pad: 0,
                    })
                }),
                FfiResult::OK
            );

            // Verify updated host layout & schema fingerprint
            let new_info = app
                .verify_component_layout_handle::<NewStats>(stats_h)
                .unwrap();
            assert_eq!(new_info.schema_fingerprint, NewStats::SCHEMA_FINGERPRINT);
            assert_eq!(new_info.size, core::mem::size_of::<NewStats>());
            assert_eq!(new_info.field_count, 2);

            // 5. ZST -> ZST migration under Miri
            assert_eq!(
                app.migrate_component_column::<OldTag, NewTag, _>(tag_h, |_| Ok(NewTag)),
                FfiResult::OK
            );
            // 6. ZST -> non-ZST POD migration via `migrate_pod_component_column`
            assert_eq!(
                app.migrate_pod_component_column::<NewTag, Counter, _>(
                    tag_h,
                    NewTag::SCHEMA_FINGERPRINT,
                    0x9999,
                    |_| Ok(Counter { value: 123 })
                ),
                FfiResult::OK
            );
            FfiResult::OK
        })
        .unwrap();

    let migrated_stats = host.chunks[0].read_dense::<NewStats>(stats_h);
    assert_eq!(
        migrated_stats,
        vec![
            NewStats {
                hp: 25.0,
                armor: 50,
                pad: 0
            },
            NewStats {
                hp: 62.5,
                armor: 50,
                pad: 0
            },
            NewStats {
                hp: 100.0,
                armor: 50,
                pad: 0
            },
        ]
    );
    let migrated_counters = host.chunks[0].read_dense::<Counter>(tag_h);
    assert_eq!(
        migrated_counters,
        vec![
            Counter { value: 123 },
            Counter { value: 123 },
            Counter { value: 123 },
        ]
    );
    host.unload(inst);
}

// ============================================================================
// 10. 16-Term `QueryTuple` & 2..=8 Branch `Or<(T0..T7)>` + `Or2`/`Or3`
// ============================================================================

#[test]
fn sixteen_term_query_tuple_and_eight_branch_or() {
    fn assert_tuple<Q: QueryTuple>() {}

    // Compile-time coverage for every QueryTuple arity 1..=16 and every Or arity 2..=8
    assert_tuple::<(&C0,)>();
    assert_tuple::<(&C0, &C1)>();
    assert_tuple::<(&C0, &C1, &C2)>();
    assert_tuple::<(&C0, &C1, &C2, &C3)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6, &C7)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6, &C7, &C8)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6, &C7, &C8, &C9)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6, &C7, &C8, &C9, &C10)>();
    assert_tuple::<(&C0, &C1, &C2, &C3, &C4, &C5, &C6, &C7, &C8, &C9, &C10, &C11)>();
    assert_tuple::<(
        &C0,
        &C1,
        &C2,
        &C3,
        &C4,
        &C5,
        &C6,
        &C7,
        &C8,
        &C9,
        &C10,
        &C11,
        &C12,
    )>();
    assert_tuple::<(
        &C0,
        &C1,
        &C2,
        &C3,
        &C4,
        &C5,
        &C6,
        &C7,
        &C8,
        &C9,
        &C10,
        &C11,
        &C12,
        &C13,
    )>();
    assert_tuple::<(
        &C0,
        &C1,
        &C2,
        &C3,
        &C4,
        &C5,
        &C6,
        &C7,
        &C8,
        &C9,
        &C10,
        &C11,
        &C12,
        &C13,
        &C14,
    )>();
    assert_tuple::<(
        &C0,
        &mut C1,
        &C2,
        &C3,
        &C4,
        &C5,
        &C6,
        &C7,
        &C8,
        &C9,
        &C10,
        &C11,
        &C12,
        &C13,
        &C14,
        &C15,
    )>();

    assert_tuple::<Or2<With<C0>, Added<C1>>>();
    assert_tuple::<Or3<With<C0>, Without<C1>, Changed<C2>>>();
    assert_tuple::<Or<(With<C0>, Without<C1>)>>();
    assert_tuple::<Or<(With<C0>, Without<C1>, Changed<C2>)>>();
    assert_tuple::<Or<(With<C0>, Without<C1>, Changed<C2>, Added<C3>)>>();
    assert_tuple::<Or<(With<C0>, Without<C1>, Changed<C2>, Added<C3>, With<C4>)>>();
    assert_tuple::<
        Or<(
            With<C0>,
            Without<C1>,
            Changed<C2>,
            Added<C3>,
            With<C4>,
            Without<C5>,
        )>,
    >();
    assert_tuple::<
        Or<(
            With<C0>,
            Without<C1>,
            Changed<C2>,
            Added<C3>,
            With<C4>,
            Without<C5>,
            Changed<C6>,
        )>,
    >();
    assert_tuple::<
        Or<(
            With<C0>,
            Without<C1>,
            Changed<C2>,
            Added<C3>,
            With<C4>,
            Without<C5>,
            Changed<C6>,
            Added<C7>,
        )>,
    >();

    // Live runtime execution of a 16-term QueryTuple + 8-branch Or + 2nd Or group
    let mut host = V03TestHost::new();
    let handles = [
        host.expose_component::<C0>("q16::C0", C0::UUID, 0, &[]),
        host.expose_component::<C1>("q16::C1", C1::UUID, 0, &[]),
        host.expose_component::<C2>("q16::C2", C2::UUID, 0, &[]),
        host.expose_component::<C3>("q16::C3", C3::UUID, 0, &[]),
        host.expose_component::<C4>("q16::C4", C4::UUID, 0, &[]),
        host.expose_component::<C5>("q16::C5", C5::UUID, 0, &[]),
        host.expose_component::<C6>("q16::C6", C6::UUID, 0, &[]),
        host.expose_component::<C7>("q16::C7", C7::UUID, 0, &[]),
        host.expose_component::<C8>("q16::C8", C8::UUID, 0, &[]),
        host.expose_component::<C9>("q16::C9", C9::UUID, 0, &[]),
        host.expose_component::<C10>("q16::C10", C10::UUID, 0, &[]),
        host.expose_component::<C11>("q16::C11", C11::UUID, 0, &[]),
        host.expose_component::<C12>("q16::C12", C12::UUID, 0, &[]),
        host.expose_component::<C13>("q16::C13", C13::UUID, 0, &[]),
        host.expose_component::<C14>("q16::C14", C14::UUID, 0, &[]),
        host.expose_component::<C15>("q16::C15", C15::UUID, 0, &[]),
    ];

    let mut chunk = MockChunk::new(vec![FfiEntity::from_bits(1), FfiEntity::from_bits(2)]);
    chunk.insert_dense(handles[0], &[C0 { v: 1 }, C0 { v: 10 }]);
    chunk.insert_dense(handles[1], &[C1 { v: 0 }, C1 { v: 0 }]);
    chunk.insert_dense(handles[2], &[C2 { v: 2 }, C2 { v: 20 }]);
    chunk.insert_dense(handles[3], &[C3 { v: 3 }, C3 { v: 30 }]);
    chunk.insert_dense(handles[4], &[C4 { v: 4 }, C4 { v: 40 }]);
    chunk.insert_dense(handles[5], &[C5 { v: 5 }, C5 { v: 50 }]);
    chunk.insert_dense(handles[6], &[C6 { v: 6 }, C6 { v: 60 }]);
    chunk.insert_dense(handles[7], &[C7 { v: 7 }, C7 { v: 70 }]);
    chunk.insert_dense(handles[8], &[C8 { v: 8 }, C8 { v: 80 }]);
    chunk.insert_dense(handles[9], &[C9 { v: 9 }, C9 { v: 90 }]);
    chunk.insert_dense(handles[10], &[C10 { v: 10 }, C10 { v: 100 }]);
    chunk.insert_dense(handles[11], &[C11 { v: 11 }, C11 { v: 110 }]);
    chunk.insert_dense(handles[12], &[C12 { v: 12 }, C12 { v: 120 }]);
    chunk.insert_dense(handles[13], &[C13 { v: 13 }, C13 { v: 130 }]);
    chunk.insert_dense(handles[14], &[C14 { v: 14 }, C14 { v: 140 }]);
    chunk.insert_dense(handles[15], &[C15 { v: 15 }, C15 { v: 150 }]);
    host.chunks.push(chunk);

    type Q16<'q> = (
        &'q C0,
        &'q mut C1,
        &'q C2,
        &'q C3,
        &'q C4,
        &'q C5,
        &'q C6,
        &'q C7,
        &'q C8,
        &'q C9,
        &'q C10,
        &'q C11,
        &'q C12,
        &'q C13,
        &'q C14,
        &'q C15,
    );
    type Or8AndOr2<'q> = (
        &'q C0,
        Or<(
            With<C1>,
            Without<C2>,
            Changed<C3>,
            Added<C4>,
            With<C5>,
            Without<C6>,
            Changed<C7>,
            Added<C8>,
        )>,
        Or2<With<C9>, Added<C10>>,
        Or3<With<C11>, Without<C12>, Changed<C13>>,
    );

    let inst = host
        .load_fn(c"q16_mod", |app| {
            let s0 = app
                .system(c"sum_16_terms")
                .query_typed::<Q16<'_>>()
                .build(|ctx| {
                    let mut typed = ctx.single_query::<Q16<'_>>();
                    typed.for_each(
                        |(c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15)| {
                            for i in 0..c0.len() {
                                c1[i].v = c0[i].v
                                    + c2[i].v
                                    + c3[i].v
                                    + c4[i].v
                                    + c5[i].v
                                    + c6[i].v
                                    + c7[i].v
                                    + c8[i].v
                                    + c9[i].v
                                    + c10[i].v
                                    + c11[i].v
                                    + c12[i].v
                                    + c13[i].v
                                    + c14[i].v
                                    + c15[i].v;
                            }
                        },
                    )
                });
            assert_eq!(s0, FfiResult::OK);

            app.system(c"or_groups_8_2_3")
                .query_typed::<Or8AndOr2<'_>>()
                .build(|ctx| {
                    let mut typed = ctx.single_query::<Or8AndOr2<'_>>();
                    let mut seen = 0usize;
                    let status = typed.for_each(|(c0, (), (), ())| {
                        seen += c0.len();
                    });
                    assert_eq!(status, FfiResult::OK);
                    assert_eq!(seen, 2);
                    FfiResult::OK
                })
        })
        .unwrap();

    // Verify registered descriptor counts and distinct `or_group` IDs (0, 1*8, 2*2, 3*3)
    assert_eq!(host.systems[0].queries[0].len(), 16);
    let or_terms = &host.systems[1].queries[0];
    assert_eq!(or_terms.len(), 1 + 8 + 2 + 3);
    assert_eq!(or_terms[0].or_group, 0);
    for t in &or_terms[1..9] {
        assert_eq!(t.or_group, 1);
    }
    for t in &or_terms[9..11] {
        assert_eq!(t.or_group, 2);
    }
    for t in &or_terms[11..14] {
        assert_eq!(t.or_group, 3);
    }

    assert_eq!(host.run_system(0), FfiResult::OK);
    // Row 0: 1 + 2 + 3 + ... + 15 = 120; Row 1: 10 + 20 + ... + 150 = 1200
    assert_eq!(
        host.chunks[0].read_dense::<C1>(handles[1]),
        vec![C1 { v: 120 }, C1 { v: 1200 }]
    );
    assert_eq!(host.run_system(1), FfiResult::OK);
    host.unload(inst);
}
