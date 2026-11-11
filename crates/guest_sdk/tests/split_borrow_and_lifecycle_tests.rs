//! Soundness, Split-Borrow Ergonomics, and Lifecycle Cleanup Tests for `guest_sdk`
//!
//! Covers:
//! 1. Disjoint multi-column read/write split borrows (`Chunk`, `GuestChunkIter::read_write`,
//!    `write_2`, `columns_mut`, `split_slices`, `entities_and_column_mut`, `entities_and_read_write`).
//! 2. Soundness rejection of duplicate column indices `(0, 0)` and overlapping host byte ranges
//!    so aliasing `&mut` slices is impossible in safe Rust.
//! 3. `entities_slice` and `Chunk::entities` bounds, alignment, null, wrapping, and empty-chunk safety.
//! 4. `GuestChunkIter::reset` and `MultiQueryContext` nested multi-query iteration.
//! 5. `GuestApp` / `GuestPlugin` / `SystemContext` automatic `user_data` cleanup on `shutdown`
//!    with `Drop` counter assertions (`0` leaks), transactional rollback on failed/panicking `init`,
//!    and re-entrant callback borrow protection (`FfiResult::ACCESS_CONFLICT`).
//! 6. Cross-language asymmetric C99 (`tests/asymmetric_c_mod`) and Rust (`tests/asymmetric_rust_mod`)
//!    build & ABI layout verification.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use core::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use guest_sdk::prelude::*;

/// Serializes tests that exercise the process-global `GUEST_ALLOCATIONS` registry.
static LIFECYCLE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pos {
    x: f32,
    y: f32,
}

// SAFETY: `Pos` is `#[repr(C)]` composed solely of `f32` primitives.
unsafe impl GuestComponent for Pos {
    const NAME: &'static CStr = c"Pos";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Vel {
    vx: f32,
    vy: f32,
}

// SAFETY: `Vel` is `#[repr(C)]` composed solely of `f32` primitives.
unsafe impl GuestComponent for Vel {
    const NAME: &'static CStr = c"Vel";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shield {
    hp: i32,
}

// SAFETY: `Shield` is `#[repr(C)]` composed solely of `i32`.
unsafe impl GuestComponent for Shield {
    const NAME: &'static CStr = c"Shield";
}

// ============================================================================
// Mock Multi-Chunk / Multi-Column Host Harness
// ============================================================================

struct MockColumn {
    ptr: *mut u8,
    stride: usize,
    mutable_allowed: bool,
    changed_rows: Vec<usize>,
}

struct MockChunk {
    count: usize,
    entities_ptr: *const FfiEntity,
    columns: Vec<MockColumn>,
}

struct MockIterState {
    chunks: Vec<MockChunk>,
    cursor: usize,
    reset_calls: usize,
}

unsafe extern "C" fn mock_iter_next_chunk(iter: *mut RawChunkIter) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*mut RawChunkIter`.
    let state = unsafe { &mut *(iter as *mut MockIterState) };
    if state.cursor >= state.chunks.len() {
        return 0;
    }
    let idx = state.cursor;
    state.cursor += 1;
    state.chunks[idx].count
}

unsafe extern "C" fn mock_iter_reset(iter: *mut RawChunkIter) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*mut RawChunkIter`.
    let state = unsafe { &mut *(iter as *mut MockIterState) };
    state.cursor = 0;
    state.reset_calls += 1;
    FfiResult::OK
}

unsafe extern "C" fn mock_iter_column_readonly(
    iter: *const RawChunkIter,
    query_index: usize,
) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*const RawChunkIter`.
    let state = unsafe { &*(iter as *const MockIterState) };
    let Some(chunk_idx) = state.cursor.checked_sub(1) else {
        return core::ptr::null();
    };
    state
        .chunks
        .get(chunk_idx)
        .and_then(|c| c.columns.get(query_index))
        .map(|col| col.ptr as *const u8)
        .unwrap_or(core::ptr::null())
}

unsafe extern "C" fn mock_iter_column_mutable(
    iter: *mut RawChunkIter,
    query_index: usize,
) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*mut RawChunkIter`.
    let state = unsafe { &mut *(iter as *mut MockIterState) };
    let Some(chunk_idx) = state.cursor.checked_sub(1) else {
        return core::ptr::null_mut();
    };
    state
        .chunks
        .get_mut(chunk_idx)
        .and_then(|c| c.columns.get_mut(query_index))
        .and_then(|col| {
            if col.mutable_allowed {
                Some(col.ptr)
            } else {
                None
            }
        })
        .unwrap_or(core::ptr::null_mut())
}

unsafe extern "C" fn mock_iter_mark_changed(
    iter: *mut RawChunkIter,
    query_index: usize,
    row_index: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*mut RawChunkIter`.
    let state = unsafe { &mut *(iter as *mut MockIterState) };
    if let Some(chunk_idx) = state.cursor.checked_sub(1) {
        if let Some(col) = state
            .chunks
            .get_mut(chunk_idx)
            .and_then(|c| c.columns.get_mut(query_index))
        {
            col.changed_rows.push(row_index);
            return FfiResult::OK;
        }
    }
    FfiResult::INTERNAL_ERROR
}

unsafe extern "C" fn mock_iter_entities(iter: *const RawChunkIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*const RawChunkIter`.
    let state = unsafe { &*(iter as *const MockIterState) };
    let Some(chunk_idx) = state.cursor.checked_sub(1) else {
        return core::ptr::null();
    };
    state
        .chunks
        .get(chunk_idx)
        .map(|c| c.entities_ptr)
        .unwrap_or(core::ptr::null())
}

unsafe extern "C" fn mock_iter_column_stride(
    iter: *const RawChunkIter,
    query_index: usize,
) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: Test harness passes `MockIterState` cast to `*const RawChunkIter`.
    let state = unsafe { &*(iter as *const MockIterState) };
    let Some(chunk_idx) = state.cursor.checked_sub(1) else {
        return 0;
    };
    state
        .chunks
        .get(chunk_idx)
        .and_then(|c| c.columns.get(query_index))
        .map(|col| col.stride)
        .unwrap_or(0)
}

unsafe extern "C" fn noop_register_system(
    _host_context: *mut c_void,
    _descriptor: *const SystemDescriptor,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_register_component(
    _host_context: *mut c_void,
    _descriptor: *const ModComponentDescriptor,
) -> FfiResult {
    FfiResult::OK
}

fn mock_host_api() -> HostApi {
    HostApi {
        struct_size: core::mem::size_of::<HostApi>() as u32,
        abi_version: ABI_VERSION_PACKED,
        register_system: noop_register_system,
        iter_next_chunk: mock_iter_next_chunk,
        iter_column_readonly: mock_iter_column_readonly,
        iter_column_mutable: mock_iter_column_mutable,
        iter_mark_changed: mock_iter_mark_changed,
        iter_entities: mock_iter_entities,
        iter_column_stride: mock_iter_column_stride,
        register_component: noop_register_component,
    }
}

// ============================================================================
// 1. Disjoint Multi-Column Split Borrow Tests
// ============================================================================

#[test]
fn test_chunk_disjoint_read_write_and_entities_simultaneous_borrow() {
    let api = mock_host_api();
    let entities = [FfiEntity(101), FfiEntity(202), FfiEntity(303)];
    let mut positions = [
        Pos { x: 1.0, y: 2.0 },
        Pos { x: 3.0, y: 4.0 },
        Pos { x: 5.0, y: 6.0 },
    ];
    let velocities = [
        Vel { vx: 10.0, vy: 20.0 },
        Vel { vx: 30.0, vy: 40.0 },
        Vel { vx: 50.0, vy: 60.0 },
    ];

    let mut state = MockIterState {
        chunks: vec![MockChunk {
            count: 3,
            entities_ptr: entities.as_ptr(),
            columns: vec![
                MockColumn {
                    ptr: positions.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Pos>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: velocities.as_ptr() as *mut u8,
                    stride: core::mem::size_of::<Vel>(),
                    mutable_allowed: false,
                    changed_rows: Vec::new(),
                },
            ],
        }],
        cursor: 0,
        reset_calls: 0,
    };

    let mut iter = GuestChunkIter::new(&mut state as *mut MockIterState as *mut RawChunkIter, &api);
    let mut seen_entities = Vec::new();

    while let Some(mut chunk) = iter.next_chunk_view() {
        let len = chunk.len();
        assert_eq!(len, 3);
        assert!(!chunk.is_empty());

        let (ents, vel_slice, pos_slice) = chunk
            .entities_and_read_write::<Vel, Pos>(1, 0)
            .expect("disjoint read col 1 + write col 0 + entities must succeed");

        assert_eq!(ents.len(), 3);
        for i in 0..len {
            seen_entities.push(ents[i]);
            pos_slice[i].x += vel_slice[i].vx;
            pos_slice[i].y += vel_slice[i].vy;
        }
    }

    assert_eq!(seen_entities, entities);
    assert_eq!(
        positions,
        [
            Pos { x: 11.0, y: 22.0 },
            Pos { x: 33.0, y: 44.0 },
            Pos { x: 55.0, y: 66.0 },
        ]
    );
}

#[test]
fn test_columns_mut_3_way_and_split_slices_disjoint_borrows() {
    let api = mock_host_api();
    let mut positions = [Pos { x: 1.0, y: 2.0 }, Pos { x: 3.0, y: 4.0 }];
    let mut velocities = [Vel { vx: 0.5, vy: 1.5 }, Vel { vx: 2.5, vy: 3.5 }];
    let mut shields = [Shield { hp: 10 }, Shield { hp: 20 }];

    let mut state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: core::ptr::null(),
            columns: vec![
                MockColumn {
                    ptr: positions.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Pos>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: velocities.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Vel>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: shields.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Shield>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
            ],
        }],
        cursor: 0,
        reset_calls: 0,
    };

    let mut iter = GuestChunkIter::with_reset(
        &mut state as *mut MockIterState as *mut RawChunkIter,
        &api,
        Some(mock_iter_reset),
    );

    // Pass 1: 3 simultaneous mutable columns via `columns_mut` + `into_typed_slice`
    {
        let mut chunk = iter.next_chunk_view().expect("chunk 0 present");
        let [pos_raw, vel_raw, sh_raw] = chunk
            .columns_mut([0, 1, 2])
            .expect("3 distinct mutable columns must succeed");

        let pos = pos_raw.into_typed_slice::<Pos>().unwrap();
        let vel = vel_raw.into_typed_slice::<Vel>().unwrap();
        let sh = sh_raw.into_typed_slice::<Shield>().unwrap();

        for i in 0..2 {
            pos[i].x += vel[i].vx;
            vel[i].vy *= 2.0;
            sh[i].hp += 5;
        }
    }
    assert!(iter.next_chunk_view().is_none());

    // Pass 2: reset iterator and test `split_slices::<1, 2>` (entities + 1 read + 2 write)
    assert_eq!(iter.reset(), FfiResult::OK);
    {
        let mut chunk = iter.next_chunk_view().expect("chunk 0 present after reset");
        let (_ents, [vel_ro], [pos_rw, sh_rw]) = chunk
            .split_slices([1], [0, 2])
            .expect("read [1] + write [0, 2] must succeed");

        let vel = vel_ro.as_typed_slice::<Vel>().unwrap();
        let pos = pos_rw.into_typed_slice::<Pos>().unwrap();
        let sh = sh_rw.into_typed_slice::<Shield>().unwrap();

        for i in 0..2 {
            pos[i].y += vel[i].vy;
            sh[i].hp += 1;
        }
    }

    assert_eq!(positions, [Pos { x: 1.5, y: 5.0 }, Pos { x: 5.5, y: 11.0 }]);
    assert_eq!(shields, [Shield { hp: 16 }, Shield { hp: 26 }]);
}

// ============================================================================
// 2. Duplicate Column & Overlapping Byte-Range Rejection Tests
// ============================================================================

#[test]
fn test_rejects_duplicate_column_indices_preventing_mutable_aliasing() {
    let api = mock_host_api();
    let entities = [FfiEntity(1), FfiEntity(2)];
    let mut positions = [Pos { x: 1.0, y: 2.0 }, Pos { x: 3.0, y: 4.0 }];
    let mut velocities = [Vel { vx: 5.0, vy: 6.0 }, Vel { vx: 7.0, vy: 8.0 }];

    let mut state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: entities.as_ptr(),
            columns: vec![
                MockColumn {
                    ptr: positions.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Pos>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: velocities.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Vel>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
            ],
        }],
        cursor: 0,
        reset_calls: 0,
    };

    let mut iter = GuestChunkIter::new(&mut state as *mut MockIterState as *mut RawChunkIter, &api);
    let mut chunk = iter.next_chunk_view().expect("chunk must exist");

    // Duplicate mutable index (0, 0) MUST return None
    assert!(chunk.write_2::<Pos, Pos>(0, 0).is_none());
    assert!(chunk.columns_mut([0, 0]).is_none());
    assert!(chunk.columns_mut([0, 1, 0]).is_none());

    // Read + Write on same column index (0, 0) or (1, 1) MUST return None
    assert!(chunk.read_write::<Pos, Pos>(0, 0).is_none());
    assert!(chunk.read_write::<Vel, Vel>(1, 1).is_none());
    assert!(chunk.entities_and_read_write::<Pos, Pos>(0, 0).is_none());
    assert!(chunk.split_slices([0, 1], [1]).is_none());
    assert!(chunk.split_slices([0], [1, 1]).is_none());

    // Duplicate shared reads [0, 0] + disjoint write [1] IS sound (`&[T]` is Copy/shared)
    let (ents, [r0_a, r0_b], [w1]) = chunk
        .split_slices([0, 0], [1])
        .expect("duplicate readonly indices with disjoint write must be allowed");
    assert_eq!(ents, &entities);
    assert_eq!(r0_a.as_typed_slice::<Pos>(), r0_b.as_typed_slice::<Pos>());
    assert_eq!(w1.into_typed_slice::<Vel>().unwrap().len(), 2);
}

#[test]
fn test_rejects_overlapping_host_byte_ranges_even_with_distinct_indices() {
    let api = mock_host_api();
    // Craft a single raw buffer where buggy/hostile host maps column 0, column 1, and entities
    // to overlapping regions despite distinct column indices!
    let mut backing = [0u64; 8];
    let base_ptr = backing.as_mut_ptr() as *mut u8;
    // Partial overlap at byte offset +8 (8 bytes into a 16-byte 2-element Pos column)
    let overlapping_col_ptr = unsafe { base_ptr.add(8) };

    let mut state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: base_ptr as *const FfiEntity,
            columns: vec![
                MockColumn {
                    ptr: base_ptr,
                    stride: core::mem::size_of::<Pos>(), // 8 bytes * 2 = 16 bytes [base..base+16)
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: overlapping_col_ptr,
                    stride: core::mem::size_of::<Vel>(), // 8 bytes * 2 = 16 bytes [base+8..base+24)
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
            ],
        }],
        cursor: 0,
        reset_calls: 0,
    };

    let mut iter = GuestChunkIter::new(&mut state as *mut MockIterState as *mut RawChunkIter, &api);
    let mut chunk = iter.next_chunk_view().expect("chunk exists");

    // Distinct indices (0, 1) with overlapping memory ranges MUST be rejected!
    assert!(chunk.write_2::<Pos, Vel>(0, 1).is_none());
    assert!(chunk.read_write::<Vel, Pos>(1, 0).is_none());
    assert!(chunk.columns_mut([0, 1]).is_none());
    assert!(chunk.split_slices([1], [0]).is_none());

    // Entities array overlaps mutable column 0 -> must reject simultaneous borrow!
    assert!(chunk.entities_and_column_mut::<Pos>(0).is_none());
    assert!(chunk.entities_and_read_write::<Vel, Pos>(1, 0).is_none());
}

// ============================================================================
// 3. Entities Slice Bounds, Alignment, Wrapping & Empty Chunk Tests
// ============================================================================

#[test]
fn test_entities_slice_defensive_bounds_and_empty_chunks() {
    let api = mock_host_api();
    let valid_entities = [FfiEntity(7), FfiEntity(8)];
    let unaligned_buf = [0u8; 32];
    let unaligned_entities_ptr = unsafe { unaligned_buf.as_ptr().add(1) } as *const FfiEntity;
    let wrapping_entities_ptr = (usize::MAX - 3) as *const FfiEntity;

    let mut state = MockIterState {
        chunks: vec![
            // Chunk 0: valid entities
            MockChunk {
                count: 2,
                entities_ptr: valid_entities.as_ptr(),
                columns: Vec::new(),
            },
            // Chunk 1: null entities pointer
            MockChunk {
                count: 2,
                entities_ptr: core::ptr::null(),
                columns: Vec::new(),
            },
            // Chunk 2: misaligned entities pointer
            MockChunk {
                count: 2,
                entities_ptr: unaligned_entities_ptr,
                columns: Vec::new(),
            },
            // Chunk 3: address-space wrapping entities pointer
            MockChunk {
                count: 2,
                entities_ptr: wrapping_entities_ptr,
                columns: Vec::new(),
            },
        ],
        cursor: 0,
        reset_calls: 0,
    };

    // Null RawChunkIter handle safety
    let mut null_iter = GuestChunkIter::new(core::ptr::null_mut(), &api);
    assert_eq!(null_iter.next_chunk(), 0);
    assert!(null_iter.next_chunk_view().is_none());
    assert!(null_iter.entities_slice(4).is_empty());
    assert_eq!(null_iter.reset(), FfiResult::NULL_POINTER);

    let mut iter = GuestChunkIter::new(&mut state as *mut MockIterState as *mut RawChunkIter, &api);

    // Chunk 0: valid
    let c0 = iter.next_chunk();
    assert_eq!(c0, 2);
    assert_eq!(iter.entities_slice(c0), &valid_entities);
    assert!(iter.entities_slice(0).is_empty());
    // Oversized count exceeding `isize::MAX` bytes must return empty slice without UB
    let huge_count = (isize::MAX as usize / core::mem::size_of::<FfiEntity>()) + 1;
    assert!(iter.entities_slice(huge_count).is_empty());

    // Chunk 1: null pointer -> `&[]`
    let chunk1 = iter.next_chunk_view().unwrap();
    assert!(chunk1.entities().is_empty());

    // Chunk 2: misaligned pointer -> `&[]`
    let chunk2 = iter.next_chunk_view().unwrap();
    assert!(chunk2.entities().is_empty());

    // Chunk 3: wrapping pointer -> `&[]`
    let chunk3 = iter.next_chunk_view().unwrap();
    assert!(chunk3.entities().is_empty());

    // End of iteration
    assert!(iter.next_chunk_view().is_none());
    // Without `IterResetFn` configured, `reset()` returns `NULL_POINTER`
    assert_eq!(iter.reset(), FfiResult::NULL_POINTER);
}

// ============================================================================
// 4. GuestApp, MultiQueryContext, Automatic Shutdown Cleanup & Drop Tests
// ============================================================================

#[derive(Clone)]
struct DropTracker {
    id: &'static str,
    live_counter: Arc<AtomicUsize>,
    drop_order: Arc<Mutex<Vec<&'static str>>>,
}

impl DropTracker {
    fn new(
        id: &'static str,
        live_counter: &Arc<AtomicUsize>,
        drop_order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> Self {
        live_counter.fetch_add(1, Ordering::SeqCst);
        Self {
            id,
            live_counter: Arc::clone(live_counter),
            drop_order: Arc::clone(drop_order),
        }
    }
}

impl Drop for DropTracker {
    fn drop(&mut self) {
        self.live_counter.fetch_sub(1, Ordering::SeqCst);
        self.drop_order
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(self.id);
    }
}

struct MockHostWorld {
    registered_single: Vec<(String, DynamicSystemCallback, *mut c_void)>,
    registered_multi: Vec<(String, MultiQuerySystemCallback, *mut c_void, usize)>,
    fail_registration_after: Option<usize>,
    spawn_counter: u64,
}

impl MockHostWorld {
    fn new() -> Self {
        Self {
            registered_single: Vec::new(),
            registered_multi: Vec::new(),
            fail_registration_after: None,
            spawn_counter: 500,
        }
    }
}

unsafe extern "C" fn world_register_system(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Test harness passes `&mut MockHostWorld` as `host_context` and valid `descriptor`.
    let world = unsafe { &mut *(host_context as *mut MockHostWorld) };
    if let Some(limit) = world.fail_registration_after {
        if world.registered_single.len() >= limit {
            return FfiResult::INVALID_COMPONENT;
        }
    }
    let desc = unsafe { &*descriptor };
    let name = unsafe { CStr::from_ptr(desc.name) }
        .to_string_lossy()
        .into_owned();
    world
        .registered_single
        .push((name, desc.callback, desc.user_data));
    FfiResult::OK
}

unsafe extern "C" fn world_register_multi_query(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Test harness passes `&mut MockHostWorld` and valid `MultiQuerySystemDescriptor`.
    let world = unsafe { &mut *(host_context as *mut MockHostWorld) };
    let desc = unsafe { &*descriptor };
    let name = unsafe { CStr::from_ptr(desc.name) }
        .to_string_lossy()
        .into_owned();
    world
        .registered_multi
        .push((name, desc.callback, desc.user_data, desc.query_spec_count));
    FfiResult::OK
}

unsafe extern "C" fn mock_cmd_spawn(queue: *mut RawCommandQueue) -> FfiEntity {
    if queue.is_null() {
        return FfiEntity(0);
    }
    // SAFETY: Test passes `&mut MockHostWorld` as `RawCommandQueue`.
    let world = unsafe { &mut *(queue as *mut MockHostWorld) };
    world.spawn_counter += 1;
    FfiEntity(world.spawn_counter)
}

unsafe extern "C" fn mock_get_proc_address(symbol: *const c_char) -> *const c_void {
    if symbol.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller supplies nul-terminated symbol string.
    let sym = unsafe { CStr::from_ptr(symbol) };
    if sym == c"host_iter_reset" {
        mock_iter_reset as *const c_void
    } else if sym == c"host_register_multi_query_system" {
        world_register_multi_query as *const c_void
    } else if sym == c"host_cmd_spawn_empty" {
        mock_cmd_spawn as *const c_void
    } else {
        core::ptr::null()
    }
}

#[test]
fn test_guest_app_zero_leak_shutdown_and_lifo_drop_order() {
    let _guard = LIFECYCLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cleanup_registered_guest_state();

    let live_count = Arc::new(AtomicUsize::new(0));
    let drop_order = Arc::new(Mutex::new(Vec::new()));

    let mut api = mock_host_api();
    api.register_system = world_register_system;
    let mut world = MockHostWorld::new();

    let live_clone = Arc::clone(&live_count);
    let order_clone = Arc::clone(&drop_order);

    let init_status = unsafe {
        run_guest_app_init(
            mock_get_proc_address,
            &api,
            &mut world as *mut MockHostWorld as *mut c_void,
            move |app| {
                let raw_ctx = app.alloc_user_data(DropTracker::new(
                    "alloc_user_data_1",
                    &live_clone,
                    &order_clone,
                ));
                assert!(!raw_ctx.is_null());

                let sys_tracker = DropTracker::new("system_closure_2", &live_clone, &order_clone);
                let mut tick_count = 0usize;
                let res = app.add_system(
                    c"kinematics_closure",
                    &[Pos::write_desc(), Vel::read_desc()],
                    move |ctx| {
                        let _keep_alive = &sys_tracker;
                        tick_count += 1;
                        let step_scale = tick_count as f32;
                        let (iter, cmds) = ctx.iter_and_commands();
                        while let Some(mut chunk) = iter.next_chunk_view() {
                            let (vel, pos) = chunk
                                .read_write::<Vel, Pos>(1, 0)
                                .ok_or(FfiResult::INTERNAL_ERROR)
                                .unwrap_or((&[], &mut []));
                            for i in 0..pos.len() {
                                pos[i].x += vel[i].vx * step_scale;
                            }
                        }
                        let spawned = cmds.spawn_empty();
                        assert_ne!(spawned.0, 0);
                        FfiResult::OK
                    },
                );
                if res.is_err() {
                    return res;
                }

                let mq_tracker =
                    DropTracker::new("multi_query_closure_3", &live_clone, &order_clone);
                let mq_q0 = [Pos::write_desc()];
                let mq_q1 = [Vel::read_desc()];
                let mq_res =
                    app.add_multi_query_system(c"nested_cartesian", &[&mq_q0, &mq_q1], move |mq| {
                        let _keep_alive = &mq_tracker;
                        // Duplicate query index must fail safely with None
                        assert!(mq.iters_2_mut(0, 0).is_none());

                        let Some((q0, q1, cmds)) = mq.iters_2_and_commands(0, 1) else {
                            return FfiResult::INTERNAL_ERROR;
                        };
                        while let Some(mut c0) = q0.next_chunk_view() {
                            let Some(pos) = c0.column_mut::<Pos>(0) else {
                                return FfiResult::INTERNAL_ERROR;
                            };
                            let rst = q1.reset();
                            if rst.is_err() {
                                return rst;
                            }
                            while let Some(c1) = q1.next_chunk_view() {
                                let Some(vel) = c1.column::<Vel>(0) else {
                                    return FfiResult::INTERNAL_ERROR;
                                };
                                for p in pos.iter_mut() {
                                    for v in vel.iter() {
                                        p.y += v.vy;
                                    }
                                }
                            }
                        }
                        let _ = cmds.spawn_empty();
                        FfiResult::OK
                    });
                if mq_res.is_err() {
                    return mq_res;
                }

                let hook_order = Arc::clone(&order_clone);
                app.on_shutdown(move || {
                    hook_order
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push("on_shutdown_hook_4");
                });

                FfiResult::OK
            },
        )
    };

    assert_eq!(init_status, FfiResult::OK);
    assert_eq!(live_count.load(Ordering::SeqCst), 3);
    assert_eq!(world.registered_single.len(), 1);
    assert_eq!(world.registered_multi.len(), 1);

    // Execute registered single-query and multi-query systems against mock ECS chunks
    let mut positions = [Pos { x: 2.0, y: 3.0 }, Pos { x: 4.0, y: 5.0 }];
    let velocities = [Vel { vx: 1.5, vy: 10.0 }, Vel { vx: 2.5, vy: 20.0 }];

    let mut single_iter_state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: core::ptr::null(),
            columns: vec![
                MockColumn {
                    ptr: positions.as_mut_ptr() as *mut u8,
                    stride: core::mem::size_of::<Pos>(),
                    mutable_allowed: true,
                    changed_rows: Vec::new(),
                },
                MockColumn {
                    ptr: velocities.as_ptr() as *mut u8,
                    stride: core::mem::size_of::<Vel>(),
                    mutable_allowed: false,
                    changed_rows: Vec::new(),
                },
            ],
        }],
        cursor: 0,
        reset_calls: 0,
    };

    let (_name, single_cb, single_ud) = world.registered_single[0].clone();
    let single_res = unsafe {
        single_cb(
            &mut single_iter_state as *mut MockIterState as *mut RawChunkIter,
            &mut world as *mut MockHostWorld as *mut RawCommandQueue,
            single_ud,
        )
    };
    assert_eq!(single_res, FfiResult::OK);
    assert_eq!(positions[0].x, 3.5);
    assert_eq!(positions[1].x, 6.5);
    assert_eq!(world.spawn_counter, 501);

    // Execute multi-query system (outer q0 Pos, inner resettable q1 Vel)
    let mut q0_state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: core::ptr::null(),
            columns: vec![MockColumn {
                ptr: positions.as_mut_ptr() as *mut u8,
                stride: core::mem::size_of::<Pos>(),
                mutable_allowed: true,
                changed_rows: Vec::new(),
            }],
        }],
        cursor: 0,
        reset_calls: 0,
    };
    let mut q1_state = MockIterState {
        chunks: vec![MockChunk {
            count: 2,
            entities_ptr: core::ptr::null(),
            columns: vec![MockColumn {
                ptr: velocities.as_ptr() as *mut u8,
                stride: core::mem::size_of::<Vel>(),
                mutable_allowed: false,
                changed_rows: Vec::new(),
            }],
        }],
        cursor: 0,
        reset_calls: 0,
    };
    let mut raw_iters = [
        &mut q0_state as *mut MockIterState as *mut RawChunkIter,
        &mut q1_state as *mut MockIterState as *mut RawChunkIter,
    ];

    let (_mq_name, mq_cb, mq_ud, mq_count) = world.registered_multi[0].clone();
    let mq_exec_res = unsafe {
        mq_cb(
            raw_iters.as_mut_ptr(),
            mq_count,
            &mut world as *mut MockHostWorld as *mut RawCommandQueue,
            mq_ud,
        )
    };
    assert_eq!(mq_exec_res, FfiResult::OK);
    assert_eq!(q1_state.reset_calls, 1);
    // Each Pos.y had (10.0 + 20.0) = 30.0 added
    assert_eq!(positions[0].y, 33.0);
    assert_eq!(positions[1].y, 35.0);
    assert_eq!(world.spawn_counter, 502);

    // Still zero drops while mod is active
    assert_eq!(live_count.load(Ordering::SeqCst), 3);

    // Simulate `bevy_abi_mod_shutdown` -> all allocations must drop in LIFO order with 0 leaks!
    cleanup_registered_guest_state();
    assert_eq!(live_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        *drop_order.lock().unwrap(),
        vec![
            "on_shutdown_hook_4",
            "multi_query_closure_3",
            "system_closure_2",
            "alloc_user_data_1",
        ]
    );
}

#[test]
fn test_guest_app_rollback_on_failed_or_panicking_init_leaves_zero_leaks() {
    let _guard = LIFECYCLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cleanup_registered_guest_state();

    let live_count = Arc::new(AtomicUsize::new(0));
    let drop_order = Arc::new(Mutex::new(Vec::new()));

    let mut api = mock_host_api();
    api.register_system = world_register_system;
    let mut world = MockHostWorld::new();
    // Fail on the 2nd system registration!
    world.fail_registration_after = Some(1);

    // Case A: Partial init returning an error -> must roll back immediately without shutdown!
    {
        let live_clone = Arc::clone(&live_count);
        let order_clone = Arc::clone(&drop_order);
        let res = unsafe {
            run_guest_app_init(
                mock_get_proc_address,
                &api,
                &mut world as *mut MockHostWorld as *mut c_void,
                move |app| {
                    let _ = app.alloc_user_data(DropTracker::new(
                        "staged_ctx",
                        &live_clone,
                        &order_clone,
                    ));
                    let t1 = DropTracker::new("sys_ok", &live_clone, &order_clone);
                    let r1 = app.add_system(c"sys_1", &[], move |_ctx| {
                        let _ = &t1;
                        FfiResult::OK
                    });
                    assert_eq!(r1, FfiResult::OK);

                    // Second registration fails on the host -> `t2` must drop immediately,
                    // and returning `r2` drops `app`, rolling back `t1` and `staged_ctx`!
                    let t2 = DropTracker::new("sys_rejected", &live_clone, &order_clone);
                    let r2 = app.add_system(c"sys_2", &[], move |_ctx| {
                        let _ = &t2;
                        FfiResult::OK
                    });
                    assert_eq!(r2, FfiResult::INVALID_COMPONENT);
                    r2
                },
            )
        };
        assert_eq!(res, FfiResult::INVALID_COMPONENT);
        assert_eq!(
            live_count.load(Ordering::SeqCst),
            0,
            "failed init must leave 0 leaked allocations even before shutdown"
        );
    }

    // Case B: Panicking init -> `catch_guest_panic` + `GuestApp::drop` must roll back with 0 leaks!
    drop_order.lock().unwrap().clear();
    world.fail_registration_after = None;
    {
        let live_clone = Arc::clone(&live_count);
        let order_clone = Arc::clone(&drop_order);
        let res = unsafe {
            run_guest_app_init(
                mock_get_proc_address,
                &api,
                &mut world as *mut MockHostWorld as *mut c_void,
                move |app| {
                    let _ = app.alloc_user_data(DropTracker::new(
                        "panic_staged_ctx",
                        &live_clone,
                        &order_clone,
                    ));
                    panic!("simulated init panic");
                },
            )
        };
        assert_eq!(res, FfiResult::PANIC_CAUGHT);
        assert_eq!(
            live_count.load(Ordering::SeqCst),
            0,
            "panicking init must roll back all staged allocations with 0 leaks"
        );
    }
}

#[test]
fn test_reentrant_closure_invocation_returns_access_conflict() {
    let _guard = LIFECYCLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cleanup_registered_guest_state();

    let mut api = mock_host_api();
    api.register_system = world_register_system;
    let mut world = MockHostWorld::new();
    let world_ptr = &mut world as *mut MockHostWorld;

    let world_addr = world_ptr as usize;
    let setup = move |app: &mut GuestApp<'_>| {
        app.add_system(c"reentrant_probe", &[], move |_ctx| {
            // Attempt illegal re-entrant invocation of the same system closure while locked
            let w = unsafe { &*(world_addr as *const MockHostWorld) };
            let (_name, cb, ud) = &w.registered_single[0];
            let nested = unsafe { (*cb)(core::ptr::null_mut(), core::ptr::null_mut(), *ud) };
            assert_eq!(nested, FfiResult::ACCESS_CONFLICT);
            FfiResult::OK
        })
    };

    let init_res =
        unsafe { run_guest_app_init(mock_get_proc_address, &api, world_ptr as *mut c_void, setup) };
    assert_eq!(init_res, FfiResult::OK);

    let (_name, cb, ud) = world.registered_single[0].clone();
    let outer = unsafe { cb(core::ptr::null_mut(), core::ptr::null_mut(), ud) };
    assert_eq!(outer, FfiResult::OK);

    cleanup_registered_guest_state();
}

// ============================================================================
// 5. Asymmetric C99 & Asymmetric Rust Guest Mod Build Verification
// ============================================================================

#[test]
fn test_asymmetric_c99_and_rust_mods_compile_cleanly() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists");

    let c_mod_dir = repo_root.join("tests").join("asymmetric_c_mod");
    let make_status = Command::new("make")
        .arg("-C")
        .arg(&c_mod_dir)
        .status()
        .expect("failed to invoke make for asymmetric_c_mod");
    assert!(make_status.success(), "asymmetric_c_mod C99 build failed");

    let rust_mod_manifest = repo_root
        .join("tests")
        .join("asymmetric_rust_mod")
        .join("Cargo.toml");
    let cargo_bin = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let rust_status = Command::new(cargo_bin)
        .arg("build")
        .arg("--offline")
        .arg("--manifest-path")
        .arg(&rust_mod_manifest)
        .status()
        .expect("failed to invoke cargo build for asymmetric_rust_mod");
    assert!(
        rust_status.success(),
        "asymmetric_rust_mod cdylib build failed"
    );
}
