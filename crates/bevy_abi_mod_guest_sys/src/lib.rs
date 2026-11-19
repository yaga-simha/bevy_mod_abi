//! Low-level `unsafe` FFI internals for `bevy_abi_mod` Rust guest modules (ABI `0.2`).
//!
//! Isolates every guest-side `unsafe` block behind documented `# Safety` contracts:
//! - Per-instance [`Host`] API table copy, SemVer validation, `"bam.v0.*"` custom symbol lookup,
//!   and [`VTableHeader`](bevy_abi_mod_sys::VTableHeader) verification.
//! - Per-World [`GuestInstance`] closure/descriptor ownership and LIFO teardown with zero
//!   process-global mutable state.
//! - Lock-free [`ReentrantCell`] (`UnsafeCell` + `AtomicBool` `Acquire`/`Release` guard) and
//!   panic-catching `extern "C"` trampolines that drop unwind payloads on the guest heap.
//! - Validated dense column slices, indirect sparse-set row views, untracked mutation with
//!   `mark_changed_range`, declaration-indexed singleton resources, and handle/batched commands.
//!
//! Mod authors should depend on `bevy_abi_mod_guest` (`#![forbid(unsafe_code)]`) rather than
//! calling this `-sys` crate directly.

#![warn(missing_docs)]
#![deny(
    unsafe_op_in_unsafe_fn,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]

pub mod commands;
pub mod component;
pub mod host;
pub mod instance;
pub mod iter;
pub mod panic;
pub mod trampoline;

pub use bevy_abi_mod_sys::{BamSystemCtx, FfiBool, FfiResult};

pub use commands::{SpawnBatchColumn, SpawnEntry, ValidatedCommands};
pub use component::{
    PodComponentHooks, generic_drop_trampoline, pod_clone_trampoline, register_component_raw,
    register_component_with_layout, register_dynamic_component_with_drop, register_pod_component,
    register_pod_component_with_hooks, resolve_component,
};
pub use host::{
    CUSTOM_SYMBOL_PREFIX, Host, ValidatedLoadCtx, ValidatedStringVTable, ValidatedWireVTable,
};
pub use instance::{
    GuestInstance, guest_restore_state_trampoline, guest_save_state_trampoline,
    init_guest_instance, qualify_mod_component_identity, resolve_guest_instance_symbol,
    resolve_mod_component, shutdown_guest_instance,
};
pub use iter::{
    ChunkColumnDispenser, SparseRows, SparseRowsIter, SparseRowsMut, UuidFilter, ValidatedIter,
    ValidatedSlice, ValidatedSliceMut,
};
pub use panic::{catch_guest_bool, catch_guest_result, catch_guest_unit};
pub use trampoline::{
    PreparedRunIf, ReentrantCell, SystemRegistrationConfig, ValidatedSystemInvocation,
    ValidatedSystemRef, allocate_run_if_trampoline, allocate_system_trampoline, iter_dynamic_slice,
    iter_dynamic_slice_mut, iter_entities_and_dynamic_slice_mut,
    register_lifecycle_observer_closure, register_observer_closure,
    register_observer_closure_with_options, register_observer_mut_closure, register_system_closure,
    verify_same_mod_dynamic_component,
};

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_sys::{
        ABI_VERSION, AbiPod, AccessMode, BAM_STORAGE_FLAG_SPARSE_SET, BamColumnInfo, BamCommands,
        BamComponent, BamHostApi, BamHostCtx, BamInstance, BamIter, BamLoadCtx, BamManifest,
        BamRunIfFn, BamSystemCtx, BamSystemDesc, BamSystemFn, ComponentDesc, ComponentUuid,
        FfiEntity, RawInsertable, VTableHeader, pack_abi_version,
    };
    use core::{
        ffi::{CStr, c_char, c_void},
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PaddedPod {
        tag: u8,
        // 3 bytes of natural C alignment padding (uninitialized on stack if constructed by literal)
        val: u32,
    }
    // SAFETY: `#[repr(C)]` struct of fixed-width `AbiPod` primitives with no drop glue.
    unsafe impl RawInsertable for PaddedPod {}
    // SAFETY: Deterministic C layout and all fields are `AbiPod`.
    unsafe impl AbiPod for PaddedPod {}

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DummyVTable {
        header: VTableHeader,
        getter: Option<unsafe extern "C" fn(*const c_void) -> u32>,
    }

    unsafe extern "C" fn dummy_getter(_ptr: *const c_void) -> u32 {
        42
    }

    static VALID_VTABLE_UUID: ComponentUuid = ComponentUuid::from_parts("mock::Dummy", 8, 4, 0);

    static VALID_VTABLE: DummyVTable = DummyVTable {
        header: VTableHeader::from_raw_parts(
            core::mem::size_of::<DummyVTable>() as u32,
            VALID_VTABLE_UUID,
            8,
            4,
        ),
        getter: Some(dummy_getter),
    };

    static TRUNCATED_HEADER: VTableHeader = VTableHeader {
        struct_size: VTableHeader::MIN_V0_2_SIZE as u32,
        abi_version: ABI_VERSION,
        uuid: VALID_VTABLE_UUID,
        size: 8,
        align: 4,
    };

    static WRONG_VERSION_VTABLE: DummyVTable = DummyVTable {
        header: VTableHeader {
            struct_size: core::mem::size_of::<DummyVTable>() as u32,
            abi_version: pack_abi_version(0, 1, 0),
            uuid: VALID_VTABLE_UUID,
            size: 8,
            align: 4,
        },
        getter: Some(dummy_getter),
    };

    unsafe extern "C" fn custom_math_fn(x: u32) -> u32 {
        x + 10
    }

    struct MockWorldCtx {
        error_msg: &'static [u8],
        spawned: usize,
        last_inserted_val: u32,
    }

    unsafe extern "C" fn mock_get_proc_address(
        _ctx: *const BamHostCtx,
        name: *const c_char,
    ) -> *const c_void {
        if name.is_null() {
            return core::ptr::null();
        }
        // SAFETY: Test passes valid NUL-terminated C strings.
        let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
        match bytes {
            b"bam.v0.mock::Dummy.vtable" => (&raw const VALID_VTABLE).cast::<c_void>(),
            b"bam.v0.mock::Truncated.vtable" => (&raw const TRUNCATED_HEADER).cast::<c_void>(),
            b"bam.v0.mock::WrongVer.vtable" => (&raw const WRONG_VERSION_VTABLE).cast::<c_void>(),
            b"bam.v0.math.add10" => custom_math_fn as *const c_void,
            _ => core::ptr::null(),
        }
    }

    unsafe extern "C" fn mock_last_error(
        ctx: *const BamHostCtx,
        buf: *mut c_char,
        cap: usize,
    ) -> usize {
        // SAFETY: `ctx` points to `MockWorldCtx` in this unit test.
        let world = unsafe { &*ctx.cast::<MockWorldCtx>() };
        let msg = world.error_msg;
        if !buf.is_null() && cap > 0 {
            let copy_len = msg.len().min(cap - 1);
            // SAFETY: `buf` has `cap` writable bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(msg.as_ptr(), buf.cast::<u8>(), copy_len);
                *buf.add(copy_len) = 0;
            }
        }
        msg.len()
    }

    unsafe extern "C" fn mock_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
        load_ctx.cast::<BamHostCtx>()
    }

    unsafe extern "C" fn mock_register_component(
        _load_ctx: *mut BamLoadCtx,
        desc: *const ComponentDesc,
        out: *mut BamComponent,
    ) -> FfiResult {
        if desc.is_null() || out.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `out` is writable.
        unsafe { *out = BamComponent::from_raw(1) };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_resolve_component(
        _ctx: *const BamHostCtx,
        _name: *const c_char,
        _uuid: *const ComponentUuid,
        out: *mut BamComponent,
    ) -> FfiResult {
        // SAFETY: `out` is writable.
        unsafe { *out = BamComponent::from_raw(2) };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_register_system(
        _load_ctx: *mut BamLoadCtx,
        desc: *const BamSystemDesc,
    ) -> FfiResult {
        if desc.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `desc` is valid.
        unsafe { (*desc).validate() }
    }

    unsafe extern "C" fn mock_sys_host_ctx(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
        sys_ctx.cast::<BamHostCtx>()
    }

    unsafe extern "C" fn mock_sys_query(
        _sys_ctx: *mut BamSystemCtx,
        _query_index: usize,
        out_iter: *mut *mut BamIter,
    ) -> FfiResult {
        // SAFETY: `out_iter` is writable.
        unsafe { *out_iter = core::ptr::null_mut() };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_sys_commands(
        sys_ctx: *mut BamSystemCtx,
        out_commands: *mut *mut BamCommands,
    ) -> FfiResult {
        // SAFETY: `out_commands` is writable; forward `sys_ctx` cast for mock commands.
        unsafe { *out_commands = sys_ctx.cast::<BamCommands>() };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_iter_next_chunk(
        _iter: *mut BamIter,
        _out_len: *mut usize,
    ) -> FfiBool {
        FfiBool::FALSE
    }
    unsafe extern "C" fn mock_iter_reset(_iter: *mut BamIter) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn mock_iter_entities(_iter: *const BamIter) -> *const FfiEntity {
        core::ptr::null()
    }
    unsafe extern "C" fn mock_iter_col_ro(_iter: *const BamIter, _idx: usize) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn mock_iter_col_mut(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn mock_iter_col_mut_untracked(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn mock_iter_mark_changed(
        _iter: *mut BamIter,
        _idx: usize,
        _start: usize,
        _count: usize,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn mock_iter_rows_ro(_iter: *mut BamIter, _idx: usize) -> *const *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn mock_iter_rows_mut(_iter: *mut BamIter, _idx: usize) -> *const *mut u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn mock_iter_col_info(
        _iter: *const BamIter,
        _idx: usize,
        _out: *mut BamColumnInfo,
    ) -> FfiResult {
        FfiResult::OUT_OF_RANGE
    }
    unsafe extern "C" fn mock_res_ptr(_iter: *const BamIter, _idx: usize) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn mock_res_mut_ptr(_iter: *mut BamIter, _idx: usize) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe extern "C" fn mock_cmd_spawn_empty(
        cmds: *mut BamCommands,
        out_entity: *mut FfiEntity,
    ) -> FfiResult {
        // SAFETY: `cmds` points to `MockWorldCtx`.
        let world = unsafe { &mut *cmds.cast::<MockWorldCtx>() };
        world.spawned += 1;
        // SAFETY: `out_entity` is writable.
        unsafe { *out_entity = FfiEntity::from_bits(world.spawned as u64) };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_cmd_spawn(
        cmds: *mut BamCommands,
        _comps: *const BamComponent,
        data_ptrs: *const *const u8,
        count: usize,
        out_entity: *mut FfiEntity,
    ) -> FfiResult {
        // SAFETY: `cmds` points to `MockWorldCtx`.
        let world = unsafe { &mut *cmds.cast::<MockWorldCtx>() };
        if count > 0 && !data_ptrs.is_null() {
            // Copy into `MaybeUninit<PaddedPod>` so padding bytes remain sound under Miri!
            let mut slot = core::mem::MaybeUninit::<PaddedPod>::uninit();
            // SAFETY: First spawned component in test is `PaddedPod`.
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
        // SAFETY: `out_entity` is writable.
        unsafe { *out_entity = FfiEntity::from_bits(world.spawned as u64) };
        FfiResult::OK
    }

    unsafe extern "C" fn mock_cmd_despawn(_cmds: *mut BamCommands, _e: FfiEntity) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn mock_cmd_insert(
        cmds: *mut BamCommands,
        _e: FfiEntity,
        _comp: BamComponent,
        data_ptr: *const u8,
        data_len: usize,
    ) -> FfiResult {
        if data_len == core::mem::size_of::<PaddedPod>() {
            let mut slot = core::mem::MaybeUninit::<PaddedPod>::uninit();
            // SAFETY: Copying raw bytes preserves uninitialized padding provenance under Miri.
            unsafe {
                core::ptr::copy_nonoverlapping(data_ptr, slot.as_mut_ptr().cast::<u8>(), data_len);
                (*cmds.cast::<MockWorldCtx>()).last_inserted_val = slot.assume_init().val;
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn mock_cmd_insert_default(
        _cmds: *mut BamCommands,
        _e: FfiEntity,
        _c: BamComponent,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn mock_cmd_remove(
        _cmds: *mut BamCommands,
        _e: FfiEntity,
        _c: BamComponent,
    ) -> FfiResult {
        FfiResult::OK
    }

    fn make_mock_api() -> BamHostApi {
        BamHostApi {
            struct_size: core::mem::size_of::<BamHostApi>() as u32,
            abi_version: ABI_VERSION,
            get_proc_address: Some(mock_get_proc_address),
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
            iter_column_readonly: Some(mock_iter_col_ro),
            iter_column_mutable: Some(mock_iter_col_mut),
            iter_column_mutable_untracked: Some(mock_iter_col_mut_untracked),
            iter_mark_changed_range: Some(mock_iter_mark_changed),
            iter_column_rows_readonly: Some(mock_iter_rows_ro),
            iter_column_rows_mutable: Some(mock_iter_rows_mut),
            iter_column_info: Some(mock_iter_col_info),
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

    #[test]
    fn reentrant_trampoline_conflict() {
        let api = make_mock_api();
        let mut world = MockWorldCtx {
            error_msg: b"diagnostic ok",
            spawned: 0,
            last_inserted_val: 0,
        };
        let host_ctx = (&raw mut world).cast::<BamHostCtx>();
        // SAFETY: `api` and `host_ctx` are live stack objects valid for this test.
        let host = unsafe { Host::from_raw(&api, host_ctx) }.unwrap();
        let mut instance = GuestInstance::new(host, "reentrant_mod");

        #[derive(Clone, Copy)]
        struct SendVoidPtr(*mut c_void);
        // SAFETY: Single-threaded test fixture preserving pointer provenance for Miri strict provenance.
        unsafe impl Send for SendVoidPtr {}

        #[derive(Clone, Copy)]
        struct SysCallbackSlot {
            func: Option<BamSystemFn>,
            data: SendVoidPtr,
        }
        let slot = std::sync::Arc::new(std::sync::Mutex::new(SysCallbackSlot {
            func: None,
            data: SendVoidPtr(core::ptr::null_mut()),
        }));
        let slot_in_sys = std::sync::Arc::clone(&slot);
        let mut outer_runs = 0usize;

        let (sys_fn, sys_data) =
            allocate_system_trampoline(&mut instance, 0, 0, None, move |inv| {
                outer_runs += 1;
                if outer_runs == 1 {
                    let copy = *slot_in_sys.lock().unwrap();
                    let nested_fn = copy.func.unwrap();
                    // Re-entrantly invoke the exact same trampoline while `in_use` is held!
                    // SAFETY: Passing active `sys_ctx` and matching `user_data` to test re-entrancy guard.
                    let nested_res = unsafe { nested_fn(inv.as_raw_sys_ctx(), copy.data.0) };
                    assert_eq!(nested_res, FfiResult::ACCESS_CONFLICT);
                    return FfiResult::OK;
                }
                if outer_runs == 2 {
                    panic!("simulated guest panic to verify ReleaseGuard resets in_use");
                }
                FfiResult::OK
            });
        {
            let mut guard = slot.lock().unwrap();
            guard.func = Some(sys_fn);
            guard.data = SendVoidPtr(sys_data);
        }

        let sys_ctx = (&raw mut world).cast::<BamSystemCtx>();
        // 1. Outer call succeeds while nested re-entrant call gets `ACCESS_CONFLICT`.
        // SAFETY: Valid `sys_ctx` and `sys_data`.
        assert_eq!(unsafe { sys_fn(sys_ctx, sys_data) }, FfiResult::OK);
        // 2. Panic is caught as `PANIC_CAUGHT` and `ReleaseGuard` resets `in_use` to `false`.
        // SAFETY: Valid `sys_ctx` and `sys_data`.
        let panic_res = unsafe { sys_fn(sys_ctx, sys_data) };
        assert_eq!(panic_res, FfiResult::PANIC_CAUGHT);
        // 3. Subsequent tick runs normally (never permanently poisoned).
        // SAFETY: Valid `sys_ctx` and `sys_data`.
        assert_eq!(unsafe { sys_fn(sys_ctx, sys_data) }, FfiResult::OK);

        // Also verify `BamRunIfFn` re-entrancy returns `FfiBool::FALSE`.
        #[derive(Clone, Copy)]
        struct RunIfSlot {
            func: Option<BamRunIfFn>,
            data: SendVoidPtr,
        }
        let pred_slot = std::sync::Arc::new(std::sync::Mutex::new(RunIfSlot {
            func: None,
            data: SendVoidPtr(core::ptr::null_mut()),
        }));
        let pred_slot_inner = std::sync::Arc::clone(&pred_slot);
        let (run_if_fn, run_if_data) = allocate_run_if_trampoline(&mut instance, move |_host| {
            let copy = *pred_slot_inner.lock().unwrap();
            let nested = copy.func.unwrap();
            // SAFETY: Invoking `run_if_fn` re-entrantly with its own `user_data`.
            let nested_bool = unsafe { nested(copy.data.0) };
            assert_eq!(nested_bool, FfiBool::FALSE);
            true
        });
        {
            let mut guard = pred_slot.lock().unwrap();
            guard.func = Some(run_if_fn);
            guard.data = SendVoidPtr(run_if_data);
        }
        // SAFETY: Valid `run_if_data`.
        assert_eq!(unsafe { run_if_fn(run_if_data) }, FfiBool::TRUE);
    }

    #[test]
    fn vtable_header_and_custom_symbol_validation_under_miri() {
        let api = make_mock_api();
        let mut world = MockWorldCtx {
            error_msg: b"sample host error",
            spawned: 0,
            last_inserted_val: 0,
        };
        let host_ctx = (&raw mut world).cast::<BamHostCtx>();
        // SAFETY: Valid stack `api` and `host_ctx`.
        let host = unsafe { Host::from_raw(&api, host_ctx) }.unwrap();
        assert_eq!(host.last_error(), "sample host error");

        // Reject non-`bam.v0.` symbol lookups.
        assert_eq!(
            host.resolve_custom_symbol(c"bevy_abi_mod_iter_reset"),
            Err(FfiResult::INVALID_ARGUMENT)
        );
        // Resolve valid `bam.v0.*` function.
        // SAFETY: `custom_math_fn` has signature `unsafe extern "C" fn(u32) -> u32`.
        let add10: unsafe extern "C" fn(u32) -> u32 =
            unsafe { host.resolve_custom_fn(c"bam.v0.math.add10") }.unwrap();
        // SAFETY: Pure integer addition.
        assert_eq!(unsafe { add10(32) }, 42);

        // Valid VTable succeeds.
        // SAFETY: `VALID_VTABLE` is a static `DummyVTable` prefixed by `VTableHeader`.
        let vt = unsafe {
            host.resolve_vtable::<DummyVTable>(c"bam.v0.mock::Dummy.vtable", VALID_VTABLE_UUID)
        }
        .unwrap();
        // SAFETY: `dummy_getter` ignores its pointer argument.
        assert_eq!(unsafe { (vt.getter.unwrap())(core::ptr::null()) }, 42);

        // Mismatched UUID -> TYPE_MISMATCH.
        let wrong_uuid = ComponentUuid::from_u128(0xDEAD_BEEF);
        // SAFETY: Testing header rejection path.
        let wrong_uuid_res =
            unsafe { host.resolve_vtable::<DummyVTable>(c"bam.v0.mock::Dummy.vtable", wrong_uuid) };
        assert_eq!(wrong_uuid_res.err(), Some(FfiResult::TYPE_MISMATCH));

        // Truncated VTable (`struct_size < size_of::<DummyVTable>()`) -> INVALID_VERSION without UB!
        // SAFETY: `read_descriptor::<VTableHeader>` reads only `VTableHeader` and rejects before `&DummyVTable`.
        let truncated_res = unsafe {
            host.resolve_vtable::<DummyVTable>(c"bam.v0.mock::Truncated.vtable", VALID_VTABLE_UUID)
        };
        assert_eq!(truncated_res.err(), Some(FfiResult::INVALID_VERSION));

        // Incompatible ABI version -> INVALID_VERSION.
        // SAFETY: Valid `DummyVTable` allocation with 0.1 version header.
        let wrong_ver_res = unsafe {
            host.resolve_vtable::<DummyVTable>(c"bam.v0.mock::WrongVer.vtable", VALID_VTABLE_UUID)
        };
        assert_eq!(wrong_ver_res.err(), Some(FfiResult::INVALID_VERSION));
    }

    #[test]
    fn sparse_rows_and_padded_commands_sound_under_miri() {
        let mut r0 = 10u32;
        let mut r1 = 20u32;
        let mut r2 = 30u32;
        let ro_ptrs: [*const u8; 3] = [
            (&raw const r0).cast::<u8>(),
            (&raw const r1).cast::<u8>(),
            (&raw const r2).cast::<u8>(),
        ];
        // SAFETY: `ro_ptrs` points to 3 live `u32` values.
        let rows =
            unsafe { SparseRows::<u32>::from_raw_parts(ro_ptrs.as_ptr(), 3, false) }.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], 10);
        assert_eq!(rows[2], 30);
        let collected: Vec<u32> = rows.iter().copied().collect();
        assert_eq!(collected, vec![10, 20, 30]);

        let mut_ptrs: [*mut u8; 3] = [
            (&raw mut r0).cast::<u8>(),
            (&raw mut r1).cast::<u8>(),
            (&raw mut r2).cast::<u8>(),
        ];
        // SAFETY: `mut_ptrs` points to 3 disjoint mutable `u32` values.
        let mut rows_mut =
            unsafe { SparseRowsMut::<u32>::from_raw_parts(mut_ptrs.as_ptr(), 3, false) }.unwrap();
        rows_mut.for_each_mut(|idx, val| {
            *val += (idx as u32) + 1;
        });
        assert_eq!([r0, r1, r2], [11, 22, 33]);

        // Verify `PaddedPod` insert & batched spawn preserve uninitialized padding without UB.
        let api = make_mock_api();
        let mut world = MockWorldCtx {
            error_msg: b"",
            spawned: 0,
            last_inserted_val: 0,
        };
        let host_ctx = (&raw mut world).cast::<BamHostCtx>();
        // SAFETY: Valid `api` and `host_ctx`.
        let host = unsafe { Host::from_raw(&api, host_ctx) }.unwrap();
        let raw_cmds = (&raw mut world).cast::<BamCommands>();
        // SAFETY: `raw_cmds` points to `world` for mock command callbacks.
        let mut cmds = unsafe { ValidatedCommands::from_raw(raw_cmds, &host) };
        let padded = PaddedPod { tag: 7, val: 999 };
        let comp = BamComponent::from_raw(1);
        let e = cmds.spawn(&[SpawnEntry::new(comp, &padded)]).unwrap();
        assert_eq!(e.to_bits(), 1);
        assert_eq!(world.last_inserted_val, 999);

        let padded2 = PaddedPod { tag: 3, val: 1234 };
        assert_eq!(cmds.insert(e, comp, &padded2), FfiResult::OK);
        assert_eq!(world.last_inserted_val, 1234);
    }

    #[test]
    fn multi_world_guest_instance_isolation_and_rollback() {
        static DROPS_A: AtomicUsize = AtomicUsize::new(0);
        static DROPS_B: AtomicUsize = AtomicUsize::new(0);
        static HOOKS_A: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter(&'static AtomicUsize);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let api = make_mock_api();
        let mut world_a = MockWorldCtx {
            error_msg: b"",
            spawned: 0,
            last_inserted_val: 0,
        };
        let mut world_b = MockWorldCtx {
            error_msg: b"",
            spawned: 0,
            last_inserted_val: 0,
        };
        let manifest = BamManifest::new(c"multi_mod".as_ptr(), c"0.2.0".as_ptr());

        let mut inst_a: *mut BamInstance = core::ptr::null_mut();
        let mut inst_b: *mut BamInstance = core::ptr::null_mut();

        // SAFETY: Valid mock pointers for World A.
        let status_a = unsafe {
            init_guest_instance(
                &api,
                (&raw mut world_a).cast::<BamLoadCtx>(),
                &mut inst_a,
                &manifest,
                |inst, _load| {
                    let _ = inst.track_allocation(Box::new(DropCounter(&DROPS_A)));
                    inst.add_shutdown_hook(|| {
                        HOOKS_A.fetch_add(1, Ordering::SeqCst);
                    });
                    FfiResult::OK
                },
            )
        };
        assert_eq!(status_a, FfiResult::OK);
        assert!(!inst_a.is_null());

        // SAFETY: Valid mock pointers for World B.
        let status_b = unsafe {
            init_guest_instance(
                &api,
                (&raw mut world_b).cast::<BamLoadCtx>(),
                &mut inst_b,
                &manifest,
                |inst, _load| {
                    let _ = inst.track_allocation(Box::new(DropCounter(&DROPS_B)));
                    FfiResult::OK
                },
            )
        };
        assert_eq!(status_b, FfiResult::OK);
        assert!(!inst_b.is_null());
        assert_ne!(inst_a, inst_b);

        // Shut down World A: World A drops, World B stays untouched!
        // SAFETY: `inst_a` came from `init_guest_instance`.
        unsafe { shutdown_guest_instance(inst_a) };
        assert_eq!(HOOKS_A.load(Ordering::SeqCst), 1);
        assert_eq!(DROPS_A.load(Ordering::SeqCst), 1);
        assert_eq!(DROPS_B.load(Ordering::SeqCst), 0);

        // Null shutdown is a no-op.
        // SAFETY: Null is explicitly allowed.
        unsafe { shutdown_guest_instance(core::ptr::null_mut()) };

        // Shut down World B.
        // SAFETY: `inst_b` came from `init_guest_instance`.
        unsafe { shutdown_guest_instance(inst_b) };
        assert_eq!(DROPS_B.load(Ordering::SeqCst), 1);

        // Failed init rolls back allocations immediately without running shutdown hooks.
        static ROLLBACK_DROPS: AtomicUsize = AtomicUsize::new(0);
        static ROLLBACK_HOOKS: AtomicUsize = AtomicUsize::new(0);
        let mut failed_inst: *mut BamInstance = core::ptr::dangling_mut();
        // SAFETY: Valid mock pointers; `failed_inst` must be reset to null on error.
        let status = unsafe {
            init_guest_instance(
                &api,
                (&raw mut world_a).cast::<BamLoadCtx>(),
                &mut failed_inst,
                &manifest,
                |inst, _load| {
                    let _ = inst.track_allocation(Box::new(DropCounter(&ROLLBACK_DROPS)));
                    inst.add_shutdown_hook(|| {
                        ROLLBACK_HOOKS.fetch_add(1, Ordering::SeqCst);
                    });
                    FfiResult::PERMISSION_DENIED
                },
            )
        };
        assert_eq!(status, FfiResult::PERMISSION_DENIED);
        assert!(failed_inst.is_null());
        assert_eq!(ROLLBACK_DROPS.load(Ordering::SeqCst), 1);
        assert_eq!(ROLLBACK_HOOKS.load(Ordering::SeqCst), 0);

        let _ = BAM_STORAGE_FLAG_SPARSE_SET;
        let _ = AccessMode::READ;
    }

    #[test]
    fn wave0_runtime_observer_alloc_tracking_and_unsupported_fallbacks() {
        static OBS_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct ObsDropGuard;
        impl Drop for ObsDropGuard {
            fn drop(&mut self) {
                OBS_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut api = make_mock_api();
        unsafe extern "C" fn mock_cmd_observe(
            _cmds: *mut BamCommands,
            desc: *const bevy_abi_mod_sys::BamObserverDesc,
            out_entity: *mut FfiEntity,
        ) -> FfiResult {
            if desc.is_null() || out_entity.is_null() {
                return FfiResult::NULL_POINTER;
            }
            // SAFETY: `desc` and `out_entity` are valid in this unit test.
            unsafe {
                let st = (*desc).validate();
                if !st.is_ok() {
                    return st;
                }
                *out_entity = FfiEntity::from_bits(77);
            }
            FfiResult::OK
        }
        api.cmd_observe = Some(mock_cmd_observe);

        let mut world = MockWorldCtx {
            error_msg: b"",
            spawned: 0,
            last_inserted_val: 0,
        };
        let manifest = BamManifest::new(c"obs_mod".as_ptr(), c"0.2.0".as_ptr());
        let mut raw_inst: *mut BamInstance = core::ptr::null_mut();

        // SAFETY: Valid mock pointers.
        let status = unsafe {
            init_guest_instance(
                &api,
                (&raw mut world).cast::<BamLoadCtx>(),
                &mut raw_inst,
                &manifest,
                |inst, _load| {
                    let host = *inst.host();
                    // Both typed and untyped (`BamComponent::INVALID`) asset loads forward to the
                    // host slot (returning `UNSUPPORTED` when the mock slot is `None`).
                    assert_eq!(
                        host.asset_load(BamComponent::from_raw(1), "mods://a.png"),
                        Err(FfiResult::UNSUPPORTED)
                    );
                    assert_eq!(
                        host.asset_load(BamComponent::INVALID, "mods://untyped.bsn"),
                        Err(FfiResult::UNSUPPORTED)
                    );
                    // Zero-copy `entity_children` accepts `BamComponent::INVALID` (defaulting to `Children`).
                    let inv = ValidatedSystemInvocation::from_raw(
                        (&raw mut world).cast(),
                        &host,
                        0,
                        0,
                        None,
                    );
                    assert_eq!(
                        inv.system_ref()
                            .entity_children(FfiEntity::from_bits(5), BamComponent::INVALID),
                        Err(FfiResult::UNSUPPORTED)
                    );
                    let mut cmds = ValidatedCommands::from_raw((&raw mut world).cast(), &host);
                    let guard = ObsDropGuard;
                    let ent = cmds
                        .observe::<u32, _>(
                            Some("runtime_obs"),
                            BamComponent::from_raw(3),
                            FfiEntity::NULL,
                            0,
                            BamComponent::INVALID,
                            move |_e, _val, _c| {
                                let _ = &guard;
                                FfiResult::OK
                            },
                        )
                        .unwrap();
                    assert_eq!(ent.to_bits(), 77);
                    assert_eq!(inst.allocation_count(), 2);
                    FfiResult::OK
                },
            )
        };
        assert_eq!(status, FfiResult::OK);
        assert_eq!(OBS_DROPS.load(Ordering::SeqCst), 0);

        // Shutting down the instance drops the runtime observer closure cleanly!
        // SAFETY: `raw_inst` was produced by `init_guest_instance`.
        unsafe { shutdown_guest_instance(raw_inst) };
        assert_eq!(OBS_DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn system_closure_trampoline_refreshes_host_ctx_via_sys_host_ctx() {
        use core::sync::atomic::AtomicPtr;

        static OBSERVED_ADDRS: AtomicUsize = AtomicUsize::new(0);
        static EXPECTED_RUNTIME_PTR: AtomicPtr<BamHostCtx> = AtomicPtr::new(core::ptr::null_mut());

        unsafe extern "C" fn runtime_sys_host_ctx(
            sys_ctx: *const BamSystemCtx,
        ) -> *const BamHostCtx {
            let _ = sys_ctx;
            EXPECTED_RUNTIME_PTR.load(Ordering::SeqCst).cast_const()
        }

        let mut init_ctx_marker = 11u8;
        let mut runtime_ctx_marker = 22u8;
        let init_ctx_ptr = (&raw mut init_ctx_marker).cast::<BamHostCtx>().cast_const();
        let runtime_ctx_mut = (&raw mut runtime_ctx_marker).cast::<BamHostCtx>();
        EXPECTED_RUNTIME_PTR.store(runtime_ctx_mut, Ordering::SeqCst);

        let mut api = make_mock_api();
        api.sys_host_ctx = Some(runtime_sys_host_ctx);

        // SAFETY: `api` and `init_ctx_ptr` are valid stack addresses for the test duration.
        let init_host = unsafe { Host::from_raw(&raw const api, init_ctx_ptr) }.unwrap();
        let mut instance = GuestInstance::new(init_host, "ctx_refresh_mod");

        let (sys_fn, user_data) = allocate_system_trampoline(&mut instance, 0, 0, None, |inv| {
            let observed = inv.host().host_ctx().addr();
            OBSERVED_ADDRS.store(observed, Ordering::SeqCst);
            assert_eq!(inv.commands_mut().host().host_ctx().addr(), observed);
            FfiResult::OK
        });

        let mut dummy_sys_ctx = 99u8;
        // SAFETY: `dummy_sys_ctx` and `user_data` are valid for this synchronous system call.
        let rc = unsafe { sys_fn((&raw mut dummy_sys_ctx).cast::<BamSystemCtx>(), user_data) };
        assert_eq!(rc, FfiResult::OK);
        assert_eq!(
            OBSERVED_ADDRS.load(Ordering::SeqCst),
            runtime_ctx_mut.addr()
        );
        assert_ne!(OBSERVED_ADDRS.load(Ordering::SeqCst), init_ctx_ptr.addr());
    }

    #[test]
    fn register_dynamic_mod_component_symbols_and_moved_command_error_drops_cleanly() {
        use bevy_abi_mod_sys::{ModComponent, SymbolEntry};

        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Default)]
        struct DynMockComp {
            label: String,
        }

        impl Drop for DynMockComp {
            fn drop(&mut self) {
                let _ = self.label.len();
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct DynMockVTable {
            header: VTableHeader,
        }

        static DYN_MOCK_UUID: ComponentUuid = ComponentUuid::from_canonical_name("DynMockComp");
        static DYN_MOCK_VT: DynMockVTable = DynMockVTable {
            header: VTableHeader::from_raw_parts(
                core::mem::size_of::<DynMockVTable>() as u32,
                DYN_MOCK_UUID,
                core::mem::size_of::<DynMockComp>(),
                core::mem::align_of::<DynMockComp>(),
            ),
        };
        static DYN_MOCK_SYMS: [SymbolEntry; 1] = [SymbolEntry {
            name: "bam.v0.DynMockComp.vtable",
            ptr: (&raw const DYN_MOCK_VT).cast::<c_void>(),
        }];

        impl ModComponent for DynMockComp {
            const CANONICAL_NAME: &'static str = "DynMockComp";
            const COMPONENT_CSTR: &'static CStr = c"DynMockComp";
            const UUID: ComponentUuid = DYN_MOCK_UUID;
            type VTable = DynMockVTable;
            fn vtable() -> &'static Self::VTable {
                &DYN_MOCK_VT
            }
            fn symbols() -> &'static [SymbolEntry] {
                &DYN_MOCK_SYMS
            }
        }

        DROP_COUNT.store(0, Ordering::SeqCst);
        let mut dummy_world = 0u64;
        let api = make_mock_api();
        // SAFETY: `api` and `dummy_world` stack pointers remain valid for this test.
        let host =
            unsafe { Host::from_raw(&raw const api, (&raw const dummy_world).cast()) }.unwrap();
        let mut instance = GuestInstance::new(host, "dyn_mod");
        let host_copy = *instance.host();
        // SAFETY: `dummy_world` acts as an opaque non-null mock load context for `mock_register_component`.
        let load_ctx =
            unsafe { ValidatedLoadCtx::from_raw((&raw mut dummy_world).cast(), &host_copy) }
                .unwrap();

        let handle = load_ctx
            .register_dynamic_mod_component::<DynMockComp>(&mut instance, 0)
            .expect("register_dynamic_mod_component succeeds");
        assert!(handle.is_valid());

        let inst_ptr = (&raw const instance).cast::<BamInstance>();
        // SAFETY: `inst_ptr` points to live `instance` and symbol names are NUL-terminated.
        let vt_canon = unsafe {
            resolve_guest_instance_symbol(inst_ptr, c"bam.v0.DynMockComp.vtable".as_ptr())
        };
        // SAFETY: Same as above.
        let vt_qual = unsafe {
            resolve_guest_instance_symbol(inst_ptr, c"bam.v0.dyn_mod::DynMockComp.vtable".as_ptr())
        };
        assert!(core::ptr::eq(vt_canon, DynMockComp::erased_vtable_ptr()));
        assert!(core::ptr::eq(vt_qual, DynMockComp::erased_vtable_ptr()));

        // SAFETY: Same as above.
        let init_ptr = unsafe {
            resolve_guest_instance_symbol(
                inst_ptr,
                c"bam.v0.dyn_mod::DynMockComp.init_default".as_ptr(),
            )
        };
        assert!(!init_ptr.is_null());
        // SAFETY: `init_default` symbol has signature `unsafe extern "C" fn(*mut c_void) -> FfiResult`.
        let init_fn: unsafe extern "C" fn(*mut c_void) -> FfiResult =
            unsafe { core::mem::transmute(init_ptr) };
        let mut uninit_slot = core::mem::MaybeUninit::<DynMockComp>::uninit();
        let init_status =
            // SAFETY: `uninit_slot` has `DynMockComp` layout and alignment.
            unsafe { init_fn(uninit_slot.as_mut_ptr().cast::<c_void>()) };
        assert_eq!(init_status, FfiResult::OK);
        // SAFETY: `init_fn` returned `FfiResult::OK`, initializing `uninit_slot`.
        unsafe { uninit_slot.assume_init_drop() };
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

        // Failed `insert_moved` / `spawn_moved` on an empty/null command buffer must drop `T` cleanly!
        let mut empty_cmds = ValidatedCommands::empty(instance.host());
        let rc = empty_cmds.insert_moved(
            FfiEntity::from_bits(1),
            handle,
            DynMockComp {
                label: String::from("dropped_on_insert_err"),
            },
        );
        assert_eq!(rc, FfiResult::NULL_POINTER);
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 2);

        let spawn_res = empty_cmds.spawn_moved(
            handle,
            DynMockComp {
                label: String::from("dropped_on_spawn_err"),
            },
            &[],
            false,
        );
        assert_eq!(spawn_res, Err(FfiResult::NULL_POINTER));
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 3);
    }
}
