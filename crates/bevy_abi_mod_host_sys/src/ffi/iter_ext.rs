//! Extended iterator entrypoints: lifecycle `RemovedComponents<T>` batch reading
//! (`iter_read_removed`) and zero-arena read-only parallel chunk dispatch
//! (`par_for_each_chunk_readonly`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{AccessMode, BamChunkTaskFn, BamIter, FfiEntity, FfiResult};
use bevy_ecs::entity::Entity;
use bevy_ecs::lifecycle::RemovedComponentEntity;
use bevy_ecs::message::MessageCursor;

use crate::{
    ffi::iter::{HostIter, par_for_each_chunk_impl},
    handle_header::{ITER_MAGIC, validate_handle_mut},
};

/// Reads up to `max_count` removed-component [`FfiEntity`] values for `term_index` (`REMOVED_READ`).
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_read` must point to writable `usize` storage, or null.
/// - When `max_count > 0`, `out_entities` must point to `max_count` writable [`FfiEntity`] slots.
pub unsafe extern "C" fn host_iter_read_removed(
    iter: *mut BamIter,
    term_index: usize,
    out_entities: *mut FfiEntity,
    max_count: usize,
    out_read: *mut usize,
) -> FfiResult {
    if out_read.is_null() || (max_count > 0 && out_entities.is_null()) {
        return FfiResult::NULL_POINTER;
    }
    if !out_read.is_aligned() || (max_count > 0 && !out_entities.is_aligned()) {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_read` is non-null, aligned, and writable.
    unsafe {
        *out_read = 0;
    }
    // SAFETY: Validates `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::REMOVED_READ || term.is_resource() || term.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    if max_count == 0 {
        return FfiResult::OK;
    }
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    let Some(ref mut cursors) = host_iter.msg_cursors else {
        return FfiResult::INVALID_STATE;
    };
    let Some(Some(cursor_box)) = cursors.get_mut(term_index) else {
        return FfiResult::INVALID_STATE;
    };
    let Some(cursor) = cursor_box.downcast_mut::<MessageCursor<RemovedComponentEntity>>() else {
        return FfiResult::INVALID_STATE;
    };
    let Some(messages) = world.removed_components().get(term.component_id) else {
        return FfiResult::OK;
    };

    let mut written = 0usize;
    for removed in cursor.read(messages).take(max_count) {
        let entity: Entity = removed.clone().into();
        // SAFETY: `out_entities` is non-null, aligned, and caller guarantees `max_count` writable
        // `FfiEntity` slots while `written < max_count`.
        unsafe {
            core::ptr::write(
                out_entities.add(written),
                FfiEntity::from_bits(entity.to_bits()),
            );
        }
        written += 1;
    }
    // SAFETY: `out_read` is non-null, aligned, and writable.
    unsafe {
        *out_read = written;
    }
    FfiResult::OK
}

/// Core implementation backing ABI `0.3.0` [`crate::host_par_for_each_chunk_readonly`].
///
/// Enforces that every declared term in `iter`'s query plan is strictly read-only (rejecting any
/// `WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`, or `MESSAGE_WRITE` term with
/// [`FfiResult::PERMISSION_DENIED`]), then dispatches disjoint table/archetype sub-ranges across
/// Bevy's [`bevy_tasks::ComputeTaskPool`] with zero command-arena allocations and zero post-join
/// command-buffer merges.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   and must never unwind across `extern "C"`.
pub(crate) unsafe fn par_for_each_chunk_readonly_impl(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Forwarding under identical FFI safety contract with `require_readonly = true`.
    unsafe { par_for_each_chunk_impl(iter, chunk_fn, user_data, true) }
}

#[cfg(test)]
mod tests {
    use super::par_for_each_chunk_readonly_impl;
    use crate::ffi::iter::{
        HostIter, QueryPlan, QueryTermMeta, SparseRowScratch, host_iter_column_info,
        host_iter_column_readonly, host_iter_column_rows_readonly, host_iter_entities,
        host_iter_next_chunk,
    };
    use bevy_abi_mod_sys::{
        AccessMode, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
        BAM_STORAGE_FLAG_SPARSE_SET, BamColumnInfo, BamComponent, BamIter, ComponentUuid,
        FfiResult,
    };
    use bevy_ecs::{change_detection::Tick, component::Component, world::World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    #[repr(C)]
    #[derive(Component, Clone, Copy, Debug)]
    struct ParRoVal(i64);

    #[repr(C)]
    #[derive(Component, Clone, Copy, Debug)]
    #[component(storage = "SparseSet")]
    struct ParRoSparse(i64);

    struct SumCtx {
        sum: AtomicI64,
        count: AtomicUsize,
        nested_rc: AtomicUsize,
        fail_after: usize,
    }

    unsafe extern "C" fn dense_sum_cb(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to a live `SumCtx` across the synchronous `par_for_each_chunk_readonly` scope.
        let ctx = unsafe { &*ud.cast::<SumCtx>() };
        // Nested call must be rejected with `ACCESS_CONFLICT`.
        // SAFETY: `sub_iter` is an active worker `BamIter`.
        let nested = unsafe { par_for_each_chunk_readonly_impl(sub_iter, Some(noop_cb), ud) };
        ctx.nested_rc.store(nested.0 as usize, Ordering::Relaxed);

        let mut info = BamColumnInfo {
            component: BamComponent::INVALID,
            mode: AccessMode::READ,
            uuid: ComponentUuid::from_bytes([0; 16]),
            size: 0,
            align: 0,
            storage_flags: 0,
            reserved0: 0,
        };
        // SAFETY: `sub_iter` and `info` are valid.
        let info_rc = unsafe { host_iter_column_info(sub_iter.cast_const(), 0, &raw mut info) };
        assert!(info_rc.is_ok(), "host_iter_column_info failed: {info_rc}");
        assert_eq!(info.size, core::mem::size_of::<ParRoVal>());

        let mut len = 0usize;
        // SAFETY: `sub_iter` is an active worker `BamIter`.
        while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
            // SAFETY: `sub_iter` is positioned on a valid chunk of length `len`.
            let col = unsafe { host_iter_column_readonly(sub_iter.cast_const(), 0) };
            // SAFETY: `sub_iter` is positioned on a valid chunk of length `len`.
            let ents = unsafe { host_iter_entities(sub_iter.cast_const()) };
            assert!(!col.is_null() && !ents.is_null());
            // SAFETY: `col` points to `len` contiguous `ParRoVal` elements aligned to `align_of::<ParRoVal>()`.
            #[allow(clippy::cast_ptr_alignment)]
            let slice = unsafe { core::slice::from_raw_parts(col.cast::<ParRoVal>(), len) };
            let mut local_sum = 0i64;
            for v in slice {
                local_sum += v.0;
            }
            ctx.sum.fetch_add(local_sum, Ordering::Relaxed);
            let prev = ctx.count.fetch_add(len, Ordering::Relaxed);
            if ctx.fail_after > 0 && prev + len >= ctx.fail_after {
                return FfiResult::PANIC_CAUGHT;
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn sparse_sum_cb(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to a live `SumCtx`.
        let ctx = unsafe { &*ud.cast::<SumCtx>() };
        let mut len = 0usize;
        // SAFETY: `sub_iter` is an active worker `BamIter`.
        while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
            // SAFETY: `sub_iter` is positioned on a valid archetypal chunk.
            let rows = unsafe { host_iter_column_rows_readonly(sub_iter, 0) };
            assert!(!rows.is_null());
            // SAFETY: `rows` points to `len` non-null `*const u8` pointers to `ParRoSparse`.
            let row_ptrs = unsafe { core::slice::from_raw_parts(rows, len) };
            let mut local_sum = 0i64;
            for &p in row_ptrs {
                // SAFETY: Each row pointer targets a live, properly aligned `ParRoSparse` component.
                #[allow(clippy::cast_ptr_alignment)]
                let val = unsafe { (*p.cast::<ParRoSparse>()).0 };
                local_sum += val;
            }
            ctx.sum.fetch_add(local_sum, Ordering::Relaxed);
            ctx.count.fetch_add(len, Ordering::Relaxed);
        }
        FfiResult::OK
    }

    unsafe extern "C" fn noop_cb(_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    #[test]
    fn par_readonly_dense_sparse_permissions_and_error_propagation() {
        let mut world = World::new();
        let val_cid = world.register_component::<ParRoVal>();
        let sparse_cid = world.register_component::<ParRoSparse>();

        let mut expected_dense = 0i64;
        for i in 1..=600i64 {
            world.spawn(ParRoVal(i));
            expected_dense += i;
        }
        let mut expected_sparse = 0i64;
        for i in 1..=300i64 {
            world.spawn(ParRoSparse(i * 3));
            expected_sparse += i * 3;
        }

        let dense_tid = world
            .archetypes()
            .iter()
            .find(|a| a.contains(val_cid))
            .unwrap()
            .table_id();
        let sparse_aid = world
            .archetypes()
            .iter()
            .find(|a| a.contains(sparse_cid))
            .unwrap()
            .id();

        let mk_term = |cid, mode, flags, size| QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: cid,
            mode,
            uuid: ComponentUuid::from_canonical_name("iter_ext::test"),
            size,
            align: 8,
            storage_flags: flags,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        };

        // 1. Dense read-only parity + nested `ACCESS_CONFLICT`.
        let ro_plan = QueryPlan::from_terms(vec![mk_term(
            val_cid,
            AccessMode::READ,
            0,
            core::mem::size_of::<ParRoVal>(),
        )]);
        let tids = [dense_tid];
        let mut iter = HostIter::new_dense(
            &world.storages().tables,
            &tids,
            &ro_plan,
            Tick::new(0),
            Tick::new(1),
        );
        let ctx = SumCtx {
            sum: AtomicI64::new(0),
            count: AtomicUsize::new(0),
            nested_rc: AtomicUsize::new(0),
            fail_after: 0,
        };
        // SAFETY: `iter` and `ctx` are live on stack across the synchronous call.
        let rc = unsafe {
            par_for_each_chunk_readonly_impl(
                iter.as_ffi_ptr(),
                Some(dense_sum_cb),
                (&raw const ctx).cast_mut().cast::<c_void>(),
            )
        };
        assert_eq!(rc, FfiResult::OK);
        assert_eq!(ctx.count.load(Ordering::Relaxed), 600);
        assert_eq!(ctx.sum.load(Ordering::Relaxed), expected_dense);
        assert_eq!(
            ctx.nested_rc.load(Ordering::Relaxed),
            FfiResult::ACCESS_CONFLICT.0 as usize
        );
        assert_eq!(iter.stats().1, 600);

        // 2. Error propagation stops dispatch and surfaces first non-OK code.
        iter.reset();
        let err_ctx = SumCtx {
            sum: AtomicI64::new(0),
            count: AtomicUsize::new(0),
            nested_rc: AtomicUsize::new(0),
            fail_after: 100,
        };
        // SAFETY: `iter` and `err_ctx` are live on stack.
        let err_rc = unsafe {
            par_for_each_chunk_readonly_impl(
                iter.as_ffi_ptr(),
                Some(dense_sum_cb),
                (&raw const err_ctx).cast_mut().cast::<c_void>(),
            )
        };
        assert_eq!(err_rc, FfiResult::PANIC_CAUGHT);

        // 3. Sparse-set read-only parallel parity.
        let sparse_plan = QueryPlan::from_terms(vec![mk_term(
            sparse_cid,
            AccessMode::READ,
            BAM_STORAGE_FLAG_SPARSE_SET,
            core::mem::size_of::<ParRoSparse>(),
        )]);
        let aids = [sparse_aid];
        let mut scratch = SparseRowScratch::default();
        let mut sparse_iter = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &aids,
            &sparse_plan,
            &mut scratch,
            Tick::new(0),
            Tick::new(1),
        );
        let sparse_ctx = SumCtx {
            sum: AtomicI64::new(0),
            count: AtomicUsize::new(0),
            nested_rc: AtomicUsize::new(0),
            fail_after: 0,
        };
        // SAFETY: `sparse_iter` and `sparse_ctx` are live on stack.
        let sparse_rc = unsafe {
            par_for_each_chunk_readonly_impl(
                sparse_iter.as_ffi_ptr(),
                Some(sparse_sum_cb),
                (&raw const sparse_ctx).cast_mut().cast::<c_void>(),
            )
        };
        assert_eq!(sparse_rc, FfiResult::OK);
        assert_eq!(sparse_ctx.count.load(Ordering::Relaxed), 300);
        assert_eq!(sparse_ctx.sum.load(Ordering::Relaxed), expected_sparse);

        // 4. Reject EVERY mutable AccessMode (`WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`, `MESSAGE_WRITE`).
        for (mode, flags) in [
            (AccessMode::WRITE, 0),
            (AccessMode::OPTIONAL_WRITE, 0),
            (AccessMode::RESOURCE_WRITE, BAM_STORAGE_FLAG_RESOURCE),
            (AccessMode::MESSAGE_WRITE, BAM_STORAGE_FLAG_MESSAGE),
        ] {
            let plan = QueryPlan::from_terms(vec![mk_term(val_cid, mode, flags, 8)]);
            let mut mut_iter = HostIter::new_dense(
                &world.storages().tables,
                &tids,
                &plan,
                Tick::new(0),
                Tick::new(1),
            );
            // SAFETY: Valid stack `HostIter` and callback.
            let deny = unsafe {
                par_for_each_chunk_readonly_impl(
                    mut_iter.as_ffi_ptr(),
                    Some(noop_cb),
                    core::ptr::null_mut(),
                )
            };
            assert_eq!(deny, FfiResult::PERMISSION_DENIED, "mode={mode:?}");
        }

        // 5. Exercise `noop_cb` directly on read-only iter plus null `chunk_fn` / null `iter` guards.
        iter.reset();
        // SAFETY: `iter` is valid and read-only; intentional null guards check early returns.
        unsafe {
            assert_eq!(
                par_for_each_chunk_readonly_impl(
                    iter.as_ffi_ptr(),
                    Some(noop_cb),
                    core::ptr::null_mut(),
                ),
                FfiResult::OK
            );
            assert_eq!(
                par_for_each_chunk_readonly_impl(iter.as_ffi_ptr(), None, core::ptr::null_mut()),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                par_for_each_chunk_readonly_impl(
                    core::ptr::null_mut(),
                    Some(noop_cb),
                    core::ptr::null_mut(),
                ),
                FfiResult::NULL_POINTER
            );
        }
    }

    #[test]
    #[allow(clippy::cast_ptr_alignment)]
    fn iter_read_removed_guards_and_batch_read() {
        use super::host_iter_read_removed;
        use bevy_abi_mod_sys::FfiEntity;
        use bevy_ecs::entity::Entity;
        use bevy_ecs::lifecycle::RemovedComponentEntity;
        use bevy_ecs::message::MessageCursor;

        let mut world = World::new();
        let val_cid = world.register_component::<ParRoVal>();
        let mk_term = |cid, mode, flags| QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: cid,
            mode,
            uuid: ComponentUuid::from_canonical_name("iter_ext::removed"),
            size: core::mem::size_of::<ParRoVal>(),
            align: 8,
            storage_flags: flags,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        };
        let plan = QueryPlan::from_terms(vec![
            mk_term(val_cid, AccessMode::REMOVED_READ, 0),
            mk_term(val_cid, AccessMode::READ, 0),
            mk_term(val_cid, AccessMode::REMOVED_READ, BAM_STORAGE_FLAG_RESOURCE),
        ]);

        let mut unaligned_buf = [0u64; 2];
        let unaligned = (&raw mut unaligned_buf).cast::<u8>().wrapping_add(1);
        let mut out_entities = [FfiEntity::NULL; 4];
        let mut out_read = 0usize;

        let mut no_world_iter = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        );
        // SAFETY: Valid stack buffers and intentionally misaligned pointers (checked via `is_aligned()`).
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    core::ptr::null_mut(),
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    core::ptr::null_mut(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    unaligned.cast::<usize>(),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    unaligned.cast::<FfiEntity>(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_iter_read_removed(
                    core::ptr::null_mut(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::NULL_POINTER
            );

            no_world_iter.in_par_iter = true;
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::ACCESS_CONFLICT
            );
            no_world_iter.in_par_iter = false;

            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    9,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::OUT_OF_RANGE
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    1,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    2,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    core::ptr::null_mut(),
                    0,
                    &raw mut out_read,
                ),
                FfiResult::OK
            );
            // Missing `world`
            assert_eq!(
                host_iter_read_removed(
                    no_world_iter.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::INVALID_STATE
            );
        }
        drop(no_world_iter);

        // With `world` but missing `msg_cursors`, empty `msg_cursors`, wrong cursor type, then real cursor.
        let mut iter_no_cursors = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly());
        // SAFETY: Valid `HostIter` and stack buffers.
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    iter_no_cursors.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::INVALID_STATE
            );
        }
        drop(iter_no_cursors);

        let mut wrong_cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> =
            vec![Some(Box::new(123u32))];
        {
            let mut iter_wrong = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly())
            .with_msg_cursors(&mut wrong_cursors);
            // SAFETY: Valid `HostIter` and stack buffers.
            unsafe {
                assert_eq!(
                    host_iter_read_removed(
                        iter_wrong.as_ffi_ptr(),
                        0,
                        out_entities.as_mut_ptr(),
                        4,
                        &raw mut out_read,
                    ),
                    FfiResult::INVALID_STATE
                );
            }
        }
        wrong_cursors[0] = None;
        {
            let mut iter_none = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly())
            .with_msg_cursors(&mut wrong_cursors);
            // SAFETY: Valid `HostIter` and stack buffers.
            unsafe {
                assert_eq!(
                    host_iter_read_removed(
                        iter_none.as_ffi_ptr(),
                        0,
                        out_entities.as_mut_ptr(),
                        4,
                        &raw mut out_read,
                    ),
                    FfiResult::INVALID_STATE
                );
            }
        }

        // Valid cursor before any entity has removed components -> OK with 0 read.
        let mut real_cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> = vec![Some(
            Box::new(MessageCursor::<RemovedComponentEntity>::default()),
        )];
        {
            let mut iter_empty = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly())
            .with_msg_cursors(&mut real_cursors);
            // SAFETY: Valid `HostIter` and stack buffers.
            unsafe {
                assert_eq!(
                    host_iter_read_removed(
                        iter_empty.as_ffi_ptr(),
                        0,
                        out_entities.as_mut_ptr(),
                        4,
                        &raw mut out_read,
                    ),
                    FfiResult::OK
                );
                assert_eq!(out_read, 0);
            }
        }

        // Despawn two entities with `ParRoVal` and read removed entities cleanly.
        let e1 = world.spawn(ParRoVal(10)).id();
        let e2 = world.spawn(ParRoVal(20)).id();
        world.despawn(e1);
        world.despawn(e2);
        {
            let mut iter_live = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly())
            .with_msg_cursors(&mut real_cursors);
            // SAFETY: Valid `HostIter` and stack buffers.
            unsafe {
                assert_eq!(
                    host_iter_read_removed(
                        iter_live.as_ffi_ptr(),
                        0,
                        out_entities.as_mut_ptr(),
                        4,
                        &raw mut out_read,
                    ),
                    FfiResult::OK
                );
                assert_eq!(out_read, 2);
                assert_eq!(
                    [
                        Entity::try_from_bits(out_entities[0].to_bits()).unwrap(),
                        Entity::try_from_bits(out_entities[1].to_bits()).unwrap(),
                    ],
                    [e1, e2]
                );
            }
        }
    }
}
