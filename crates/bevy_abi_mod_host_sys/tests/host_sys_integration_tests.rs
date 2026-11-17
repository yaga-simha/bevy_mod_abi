//! Integration tests for `bevy_abi_mod_host_sys` (`ABI 0.2.0`):
//! - 27-slot `HOST_API`, context-scoped symbols & `last_error`, 1-based `BamComponent`
//!   handles, `iter_column_info`, `res_ptr`/`res_mut_ptr`
//! - `{ magic, generation }` handle headers + poison-on-drop
//! - dense tick skip, batched sparse rows, `marked_changed_mask`, untracked mutable +
//!   `iter_mark_changed_range`
//! - `WorldDropGuard` reclamation & `CAPACITY_EXCEEDED`, CoW `ModRegistry`,
//!   `BAM_SYSTEM_FLAG_MAIN_THREAD`, `SPARSE_SET` & `IMMUTABLE` dynamic components

use bevy_abi_mod_host_sys::drop_trampoline::{
    MAX_DROP_TRAMPOLINES, available_drop_trampoline_slots,
};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, ComponentOptions, GuestSystem, HOST_API, HostCommands, HostCommandsStorage,
    LoadContext, LoadOptions, ModErrorPolicy, ModLoadError, ModRegistry, POISONED_MAGIC,
    WorldDropGuard, host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_info, host_iter_column_mutable, host_iter_column_mutable_untracked,
    host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_column_rows_readonly,
    host_iter_entities, host_iter_mark_changed_range, host_iter_next_chunk, host_last_error,
    host_load_host_ctx, host_register_component, host_register_system, host_res_mut_ptr,
    host_res_ptr, host_resolve_component, host_sys_commands, host_sys_host_ctx, host_sys_query,
    load_mod_in_world,
};
use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BAM_SYSTEM_FLAG_MAIN_THREAD, BamColumnInfo, BamCommands, BamComponent, BamHostCtx, BamIter,
    BamLoadCtx, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc,
    ComponentUuid, FfiBool, FfiEntity, FfiResult, ModComponent, RawInsertable, SymbolEntry,
    VTableHeader,
};
use bevy_ecs::prelude::{Component, Resource, World};
use bevy_ecs::system::{System, SystemStateFlags};
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::error::Error as _;
use std::path::Path;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct Vel {
    vx: f32,
    vy: f32,
}

// SAFETY: `#[repr(C)]` struct of `f32` fields.
unsafe impl RawInsertable for Vel {}
// SAFETY: Cross-language POD layout.
unsafe impl AbiPod for Vel {}

impl ModComponent for Vel {
    const CANONICAL_NAME: &'static str = "test_game::Vel";
    const COMPONENT_CSTR: &'static CStr = c"test_game::Vel";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("test_game::Vel");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;

    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader = VTableHeader::new::<VTableHeader, Vel>(Vel::UUID);
        &VT
    }

    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
struct SimClock {
    dt: f32,
    frame: u32,
}

// SAFETY: `#[repr(C)]` struct of `f32` + `u32`.
unsafe impl RawInsertable for SimClock {}
// SAFETY: Cross-language POD layout.
unsafe impl AbiPod for SimClock {}

impl ModComponent for SimClock {
    const CANONICAL_NAME: &'static str = "test_game::SimClock";
    const COMPONENT_CSTR: &'static CStr = c"test_game::SimClock";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("test_game::SimClock");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;

    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader = VTableHeader::new::<VTableHeader, SimClock>(SimClock::UUID);
        &VT
    }

    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

#[test]
fn host_api_has_all_27_slots_and_registry_has_zero_builtin_symbols() {
    assert_eq!(HOST_API.abi_version, ABI_VERSION);
    assert_eq!(HOST_API.validate_for_guest(ABI_VERSION), FfiResult::OK);
    assert!(HOST_API.get_proc_address.is_some());
    assert!(HOST_API.last_error.is_some());
    assert!(HOST_API.load_host_ctx.is_some());
    assert!(HOST_API.register_component.is_some());
    assert!(HOST_API.resolve_component.is_some());
    assert!(HOST_API.register_system.is_some());
    assert!(HOST_API.sys_host_ctx.is_some());
    assert!(HOST_API.sys_query.is_some());
    assert!(HOST_API.sys_commands.is_some());
    assert!(HOST_API.iter_next_chunk.is_some());
    assert!(HOST_API.iter_reset.is_some());
    assert!(HOST_API.iter_entities.is_some());
    assert!(HOST_API.iter_column_readonly.is_some());
    assert!(HOST_API.iter_column_mutable.is_some());
    assert!(HOST_API.iter_column_mutable_untracked.is_some());
    assert!(HOST_API.iter_mark_changed_range.is_some());
    assert!(HOST_API.iter_column_rows_readonly.is_some());
    assert!(HOST_API.iter_column_rows_mutable.is_some());
    assert!(HOST_API.iter_column_info.is_some());
    assert!(HOST_API.res_ptr.is_some());
    assert!(HOST_API.res_mut_ptr.is_some());
    assert!(HOST_API.cmd_spawn_empty.is_some());
    assert!(HOST_API.cmd_spawn.is_some());
    assert!(HOST_API.cmd_despawn.is_some());
    assert!(HOST_API.cmd_insert.is_some());
    assert!(HOST_API.cmd_insert_default.is_some());
    assert!(HOST_API.cmd_remove.is_some());

    let reg = ModRegistry::new();
    assert_eq!(reg.snapshot().symbol_count(), 0);
    assert_eq!(reg.snapshot().component_count(), 0);
}

#[test]
fn context_scoped_symbols_and_last_error_are_isolated_per_world() {
    static SYM_A: u8 = 0xAA;
    static SYM_B: u8 = 0xBB;

    let mut reg_a = ModRegistry::new();
    let mut reg_b = ModRegistry::new();
    // SAFETY: Static addresses valid for `'static`.
    unsafe {
        reg_a.register_custom_symbol("bam.v0.custom.hook", (&SYM_A as *const u8).cast::<c_void>());
        reg_b.register_custom_symbol("custom.hook", (&SYM_B as *const u8).cast::<c_void>());
    }

    let mut load_a = LoadContext::new(reg_a);
    let mut load_b = LoadContext::new(reg_b);

    // SAFETY: Valid active `LoadContext`s.
    let ctx_a = unsafe { host_load_host_ctx(load_a.as_ffi_ptr()) };
    // SAFETY: Valid active `LoadContext`s.
    let ctx_b = unsafe { host_load_host_ctx(load_b.as_ffi_ptr()) };

    // SAFETY: Resolving `"bam.v0.*"` symbols against distinct `BamHostCtx` handles.
    let resolved_a = unsafe { host_get_proc_address(ctx_a, c"bam.v0.custom.hook".as_ptr()) };
    // SAFETY: Resolving `"bam.v0.*"` symbols against distinct `BamHostCtx` handles.
    let resolved_b = unsafe { host_get_proc_address(ctx_b, c"bam.v0.custom.hook".as_ptr()) };
    assert_eq!(resolved_a, (&SYM_A as *const u8).cast::<c_void>());
    assert_eq!(resolved_b, (&SYM_B as *const u8).cast::<c_void>());

    // Non-"bam.v0." names must always return null.
    // SAFETY: Valid `ctx_a` and C string.
    let bad_ns = unsafe { host_get_proc_address(ctx_a, c"host_register_system".as_ptr()) };
    assert!(bad_ns.is_null());

    // Trigger an error on `ctx_a` and verify `ctx_b` diagnostic stays empty + `snprintf` semantics.
    let mut out_comp = BamComponent::INVALID;
    // SAFETY: Valid `ctx_a`, C string, and `out_comp`.
    let st = unsafe {
        host_resolve_component(
            ctx_a,
            c"missing::Comp".as_ptr(),
            core::ptr::null(),
            &mut out_comp,
        )
    };
    assert_eq!(st, FfiResult::NOT_FOUND);

    // SAFETY: Querying full length with null buffer, then truncated copy into 8-byte buffer.
    let full_len = unsafe { host_last_error(ctx_a, core::ptr::null_mut(), 0) };
    assert!(full_len > 10);
    let mut small_buf = [0 as c_char; 8];
    // SAFETY: `small_buf` has 8 writable bytes.
    let reported_len = unsafe { host_last_error(ctx_a, small_buf.as_mut_ptr(), small_buf.len()) };
    assert_eq!(reported_len, full_len);
    assert_eq!(small_buf[7], 0);

    // SAFETY: `ctx_b` had no error recorded.
    let len_b = unsafe { host_last_error(ctx_b, core::ptr::null_mut(), 0) };
    assert_eq!(len_b, 0);

    load_a.finish_init();
    load_b.finish_init();
}

#[test]
fn handle_headers_reject_mismatched_and_poisoned_handles() {
    let mut world = World::new();
    let vel_id = world.register_component::<Vel>();
    let mut reg = ModRegistry::new();
    let _vel_handle = reg
        .register_component::<Vel>(vel_id, ComponentOptions::new())
        .unwrap();
    world.insert_resource(reg.clone());

    // 1. `LoadContext` + embedded `HostContext` poison on `finish_init`.
    let mut load_ctx = LoadContext::new(reg.clone());
    let raw_load_ptr: *mut BamLoadCtx = load_ctx.as_ffi_ptr();
    // SAFETY: Valid while `in_init == true`.
    let raw_host_ptr: *const BamHostCtx = unsafe { host_load_host_ctx(raw_load_ptr) };
    assert!(!raw_host_ptr.is_null());

    load_ctx.finish_init();
    assert_eq!(load_ctx.header.magic, POISONED_MAGIC);
    assert_eq!(load_ctx.header.generation, 0);

    let mut out_comp = BamComponent::INVALID;
    // SAFETY: Passing poisoned `raw_load_ptr` and `raw_host_ptr` while `load_ctx` stack frame is still live.
    unsafe {
        assert!(host_load_host_ctx(raw_load_ptr).is_null());
        assert_eq!(
            host_resolve_component(
                raw_host_ptr,
                c"test_game::Vel".as_ptr(),
                core::ptr::null(),
                &mut out_comp
            ),
            FfiResult::INVALID_STATE
        );
    }

    // 2. Cross-handle type confusion inside an active `GuestSystem` callback.
    unsafe extern "C" fn cross_handle_check_cb(
        sys_ctx: *mut BamSystemCtx,
        _ud: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: Valid `sys_ctx` during callback.
        unsafe {
            assert_eq!(host_sys_query(sys_ctx, 0, &mut iter), FfiResult::OK);
            assert_eq!(host_sys_commands(sys_ctx, &mut cmds), FfiResult::OK);

            // Passing `BamCommands*` cast as `BamIter*` must fail header validation (`FfiBool::FALSE`).
            let mut len = 99usize;
            assert_eq!(
                host_iter_next_chunk(cmds.cast::<BamIter>(), &mut len),
                FfiBool::FALSE
            );

            // Passing `BamIter*` cast as `BamCommands*` must return `FfiResult::INVALID_STATE`.
            let mut ent = FfiEntity::NULL;
            assert_eq!(
                host_cmd_spawn_empty(iter.cast::<BamCommands>(), &mut ent),
                FfiResult::INVALID_STATE
            );

            // Passing `BamSystemCtx*` cast as `BamLoadCtx*` must return `FfiResult::INVALID_STATE`.
            assert_eq!(
                host_register_system(sys_ctx.cast::<BamLoadCtx>(), core::ptr::null()),
                FfiResult::INVALID_STATE
            );
        }
        FfiResult::OK
    }

    let query = BamQuerySpecOwned {
        flags: 0,
        terms: vec![BamTermDesc {
            component: BamComponent::from_raw(1),
            mode: AccessMode::READ,
            flags: 0,
        }],
    };
    let mut sys = GuestSystem::new(
        "cross_handle_check",
        vec![query],
        cross_handle_check_cb,
        core::ptr::null_mut(),
        reg,
    );
    sys.initialize(&mut world);
    sys.run((), &mut world).unwrap();
}

#[test]
fn sparse_set_batching_untracked_mutation_tick_skip_resources_and_column_info() {
    let mut world = World::new();
    let vel_id = world.register_component::<Vel>();
    let clock_id = world.register_component::<SimClock>();
    world.insert_resource(SimClock { dt: 0.5, frame: 10 });

    let mut reg = ModRegistry::new();
    let vel_comp = reg
        .register_component::<Vel>(vel_id, ComponentOptions::new())
        .unwrap();
    let clock_comp = reg.register_resource::<SimClock>(clock_id).unwrap();
    world.insert_resource(reg.clone());

    // Register a dynamic `SparseSet` u32 component and a dynamic `Immutable` u32 component via `LoadContext`.
    let sparse_uuid = ComponentUuid::from_parts("test_mod::Shield", 4, 4, 1);
    let immut_uuid = ComponentUuid::from_parts("test_mod::TeamId", 4, 4, 2);

    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true)),
        &mut world as *mut World,
        None,
        Some(std::sync::Arc::from("test_mod")),
    );

    let sparse_desc = ComponentDesc::new(c"Shield".as_ptr(), sparse_uuid, 4, 4)
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET);
    let immut_desc = ComponentDesc::new(c"TeamId".as_ptr(), immut_uuid, 4, 4)
        .with_flags(BAM_COMPONENT_FLAG_IMMUTABLE);

    let mut shield_comp = BamComponent::INVALID;
    let mut team_comp = BamComponent::INVALID;
    // SAFETY: Valid `load_ctx` and `ComponentDesc`s.
    unsafe {
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &sparse_desc, &mut shield_comp),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &immut_desc, &mut team_comp),
            FfiResult::OK
        );

        // Unqualified `"Shield"` resolves inside `test_mod`'s `HostContext`.
        let hctx = host_load_host_ctx(load_ctx.as_ffi_ptr());
        let mut resolved = BamComponent::INVALID;
        assert_eq!(
            host_resolve_component(hctx, c"Shield".as_ptr(), &sparse_uuid, &mut resolved),
            FfiResult::OK
        );
        assert_eq!(resolved, shield_comp);

        // Mismatched UUID returns `TYPE_MISMATCH`.
        assert_eq!(
            host_resolve_component(hctx, c"Shield".as_ptr(), &immut_uuid, &mut resolved),
            FfiResult::TYPE_MISMATCH
        );

        // Attempting to register a system that writes to an `IMMUTABLE` component fails with `PERMISSION_DENIED`.
        let bad_term = BamTermDesc {
            component: team_comp,
            mode: AccessMode::WRITE,
            flags: 0,
        };
        let bad_query = BamQueryDesc {
            struct_size: BamQueryDesc::MIN_V0_2_SIZE as u32,
            flags: 0,
            terms: &bad_term,
            term_count: 1,
        };
        unsafe extern "C" fn noop_cb(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            FfiResult::OK
        }
        let bad_sys = BamSystemDesc::new(
            c"bad_immut_writer".as_ptr(),
            &bad_query,
            1,
            noop_cb,
            core::ptr::null_mut(),
        )
        .with_flags(BAM_SYSTEM_FLAG_MAIN_THREAD);
        assert_eq!(
            host_register_system(load_ctx.as_ffi_ptr(), &bad_sys),
            FfiResult::PERMISSION_DENIED
        );
    }
    let updated_reg = load_ctx.registry.clone();
    load_ctx.finish_init();

    let shield_id = updated_reg.get_by_handle(shield_comp).unwrap().component_id;
    let team_id = updated_reg.get_by_handle(team_comp).unwrap().component_id;

    // Spawn 4 entities in the SAME archetype carrying dense `Vel`, sparse `Shield`, and immutable `TeamId`.
    for i in 0..4u32 {
        let mut e = world.spawn(Vel {
            vx: (i + 1) as f32,
            vy: 0.0,
        });
        let shield_val: u32 = 100 + i;
        let team_val: u32 = 7;
        // SAFETY: `shield_id` and `team_id` were registered with `(size = 4, align = 4)`.
        unsafe {
            bevy_ecs::ptr::OwningPtr::make(shield_val, |ptr| {
                e.insert_by_id(shield_id, ptr);
            });
            bevy_ecs::ptr::OwningPtr::make(team_val, |ptr| {
                e.insert_by_id(team_id, ptr);
            });
        }
    }

    // System 1: Tests sparse batching (`chunk_len == 4`), `iter_column_info`,
    // `res_ptr`/`res_mut_ptr`, and `iter_column_mutable_untracked` + `iter_mark_changed_range`
    // marking ONLY row 2 of `Vel` changed!
    static OBSERVED_SPARSE_CHUNK_LEN: AtomicUsize = AtomicUsize::new(0);

    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn writer_cb(sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        // SAFETY: Active `sys_ctx` with 1 query.
        unsafe {
            assert_eq!(host_sys_query(sys_ctx, 0, &mut iter), FfiResult::OK);
            assert!(!host_sys_host_ctx(sys_ctx).is_null());

            // Check `BamColumnInfo` on all 4 terms.
            let mut info = core::mem::MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                host_iter_column_info(iter, 0, info.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(info.assume_init().storage_flags, 0);

            assert_eq!(
                host_iter_column_info(iter, 1, info.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(
                info.assume_init().storage_flags,
                BAM_STORAGE_FLAG_SPARSE_SET
            );

            assert_eq!(
                host_iter_column_info(iter, 2, info.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(info.assume_init().storage_flags, BAM_STORAGE_FLAG_IMMUTABLE);

            assert_eq!(
                host_iter_column_info(iter, 3, info.as_mut_ptr()),
                FfiResult::OK
            );
            assert_eq!(info.assume_init().storage_flags, BAM_STORAGE_FLAG_RESOURCE);

            // Mutate `SimClock` resource (`term_index == 3`).
            let clock_ro = host_res_ptr(iter, 3).cast::<SimClock>();
            assert!(!clock_ro.is_null());
            assert_eq!((*clock_ro).frame, 10);
            let clock_mut = host_res_mut_ptr(iter, 3).cast::<SimClock>();
            assert!(!clock_mut.is_null());
            (*clock_mut).frame = 11;

            let mut len = 0usize;
            while host_iter_next_chunk(iter, &mut len).get() {
                OBSERVED_SPARSE_CHUNK_LEN.store(len, Ordering::SeqCst);
                let ents = host_iter_entities(iter);
                assert!(!ents.is_null());

                // Untracked mutable write on dense `Vel` (`term_index == 0`), marking ONLY row 2 changed!
                let vel_ptr = host_iter_column_mutable_untracked(iter, 0).cast::<Vel>();
                assert!(!vel_ptr.is_null());
                (*vel_ptr.add(2)).vy = 99.0;
                assert_eq!(host_iter_mark_changed_range(iter, 0, 2, 1), FfiResult::OK);

                // Indirect sparse-set mutable rows (`term_index == 1`).
                let shield_rows = host_iter_column_rows_mutable(iter, 1);
                assert!(!shield_rows.is_null());
                for r in 0..len {
                    let row_p = (*shield_rows.add(r)).cast::<u32>();
                    *row_p += 50;
                }
                let shield_ro = host_iter_column_rows_readonly(iter, 1);
                assert!(!shield_ro.is_null());
                assert_eq!(*(*shield_ro).cast::<u32>(), 150);

                // Immutable dense `TeamId` (`term_index == 2`) readable, not writable.
                let team_ro = host_iter_column_readonly(iter, 2).cast::<u32>();
                assert!(!team_ro.is_null());
                assert_eq!(*team_ro, 7);
                assert!(host_iter_column_mutable(iter, 2).is_null());
            }
        }
        FfiResult::OK
    }

    // System 2: `AccessMode::CHANGED` reader on `Vel` (`verification).
    static CHANGED_ROW_COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn changed_vel_reader_cb(
        sys_ctx: *mut BamSystemCtx,
        _ud: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        // SAFETY: Active `sys_ctx`.
        unsafe {
            assert_eq!(host_sys_query(sys_ctx, 0, &mut iter), FfiResult::OK);
            let mut total = 0usize;
            let mut len = 0usize;
            while host_iter_next_chunk(iter, &mut len).get() {
                total += len;
            }
            CHANGED_ROW_COUNT.store(total, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    let mut changed_sys = GuestSystem::new(
        "changed_vel_reader",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc {
                component: vel_comp,
                mode: AccessMode::CHANGED,
                flags: 0,
            }],
        }],
        changed_vel_reader_cb,
        core::ptr::null_mut(),
        updated_reg.clone(),
    );
    changed_sys.initialize(&mut world);
    // Initial run consumes initial spawn ticks (all 4 entities).
    changed_sys.run((), &mut world).unwrap();
    assert_eq!(CHANGED_ROW_COUNT.load(Ordering::SeqCst), 4);
    // Second run with zero mutations skips the entire table via summary tick check -> 0 rows!
    changed_sys.run((), &mut world).unwrap();
    assert_eq!(CHANGED_ROW_COUNT.load(Ordering::SeqCst), 0);

    // Run `writer_sys` (also verify `BAM_SYSTEM_FLAG_MAIN_THREAD` -> `NON_SEND` flag).
    let mut writer_sys = GuestSystem::new(
        "writer_sys",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc {
                    component: vel_comp,
                    mode: AccessMode::WRITE,
                    flags: 0,
                },
                BamTermDesc {
                    component: shield_comp,
                    mode: AccessMode::WRITE,
                    flags: 0,
                },
                BamTermDesc {
                    component: team_comp,
                    mode: AccessMode::READ,
                    flags: 0,
                },
                BamTermDesc {
                    component: clock_comp,
                    mode: AccessMode::RESOURCE_WRITE,
                    flags: 0,
                },
            ],
        }],
        writer_cb,
        core::ptr::null_mut(),
        updated_reg,
    )
    .with_main_thread(true);
    assert!(writer_sys.flags().contains(SystemStateFlags::NON_SEND));
    writer_sys.initialize(&mut world);
    writer_sys.run((), &mut world).unwrap();

    // All 4 sparse entities in the same archetype were batched into a SINGLE chunk of length 4!
    assert_eq!(OBSERVED_SPARSE_CHUNK_LEN.load(Ordering::SeqCst), 4);
    assert_eq!(world.resource::<SimClock>().frame, 11);

    // Because `writer_sys` used `iter_column_mutable_untracked` + `iter_mark_changed_range(0, 2, 1)`,
    // `changed_sys` sees ONLY the 1 modified row (not all 4 rows)!
    changed_sys.run((), &mut world).unwrap();
    assert_eq!(CHANGED_ROW_COUNT.load(Ordering::SeqCst), 1);
}

static DROP_POOL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn world_drop_guard_reclaims_slots_on_world_drop_and_reports_capacity_exceeded() {
    let _serial = DROP_POOL_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn custom_drop(ptr: *mut c_void) {
        if !ptr.is_null() {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    let baseline_free = available_drop_trampoline_slots();
    DROP_COUNT.store(0, Ordering::SeqCst);

    {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true)),
            &mut world as *mut World,
            None,
        );
        let uuid = ComponentUuid::from_parts("drop_test::OwnedBlob", 8, 8, 99);
        let mut desc =
            ComponentDesc::new(c"drop_test::OwnedBlob".as_ptr(), uuid, 8, 8).with_drop(custom_drop);
        desc.struct_size = ComponentDesc::MIN_V0_2_SIZE as u32;
        let mut handle = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx` and `desc`.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle),
                FfiResult::OK
            );
        }
        let comp_id = load_ctx
            .registry
            .get_by_handle(handle)
            .unwrap()
            .component_id;
        load_ctx.finish_init();

        assert_eq!(available_drop_trampoline_slots(), baseline_free - 1);

        // Spawn 3 entities; despawn 1 while active (`DROP_COUNT` -> 1), and leave 2 alive when `World` drops!
        let mut entities = Vec::new();
        for i in 0..3u64 {
            let mut e = world.spawn_empty();
            // SAFETY: `comp_id` has layout `(8, 8)` matching `u64`.
            unsafe {
                bevy_ecs::ptr::OwningPtr::make(i + 1, |ptr| {
                    e.insert_by_id(comp_id, ptr);
                });
            }
            entities.push(e.id());
        }
        world.entity_mut(entities[0]).despawn();
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
    }

    // After `World` drops (even with 2 entities still in storage at `World::drop` time), the slot MUST be reclaimed!
    assert_eq!(available_drop_trampoline_slots(), baseline_free);

    // Exhaust all available slots in a separate `World` and verify `FfiResult::CAPACITY_EXCEEDED`.
    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        let reg = ModRegistry::new();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true)),
            &mut world as *mut World,
            None,
        );
        let to_fill = available_drop_trampoline_slots();
        for idx in 0..to_fill {
            let cname = std::ffi::CString::new(format!("cap_test::C{idx}")).unwrap();
            let uuid = ComponentUuid::from_parts(cname.to_str().unwrap(), 4, 4, idx as u64);
            let mut desc = ComponentDesc::new(cname.as_ptr(), uuid, 4, 4).with_drop(custom_drop);
            desc.struct_size = ComponentDesc::MIN_V0_2_SIZE as u32;
            let mut h = BamComponent::INVALID;
            // SAFETY: Valid `load_ctx` and `desc`.
            unsafe {
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut h),
                    FfiResult::OK
                );
            }
        }
        assert_eq!(available_drop_trampoline_slots(), 0);

        // The 1025th distinct foreign-drop component must fail cleanly with `CAPACITY_EXCEEDED`!
        let mut overflow_desc = ComponentDesc::new(
            c"cap_test::Overflow".as_ptr(),
            ComponentUuid::from_parts("cap_test::Overflow", 4, 4, 9999),
            4,
            4,
        )
        .with_drop(custom_drop);
        overflow_desc.struct_size = ComponentDesc::MIN_V0_2_SIZE as u32;
        let mut h = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx` and `overflow_desc`.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &overflow_desc, &mut h),
                FfiResult::CAPACITY_EXCEEDED
            );
        }
        load_ctx.finish_init();
    }

    // All 1024 slots reclaimed upon `World` drop!
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn mod_load_error_preserves_thiserror_sources() {
    let mut world = World::new();
    let missing_path = Path::new("/nonexistent/dir/libmissing_bam_mod.dylib");

    // 1. With `shadow_copy: true`, missing source file yields `ModLoadError::ShadowCopy` with `io::Error` `source()`.
    let err_shadow = load_mod_in_world(
        &mut world,
        missing_path,
        &LoadOptions::new().with_shadow_copy(true),
    )
    .err()
    .expect("missing file should fail shadow copy");
    assert!(matches!(err_shadow, ModLoadError::ShadowCopy { .. }));
    assert!(err_shadow.source().is_some());

    // 2. With `shadow_copy: false`, missing file yields `ModLoadError::DlOpen` with `libloading::Error` `source()`.
    let err_dlopen = load_mod_in_world(
        &mut world,
        missing_path,
        &LoadOptions::new().with_shadow_copy(false),
    )
    .err()
    .expect("missing file should fail dlopen");
    assert!(matches!(err_dlopen, ModLoadError::DlOpen { .. }));
    assert!(err_dlopen.source().is_some());
}

#[test]
fn uncommitted_dynamic_drop_payloads_cleaned_on_fault_missing_entity_and_storage_drop() {
    let _serial = DROP_POOL_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static UNCOMMITTED_DROPS: AtomicUsize = AtomicUsize::new(0);
    UNCOMMITTED_DROPS.store(0, Ordering::SeqCst);

    unsafe extern "C" fn track_drop(ptr: *mut c_void) {
        // SAFETY: `ptr` points to a 4-byte `u32` payload (`0xDEAD_BEEF`).
        let val = unsafe { *ptr.cast::<u32>() };
        assert_eq!(val, 0xDEAD_BEEF);
        UNCOMMITTED_DROPS.fetch_add(1, Ordering::SeqCst);
    }

    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        let reg = ModRegistry::new();
        let is_active = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));

        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            std::sync::Arc::clone(&is_active),
            &mut world as *mut World,
            None,
        );
        let desc = ComponentDesc::new(
            c"drop_cmd::OwnedItem".as_ptr(),
            ComponentUuid::from_parts("drop_cmd::OwnedItem", 4, 4, 1),
            4,
            4,
        )
        .with_drop(track_drop);

        let mut item_comp = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx`, `desc`, and `item_comp`.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut item_comp),
                FfiResult::OK
            );
        }
        let reg = load_ctx.registry.clone();
        load_ctx.finish_init();

        // (a) Faulting system queues `cmd_spawn` + `cmd_insert` and returns `INVALID_STATE`:
        //     `HostCommandsStorage::clear()` must drop both uncommitted payloads!
        #[repr(C)]
        struct FaultUd {
            comp: BamComponent,
            live_entity: FfiEntity,
        }

        unsafe extern "C" fn faulting_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
            // SAFETY: Valid `sys_ctx` and `FaultUd` pointer.
            unsafe {
                let state = &*ud.cast::<FaultUd>();
                let mut cmds: *mut BamCommands = core::ptr::null_mut();
                assert_eq!(host_sys_commands(sys_ctx, &mut cmds), FfiResult::OK);

                let payload: u32 = 0xDEAD_BEEF;
                let comps = [state.comp];
                let ptrs = [(&payload as *const u32).cast::<u8>()];
                let mut spawned = FfiEntity::NULL;
                assert_eq!(
                    host_cmd_spawn(cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut spawned),
                    FfiResult::OK
                );
                assert_eq!(
                    host_cmd_insert(
                        cmds,
                        state.live_entity,
                        state.comp,
                        (&payload as *const u32).cast::<u8>(),
                        4,
                    ),
                    FfiResult::OK
                );
            }
            FfiResult::INVALID_STATE
        }

        let live_ent = world.spawn_empty().id();
        let mut ud = FaultUd {
            comp: item_comp,
            live_entity: FfiEntity::from_bits(live_ent.to_bits()),
        };
        let mut fault_sys = GuestSystem::new(
            "faulting_cmd_sys",
            Vec::new(),
            faulting_cb,
            (&mut ud as *mut FaultUd).cast::<c_void>(),
            reg.clone(),
        )
        .with_error_policy(ModErrorPolicy::Quarantine);
        fault_sys.initialize(&mut world);
        fault_sys.run((), &mut world).unwrap();
        assert_eq!(UNCOMMITTED_DROPS.load(Ordering::SeqCst), 2);

        // (b) `apply_to_world` with `cmd_insert` targeting a despawned entity drops the payload!
        let doomed = world.spawn_empty().id();
        let doomed_ffi = FfiEntity::from_bits(doomed.to_bits());
        world.despawn(doomed);

        let snap = reg.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut host_cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let payload: u32 = 0xDEAD_BEEF;
            // SAFETY: Valid `host_cmds` and `payload`.
            unsafe {
                assert_eq!(
                    host_cmd_insert(
                        host_cmds.as_ffi_ptr(),
                        doomed_ffi,
                        item_comp,
                        (&payload as *const u32).cast::<u8>(),
                        4,
                    ),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(&mut world, &snap);
        assert_eq!(UNCOMMITTED_DROPS.load(Ordering::SeqCst), 3);

        // (c) `HostCommandsStorage::drop` while non-empty drops both `SpawnBatch` and `Insert` payloads!
        {
            let mut dropped_storage = HostCommandsStorage::new();
            let mut host_cmds =
                HostCommands::new(&mut dropped_storage, world.entity_allocator(), &snap);
            let payload: u32 = 0xDEAD_BEEF;
            let comps = [item_comp];
            let ptrs = [(&payload as *const u32).cast::<u8>()];
            let mut spawned = FfiEntity::NULL;
            // SAFETY: Valid `host_cmds` and `payload`.
            unsafe {
                assert_eq!(
                    host_cmd_spawn(
                        host_cmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        &mut spawned,
                    ),
                    FfiResult::OK
                );
                assert_eq!(
                    host_cmd_insert(
                        host_cmds.as_ffi_ptr(),
                        FfiEntity::from_bits(live_ent.to_bits()),
                        item_comp,
                        (&payload as *const u32).cast::<u8>(),
                        4,
                    ),
                    FfiResult::OK
                );
            }
        }
        assert_eq!(UNCOMMITTED_DROPS.load(Ordering::SeqCst), 5);
    }

    // `SLOT_LIFECYCLE` remained balanced (`live_values == 0`), so dropping `World` reclaims the slot!
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn reregister_component_rejects_mismatched_drop_fn_presence() {
    let _serial = DROP_POOL_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe extern "C" fn dummy_drop(_ptr: *mut c_void) {}

    let baseline_free = available_drop_trampoline_slots();
    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        let vel_id = world.register_component::<Vel>();
        let mut reg = ModRegistry::new();
        reg.register_component::<Vel>(vel_id, ComponentOptions::new())
            .unwrap();
        let host_vtable = reg.get_by_name(Vel::CANONICAL_NAME).unwrap().vtable_ptr;
        world.insert_resource(reg.clone());

        let is_active = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, is_active, &mut world as *mut World, None);

        // 1. Host-owned `Vel` cannot be re-registered by a guest with `drop_fn: Some(...)`.
        let host_drop_attempt = ComponentDesc::new(
            Vel::COMPONENT_CSTR.as_ptr(),
            Vel::UUID,
            core::mem::size_of::<Vel>(),
            core::mem::align_of::<Vel>(),
        )
        .with_drop(dummy_drop);
        let mut out = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx`, `host_drop_attempt`, and `out`.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &host_drop_attempt, &mut out),
                FfiResult::TYPE_MISMATCH
            );
        }
        assert_eq!(available_drop_trampoline_slots(), baseline_free);
        assert_eq!(
            load_ctx
                .registry
                .get_by_name(Vel::CANONICAL_NAME)
                .unwrap()
                .vtable_ptr,
            host_vtable
        );

        // 2. Dynamic POD component registered WITHOUT `drop_fn` rejects re-registration WITH `drop_fn`.
        let pod_uuid = ComponentUuid::from_parts("dyn::PodOnly", 4, 4, 1);
        let pod_desc = ComponentDesc::new(c"dyn::PodOnly".as_ptr(), pod_uuid, 4, 4);
        let pod_with_drop = pod_desc.with_drop(dummy_drop);
        // SAFETY: Valid `load_ctx` and descriptors.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &pod_desc, &mut out),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &pod_with_drop, &mut out),
                FfiResult::TYPE_MISMATCH
            );
        }
        assert_eq!(available_drop_trampoline_slots(), baseline_free);

        // 3. Dynamic drop-bearing component registered WITH `drop_fn` rejects re-registration WITHOUT `drop_fn`.
        let drop_uuid = ComponentUuid::from_parts("dyn::WithDrop", 4, 4, 2);
        let drop_desc =
            ComponentDesc::new(c"dyn::WithDrop".as_ptr(), drop_uuid, 4, 4).with_drop(dummy_drop);
        let drop_omitted = ComponentDesc::new(c"dyn::WithDrop".as_ptr(), drop_uuid, 4, 4);
        // SAFETY: Valid `load_ctx` and descriptors.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &drop_desc, &mut out),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &drop_omitted, &mut out),
                FfiResult::TYPE_MISMATCH
            );
        }
        assert_eq!(available_drop_trampoline_slots(), baseline_free - 1);
        load_ctx.finish_init();
    }
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn failed_load_rollback_reclaims_fresh_slots_and_restores_hot_updated_snapshots() {
    let _serial = DROP_POOL_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static INITIAL_DROPS: AtomicUsize = AtomicUsize::new(0);
    static RELOADED_DROPS: AtomicUsize = AtomicUsize::new(0);
    INITIAL_DROPS.store(0, Ordering::SeqCst);
    RELOADED_DROPS.store(0, Ordering::SeqCst);

    unsafe extern "C" fn initial_drop(_ptr: *mut c_void) {
        INITIAL_DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn reloaded_drop(_ptr: *mut c_void) {
        RELOADED_DROPS.fetch_add(1, Ordering::SeqCst);
    }

    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());

        // 1. Over 1,100 (> MAX_DROP_TRAMPOLINES) failed loads rolling back in a single live `World`
        //    must return every fresh slot immediately to `FREE_LIST` without exhausting capacity!
        for idx in 0..(MAX_DROP_TRAMPOLINES + 76) {
            let reg = world.resource::<ModRegistry>().clone();
            let is_active = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, is_active, &mut world as *mut World, None);

            let cname = std::ffi::CString::new(format!("rollback_test::Fresh{idx}")).unwrap();
            let uuid = ComponentUuid::from_parts(cname.to_str().unwrap(), 4, 4, idx as u64);
            let desc = ComponentDesc::new(cname.as_ptr(), uuid, 4, 4).with_drop(initial_drop);

            let mut out = BamComponent::INVALID;
            // SAFETY: Valid `load_ctx`, `desc`, and `out`.
            unsafe {
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut out),
                    FfiResult::OK,
                    "failed load #{idx} leaked trampoline slots"
                );
            }
            load_ctx.rollback();
        }

        assert_eq!(world.resource::<WorldDropGuard>().len(), 0);
        assert_eq!(available_drop_trampoline_slots(), baseline_free);

        // 2. Commit initial version with a live entity, then simulate a failed reload that updates
        //    the same UUID with `reloaded_drop` and rolls back: `initial_drop` must be restored!
        let shared_uuid = ComponentUuid::from_parts("rollback_test::Shared", 4, 4, 77);
        let initial_guard = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));
        let mut initial_ctx = LoadContext::with_world_and_mod_id(
            world.resource::<ModRegistry>().clone(),
            std::sync::Arc::clone(&initial_guard),
            &mut world as *mut World,
            None,
        );
        let initial_desc = ComponentDesc::new(c"rollback_test::Shared".as_ptr(), shared_uuid, 4, 4)
            .with_drop(initial_drop);
        let mut comp_h = BamComponent::INVALID;
        // SAFETY: Valid `initial_ctx`, `initial_desc`, and `comp_h`.
        unsafe {
            assert_eq!(
                host_register_component(initial_ctx.as_ffi_ptr(), &initial_desc, &mut comp_h),
                FfiResult::OK
            );
        }
        let comp_id = initial_ctx
            .registry
            .get_by_handle(comp_h)
            .unwrap()
            .component_id;
        initial_ctx.finish_init();

        let val: u32 = 42;
        let mut ent1 = world.spawn_empty();
        // SAFETY: `comp_id` has `(size = 4, align = 4)`.
        unsafe {
            bevy_ecs::ptr::OwningPtr::make(val, |ptr| {
                ent1.insert_by_id(comp_id, ptr);
            });
        }
        let ent1_id = ent1.id();

        let mut ent2 = world.spawn_empty();
        // SAFETY: `comp_id` has `(size = 4, align = 4)`.
        unsafe {
            bevy_ecs::ptr::OwningPtr::make(val, |ptr| {
                ent2.insert_by_id(comp_id, ptr);
            });
        }

        // Failed hot-reload updates `shared_uuid` to `reloaded_drop`, then aborts via `rollback()`.
        let reloaded_guard = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true));
        let mut reloaded_ctx = LoadContext::with_world_and_mod_id(
            world.resource::<ModRegistry>().clone(),
            reloaded_guard,
            &mut world as *mut World,
            None,
        );
        let reloaded_desc =
            ComponentDesc::new(c"rollback_test::Shared".as_ptr(), shared_uuid, 4, 4)
                .with_drop(reloaded_drop);
        // SAFETY: Valid `reloaded_ctx` and `reloaded_desc`.
        unsafe {
            assert_eq!(
                host_register_component(reloaded_ctx.as_ffi_ptr(), &reloaded_desc, &mut comp_h),
                FfiResult::OK
            );
        }
        reloaded_ctx.rollback();

        // Despawning `ent1` in the live `World` must invoke restored `initial_drop` (not rolled-back `reloaded_drop`),
        // while leaving `ent2` alive exercises deferred slot reclamation across `World::drop`.
        world.despawn(ent1_id);
        assert_eq!(INITIAL_DROPS.load(Ordering::SeqCst), 1);
        assert_eq!(RELOADED_DROPS.load(Ordering::SeqCst), 0);
    }

    assert_eq!(RELOADED_DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}
