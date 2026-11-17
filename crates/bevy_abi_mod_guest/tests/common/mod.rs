//! Shared ABI v0.2 mock host world for `bevy_abi_mod_guest` integration, regression, and Miri tests.

#![allow(
    dead_code,
    unreachable_pub,
    unsafe_code,
    clippy::undocumented_unsafe_blocks,
    clippy::vec_box
)]

use core::{
    ffi::{CStr, c_char, c_void},
    mem::MaybeUninit,
};
use std::collections::HashMap;

use bevy_abi_mod_guest::raw::{
    ABI_VERSION, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BamChunkTaskFn, BamColumnInfo, BamCommands, BamComponent,
    BamHostApi, BamHostCtx, BamInstance, BamIter, BamLoadCtx, BamManifest, BamRunIfFn,
    BamSystemCtx, BamSystemDesc, BamSystemFn, BamTermDesc, ComponentDesc, ComponentDropFn,
    ComponentHookFn, ComponentUuid, FfiBool, FfiEntity, FfiResult, ScheduleId, init_guest_instance,
    shutdown_guest_instance,
};
use bevy_abi_mod_guest::{GuestApp, GuestPlugin, build_guest_fn, build_guest_plugin};

#[derive(Clone)]
pub struct RegisteredComp {
    pub handle: BamComponent,
    pub name: String,
    pub uuid: ComponentUuid,
    pub size: usize,
    pub align: usize,
    pub flags: u32,
    pub has_default: bool,
    pub drop_fn: Option<ComponentDropFn>,
    pub vtable_ptr: *const c_void,
    pub on_add: Option<ComponentHookFn>,
    pub on_remove: Option<ComponentHookFn>,
    pub hook_user_data: *mut c_void,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedCommand {
    SpawnEmpty(FfiEntity),
    SpawnBatch {
        entity: FfiEntity,
        components: Vec<(BamComponent, usize)>,
    },
    Despawn(FfiEntity),
    Insert {
        entity: FfiEntity,
        component: BamComponent,
        byte_len: usize,
    },
    InsertDefault {
        entity: FfiEntity,
        component: BamComponent,
    },
    Remove {
        entity: FfiEntity,
        component: BamComponent,
    },
}

pub struct RegisteredSystem {
    pub name: String,
    pub queries: Vec<Vec<BamTermDesc>>,
    pub schedule: ScheduleId,
    pub flags: u32,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub callback: BamSystemFn,
    pub user_data: *mut c_void,
    pub run_if: Option<BamRunIfFn>,
    pub run_if_user_data: *mut c_void,
}

pub struct MockChunkData {
    pub entities: Vec<FfiEntity>,
    /// Dense columns keyed by `BamComponent` -> aligned raw byte buffer (`len * comp.size`).
    pub dense_columns: HashMap<BamComponent, Vec<u64>>,
    /// Per-column tracked changed flags: `true` if whole chunk marked changed via `iter_column_mutable`.
    pub chunk_marked_changed: HashMap<BamComponent, bool>,
    /// Per-column fine-grained row indices marked changed via `iter_mark_changed_range`.
    pub marked_rows: HashMap<BamComponent, Vec<usize>>,
    /// Indirect sparse-set row allocations keyed by `BamComponent` -> `Vec<Option<Vec<u64>>>`.
    pub sparse_columns: HashMap<BamComponent, Vec<Option<Vec<u64>>>>,
}

impl MockChunkData {
    pub fn new(entities: Vec<FfiEntity>) -> Self {
        Self {
            entities,
            dense_columns: HashMap::new(),
            chunk_marked_changed: HashMap::new(),
            marked_rows: HashMap::new(),
            sparse_columns: HashMap::new(),
        }
    }

    pub fn insert_dense<T: Copy>(&mut self, comp: BamComponent, values: &[T]) {
        assert_eq!(values.len(), self.entities.len());
        let byte_len = core::mem::size_of_val(values);
        let word_len = byte_len.div_ceil(8).max(1);
        let mut words = vec![0u64; word_len];
        if byte_len > 0 {
            // SAFETY: `words` has `word_len * 8 >= byte_len` writable bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    values.as_ptr().cast::<u8>(),
                    words.as_mut_ptr().cast::<u8>(),
                    byte_len,
                );
            }
        }
        self.dense_columns.insert(comp, words);
    }

    pub fn read_dense<T: Copy>(&self, comp: BamComponent) -> Vec<T> {
        let count = self.entities.len();
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: Copy`.
            return (0..count).map(|_| unsafe { core::mem::zeroed() }).collect();
        }
        let words = self.dense_columns.get(&comp).expect("missing dense column");
        let mut out = Vec::<T>::with_capacity(count);
        // SAFETY: `words` was populated with `count` `T` elements.
        unsafe {
            core::ptr::copy_nonoverlapping(words.as_ptr().cast::<T>(), out.as_mut_ptr(), count);
            out.set_len(count);
        }
        out
    }

    pub fn insert_sparse<T: Copy>(&mut self, comp: BamComponent, rows: Vec<Option<T>>) {
        assert_eq!(rows.len(), self.entities.len());
        let elem_size = core::mem::size_of::<T>();
        let word_len = elem_size.div_ceil(8).max(1);
        let boxed_rows = rows
            .into_iter()
            .map(|opt| {
                opt.map(|val| {
                    let mut words = vec![0u64; word_len];
                    if elem_size > 0 {
                        // SAFETY: `words` has `>= elem_size` writable bytes.
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (&raw const val).cast::<u8>(),
                                words.as_mut_ptr().cast::<u8>(),
                                elem_size,
                            );
                        }
                    }
                    words
                })
            })
            .collect();
        self.sparse_columns.insert(comp, boxed_rows);
    }

    pub fn read_sparse<T: Copy>(&self, comp: BamComponent) -> Vec<Option<T>> {
        let rows = self
            .sparse_columns
            .get(&comp)
            .expect("missing sparse column");
        rows.iter()
            .map(|opt| {
                opt.as_ref().map(|words| {
                    // SAFETY: `words` holds an initialized `T`.
                    unsafe { core::ptr::read(words.as_ptr().cast::<T>()) }
                })
            })
            .collect()
    }
}

pub struct MockIterState {
    pub world: *mut MockHostWorld,
    pub sys_idx: usize,
    pub query_idx: usize,
    pub terms: Vec<BamTermDesc>,
    pub next_chunk_idx: usize,
    pub end_chunk_idx: Option<usize>,
    pub active_chunk_idx: Option<usize>,
    pub in_par_iter: bool,
    pub ro_rows_scratch: Vec<*const u8>,
    pub mut_rows_scratch: Vec<*mut u8>,
}

pub struct MockSystemFrame {
    pub world: *mut MockHostWorld,
    pub sys_idx: usize,
    pub iters: Vec<Box<MockIterState>>,
}

pub struct MockHostWorld {
    pub api: BamHostApi,
    pub components: Vec<RegisteredComp>,
    pub name_to_handle: HashMap<String, BamComponent>,
    pub uuid_to_handle: HashMap<ComponentUuid, BamComponent>,
    pub resources: HashMap<BamComponent, Vec<u64>>,
    pub messages: HashMap<BamComponent, Vec<Vec<MaybeUninit<u8>>>>,
    pub msg_cursors: HashMap<(usize, usize, usize), usize>,
    pub chunks: Vec<MockChunkData>,
    pub systems: Vec<RegisteredSystem>,
    pub commands: Vec<RecordedCommand>,
    pub last_inserted_bytes: Vec<MaybeUninit<u8>>,
    pub next_entity_bits: u64,
    pub last_error: String,
    pub symbols: HashMap<String, *const c_void>,
}

pub struct TestHost {
    raw: *mut MockHostWorld,
}

impl Drop for TestHost {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { drop(Box::from_raw(self.raw)) };
        }
    }
}

impl core::ops::Deref for TestHost {
    type Target = MockHostWorld;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.raw }
    }
}

impl core::ops::DerefMut for TestHost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.raw }
    }
}

impl TestHost {
    pub const fn raw_ptr(&self) -> *mut MockHostWorld {
        self.raw
    }

    pub const fn api_ptr(&self) -> *const BamHostApi {
        unsafe { &raw const (*self.raw).api }
    }

    pub fn load_plugin<P: GuestPlugin>(&self) -> Result<*mut BamInstance, FfiResult> {
        let manifest = P::MANIFEST;
        let api_ptr = self.api_ptr();
        let load_ctx = self.raw.cast::<BamLoadCtx>();
        let mut instance_out: *mut BamInstance = core::ptr::null_mut();
        let status = unsafe {
            init_guest_instance(
                api_ptr,
                load_ctx,
                &mut instance_out,
                &manifest,
                build_guest_plugin::<P>,
            )
        };
        if status.is_ok() {
            Ok(instance_out)
        } else {
            Err(status)
        }
    }

    pub fn load_fn<F>(&self, mod_name: &CStr, build_fn: F) -> Result<*mut BamInstance, FfiResult>
    where
        F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
    {
        let manifest = BamManifest::new(mod_name.as_ptr(), c"0.2.0".as_ptr());
        let api_ptr = self.api_ptr();
        let load_ctx = self.raw.cast::<BamLoadCtx>();
        let mut instance_out: *mut BamInstance = core::ptr::null_mut();
        let status = unsafe {
            init_guest_instance(
                api_ptr,
                load_ctx,
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

    pub fn unload_instance(&self, instance: *mut BamInstance) {
        unsafe { shutdown_guest_instance(instance) };
    }

    pub fn run_system(&self, sys_idx: usize) -> FfiResult {
        let (run_if, run_if_data) = unsafe {
            let world_ref = &*self.raw;
            let sys = &world_ref.systems[sys_idx];
            (sys.run_if, sys.run_if_user_data)
        };
        if let Some(pred) = run_if {
            let should_run = unsafe { pred(run_if_data) };
            if !should_run.get() {
                return FfiResult::OK;
            }
        }
        let (callback, user_data, queries) = unsafe {
            let world_ref = &*self.raw;
            let sys = &world_ref.systems[sys_idx];
            (sys.callback, sys.user_data, sys.queries.clone())
        };
        let world_ptr = self.raw;
        let iters = queries
            .into_iter()
            .enumerate()
            .map(|(q_idx, terms)| {
                Box::new(MockIterState {
                    world: world_ptr,
                    sys_idx,
                    query_idx: q_idx,
                    terms,
                    next_chunk_idx: 0,
                    end_chunk_idx: None,
                    active_chunk_idx: None,
                    in_par_iter: false,
                    ro_rows_scratch: Vec::new(),
                    mut_rows_scratch: Vec::new(),
                })
            })
            .collect();
        let mut frame = MockSystemFrame {
            world: world_ptr,
            sys_idx,
            iters,
        };
        let sys_ctx = (&raw mut frame).cast::<BamSystemCtx>();
        unsafe { callback(sys_ctx, user_data) }
    }
}

impl MockHostWorld {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> TestHost {
        TestHost {
            raw: Box::into_raw(Box::new(Self {
                api: make_mock_host_api(),
                components: Vec::new(),
                name_to_handle: HashMap::new(),
                uuid_to_handle: HashMap::new(),
                resources: HashMap::new(),
                messages: HashMap::new(),
                msg_cursors: HashMap::new(),
                chunks: Vec::new(),
                systems: Vec::new(),
                commands: Vec::new(),
                last_inserted_bytes: Vec::new(),
                next_entity_bits: 100,
                last_error: String::new(),
                symbols: HashMap::new(),
            })),
        }
    }

    pub fn expose_component<T: 'static>(
        &mut self,
        name: &str,
        uuid: ComponentUuid,
        flags: u32,
        has_default: bool,
    ) -> BamComponent {
        let raw_id = (self.components.len() + 1) as u32;
        let handle = BamComponent::from_raw(raw_id);
        self.components.push(RegisteredComp {
            handle,
            name: name.to_owned(),
            uuid,
            size: core::mem::size_of::<T>(),
            align: core::mem::align_of::<T>(),
            flags,
            has_default,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
            on_add: None,
            on_remove: None,
            hook_user_data: core::ptr::null_mut(),
        });
        self.name_to_handle.insert(name.to_owned(), handle);
        if !uuid.is_zero() {
            self.uuid_to_handle.insert(uuid, handle);
        }
        handle
    }

    pub fn insert_resource<R: Copy + 'static>(
        &mut self,
        name: &str,
        uuid: ComponentUuid,
        value: R,
    ) -> BamComponent {
        let handle = self
            .name_to_handle
            .get(name)
            .copied()
            .unwrap_or_else(|| self.expose_component::<R>(name, uuid, 0, false));
        let size = core::mem::size_of::<R>();
        let word_len = size.div_ceil(8).max(1);
        let mut words = vec![0u64; word_len];
        if size > 0 {
            // SAFETY: `words` has `>= size` writable bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (&raw const value).cast::<u8>(),
                    words.as_mut_ptr().cast::<u8>(),
                    size,
                );
            }
        }
        self.resources.insert(handle, words);
        handle
    }

    pub fn read_resource<R: Copy>(&self, handle: BamComponent) -> R {
        let words = self.resources.get(&handle).expect("missing resource");
        // SAFETY: `words` holds an initialized `R`.
        unsafe { core::ptr::read(words.as_ptr().cast::<R>()) }
    }

    fn comp_meta(&self, handle: BamComponent) -> Option<&RegisteredComp> {
        let idx = (handle.to_raw() as usize).checked_sub(1)?;
        self.components.get(idx)
    }
}

unsafe extern "C" fn host_get_proc_address(
    ctx: *const BamHostCtx,
    name: *const c_char,
) -> *const c_void {
    if ctx.is_null() || name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: `ctx` points to `MockHostWorld`, `name` is NUL-terminated.
    let world = unsafe { &*ctx.cast::<MockHostWorld>() };
    let Ok(key) = (unsafe { CStr::from_ptr(name) }).to_str() else {
        return core::ptr::null();
    };
    world.symbols.get(key).copied().unwrap_or(core::ptr::null())
}

unsafe extern "C" fn host_last_error(
    ctx: *const BamHostCtx,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if ctx.is_null() {
        return 0;
    }
    // SAFETY: `ctx` points to `MockHostWorld`.
    let world = unsafe { &*ctx.cast::<MockHostWorld>() };
    let bytes = world.last_error.as_bytes();
    if !buf.is_null() && cap > 0 {
        let copy_len = bytes.len().min(cap - 1);
        // SAFETY: `buf` has `cap` writable bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), copy_len);
            *buf.add(copy_len) = 0;
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
    out_comp: *mut BamComponent,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() || out_comp.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Valid `load_ctx` and `desc`.
    let world = unsafe { &mut *load_ctx.cast::<MockHostWorld>() };
    let d = unsafe { &*desc };
    let valid = d.validate();
    if !valid.is_ok() {
        return valid;
    }
    let Some(cstr) = (unsafe { d.name_cstr() }) else {
        return FfiResult::NULL_POINTER;
    };
    let Ok(name_str) = cstr.to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    if let Some(&existing) = world.name_to_handle.get(name_str) {
        let meta = world.comp_meta(existing).unwrap();
        if meta.uuid != d.uuid || meta.size != d.size || meta.align != d.align {
            return FfiResult::TYPE_MISMATCH;
        }
        unsafe { *out_comp = existing };
        return FfiResult::OK;
    }
    let raw_id = (world.components.len() + 1) as u32;
    let handle = BamComponent::from_raw(raw_id);
    world.components.push(RegisteredComp {
        handle,
        name: name_str.to_owned(),
        uuid: d.uuid,
        size: d.size,
        align: d.align,
        flags: d.flags,
        has_default: false,
        drop_fn: d.drop_fn,
        vtable_ptr: d.vtable_ptr,
        on_add: d.on_add,
        on_remove: d.on_remove,
        hook_user_data: d.hook_user_data,
    });
    world.name_to_handle.insert(name_str.to_owned(), handle);
    if !d.uuid.is_zero() {
        world.uuid_to_handle.insert(d.uuid, handle);
    }
    unsafe { *out_comp = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_resolve_component(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_comp: *mut BamComponent,
) -> FfiResult {
    if ctx.is_null() || out_comp.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if name.is_null() && uuid.is_null() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `ctx` points to `MockHostWorld`.
    let world = unsafe { &*ctx.cast::<MockHostWorld>() };
    if !name.is_null() {
        let Ok(name_str) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return FfiResult::INVALID_UTF8;
        };
        let Some(&handle) = world.name_to_handle.get(name_str) else {
            return FfiResult::NOT_FOUND;
        };
        if !uuid.is_null() {
            let expected = unsafe { *uuid };
            let meta = world.comp_meta(handle).unwrap();
            if !meta.uuid.is_zero() && meta.uuid != expected {
                return FfiResult::TYPE_MISMATCH;
            }
        }
        unsafe { *out_comp = handle };
        return FfiResult::OK;
    }
    let expected = unsafe { *uuid };
    let Some(&handle) = world.uuid_to_handle.get(&expected) else {
        return FfiResult::NOT_FOUND;
    };
    unsafe { *out_comp = handle };
    FfiResult::OK
}

unsafe extern "C" fn host_register_system(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamSystemDesc,
) -> FfiResult {
    if load_ctx.is_null() || desc.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Valid `load_ctx` and `desc`.
    let world = unsafe { &mut *load_ctx.cast::<MockHostWorld>() };
    let d = unsafe { &*desc };
    let status = d.validate();
    if !status.is_ok() {
        return status;
    }
    let Ok(sys_name) = (unsafe { CStr::from_ptr(d.name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    let mut queries = Vec::with_capacity(d.query_count);
    for q_idx in 0..d.query_count {
        let q = unsafe { &*d.queries.add(q_idx) };
        let q_status = q.validate();
        if !q_status.is_ok() {
            return q_status;
        }
        let terms = if q.term_count == 0 {
            Vec::new()
        } else {
            unsafe { core::slice::from_raw_parts(q.terms, q.term_count) }.to_vec()
        };
        queries.push(terms);
    }
    let mut after = Vec::with_capacity(d.after_count);
    for i in 0..d.after_count {
        let ptr = unsafe { *d.after.add(i) };
        after.push(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned());
    }
    let mut before = Vec::with_capacity(d.before_count);
    for i in 0..d.before_count {
        let ptr = unsafe { *d.before.add(i) };
        before.push(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned());
    }
    world.systems.push(RegisteredSystem {
        name: sys_name.to_owned(),
        queries,
        schedule: d.schedule,
        flags: d.flags,
        after,
        before,
        callback: d.callback.unwrap(),
        user_data: d.user_data,
        run_if: d.run_if,
        run_if_user_data: d.run_if_user_data,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_sys_host_ctx(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    if sys_ctx.is_null() {
        return core::ptr::null();
    }
    let frame = unsafe { &*sys_ctx.cast::<MockSystemFrame>() };
    frame.world.cast::<BamHostCtx>()
}

unsafe extern "C" fn host_sys_query(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult {
    if sys_ctx.is_null() || out_iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let frame = unsafe { &mut *sys_ctx.cast::<MockSystemFrame>() };
    let Some(iter_box) = frame.iters.get_mut(query_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    let ptr: *mut MockIterState = &mut **iter_box;
    unsafe { *out_iter = ptr.cast::<BamIter>() };
    FfiResult::OK
}

unsafe extern "C" fn host_sys_commands(
    sys_ctx: *mut BamSystemCtx,
    out_commands: *mut *mut BamCommands,
) -> FfiResult {
    if sys_ctx.is_null() || out_commands.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let frame = unsafe { &mut *sys_ctx.cast::<MockSystemFrame>() };
    unsafe { *out_commands = frame.world.cast::<BamCommands>() };
    FfiResult::OK
}

unsafe extern "C" fn host_iter_next_chunk(iter: *mut BamIter, out_len: *mut usize) -> FfiBool {
    if iter.is_null() || out_len.is_null() {
        return FfiBool::FALSE;
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let world = unsafe { &*state.world };
    let limit = state
        .end_chunk_idx
        .map_or(world.chunks.len(), |end| end.min(world.chunks.len()));
    while state.next_chunk_idx < limit {
        let idx = state.next_chunk_idx;
        state.next_chunk_idx += 1;
        let chunk = &world.chunks[idx];
        // Filter check: skip chunks missing required non-optional entity components or matching `WITHOUT`.
        let matches = state.terms.iter().all(|term| {
            if term.mode.is_resource() || term.mode.is_message() {
                return true;
            }
            let has_dense = chunk.dense_columns.contains_key(&term.component);
            let has_sparse = chunk.sparse_columns.contains_key(&term.component);
            let present = has_dense || has_sparse;
            if term.mode.is_without() {
                !present
            } else if term.mode.is_optional() {
                true
            } else {
                present
            }
        });
        if matches && !chunk.entities.is_empty() {
            state.active_chunk_idx = Some(idx);
            unsafe { *out_len = chunk.entities.len() };
            return FfiBool::TRUE;
        }
    }
    state.active_chunk_idx = None;
    unsafe { *out_len = 0 };
    FfiBool::FALSE
}

unsafe extern "C" fn host_iter_reset(iter: *mut BamIter) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    state.next_chunk_idx = 0;
    state.active_chunk_idx = None;
    FfiResult::OK
}

unsafe extern "C" fn host_iter_entities(iter: *const BamIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let world = unsafe { &*state.world };
    world.chunks[c_idx].entities.as_ptr()
}

unsafe extern "C" fn host_iter_col_ro(iter: *const BamIter, term_index: usize) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null();
    };
    if !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    let world = unsafe { &*state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return core::ptr::null();
    };
    if (meta.flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0 {
        return core::ptr::null();
    }
    world.chunks[c_idx]
        .dense_columns
        .get(&term.component)
        .map_or(core::ptr::null(), |words| words.as_ptr().cast::<u8>())
}

unsafe extern "C" fn host_iter_col_mut(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    let ptr = unsafe { host_iter_col_mut_untracked(iter, term_index) };
    if !ptr.is_null() {
        let state = unsafe { &mut *iter.cast::<MockIterState>() };
        let c_idx = state.active_chunk_idx.unwrap();
        let comp = state.terms[term_index].component;
        let world = unsafe { &mut *state.world };
        world.chunks[c_idx].chunk_marked_changed.insert(comp, true);
    }
    ptr
}

unsafe extern "C" fn host_iter_col_mut_untracked(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return core::ptr::null_mut();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null_mut();
    };
    if !term.mode.is_mutable() || !term.mode.has_entity_column_data() {
        return core::ptr::null_mut();
    }
    let world = unsafe { &mut *state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return core::ptr::null_mut();
    };
    if (meta.flags & (BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE)) != 0 {
        return core::ptr::null_mut();
    }
    world.chunks[c_idx]
        .dense_columns
        .get_mut(&term.component)
        .map_or(core::ptr::null_mut(), |words| {
            words.as_mut_ptr().cast::<u8>()
        })
}

unsafe extern "C" fn host_iter_mark_changed_range(
    iter: *mut BamIter,
    term_index: usize,
    start_row: usize,
    row_count: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return FfiResult::INVALID_STATE;
    };
    let Some(term) = state.terms.get(term_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if !term.mode.is_mutable() {
        return FfiResult::PERMISSION_DENIED;
    }
    let world = unsafe { &mut *state.world };
    let chunk = &mut world.chunks[c_idx];
    if start_row.saturating_add(row_count) > chunk.entities.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let entry = chunk.marked_rows.entry(term.component).or_default();
    for r in start_row..(start_row + row_count) {
        entry.push(r);
    }
    FfiResult::OK
}

unsafe extern "C" fn host_iter_rows_ro(iter: *mut BamIter, term_index: usize) -> *const *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null();
    };
    if !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    let world = unsafe { &*state.world };
    let chunk = &world.chunks[c_idx];
    let Some(rows) = chunk.sparse_columns.get(&term.component) else {
        return core::ptr::null();
    };
    state.ro_rows_scratch.clear();
    for row in rows {
        match row {
            Some(words) => state.ro_rows_scratch.push(words.as_ptr().cast::<u8>()),
            None => state.ro_rows_scratch.push(core::ptr::null()),
        }
    }
    state.ro_rows_scratch.as_ptr()
}

unsafe extern "C" fn host_iter_rows_mut(iter: *mut BamIter, term_index: usize) -> *const *mut u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(c_idx) = state.active_chunk_idx else {
        return core::ptr::null();
    };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null();
    };
    if !term.mode.is_mutable() || !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    let world = unsafe { &mut *state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return core::ptr::null();
    };
    if (meta.flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0 {
        return core::ptr::null();
    }
    let chunk = &mut world.chunks[c_idx];
    let Some(rows) = chunk.sparse_columns.get_mut(&term.component) else {
        return core::ptr::null();
    };
    state.mut_rows_scratch.clear();
    for row in rows.iter_mut() {
        match row {
            Some(words) => state.mut_rows_scratch.push(words.as_mut_ptr().cast::<u8>()),
            None => state.mut_rows_scratch.push(core::ptr::null_mut()),
        }
    }
    state.mut_rows_scratch.as_ptr()
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
        return FfiResult::INVALID_COMPONENT;
    };
    let mut storage_flags = 0u32;
    if (meta.flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0 {
        storage_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
    }
    if (meta.flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0 {
        storage_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
    }
    if term.mode.is_resource() {
        storage_flags |= BAM_STORAGE_FLAG_RESOURCE;
    }
    if term.mode.is_message() {
        storage_flags |= BAM_STORAGE_FLAG_MESSAGE;
    }
    unsafe {
        *out_info = BamColumnInfo {
            component: term.component,
            mode: term.mode,
            uuid: meta.uuid,
            size: meta.size,
            align: meta.align,
            storage_flags,
            reserved0: 0,
        };
    }
    FfiResult::OK
}

unsafe extern "C" fn host_res_ptr(iter: *const BamIter, term_index: usize) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { &*iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null();
    };
    if !term.mode.is_resource() {
        return core::ptr::null();
    }
    let world = unsafe { &*state.world };
    world
        .resources
        .get(&term.component)
        .map_or(core::ptr::null(), |words| words.as_ptr().cast::<u8>())
}

unsafe extern "C" fn host_res_mut_ptr(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return core::ptr::null_mut();
    };
    if term.mode != AccessMode::RESOURCE_WRITE {
        return core::ptr::null_mut();
    }
    let world = unsafe { &mut *state.world };
    world
        .resources
        .get_mut(&term.component)
        .map_or(core::ptr::null_mut(), |words| {
            words.as_mut_ptr().cast::<u8>()
        })
}

unsafe extern "C" fn host_cmd_spawn_empty(
    cmds: *mut BamCommands,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if cmds.is_null() || out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    let e = FfiEntity::from_bits(world.next_entity_bits);
    world.next_entity_bits += 1;
    world.commands.push(RecordedCommand::SpawnEmpty(e));
    unsafe { *out_entity = e };
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_spawn(
    cmds: *mut BamCommands,
    comps: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if cmds.is_null() || out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if count > 0 && (comps.is_null() || data_ptrs.is_null()) {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    let mut recorded = Vec::with_capacity(count);
    for i in 0..count {
        let comp = unsafe { *comps.add(i) };
        let Some(meta) = world.comp_meta(comp) else {
            return FfiResult::INVALID_COMPONENT;
        };
        let ptr = unsafe { *data_ptrs.add(i) };
        if meta.size > 0 && ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let sz = meta.size;
        if sz > 0 {
            world.last_inserted_bytes.resize(sz, MaybeUninit::uninit());
            unsafe {
                core::ptr::copy_nonoverlapping(
                    ptr.cast::<MaybeUninit<u8>>(),
                    world.last_inserted_bytes.as_mut_ptr(),
                    sz,
                );
            }
        }
        recorded.push((comp, sz));
    }
    let e = FfiEntity::from_bits(world.next_entity_bits);
    world.next_entity_bits += 1;
    world.commands.push(RecordedCommand::SpawnBatch {
        entity: e,
        components: recorded,
    });
    unsafe { *out_entity = e };
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_despawn(cmds: *mut BamCommands, entity: FfiEntity) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    world.commands.push(RecordedCommand::Despawn(entity));
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_insert(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    comp: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    let Some(meta) = world.comp_meta(comp) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.size != data_len {
        return FfiResult::TYPE_MISMATCH;
    }
    if data_len > 0 {
        if data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        world
            .last_inserted_bytes
            .resize(data_len, MaybeUninit::uninit());
        unsafe {
            core::ptr::copy_nonoverlapping(
                data_ptr.cast::<MaybeUninit<u8>>(),
                world.last_inserted_bytes.as_mut_ptr(),
                data_len,
            );
        }
    }
    world.commands.push(RecordedCommand::Insert {
        entity,
        component: comp,
        byte_len: data_len,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_insert_default(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    comp: BamComponent,
) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    let Some(meta) = world.comp_meta(comp) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if !meta.has_default {
        return FfiResult::PERMISSION_DENIED;
    }
    world.commands.push(RecordedCommand::InsertDefault {
        entity,
        component: comp,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_cmd_remove(
    cmds: *mut BamCommands,
    entity: FfiEntity,
    comp: BamComponent,
) -> FfiResult {
    if cmds.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let world = unsafe { &mut *cmds.cast::<MockHostWorld>() };
    if world.comp_meta(comp).is_none() {
        return FfiResult::INVALID_COMPONENT;
    }
    world.commands.push(RecordedCommand::Remove {
        entity,
        component: comp,
    });
    FfiResult::OK
}

unsafe extern "C" fn host_par_for_each_chunk(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(cb) = chunk_fn else {
        return FfiResult::NULL_POINTER;
    };
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    if state.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    let world_len = unsafe { (*state.world).chunks.len() };
    let start = state.next_chunk_idx;
    state.next_chunk_idx = world_len;
    state.active_chunk_idx = None;
    for chunk_idx in start..world_len {
        let mut sub = MockIterState {
            world: state.world,
            sys_idx: state.sys_idx,
            query_idx: state.query_idx,
            terms: state.terms.clone(),
            next_chunk_idx: chunk_idx,
            end_chunk_idx: Some(chunk_idx + 1),
            active_chunk_idx: None,
            in_par_iter: true,
            ro_rows_scratch: Vec::new(),
            mut_rows_scratch: Vec::new(),
        };
        let status = unsafe { cb((&raw mut sub).cast::<BamIter>(), user_data) };
        if !status.is_ok() {
            return status;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn host_sys_entity_get(
    sys_ctx: *const BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *const u8 {
    if sys_ctx.is_null() || entity.is_null() {
        return core::ptr::null();
    }
    let frame = unsafe { &*sys_ctx.cast::<MockSystemFrame>() };
    let Some(iter_state) = frame.iters.get(query_index) else {
        return core::ptr::null();
    };
    let Some(term) = iter_state.terms.get(term_index) else {
        return core::ptr::null();
    };
    if !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    let world = unsafe { &*frame.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return core::ptr::null();
    };
    for chunk in &world.chunks {
        if let Some(row) = chunk.entities.iter().position(|&e| e == entity) {
            // Check query filter membership on this chunk.
            let matches = iter_state.terms.iter().all(|t| {
                if t.mode.is_resource() || t.mode.is_message() {
                    return true;
                }
                let present = chunk.dense_columns.contains_key(&t.component)
                    || chunk.sparse_columns.contains_key(&t.component);
                if t.mode.is_without() {
                    !present
                } else if t.mode.is_optional() {
                    true
                } else {
                    present
                }
            });
            if !matches {
                return core::ptr::null();
            }
            if let Some(words) = chunk.dense_columns.get(&term.component) {
                return unsafe { words.as_ptr().cast::<u8>().add(row * meta.size) };
            }
            return core::ptr::null();
        }
    }
    core::ptr::null()
}

unsafe extern "C" fn host_sys_entity_get_mut(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *mut u8 {
    if sys_ctx.is_null() || entity.is_null() {
        return core::ptr::null_mut();
    }
    let frame = unsafe { &mut *sys_ctx.cast::<MockSystemFrame>() };
    let Some(iter_state) = frame.iters.get(query_index) else {
        return core::ptr::null_mut();
    };
    let Some(term) = iter_state.terms.get(term_index) else {
        return core::ptr::null_mut();
    };
    if !term.mode.is_mutable() || !term.mode.has_entity_column_data() {
        return core::ptr::null_mut();
    }
    let world = unsafe { &mut *frame.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return core::ptr::null_mut();
    };
    if (meta.flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0 {
        return core::ptr::null_mut();
    }
    let elem_size = meta.size;
    let terms = iter_state.terms.clone();
    let comp = term.component;
    for chunk in &mut world.chunks {
        if let Some(row) = chunk.entities.iter().position(|&e| e == entity) {
            let matches = terms.iter().all(|t| {
                if t.mode.is_resource() || t.mode.is_message() {
                    return true;
                }
                let present = chunk.dense_columns.contains_key(&t.component)
                    || chunk.sparse_columns.contains_key(&t.component);
                if t.mode.is_without() {
                    !present
                } else if t.mode.is_optional() {
                    true
                } else {
                    present
                }
            });
            if !matches {
                return core::ptr::null_mut();
            }
            chunk.marked_rows.entry(comp).or_default().push(row);
            if let Some(words) = chunk.dense_columns.get_mut(&comp) {
                return unsafe { words.as_mut_ptr().cast::<u8>().add(row * elem_size) };
            }
            return core::ptr::null_mut();
        }
    }
    core::ptr::null_mut()
}

unsafe extern "C" fn host_msg_read_batch(
    iter: *mut BamIter,
    term_index: usize,
    out_buf: *mut u8,
    max_count: usize,
    out_read: *mut usize,
) -> FfiResult {
    if iter.is_null() || out_read.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe { *out_read = 0 };
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if !term.mode.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    let world = unsafe { &mut *state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return FfiResult::INVALID_COMPONENT;
    };
    let elem_size = meta.size;
    if max_count > 0 && elem_size > 0 && out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let cursor_key = (state.sys_idx, state.query_idx, term_index);
    let cursor = world.msg_cursors.get(&cursor_key).copied().unwrap_or(0);
    let Some(stream) = world.messages.get(&term.component) else {
        return FfiResult::OK;
    };
    let avail = stream.len().saturating_sub(cursor);
    let to_copy = avail.min(max_count);
    for i in 0..to_copy {
        if elem_size > 0 {
            let bytes = &stream[cursor + i];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    out_buf.add(i * elem_size).cast::<MaybeUninit<u8>>(),
                    elem_size,
                );
            }
        }
    }
    world.msg_cursors.insert(cursor_key, cursor + to_copy);
    unsafe { *out_read = to_copy };
    FfiResult::OK
}

unsafe extern "C" fn host_msg_send(
    iter: *mut BamIter,
    term_index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let state = unsafe { &mut *iter.cast::<MockIterState>() };
    let Some(term) = state.terms.get(term_index) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if term.mode != AccessMode::MESSAGE_WRITE {
        return FfiResult::PERMISSION_DENIED;
    }
    let world = unsafe { &mut *state.world };
    let Some(meta) = world.comp_meta(term.component) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.size != data_len {
        return FfiResult::TYPE_MISMATCH;
    }
    let mut entry = vec![MaybeUninit::uninit(); data_len];
    if data_len > 0 {
        if data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                data_ptr.cast::<MaybeUninit<u8>>(),
                entry.as_mut_ptr(),
                data_len,
            );
        }
    }
    world
        .messages
        .entry(term.component)
        .or_default()
        .push(entry);
    FfiResult::OK
}

pub fn make_mock_host_api() -> BamHostApi {
    BamHostApi {
        struct_size: core::mem::size_of::<BamHostApi>() as u32,
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
        iter_mark_changed_range: Some(host_iter_mark_changed_range),
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
        par_for_each_chunk: Some(host_par_for_each_chunk),
        sys_entity_get: Some(host_sys_entity_get),
        sys_entity_get_mut: Some(host_sys_entity_get_mut),
        msg_read_batch: Some(host_msg_read_batch),
        msg_send: Some(host_msg_send),
        msg_write_batch: None,
        res_ticks: None,
        sys_res_ptr: None,
        sys_res_mut_ptr: None,
        register_observer: None,
        cmd_trigger: None,
    }
}
