//! Comprehensive ABI `0.3.0`, `ffi/`, and `system/` integration coverage tests for
//! `bevy_abi_mod_host_sys`.
//!
//! Imports exclusively from the sealed crate-root `bevy_abi_mod_host_sys::*` surface (never
//! `bevy_abi_mod_host_sys::ffi::<submod>`), exercising:
//! - Every ABI `0.3.0` active and reserved entrypoint in `ffi/v03_ext.rs`
//! - Asset and scene backend FFI guards (`ffi/assets_scene.rs`)
//! - Deferred command / arena / batch / despawn / trigger edge paths (`ffi/commands.rs`, `ffi/commands_ext.rs`)
//! - Dense, sparse, tick-filter, `Or<...>`, resource, message, removed, and parallel iterator paths (`ffi/iter.rs`, `ffi/iter_ext.rs`)
//! - Observer registration, lifecycle triggers, propagation, and dynamic relationships (`ffi/register.rs`, `ffi/observers_ext.rs`)
//! - Hierarchy inspection edge cases (`ffi/hierarchy_ext.rs`)
//! - System access validation, quarantine, error policies, diagnostics, and `HostSystemCtx` guards (`system/*`)

use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, ComponentOptions, GuestSystem, HOST_API, HandleHeader, HostAssetBackend,
    HostCommands, HostCommandsStorage, HostContext, HostIter, HostSceneBackend, HostSystemCtx,
    LoadContext, MAX_SPAWN_BATCH_COMPONENTS, MAX_SYSTEM_QUERIES, ModErrorPolicy, ModHost, ModId,
    ModQuarantine, ModRegistry, POISONED_MAGIC, QueryPlan, QueryTermMeta, RegistrySnapshot,
    SparseRowScratch, SystemInitError, TargetEntity, WorldDropGuard,
    ensure_children_registered_in_world, ensure_dynamic_hook_map_in_world,
    host_asset_get_load_state, host_asset_load, host_asset_release, host_bind_named_object,
    host_cmd_checkpoint, host_cmd_despawn, host_cmd_despawn_all, host_cmd_insert,
    host_cmd_insert_default, host_cmd_insert_if_new, host_cmd_insert_relation, host_cmd_observe,
    host_cmd_remove, host_cmd_remove_relation, host_cmd_rollback, host_cmd_spawn,
    host_cmd_spawn_batch, host_cmd_spawn_empty, host_cmd_spawn_persistent, host_cmd_spawn_scene,
    host_cmd_trigger, host_cmd_trigger_ex, host_component_layout_info, host_configure_schedule_set,
    host_get_proc_address, host_intern_symbol, host_iter_column_info, host_iter_column_mutable,
    host_iter_column_mutable_untracked, host_iter_column_readonly, host_iter_column_rows_mutable,
    host_iter_column_rows_readonly, host_iter_count, host_iter_entities,
    host_iter_mark_changed_range, host_iter_next_chunk, host_iter_read_removed, host_iter_reset,
    host_iter_up_cascade, host_load_host_ctx, host_log_diagnostic, host_migrate_component_column,
    host_mount_system_in_schedule, host_msg_read_batch, host_msg_send, host_msg_write_batch,
    host_par_for_each_chunk, host_par_for_each_chunk_readonly, host_register_component,
    host_register_observer, host_register_relation, host_register_system,
    host_register_system_handle, host_report_guest_fault, host_res_mut_ptr, host_res_ptr,
    host_res_ticks, host_resolve_component, host_resolve_component_by_uuid, host_sys_commands,
    host_sys_entity_children, host_sys_entity_get, host_sys_entity_get_mut, host_sys_host_ctx,
    host_sys_query, host_sys_res_mut_ptr, host_sys_res_ptr,
};
#[cfg(feature = "diagnostics")]
use bevy_abi_mod_host_sys::{ModDiagnosticsCollector, ModDiagnosticsSample};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO,
    BAM_LOG_TRACE, BAM_LOG_WARN, BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BamChangeTicks, BamColumnInfo, BamCommands, BamComponent,
    BamEventKind, BamFieldDesc, BamFieldKind, BamHostCtx, BamIter, BamLayoutInfo, BamLoadCtx,
    BamObserverDesc, BamQueryDesc, BamRelationDesc, BamRequiredComponent, BamScheduleSetDesc,
    BamSymbol, BamSystem, BamSystemCtx, BamSystemDesc, BamSystemSet, BamTermDesc, BamTriggerDesc,
    ComponentDesc, ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiBool, FfiChildOf,
    FfiEntity, FfiResult, ModComponent as _, ScheduleId, StrView,
};
use bevy_ecs::{
    change_detection::Tick,
    component::Component,
    entity::Entity,
    event::{EntityEvent, Event},
    hierarchy::{ChildOf, Children},
    message::{Message, Messages},
    prelude::Resource,
    system::System,
    world::{DeferredWorld, World},
};
use core::ffi::{c_char, c_void};
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::ffi::CString;
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::Pos")]
struct CovPos {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::Vel")]
struct CovVel {
    dx: f32,
    dy: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "cov::SparseTag")]
struct CovSparseTag {
    v: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(immutable)]
#[mod_component(name = "cov::ImmComp")]
struct CovImmComp {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::ZeroComp")]
struct CovZeroComp;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "cov::NoDefaultComp")]
struct CovNoDefaultComp {
    raw: u32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::ResVal")]
struct CovResVal {
    tick: u32,
    scale: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::MsgItem")]
struct CovMsgItem {
    code: u32,
}

impl Message for CovMsgItem {}

#[repr(C)]
#[derive(Event, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::GlobalEv")]
struct CovGlobalEv {
    val: u32,
}

#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "cov::EntEv")]
struct CovEntEv {
    #[event_target]
    target: TargetEntity,
    amount: u64,
}

#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
struct CovReadyEv {
    #[event_target]
    entity: Entity,
}

impl From<Entity> for CovReadyEv {
    fn from(entity: Entity) -> Self {
        Self { entity }
    }
}

fn invalid_entity_bits() -> FfiEntity {
    for bits in [
        1u64,
        u64::MAX,
        0xFFFF_FFFF_0000_0000u64,
        0x0000_0000_FFFF_FFFFu64,
    ] {
        let ffi = FfiEntity::from_bits(bits);
        if !ffi.is_null() && Entity::try_from_bits(bits).is_none() {
            return ffi;
        }
    }
    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits())
}

fn placeholder_entity() -> FfiEntity {
    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits())
}

fn poisoned_header() -> HandleHeader {
    HandleHeader {
        magic: POISONED_MAGIC,
        generation: 0,
    }
}

unsafe extern "C" fn noop_system(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_migrate(
    _src: *const c_void,
    _dst: *mut c_void,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_chunk_task(_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_observer(
    _entity: FfiEntity,
    _data: *const c_void,
    _cmds: *mut BamCommands,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_observer_mut(
    _entity: FfiEntity,
    _data: *mut u8,
    _propagate: *mut FfiBool,
    _cmds: *mut BamCommands,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_run_if(_ud: *mut c_void) -> FfiBool {
    FfiBool::TRUE
}

// ============================================================================
// 1. ABI 0.3.0 active slots + diagnostics/UUID/layout/checkpoints/rollback
// ============================================================================

#[test]
fn v03_diagnostics_uuid_layout_system_handle_and_rollback_edge_cases() {
    assert!(HOST_API.has_all_slots());

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    let pos_id = world.register_component::<CovPos>();
    let zero_id = world.register_component::<CovZeroComp>();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let zero_h = registry
        .register_component::<CovZeroComp>(zero_id, ComponentOptions::default())
        .unwrap();
    let snap = registry.snapshot_arc();
    let host_ctx = HostContext::new(Arc::clone(&snap)).with_namespace(Some(Arc::from("cov_mod")));
    let host_ptr = host_ctx.as_ffi_ptr();
    let mut poisoned = poisoned_header();
    let bad_host = (&raw const poisoned).cast::<BamHostCtx>();

    let invalid_utf8 = [0xFFu8, 0xFEu8];
    let bad_utf8_view = StrView {
        ptr: invalid_utf8.as_ptr(),
        len: invalid_utf8.len(),
    };
    let null_len_view = StrView {
        ptr: null(),
        len: 4,
    };

    // SAFETY: Valid `host_ptr`, poisoned header for negative tests, and valid stack buffers.
    unsafe {
        // `host_log_diagnostic`: all 5 levels + invalid level + bad ctx + bad StrView
        for level in [
            BAM_LOG_TRACE,
            BAM_LOG_DEBUG,
            BAM_LOG_INFO,
            BAM_LOG_WARN,
            BAM_LOG_ERROR,
        ] {
            assert_eq!(
                host_log_diagnostic(host_ptr, level, StrView::from_str("diag msg")),
                FfiResult::OK
            );
        }
        assert_eq!(
            host_log_diagnostic(host_ptr, 42, StrView::from_str("bad")),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_log_diagnostic(null(), BAM_LOG_INFO, StrView::from_str("x")),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_log_diagnostic(bad_host, BAM_LOG_INFO, StrView::from_str("x")),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_log_diagnostic(host_ptr, BAM_LOG_INFO, bad_utf8_view),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_log_diagnostic(host_ptr, BAM_LOG_INFO, null_len_view),
            FfiResult::NULL_POINTER
        );

        // `host_report_guest_fault`
        assert_eq!(
            host_report_guest_fault(null(), StrView::from_str("x")),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_report_guest_fault(bad_host, StrView::from_str("x")),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_report_guest_fault(host_ptr, bad_utf8_view),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_report_guest_fault(host_ptr, StrView::from_str("guest panic")),
            FfiResult::OK
        );
        assert_eq!(host_ctx.last_error_message(), "guest panic");

        // `host_resolve_component_by_uuid`
        let mut out_comp = BamComponent::INVALID;
        let uuid = CovPos::UUID;
        assert_eq!(
            host_resolve_component_by_uuid(host_ptr, &uuid, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component_by_uuid(null(), &uuid, &raw mut out_comp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component_by_uuid(bad_host, &uuid, &raw mut out_comp),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_resolve_component_by_uuid(host_ptr, null(), &raw mut out_comp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component_by_uuid(host_ptr, &uuid, &raw mut out_comp),
            FfiResult::OK
        );
        assert_eq!(out_comp, pos_h);

        // `host_component_layout_info`: null out, bad ctx, invalid handle, 0-field ZST component
        let mut layout = BamLayoutInfo::empty();
        assert_eq!(
            host_component_layout_info(host_ptr, pos_h, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_component_layout_info(null(), pos_h, &raw mut layout),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_component_layout_info(bad_host, pos_h, &raw mut layout),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_component_layout_info(host_ptr, BamComponent::from_raw(9999), &raw mut layout),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_component_layout_info(host_ptr, zero_h, &raw mut layout),
            FfiResult::OK
        );
        assert_eq!(layout.field_count, 0);
        assert!(layout.fields.is_null());

        // `host_register_system_handle`
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let sys_desc = BamSystemDesc::new(
            c"cov::handle_sys".as_ptr(),
            null(),
            0,
            noop_system,
            null_mut(),
        );
        let mut out_sys = BamSystem::INVALID;
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &sys_desc, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), null(), &raw mut out_sys),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &sys_desc, &raw mut out_sys),
            FfiResult::OK
        );
        assert_eq!(out_sys, BamSystem::from_raw(1));

        // Register a dynamic component with a destructor so rollback exercises `SpawnSoaBatch`,
        // `InsertIfNew`, and `Custom` rollback branches in `ffi/commands.rs`.
        static ROLLBACK_DROPS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn count_rollback_drop(_ptr: *mut c_void) {
            ROLLBACK_DROPS.fetch_add(1, Ordering::SeqCst);
        }
        let dyn_desc = ComponentDesc::new(
            c"cov::RollbackDrop".as_ptr(),
            ComponentUuid::from_parts("cov::RollbackDrop", 8, 8, 0),
            8,
            8,
        )
        .with_drop(count_rollback_drop);
        let mut dyn_h = BamComponent::INVALID;
        assert_eq!(
            host_register_component(load_ctx.as_ffi_ptr(), &dyn_desc, &raw mut dyn_h),
            FfiResult::OK
        );
        let updated_snap = load_ctx.registry.snapshot_arc();
        load_ctx.finish_init();

        // `host_cmd_checkpoint` & `host_cmd_rollback`
        let target = FfiEntity::from_bits(world.spawn_empty().id().to_bits());
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &updated_snap);
        let cmd_ptr = cmds.as_ffi_ptr();
        let bad_cmds = (&raw mut poisoned).cast::<BamCommands>();
        let mut cp = 0usize;

        assert_eq!(
            host_cmd_checkpoint(cmd_ptr, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_checkpoint(null_mut(), &raw mut cp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_checkpoint(bad_cmds, &raw mut cp),
            FfiResult::INVALID_STATE
        );
        assert_eq!(host_cmd_rollback(null_mut(), 0), FfiResult::NULL_POINTER);
        assert_eq!(host_cmd_rollback(bad_cmds, 0), FfiResult::INVALID_STATE);

        assert_eq!(host_cmd_checkpoint(cmd_ptr, &raw mut cp), FfiResult::OK);
        assert_eq!(cp, 0);
        // No-op rollback when `checkpoint == commands.len()`.
        assert_eq!(host_cmd_rollback(cmd_ptr, cp), FfiResult::OK);

        // Enqueue `InsertIfNew` + `SpawnSoaBatch` + `Custom`, then roll back all three.
        let payload: u64 = 0x1122_3344_5566_7788;
        assert_eq!(
            host_cmd_insert_if_new(cmd_ptr, target, dyn_h, (&raw const payload).cast::<u8>(), 8),
            FfiResult::OK
        );
        let soa_vals = [1u64, 2u64, 3u64];
        let soa_comps = [dyn_h];
        let soa_ptrs = [soa_vals.as_ptr().cast::<u8>()];
        let mut soa_ents = [FfiEntity::NULL; 3];
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                soa_comps.as_ptr(),
                soa_ptrs.as_ptr(),
                1,
                3,
                soa_ents.as_mut_ptr(),
            ),
            FfiResult::OK
        );
        let custom_ran = Arc::new(AtomicBool::new(false));
        let custom_flag = Arc::clone(&custom_ran);
        cmds.queue_custom(move |_, _| {
            custom_flag.store(true, Ordering::SeqCst);
        });
        assert_eq!(host_cmd_rollback(cmd_ptr, cp), FfiResult::OK);
        assert_eq!(ROLLBACK_DROPS.load(Ordering::SeqCst), 4);
        drop(cmds);
        storage.apply_to_world(&mut world, &updated_snap);
        assert!(!custom_ran.load(Ordering::SeqCst));
    }
}

// ============================================================================
// 2. All 10 reserved ABI 0.3.0 slots exhaustive validation
// ============================================================================

#[test]
fn v03_reserved_slots_exhaustive_validation() {
    let mut world = World::new();
    let pos_id = world.register_component::<CovPos>();
    let vel_id = world.register_component::<CovVel>();
    let live_ent = FfiEntity::from_bits(
        world
            .spawn((CovPos::default(), CovVel::default()))
            .id()
            .to_bits(),
    );
    let bad_ent = invalid_entity_bits();
    let ph_ent = placeholder_entity();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let vel_h = registry
        .register_component::<CovVel>(vel_id, ComponentOptions::default())
        .unwrap();

    let mut load_ctx = LoadContext::with_manifest_name(
        registry.clone(),
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        None,
        None,
    );
    let load_ptr = load_ctx.as_ffi_ptr();
    let host_ptr = load_ctx.host_ctx.as_ffi_ptr();

    let mut poisoned = poisoned_header();
    let bad_load = (&raw mut poisoned).cast::<BamLoadCtx>();
    let bad_host = (&raw const poisoned).cast::<BamHostCtx>();
    let bad_iter = (&raw mut poisoned).cast::<BamIter>();
    let bad_cmds = (&raw mut poisoned).cast::<BamCommands>();

    let bad_utf8_cstr = [0xFFu8, 0u8];
    let bad_utf8_ptr = bad_utf8_cstr.as_ptr().cast::<c_char>();
    let bad_utf8_view = StrView {
        ptr: bad_utf8_cstr.as_ptr(),
        len: 1,
    };

    // SAFETY: Testing all validation guards with intentional null/invalid descriptors and handles.
    unsafe {
        // 1. `host_register_relation`
        let rel_uuid = ComponentUuid::from_canonical_name("cov::RelA");
        let valid_rel = BamRelationDesc::new(c"cov::RelA".as_ptr(), rel_uuid);
        let mut out_rel = BamComponent::INVALID;
        assert_eq!(
            host_register_relation(load_ptr, &valid_rel, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_relation(null_mut(), &valid_rel, &raw mut out_rel),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_relation(bad_load, &valid_rel, &raw mut out_rel),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_register_relation(load_ptr, null(), &raw mut out_rel),
            FfiResult::NULL_POINTER
        );
        let mut bad_rel_flags = valid_rel;
        bad_rel_flags.flags = 0x8000_0000;
        assert_eq!(
            host_register_relation(load_ptr, &bad_rel_flags, &raw mut out_rel),
            FfiResult::UNSUPPORTED
        );
        let empty_name_rel = BamRelationDesc::new(c"".as_ptr(), rel_uuid);
        assert_eq!(
            host_register_relation(load_ptr, &empty_name_rel, &raw mut out_rel),
            FfiResult::INVALID_ARGUMENT
        );
        let bad_utf8_rel = BamRelationDesc::new(bad_utf8_ptr, rel_uuid);
        assert_eq!(
            host_register_relation(load_ptr, &bad_utf8_rel, &raw mut out_rel),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_register_relation(load_ptr, &valid_rel, &raw mut out_rel),
            FfiResult::UNSUPPORTED
        );

        // 2. `host_intern_symbol`
        let mut out_sym = BamSymbol::INVALID;
        assert_eq!(
            host_intern_symbol(host_ptr, StrView::from_str("sym"), null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_intern_symbol(null(), StrView::from_str("sym"), &raw mut out_sym),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_intern_symbol(bad_host, StrView::from_str("sym"), &raw mut out_sym),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_intern_symbol(host_ptr, StrView::from_str(""), &raw mut out_sym),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_intern_symbol(host_ptr, bad_utf8_view, &raw mut out_sym),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_intern_symbol(host_ptr, StrView::from_str("sym"), &raw mut out_sym),
            FfiResult::UNSUPPORTED
        );

        // 3. `host_bind_named_object`
        assert_eq!(
            host_bind_named_object(null(), StrView::from_str("hero"), live_ent),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_bind_named_object(bad_host, StrView::from_str("hero"), live_ent),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str(""), live_ent),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_bind_named_object(host_ptr, bad_utf8_view, live_ent),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), FfiEntity::NULL),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), bad_ent),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), ph_ent),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), live_ent),
            FfiResult::UNSUPPORTED
        );

        // 4. `host_configure_schedule_set`
        let valid_set = BamScheduleSetDesc::new(c"cov::Physics".as_ptr());
        let mut out_set = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(load_ptr, &valid_set, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_configure_schedule_set(null_mut(), &valid_set, &raw mut out_set),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_configure_schedule_set(bad_load, &valid_set, &raw mut out_set),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_configure_schedule_set(load_ptr, null(), &raw mut out_set),
            FfiResult::NULL_POINTER
        );
        let mut bad_set_size = valid_set;
        bad_set_size.struct_size = 4;
        assert_eq!(
            host_configure_schedule_set(load_ptr, &bad_set_size, &raw mut out_set),
            FfiResult::INVALID_VERSION
        );
        let mut bad_set_flags = valid_set;
        bad_set_flags.flags = 1;
        assert_eq!(
            host_configure_schedule_set(load_ptr, &bad_set_flags, &raw mut out_set),
            FfiResult::UNSUPPORTED
        );
        let empty_set = BamScheduleSetDesc::new(c"".as_ptr());
        assert_eq!(
            host_configure_schedule_set(load_ptr, &empty_set, &raw mut out_set),
            FfiResult::INVALID_ARGUMENT
        );
        let bad_utf8_set = BamScheduleSetDesc::new(bad_utf8_ptr);
        assert_eq!(
            host_configure_schedule_set(load_ptr, &bad_utf8_set, &raw mut out_set),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_configure_schedule_set(load_ptr, &valid_set, &raw mut out_set),
            FfiResult::UNSUPPORTED
        );

        // 5. `host_mount_system_in_schedule`
        let sys_desc = BamSystemDesc::new(
            c"cov::mount_sys".as_ptr(),
            null(),
            0,
            noop_system,
            null_mut(),
        );
        let mut sys_h = BamSystem::INVALID;
        assert_eq!(
            host_register_system_handle(load_ptr, &sys_desc, &raw mut sys_h),
            FfiResult::OK
        );
        assert_eq!(
            host_mount_system_in_schedule(
                null_mut(),
                sys_h,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_mount_system_in_schedule(
                bad_load,
                sys_h,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_mount_system_in_schedule(
                load_ptr,
                BamSystem::INVALID,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(
                load_ptr,
                BamSystem::from_raw(99),
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(load_ptr, sys_h, ScheduleId(999), BamSystemSet::INVALID),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(
                load_ptr,
                sys_h,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID
            ),
            FfiResult::UNSUPPORTED
        );

        // 6. `host_migrate_component_column`
        let mig_desc = ComponentDesc::new(c"cov::Pos".as_ptr(), CovPos::UUID, 8, 4);
        assert_eq!(
            host_migrate_component_column(
                null_mut(),
                pos_h,
                0,
                &mig_desc,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_migrate_component_column(
                bad_load,
                pos_h,
                0,
                &mig_desc,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                BamComponent::INVALID,
                0,
                &mig_desc,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                BamComponent::from_raw(999),
                0,
                &mig_desc,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                pos_h,
                0,
                null(),
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        let mut bad_mig_size = mig_desc;
        bad_mig_size.struct_size = 8;
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                pos_h,
                0,
                &bad_mig_size,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::INVALID_VERSION
        );
        let mut bad_mig_flags = mig_desc;
        bad_mig_flags.flags = 0x8000_0000;
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                pos_h,
                0,
                &bad_mig_flags,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::UNSUPPORTED
        );
        assert_eq!(
            host_migrate_component_column(load_ptr, pos_h, 0, &mig_desc, None, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_migrate_component_column(
                load_ptr,
                pos_h,
                0,
                &mig_desc,
                Some(noop_migrate),
                null_mut()
            ),
            FfiResult::UNSUPPORTED
        );

        load_ctx.finish_init();

        // 7 & 8. `host_iter_up_cascade` & `host_par_for_each_chunk_readonly`
        let mixed_plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: pos_h,
                component_id: pos_id,
                mode: AccessMode::READ,
                uuid: CovPos::UUID,
                size: 8,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: vel_h,
                component_id: vel_id,
                mode: AccessMode::WRITE,
                uuid: CovVel::UUID,
                size: 8,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: vel_h,
                component_id: vel_id,
                mode: AccessMode::OPTIONAL_WRITE,
                uuid: CovVel::UUID,
                size: 8,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);
        let ro_plan = QueryPlan::from_terms(vec![QueryTermMeta {
            component: pos_h,
            component_id: pos_id,
            mode: AccessMode::READ,
            uuid: CovPos::UUID,
            size: 8,
            align: 4,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        }]);
        let table_id = world
            .archetypes()
            .iter()
            .find(|a| a.contains(pos_id))
            .unwrap()
            .table_id();
        let tables = [table_id];
        let mut mixed_iter = HostIter::new_dense(
            &world.storages().tables,
            &tables,
            &mixed_plan,
            Tick::new(0),
            Tick::new(1),
        );
        let mut ro_iter = HostIter::new_dense(
            &world.storages().tables,
            &tables,
            &ro_plan,
            Tick::new(0),
            Tick::new(1),
        );
        let mut out_ptr: *const c_void = null();
        let mut out_owner = FfiEntity::NULL;

        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                0,
                null_mut(),
                &raw mut out_owner
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                0,
                &raw mut out_ptr,
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_up_cascade(
                null_mut(),
                pos_h,
                live_ent,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_up_cascade(
                bad_iter,
                pos_h,
                live_ent,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                BamComponent::INVALID,
                live_ent,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                FfiEntity::NULL,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                ph_ent,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                99,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                1,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                2,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_iter_up_cascade(
                mixed_iter.as_ffi_ptr(),
                pos_h,
                live_ent,
                0,
                &raw mut out_ptr,
                &raw mut out_owner
            ),
            FfiResult::UNSUPPORTED
        );

        assert_eq!(
            host_par_for_each_chunk_readonly(ro_iter.as_ffi_ptr(), None, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_par_for_each_chunk_readonly(null_mut(), Some(noop_chunk_task), null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_par_for_each_chunk_readonly(bad_iter, Some(noop_chunk_task), null_mut()),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_par_for_each_chunk_readonly(
                mixed_iter.as_ffi_ptr(),
                Some(noop_chunk_task),
                null_mut()
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_par_for_each_chunk_readonly(
                ro_iter.as_ffi_ptr(),
                Some(noop_chunk_task),
                null_mut()
            ),
            FfiResult::UNSUPPORTED
        );
        drop(mixed_iter);
        drop(ro_iter);

        // 9 & 10. `host_cmd_insert_relation` & `host_cmd_remove_relation`
        let snap = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let cmd_ptr = cmds.as_ffi_ptr();
        let edge_data = 1234u32;

        assert_eq!(
            host_cmd_insert_relation(cmd_ptr, live_ent, pos_h, live_ent, null(), 4),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_insert_relation(null_mut(), live_ent, pos_h, live_ent, null(), 0),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_insert_relation(bad_cmds, live_ent, pos_h, live_ent, null(), 0),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_insert_relation(
                cmd_ptr,
                live_ent,
                BamComponent::INVALID,
                live_ent,
                null(),
                0
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_insert_relation(
                cmd_ptr,
                live_ent,
                BamComponent::from_raw(999),
                live_ent,
                null(),
                0
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_insert_relation(cmd_ptr, FfiEntity::NULL, pos_h, live_ent, null(), 0),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_insert_relation(cmd_ptr, live_ent, pos_h, ph_ent, null(), 0),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_insert_relation(
                cmd_ptr,
                live_ent,
                pos_h,
                live_ent,
                (&raw const edge_data).cast::<c_void>(),
                4,
            ),
            FfiResult::UNSUPPORTED
        );

        assert_eq!(
            host_cmd_remove_relation(null_mut(), live_ent, pos_h, live_ent),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_remove_relation(bad_cmds, live_ent, pos_h, live_ent),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_remove_relation(cmd_ptr, live_ent, BamComponent::INVALID, live_ent),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_remove_relation(cmd_ptr, live_ent, BamComponent::from_raw(999), live_ent),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_remove_relation(cmd_ptr, bad_ent, pos_h, live_ent),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_remove_relation(cmd_ptr, live_ent, pos_h, FfiEntity::NULL),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_remove_relation(cmd_ptr, live_ent, pos_h, live_ent),
            FfiResult::UNSUPPORTED
        );
    }
}

// ============================================================================
// 3. `ffi/assets_scene.rs` exhaustive coverage
// ============================================================================

struct MockAssetBackend {
    fail_with: Mutex<Option<(FfiResult, String)>>,
    next_idx: AtomicUsize,
}

impl MockAssetBackend {
    fn new() -> Self {
        Self {
            fail_with: Mutex::new(None),
            next_idx: AtomicUsize::new(1),
        }
    }

    fn set_error(&self, err: Option<(FfiResult, &str)>) {
        *self.fail_with.lock().unwrap() = err.map(|(r, m)| (r, m.to_string()));
    }
}

impl HostAssetBackend for MockAssetBackend {
    fn load_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        _owner_namespace: Option<&str>,
        asset_type: BamComponent,
        _path: &str,
    ) -> Result<FfiAssetHandle, (FfiResult, String)> {
        if let Some(err) = self.fail_with.lock().unwrap().clone() {
            return Err(err);
        }
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed) as u32;
        let ty = if asset_type.is_valid() {
            asset_type
        } else {
            BamComponent::from_raw(1)
        };
        Ok(FfiAssetHandle::from_index(ty, idx, 1))
    }

    fn get_load_state(
        &self,
        _snapshot: &RegistrySnapshot,
        _handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, (FfiResult, String)> {
        if let Some(err) = self.fail_with.lock().unwrap().clone() {
            return Err(err);
        }
        Ok(FfiAssetLoadState::LOADED)
    }

    fn release_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        _owner_namespace: Option<&str>,
        _handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        if let Some(err) = self.fail_with.lock().unwrap().clone() {
            return Err(err);
        }
        Ok(())
    }
}

struct MockSceneBackend {
    spawned: Arc<Mutex<Vec<(Entity, Option<Entity>, FfiAssetHandle)>>>,
}

impl HostSceneBackend for MockSceneBackend {
    fn spawn_scene(
        &self,
        _world: &mut World,
        _snapshot: &RegistrySnapshot,
        root_entity: Entity,
        parent: Option<Entity>,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), FfiResult> {
        self.spawned
            .lock()
            .unwrap()
            .push((root_entity, parent, *scene_handle));
        Ok(())
    }
}

#[test]
fn assets_scene_exhaustive_validation() {
    let mut world = World::new();
    let parent_ent = world.spawn_empty().id();
    let parent_ffi = FfiEntity::from_bits(parent_ent.to_bits());
    let bad_ent = invalid_entity_bits();
    let ph_ent = placeholder_entity();

    let mut reg_no_backend = ModRegistry::new();
    let pos_id = world.register_component::<CovPos>();
    let pos_h = reg_no_backend
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let snap_no_backend = reg_no_backend.snapshot_arc();
    let ctx_no_backend = HostContext::new(Arc::clone(&snap_no_backend));

    let mut poisoned = poisoned_header();
    let bad_ctx = (&raw const poisoned).cast::<BamHostCtx>();
    let bad_cmds = (&raw mut poisoned).cast::<BamCommands>();

    let unaligned_handle_mut = core::ptr::without_provenance_mut::<FfiAssetHandle>(1);
    let unaligned_handle_const = core::ptr::without_provenance::<FfiAssetHandle>(1);
    let unaligned_state_mut = core::ptr::without_provenance_mut::<FfiAssetLoadState>(1);
    let unaligned_entity_mut = core::ptr::without_provenance_mut::<FfiEntity>(1);

    let bad_utf8 = [0xFFu8, 0xFEu8];
    let bad_utf8_view = StrView {
        ptr: bad_utf8.as_ptr(),
        len: 2,
    };
    let mut invalid_asset_handle = FfiAssetHandle::NONE;
    invalid_asset_handle.kind = 99;
    let valid_asset_handle = FfiAssetHandle::from_index(pos_h, 7, 1);

    // SAFETY: Intentional null, misaligned, poisoned, and valid pointers testing every FFI guard.
    unsafe {
        let mut out_handle = FfiAssetHandle::NONE;
        let mut out_state = FfiAssetLoadState::NOT_LOADED;

        // `host_asset_load` guards + no-backend `UNSUPPORTED`
        assert_eq!(
            host_asset_load(
                ctx_no_backend.as_ffi_ptr(),
                pos_h,
                StrView::from_str("a.png"),
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_asset_load(
                ctx_no_backend.as_ffi_ptr(),
                pos_h,
                StrView::from_str("a.png"),
                unaligned_handle_mut
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_load(
                null(),
                pos_h,
                StrView::from_str("a.png"),
                &raw mut out_handle
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_asset_load(
                bad_ctx,
                pos_h,
                StrView::from_str("a.png"),
                &raw mut out_handle
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_asset_load(
                ctx_no_backend.as_ffi_ptr(),
                pos_h,
                bad_utf8_view,
                &raw mut out_handle
            ),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_asset_load(
                ctx_no_backend.as_ffi_ptr(),
                pos_h,
                StrView::from_str(""),
                &raw mut out_handle
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_load(
                ctx_no_backend.as_ffi_ptr(),
                pos_h,
                StrView::from_str("a.png"),
                &raw mut out_handle
            ),
            FfiResult::UNSUPPORTED
        );

        // `host_asset_get_load_state` guards + no-backend `UNSUPPORTED`
        assert_eq!(
            host_asset_get_load_state(ctx_no_backend.as_ffi_ptr(), null(), &raw mut out_state),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_asset_get_load_state(ctx_no_backend.as_ffi_ptr(), &valid_asset_handle, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_asset_get_load_state(
                ctx_no_backend.as_ffi_ptr(),
                unaligned_handle_const,
                &raw mut out_state
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_get_load_state(
                ctx_no_backend.as_ffi_ptr(),
                &valid_asset_handle,
                unaligned_state_mut
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_get_load_state(bad_ctx, &valid_asset_handle, &raw mut out_state),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_asset_get_load_state(
                ctx_no_backend.as_ffi_ptr(),
                &invalid_asset_handle,
                &raw mut out_state
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_get_load_state(
                ctx_no_backend.as_ffi_ptr(),
                &valid_asset_handle,
                &raw mut out_state
            ),
            FfiResult::UNSUPPORTED
        );

        // `host_asset_release` guards + no-backend `UNSUPPORTED`
        assert_eq!(
            host_asset_release(ctx_no_backend.as_ffi_ptr(), null()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_asset_release(ctx_no_backend.as_ffi_ptr(), unaligned_handle_const),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_release(bad_ctx, &valid_asset_handle),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_asset_release(ctx_no_backend.as_ffi_ptr(), &invalid_asset_handle),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_asset_release(ctx_no_backend.as_ffi_ptr(), &valid_asset_handle),
            FfiResult::UNSUPPORTED
        );

        // `host_cmd_spawn_scene` guards + no-backend `UNSUPPORTED`
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap_no_backend);
        let cmd_ptr = cmds.as_ffi_ptr();
        let mut out_ent = FfiEntity::NULL;

        assert_eq!(
            host_cmd_spawn_scene(cmd_ptr, null(), FfiEntity::NULL, &raw mut out_ent),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_scene(cmd_ptr, &valid_asset_handle, FfiEntity::NULL, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_scene(
                cmd_ptr,
                unaligned_handle_const,
                FfiEntity::NULL,
                &raw mut out_ent
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_spawn_scene(
                cmd_ptr,
                &valid_asset_handle,
                FfiEntity::NULL,
                unaligned_entity_mut
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_spawn_scene(
                null_mut(),
                &valid_asset_handle,
                FfiEntity::NULL,
                &raw mut out_ent
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_scene(
                bad_cmds,
                &valid_asset_handle,
                FfiEntity::NULL,
                &raw mut out_ent
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn_scene(
                cmd_ptr,
                &invalid_asset_handle,
                FfiEntity::NULL,
                &raw mut out_ent
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_spawn_scene(
                cmd_ptr,
                &FfiAssetHandle::NONE,
                FfiEntity::NULL,
                &raw mut out_ent
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_spawn_scene(cmd_ptr, &valid_asset_handle, bad_ent, &raw mut out_ent),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_spawn_scene(cmd_ptr, &valid_asset_handle, ph_ent, &raw mut out_ent),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_spawn_scene(cmd_ptr, &valid_asset_handle, parent_ffi, &raw mut out_ent),
            FfiResult::UNSUPPORTED
        );
        drop(cmds);

        // Now attach mock asset & scene backends (using trait default methods!)
        let asset_backend = Arc::new(MockAssetBackend::new());
        let spawned_scenes = Arc::new(Mutex::new(Vec::new()));
        let scene_backend = Arc::new(MockSceneBackend {
            spawned: Arc::clone(&spawned_scenes),
        });
        asset_backend.release_all_for_owner("cov_mod");

        let mut reg_with = reg_no_backend;
        reg_with.set_asset_backend(Arc::clone(&asset_backend) as Arc<dyn HostAssetBackend>);
        reg_with.set_scene_backend(Arc::clone(&scene_backend) as Arc<dyn HostSceneBackend>);
        let snap_with = reg_with.snapshot_arc();
        let ctx_with = HostContext::new(Arc::clone(&snap_with));

        // Happy paths + `FfiAssetHandle::NONE` fast paths + backend `Err` paths
        assert_eq!(
            host_asset_load(
                ctx_with.as_ffi_ptr(),
                pos_h,
                StrView::from_str("hero.glb"),
                &raw mut out_handle
            ),
            FfiResult::OK
        );
        assert!(out_handle.is_some());
        assert_eq!(
            host_asset_get_load_state(
                ctx_with.as_ffi_ptr(),
                &FfiAssetHandle::NONE,
                &raw mut out_state
            ),
            FfiResult::OK
        );
        assert_eq!(out_state, FfiAssetLoadState::NOT_LOADED);
        assert_eq!(
            host_asset_get_load_state(ctx_with.as_ffi_ptr(), &out_handle, &raw mut out_state),
            FfiResult::OK
        );
        assert_eq!(out_state, FfiAssetLoadState::LOADED);
        assert_eq!(
            host_asset_release(ctx_with.as_ffi_ptr(), &FfiAssetHandle::NONE),
            FfiResult::OK
        );
        assert_eq!(
            host_asset_release(ctx_with.as_ffi_ptr(), &out_handle),
            FfiResult::OK
        );

        asset_backend.set_error(Some((FfiResult::NOT_FOUND, "missing asset")));
        assert_eq!(
            host_asset_load(
                ctx_with.as_ffi_ptr(),
                pos_h,
                StrView::from_str("missing"),
                &raw mut out_handle
            ),
            FfiResult::NOT_FOUND
        );
        assert_eq!(ctx_with.last_error_message(), "missing asset");
        assert_eq!(
            host_asset_get_load_state(
                ctx_with.as_ffi_ptr(),
                &valid_asset_handle,
                &raw mut out_state
            ),
            FfiResult::NOT_FOUND
        );
        assert_eq!(
            host_asset_release(ctx_with.as_ffi_ptr(), &valid_asset_handle),
            FfiResult::NOT_FOUND
        );

        // Spawn scene with parent and apply to world
        let mut cmds2 = HostCommands::new(&mut storage, world.entity_allocator(), &snap_with);
        assert_eq!(
            host_cmd_spawn_scene(
                cmds2.as_ffi_ptr(),
                &valid_asset_handle,
                parent_ffi,
                &raw mut out_ent
            ),
            FfiResult::OK
        );
        drop(cmds2);
        storage.apply_to_world(&mut world, &snap_with);
        let root_e = Entity::from_bits(out_ent.to_bits());
        assert_eq!(
            world.get::<ChildOf>(root_e).map(ChildOf::parent),
            Some(parent_ent)
        );
        assert_eq!(spawned_scenes.lock().unwrap().len(), 1);
    }
}

// ============================================================================
// 4. `ffi/iter.rs` (`count_matching`, filters, sparse, res/msg/par) + `ffi/iter_ext.rs`
// ============================================================================

#[test]
fn iter_count_filters_res_msg_removed_and_parallel_edge_cases() {
    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    let pos_id = world.register_component::<CovPos>();
    let vel_id = world.register_component::<CovVel>();
    let sparse_id = world.register_component::<CovSparseTag>();
    let imm_id = world.register_component::<CovImmComp>();
    let res_id = world.register_component::<CovResVal>();
    world.init_resource::<Messages<CovMsgItem>>();
    let msg_res_id = world
        .components()
        .component_id::<Messages<CovMsgItem>>()
        .unwrap();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let vel_h = registry
        .register_component::<CovVel>(vel_id, ComponentOptions::default())
        .unwrap();
    let sparse_h = registry
        .register_component::<CovSparseTag>(sparse_id, ComponentOptions::default())
        .unwrap();
    let imm_h = registry
        .register_component::<CovImmComp>(imm_id, ComponentOptions::default())
        .unwrap();
    let res_h = registry.register_resource::<CovResVal>(res_id).unwrap();
    let msg_h = registry.register_message::<CovMsgItem>(msg_res_id).unwrap();

    // Also register a dynamic `SUMMARY_TICK` component to exercise summary-tick fast-skip and update!
    let mut sum_h = BamComponent::INVALID;
    {
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let sum_desc = ComponentDesc::new(
            c"cov::SumTick".as_ptr(),
            ComponentUuid::from_parts("cov::SumTick", 4, 4, 0),
            4,
            4,
        )
        .with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK);
        // SAFETY: Valid active `load_ctx`, `sum_desc`, and writable `sum_h`.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &sum_desc, &raw mut sum_h),
                FfiResult::OK
            );
        }
        registry = load_ctx.registry.clone();
        load_ctx.finish_init();
    }
    let sum_id = registry.get_by_handle(sum_h).unwrap().component_id;

    // Spawn dense + sparse + summary-tick entities across multiple ticks.
    let e_dense_1 = world
        .spawn((CovPos { x: 1.0, y: 0.0 }, CovVel::default()))
        .id();
    let e_dense_2 = world
        .spawn((CovPos { x: 2.0, y: 0.0 }, CovVel::default()))
        .id();
    let e_sparse_1 = world
        .spawn((CovPos { x: 3.0, y: 0.0 }, CovSparseTag { v: 10 }))
        .id();
    let e_sparse_2 = world
        .spawn((CovPos { x: 4.0, y: 0.0 }, CovSparseTag { v: 20 }))
        .id();
    let e_sum = world.spawn(CovPos { x: 5.0, y: 0.0 }).id();
    {
        let snap = registry.snapshot_arc();
        let mut st = HostCommandsStorage::new();
        let mut c = HostCommands::new(&mut st, world.entity_allocator(), &snap);
        let v: u32 = 99;
        // SAFETY: Valid `c` and 4-byte payload matching `sum_h`.
        unsafe {
            assert_eq!(
                host_cmd_insert(
                    c.as_ffi_ptr(),
                    FfiEntity::from_bits(e_sum.to_bits()),
                    sum_h,
                    (&raw const v).cast::<u8>(),
                    4,
                ),
                FfiResult::OK
            );
        }
        drop(c);
        st.apply_to_world(&mut world, &snap);
    }

    // Advance world tick and mutate only `e_dense_1` and `e_sparse_1` so tick filters selectively match!
    let t_after_spawn = world.increment_change_tick();
    let _t_mid = world.increment_change_tick();
    world.get_mut::<CovPos>(e_dense_1).unwrap().x = 11.0;
    world.get_mut::<CovSparseTag>(e_sparse_1).unwrap().v = 110;
    let t_now = world.read_change_tick();
    let _ = (e_dense_2, e_sparse_2);

    let msg_meta = registry.get_by_handle(msg_h).unwrap().clone();
    let mut poisoned = poisoned_header();
    let bad_iter = (&raw mut poisoned).cast::<BamIter>();

    // Helper to build `QueryTermMeta`
    let mk_term = |h: BamComponent, cid, mode, flags, size, align| QueryTermMeta {
        component: h,
        component_id: cid,
        mode,
        uuid: registry.get_by_handle(h).unwrap().uuid,
        size,
        align,
        storage_flags: flags,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    };

    // 1. Resource-only `HostIter` (`count_matching` == 0, `reset`, `par_for_each_chunk`)
    let res_only_plan = QueryPlan::from_terms(vec![
        mk_term(
            res_h,
            res_id,
            AccessMode::RESOURCE_READ,
            BAM_STORAGE_FLAG_RESOURCE,
            8,
            4,
        ),
        mk_term(
            res_h,
            res_id,
            AccessMode::RESOURCE_WRITE,
            BAM_STORAGE_FLAG_RESOURCE,
            8,
            4,
        ),
        QueryTermMeta {
            msg_read_batch_fn: msg_meta.msg_read_batch_fn,
            msg_send_fn: msg_meta.msg_send_fn,
            msg_write_batch_fn: msg_meta.msg_write_batch_fn,
            ..mk_term(
                msg_h,
                msg_res_id,
                AccessMode::MESSAGE_WRITE,
                BAM_STORAGE_FLAG_MESSAGE,
                4,
                4,
            )
        },
        // Message term with `None` fn pointers to hit `INVALID_COMPONENT` guards!
        mk_term(
            msg_h,
            msg_res_id,
            AccessMode::MESSAGE_WRITE,
            BAM_STORAGE_FLAG_MESSAGE,
            4,
            4,
        ),
        mk_term(pos_h, pos_id, AccessMode::REMOVED_READ, 0, 8, 4),
    ]);
    // SAFETY: Testing `HostIter` without `world` / `msg_cursors` and with invalid pointers.
    unsafe {
        let mut scratch = SparseRowScratch::default();
        let mut iter_ro = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &[],
            &res_only_plan,
            &mut scratch,
            t_after_spawn,
            t_now,
        )
        .with_generation(77);
        let it_ptr = iter_ro.as_ffi_ptr();

        let mut count = 99usize;
        assert_eq!(host_iter_count(it_ptr, null_mut()), FfiResult::NULL_POINTER);
        assert_eq!(
            host_iter_count(null_mut(), &raw mut count),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_count(bad_iter, &raw mut count),
            FfiResult::INVALID_STATE
        );
        assert_eq!(host_iter_count(it_ptr, &raw mut count), FfiResult::OK);
        assert_eq!(count, 0);
        assert_eq!(host_iter_reset(it_ptr), FfiResult::OK);
        assert_eq!(
            host_par_for_each_chunk(it_ptr, None, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_par_for_each_chunk(bad_iter, Some(noop_chunk_task), null_mut()),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_par_for_each_chunk(it_ptr, Some(noop_chunk_task), null_mut()),
            FfiResult::OK
        );

        // `host_res_ptr`, `host_res_mut_ptr`, `host_res_ticks` without `world` + error guards
        let mut ticks = BamChangeTicks::default();
        let unaligned_ticks = core::ptr::without_provenance_mut::<BamChangeTicks>(1);
        assert!(host_res_ptr(it_ptr, 0).is_null());
        assert!(host_res_mut_ptr(it_ptr, 1).is_null());
        assert_eq!(
            host_res_ticks(it_ptr, 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_res_ticks(it_ptr, 0, unaligned_ticks),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_res_ticks(bad_iter, 0, &raw mut ticks),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_res_ticks(it_ptr, 99, &raw mut ticks),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_res_ticks(it_ptr, 2, &raw mut ticks),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_res_ticks(it_ptr, 0, &raw mut ticks),
            FfiResult::INVALID_STATE
        );

        // `host_msg_read_batch`, `host_msg_send`, `host_msg_write_batch` guards
        let mut out_cnt = 0usize;
        let msg_val = CovMsgItem { code: 7 };
        assert_eq!(
            host_msg_read_batch(it_ptr, 2, null_mut(), 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_msg_read_batch(bad_iter, 2, null_mut(), 0, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_msg_read_batch(it_ptr, 99, null_mut(), 0, &raw mut out_cnt),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_msg_read_batch(it_ptr, 0, null_mut(), 0, &raw mut out_cnt),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_msg_read_batch(it_ptr, 3, null_mut(), 0, &raw mut out_cnt),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_msg_read_batch(it_ptr, 2, null_mut(), 0, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );

        assert_eq!(
            host_msg_send(bad_iter, 2, (&raw const msg_val).cast::<u8>(), 4),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_msg_send(it_ptr, 99, (&raw const msg_val).cast::<u8>(), 4),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_msg_send(it_ptr, 0, (&raw const msg_val).cast::<u8>(), 4),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_msg_send(it_ptr, 3, (&raw const msg_val).cast::<u8>(), 4),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_msg_send(it_ptr, 2, (&raw const msg_val).cast::<u8>(), 4),
            FfiResult::INVALID_STATE
        );

        assert_eq!(
            host_msg_write_batch(bad_iter, 2, (&raw const msg_val).cast::<u8>(), 1),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_msg_write_batch(it_ptr, 99, (&raw const msg_val).cast::<u8>(), 1),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_msg_write_batch(it_ptr, 0, (&raw const msg_val).cast::<u8>(), 1),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(host_msg_write_batch(it_ptr, 2, null(), 0), FfiResult::OK);
        assert_eq!(
            host_msg_write_batch(it_ptr, 2, null(), 1),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_msg_write_batch(it_ptr, 2, (&raw const msg_val).cast::<u8>(), usize::MAX),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_msg_write_batch(it_ptr, 3, (&raw const msg_val).cast::<u8>(), 1),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_msg_write_batch(it_ptr, 2, (&raw const msg_val).cast::<u8>(), 1),
            FfiResult::INVALID_STATE
        );

        // `host_iter_read_removed` (`ffi/iter_ext.rs`) guards
        let mut ent_buf = [FfiEntity::NULL; 4];
        let unaligned_usize = core::ptr::without_provenance_mut::<usize>(1);
        let unaligned_ent = core::ptr::without_provenance_mut::<FfiEntity>(1);
        assert_eq!(
            host_iter_read_removed(it_ptr, 4, ent_buf.as_mut_ptr(), 4, unaligned_usize),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_iter_read_removed(it_ptr, 4, unaligned_ent, 4, &raw mut out_cnt),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_iter_read_removed(bad_iter, 4, ent_buf.as_mut_ptr(), 4, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );
        // No `world` -> `INVALID_STATE`
        assert_eq!(
            host_iter_read_removed(it_ptr, 4, ent_buf.as_mut_ptr(), 4, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );
        drop(iter_ro);

        // With `world` (resource absent in world -> NOT_FOUND / null) and missing/wrong cursors!
        let cell = world.as_unsafe_world_cell();
        let mut iter_w = HostIter::new_dense(
            &cell.storages().tables,
            &[],
            &res_only_plan,
            t_after_spawn,
            t_now,
        )
        .with_world(cell);
        assert!(host_res_ptr(iter_w.as_ffi_ptr(), 0).is_null());
        assert!(host_res_mut_ptr(iter_w.as_ffi_ptr(), 1).is_null());
        assert_eq!(
            host_res_ticks(iter_w.as_ffi_ptr(), 0, &raw mut ticks),
            FfiResult::NOT_FOUND
        );
        // No `msg_cursors` -> `INVALID_STATE` on both `host_msg_read_batch` and `host_iter_read_removed`
        assert_eq!(
            host_msg_read_batch(iter_w.as_ffi_ptr(), 2, null_mut(), 0, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_iter_read_removed(
                iter_w.as_ffi_ptr(),
                4,
                ent_buf.as_mut_ptr(),
                4,
                &raw mut out_cnt
            ),
            FfiResult::INVALID_STATE
        );
        drop(iter_w);

        // With `msg_cursors` having `None` slot, then wrong boxed cursor type!
        let mut cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> =
            vec![None, None, None, None, None];
        let mut iter_c1 = HostIter::new_dense(
            &cell.storages().tables,
            &[],
            &res_only_plan,
            t_after_spawn,
            t_now,
        )
        .with_world(cell)
        .with_msg_cursors(&mut cursors);
        assert_eq!(
            host_msg_read_batch(iter_c1.as_ffi_ptr(), 2, null_mut(), 0, &raw mut out_cnt),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_iter_read_removed(
                iter_c1.as_ffi_ptr(),
                4,
                ent_buf.as_mut_ptr(),
                4,
                &raw mut out_cnt
            ),
            FfiResult::INVALID_STATE
        );
        drop(iter_c1);

        cursors[4] = Some(Box::new(123u32));
        let mut iter_c2 = HostIter::new_dense(
            &cell.storages().tables,
            &[],
            &res_only_plan,
            t_after_spawn,
            t_now,
        )
        .with_world(cell)
        .with_msg_cursors(&mut cursors);
        assert_eq!(
            host_iter_read_removed(
                iter_c2.as_ffi_ptr(),
                4,
                ent_buf.as_mut_ptr(),
                4,
                &raw mut out_cnt
            ),
            FfiResult::INVALID_STATE
        );
        drop(iter_c2);
    }

    // 2. Dense `count_matching` + `next_chunk` with tick filter, summary tick skip, `Or<...>`,
    //    `host_iter_column_readonly` edge branches, `host_iter_column_mutable_untracked`,
    //    and `host_iter_mark_changed_range` (empty, absent optional, summary tick)!
    let dense_tables: Vec<_> = world
        .archetypes()
        .iter()
        .filter(|a| a.contains(pos_id))
        .map(|a| a.table_id())
        .collect();
    let sum_tables: Vec<_> = world
        .archetypes()
        .iter()
        .filter(|a| a.contains(sum_id))
        .map(|a| a.table_id())
        .collect();

    let dense_filtered_plan = QueryPlan::from_terms_with_or_groups(
        vec![
            mk_term(pos_h, pos_id, AccessMode::CHANGED, 0, 8, 4),
            mk_term(vel_h, vel_id, AccessMode::OPTIONAL_WRITE, 0, 8, 4),
            mk_term(
                res_h,
                res_id,
                AccessMode::RESOURCE_READ,
                BAM_STORAGE_FLAG_RESOURCE,
                8,
                4,
            ),
            mk_term(vel_h, vel_id, AccessMode::FILTER_ONLY, 0, 8, 4),
            mk_term(pos_h, pos_id, AccessMode::ADDED, 0, 8, 4),
        ],
        &[0, 0, 0, 1, 1],
    );
    let sum_plan = QueryPlan::from_terms(vec![
        mk_term(sum_h, sum_id, AccessMode::WRITE, 0, 4, 4),
        mk_term(sum_h, sum_id, AccessMode::CHANGED, 0, 4, 4),
    ]);
    let dense_or_skip_plan = QueryPlan::from_terms_with_or_groups(
        vec![
            mk_term(pos_h, pos_id, AccessMode::READ, 0, 8, 4),
            mk_term(sum_h, sum_id, AccessMode::CHANGED, 0, 4, 4),
        ],
        &[0, 1],
    );
    // SAFETY: Valid `HostIter`s and stack outputs.
    unsafe {
        let mut cnt = 0usize;
        let mut it_df = HostIter::new_dense(
            &world.storages().tables,
            &dense_tables,
            &dense_filtered_plan,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_df.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        assert_eq!(cnt, 1);

        let mut chunk_len = 0usize;
        assert_eq!(
            host_iter_next_chunk(it_df.as_ffi_ptr(), &raw mut chunk_len),
            FfiBool::TRUE
        );
        assert_eq!(chunk_len, 1);
        // `host_iter_column_readonly` on resource term (2) and or_group term (3) -> null
        assert!(host_iter_column_readonly(it_df.as_ffi_ptr(), 2).is_null());
        assert!(host_iter_column_readonly(it_df.as_ffi_ptr(), 3).is_null());
        // `host_iter_column_mutable_untracked` on out-of-range and readonly term -> null
        assert!(host_iter_column_mutable_untracked(it_df.as_ffi_ptr(), 99).is_null());
        assert!(host_iter_column_mutable_untracked(it_df.as_ffi_ptr(), 0).is_null());
        // `host_iter_mark_changed_range` with `row_count == 0` -> OK
        assert_eq!(
            host_iter_mark_changed_range(it_df.as_ffi_ptr(), 1, 0, 0),
            FfiResult::OK
        );
        drop(it_df);

        // Summary tick fast-skip when unchanged (`last_run = t_after_spawn`, `this_run = t_now`)
        let mut it_sum_skip = HostIter::new_dense(
            &world.storages().tables,
            &sum_tables,
            &sum_plan,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_sum_skip.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        assert_eq!(cnt, 0);
        assert_eq!(
            host_iter_next_chunk(it_sum_skip.as_ffi_ptr(), &raw mut chunk_len),
            FfiBool::FALSE
        );
        drop(it_sum_skip);

        // Dense `Or` group with summary tick skip (`df.skip == true`)
        let mut it_or_skip = HostIter::new_dense(
            &world.storages().tables,
            &sum_tables,
            &dense_or_skip_plan,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_or_skip.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        assert_eq!(cnt, 0);
        drop(it_or_skip);

        // Summary tick update via `host_iter_column_mutable` and `host_iter_mark_changed_range`
        let sum_write_plan = QueryPlan::from_terms(vec![
            mk_term(sum_h, sum_id, AccessMode::WRITE, 0, 4, 4),
            mk_term(vel_h, vel_id, AccessMode::OPTIONAL_WRITE, 0, 8, 4),
        ]);
        let mut it_sum_w = HostIter::new_dense(
            &world.storages().tables,
            &sum_tables,
            &sum_write_plan,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_iter_next_chunk(it_sum_w.as_ffi_ptr(), &raw mut chunk_len),
            FfiBool::TRUE
        );
        assert!(!host_iter_column_mutable(it_sum_w.as_ffi_ptr(), 0).is_null());
        assert_eq!(
            host_iter_mark_changed_range(it_sum_w.as_ffi_ptr(), 0, 0, 1),
            FfiResult::OK
        );
        // Absent dense `OPTIONAL_WRITE` term -> `OK`
        assert_eq!(
            host_iter_mark_changed_range(it_sum_w.as_ffi_ptr(), 1, 0, 1),
            FfiResult::OK
        );
        drop(it_sum_w);
    }

    // 3. Archetypal (sparse) `count_matching` + `next_chunk` with tick filters + `Or` groups
    //    (dense + sparse `FILTER_ONLY`, `WITHOUT`, `CHANGED`, `ADDED`) + absent sparse `OPTIONAL_WRITE`
    //    + `host_par_for_each_chunk` on dense & archetypal (including error return and `in_par_iter`!)
    let sparse_archs: Vec<_> = world
        .archetypes()
        .iter()
        .filter(|a| a.contains(pos_id))
        .map(|a| a.id())
        .collect();
    let arch_unfiltered_plan = QueryPlan::from_terms(vec![
        mk_term(pos_h, pos_id, AccessMode::READ, 0, 8, 4),
        mk_term(
            sparse_h,
            sparse_id,
            AccessMode::OPTIONAL_WRITE,
            BAM_STORAGE_FLAG_SPARSE_SET,
            4,
            4,
        ),
    ]);
    let arch_filtered_plan = QueryPlan::from_terms_with_or_groups(
        vec![
            mk_term(pos_h, pos_id, AccessMode::READ, 0, 8, 4),
            mk_term(
                sparse_h,
                sparse_id,
                AccessMode::CHANGED,
                BAM_STORAGE_FLAG_SPARSE_SET,
                4,
                4,
            ),
            mk_term(vel_h, vel_id, AccessMode::WITHOUT, 0, 8, 4),
            mk_term(
                sparse_h,
                sparse_id,
                AccessMode::ADDED,
                BAM_STORAGE_FLAG_SPARSE_SET,
                4,
                4,
            ),
            mk_term(pos_h, pos_id, AccessMode::CHANGED, 0, 8, 4),
        ],
        &[0, 0, 1, 2, 2],
    );
    let arch_skip_plan = QueryPlan::from_terms_with_or_groups(
        vec![
            mk_term(pos_h, pos_id, AccessMode::READ, 0, 8, 4),
            mk_term(sum_h, sum_id, AccessMode::CHANGED, 0, 4, 4),
            mk_term(
                sparse_h,
                sparse_id,
                AccessMode::FILTER_ONLY,
                BAM_STORAGE_FLAG_SPARSE_SET,
                4,
                4,
            ),
        ],
        &[0, 0, 1],
    );
    // SAFETY: Valid `HostIter`s, `SparseRowScratch`, and callbacks.
    unsafe {
        let mut scratch = SparseRowScratch::default();
        let mut cnt = 0usize;
        let mut it_au = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &sparse_archs,
            &arch_unfiltered_plan,
            &mut scratch,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_au.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        assert!(cnt >= 5);
        // Iterate chunks: hit absent sparse `OPTIONAL_WRITE` on dense-only archetype!
        let mut chunk_len = 0usize;
        while host_iter_next_chunk(it_au.as_ffi_ptr(), &raw mut chunk_len).get() {
            assert_eq!(
                host_iter_mark_changed_range(it_au.as_ffi_ptr(), 1, 0, 1),
                FfiResult::OK
            );
        }
        drop(it_au);

        // Archetypal filtered `count_matching` + `next_chunk`
        let mut it_af = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &sparse_archs,
            &arch_filtered_plan,
            &mut scratch,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_af.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        // `e_sparse_1` has `CovSparseTag` changed, no `CovVel`, and `CovPos` added/unchanged
        let _ = host_iter_next_chunk(it_af.as_ffi_ptr(), &raw mut chunk_len);
        drop(it_af);

        // Archetypal skip plan (`af.skip == true` on archetypes missing `sum_id`)
        let mut it_as = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &sparse_archs,
            &arch_skip_plan,
            &mut scratch,
            t_after_spawn,
            t_now,
        );
        assert_eq!(
            host_iter_count(it_as.as_ffi_ptr(), &raw mut cnt),
            FfiResult::OK
        );
        assert_eq!(
            host_iter_next_chunk(it_as.as_ffi_ptr(), &raw mut chunk_len),
            FfiBool::FALSE
        );
        drop(it_as);

        // Empty dense / empty archetypal `host_par_for_each_chunk` -> `OK`
        let mut it_empty_d = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &sum_plan,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_par_for_each_chunk(it_empty_d.as_ffi_ptr(), Some(noop_chunk_task), null_mut()),
            FfiResult::OK
        );
        drop(it_empty_d);
        let mut it_empty_a = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &[],
            &arch_unfiltered_plan,
            &mut scratch,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_par_for_each_chunk(it_empty_a.as_ffi_ptr(), Some(noop_chunk_task), null_mut()),
            FfiResult::OK
        );
        drop(it_empty_a);

        // `host_par_for_each_chunk` on dense & archetypal exercising `in_par_iter` guards
        // (`reset`, `res_mut_ptr`, `msg_read_batch`, `msg_send`, `msg_write_batch`, `iter_read_removed`,
        // `iter_count` inside `in_par_iter`, AND worker returning non-OK `FfiResult::INTERNAL_ERROR`!).
        unsafe extern "C" fn par_probe_cb(sub_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
            // SAFETY: `sub_iter` is a valid task-local `HostIter` with `in_par_iter == true`.
            unsafe {
                if host_iter_reset(sub_iter) != FfiResult::ACCESS_CONFLICT {
                    return FfiResult::INVALID_ARGUMENT;
                }
                if !host_res_mut_ptr(sub_iter, 0).is_null() {
                    return FfiResult::INVALID_ARGUMENT;
                }
                let mut out_c = 0usize;
                if host_msg_read_batch(sub_iter, 0, null_mut(), 0, &raw mut out_c)
                    != FfiResult::ACCESS_CONFLICT
                {
                    return FfiResult::INVALID_ARGUMENT;
                }
                if host_msg_send(sub_iter, 0, null(), 0) != FfiResult::ACCESS_CONFLICT {
                    return FfiResult::INVALID_ARGUMENT;
                }
                if host_msg_write_batch(sub_iter, 0, null(), 0) != FfiResult::ACCESS_CONFLICT {
                    return FfiResult::INVALID_ARGUMENT;
                }
                if host_iter_read_removed(sub_iter, 0, null_mut(), 0, &raw mut out_c)
                    != FfiResult::ACCESS_CONFLICT
                {
                    return FfiResult::INVALID_ARGUMENT;
                }
                let mut sub_cnt = 0usize;
                if host_iter_count(sub_iter, &raw mut sub_cnt) != FfiResult::OK || sub_cnt == 0 {
                    return FfiResult::INVALID_ARGUMENT;
                }
            }
            FfiResult::INTERNAL_ERROR
        }

        let dense_pos_plan =
            QueryPlan::from_terms(vec![mk_term(pos_h, pos_id, AccessMode::READ, 0, 8, 4)]);
        let mut it_par_d = HostIter::new_dense(
            &world.storages().tables,
            &dense_tables,
            &dense_pos_plan,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_par_for_each_chunk(it_par_d.as_ffi_ptr(), Some(par_probe_cb), null_mut()),
            FfiResult::INTERNAL_ERROR
        );
        drop(it_par_d);

        // Single-partition archetypal parallel run + error propagation
        let mut it_par_a = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &sparse_archs[..1],
            &arch_unfiltered_plan,
            &mut scratch,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_par_for_each_chunk(it_par_a.as_ffi_ptr(), Some(par_probe_cb), null_mut()),
            FfiResult::INTERNAL_ERROR
        );
        drop(it_par_a);

        // Multi-partition (>512 sparse entities) archetypal parallel run where error aborts peers!
        for i in 0..600u32 {
            world.spawn((
                CovPos {
                    x: i as f32,
                    y: 0.0,
                },
                CovSparseTag { v: i },
            ));
        }
        let big_sparse_archs: Vec<_> = world
            .archetypes()
            .iter()
            .filter(|a| a.contains(pos_id) && a.contains(sparse_id))
            .map(|a| a.id())
            .collect();
        let mut it_par_big = HostIter::new_archetypal(
            &world.storages().tables,
            world.archetypes(),
            &world.storages().sparse_sets,
            &big_sparse_archs,
            &arch_unfiltered_plan,
            &mut scratch,
            Tick::new(0),
            t_now,
        );
        assert_eq!(
            host_par_for_each_chunk(it_par_big.as_ffi_ptr(), Some(par_probe_cb), null_mut()),
            FfiResult::INTERNAL_ERROR
        );
        drop(it_par_big);

        // Also check immutable column rejection in `host_iter_column_mutable` / `column_rows_readonly`
        let imm_plan = QueryPlan::from_terms(vec![mk_term(
            imm_h,
            imm_id,
            AccessMode::READ,
            BAM_STORAGE_FLAG_IMMUTABLE,
            4,
            4,
        )]);
        let mut it_imm = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &imm_plan,
            Tick::new(0),
            t_now,
        );
        assert!(host_iter_entities(it_imm.as_ffi_ptr()).is_null());
        assert!(host_iter_column_rows_readonly(it_imm.as_ffi_ptr(), 0).is_null());
        assert!(host_iter_column_rows_mutable(it_imm.as_ffi_ptr(), 0).is_null());
        let mut col_info = BamColumnInfo {
            component: BamComponent::INVALID,
            mode: AccessMode::READ,
            uuid: ComponentUuid::ZERO,
            size: 0,
            align: 1,
            storage_flags: 0,
            reserved0: 0,
        };
        assert_eq!(
            host_iter_column_info(it_imm.as_ffi_ptr(), 0, &raw mut col_info),
            FfiResult::OK
        );
        drop(it_imm);
    }
}

// ============================================================================
// 5. `ffi/commands.rs` & `ffi/commands_ext.rs` edge cases
// ============================================================================

#[test]
fn commands_and_commands_ext_edge_cases() {
    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    let pos_id = world.register_component::<CovPos>();
    let vel_id = world.register_component::<CovVel>();
    let no_def_id = world.register_component::<CovNoDefaultComp>();
    let res_id = world.register_component::<CovResVal>();
    let child_of_id = world.register_component::<ChildOf>();
    let global_ev_key = world.register_event_key::<CovGlobalEv>();
    let ent_ev_key = world.register_event_key::<CovEntEv>();
    world.init_resource::<Messages<CovMsgItem>>();
    let msg_res_id = world
        .components()
        .component_id::<Messages<CovMsgItem>>()
        .unwrap();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let vel_h = registry
        .register_component::<CovVel>(vel_id, ComponentOptions::default())
        .unwrap();
    // Register `CovNoDefaultComp` as opaque (`allow_raw_insert = false`, no default)
    let no_raw_h = registry
        .register_component::<CovNoDefaultComp>(no_def_id, ComponentOptions::new().opaque())
        .unwrap();
    let res_h = registry.register_resource::<CovResVal>(res_id).unwrap();
    let msg_h = registry.register_message::<CovMsgItem>(msg_res_id).unwrap();
    let child_of_h = registry.register_hierarchy(child_of_id, true).unwrap();
    let global_ev_h = registry
        .register_event::<CovGlobalEv>(global_ev_key)
        .unwrap();
    let ent_ev_h = registry
        .register_entity_event::<CovEntEv>(ent_ev_key)
        .unwrap();
    let ready_h = registry
        .register_bridged_entity_event::<CovReadyEv>(&mut world, "bevy_scene::Ready", Some("Ready"))
        .unwrap();

    // Register a large 70KB dynamic component so 3 inserts force 3 slabs in `CommandArena::reset`
    // (`self.slabs.len() > 2` truncation branch!).
    let mut big_h = BamComponent::INVALID;
    let mut dyn_drop_h = BamComponent::INVALID;
    let mut dyn_ev_h = BamComponent::INVALID;
    static CMD_DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn cmd_drop_fn(_ptr: *mut c_void) {
        CMD_DROPS.fetch_add(1, Ordering::SeqCst);
    }
    {
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(77)),
            None,
        );
        let big_desc = ComponentDesc::new(
            c"cov::Big70K".as_ptr(),
            ComponentUuid::from_parts("cov::Big70K", 70 * 1024, 16, 0),
            70 * 1024,
            16,
        );
        let drop_desc = ComponentDesc::new(
            c"cov::CmdDrop".as_ptr(),
            ComponentUuid::from_parts("cov::CmdDrop", 8, 8, 0),
            8,
            8,
        )
        .with_drop(cmd_drop_fn);
        let dyn_ev_desc = ComponentDesc::new(
            c"cov::DynEv".as_ptr(),
            ComponentUuid::from_parts("cov::DynEv", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid active `load_ctx`, descriptors, and writable outputs.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &big_desc, &raw mut big_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &drop_desc, &raw mut dyn_drop_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &dyn_ev_desc, &raw mut dyn_ev_h),
                FfiResult::OK
            );
        }
        registry = load_ctx.registry.clone();
        load_ctx.finish_init();
    }

    let snap = registry.snapshot_arc();
    let live_e = world.spawn_empty().id();
    let live_ffi = FfiEntity::from_bits(live_e.to_bits());
    let temp_parent = world.spawn_empty().id();
    let temp_parent_ffi = FfiEntity::from_bits(temp_parent.to_bits());
    let doomed_ev_target = world.spawn_empty().id();
    let bad_ent = invalid_entity_bits();
    let ph_ent = placeholder_entity();

    let mut poisoned = poisoned_header();
    let bad_cmds = (&raw mut poisoned).cast::<BamCommands>();

    // SAFETY: Testing all FFI command validation and apply paths with valid/invalid handles.
    unsafe {
        let mut storage = HostCommandsStorage::new();
        let mut cmds =
            HostCommands::with_generation(99, &mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(77)), None);
        let cmd_ptr = cmds.as_ffi_ptr();
        let mut out_e = FfiEntity::NULL;
        let pos = CovPos { x: 1.0, y: 2.0 };
        let pos_ptr = (&raw const pos).cast::<u8>();

        // 1. `ffi/commands_ext.rs` all 5 previously uncovered guards
        assert_eq!(
            host_cmd_spawn_persistent(cmd_ptr, null(), null(), 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_persistent(bad_cmds, null(), null(), 0, &raw mut out_e),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn_batch(bad_cmds, null(), null(), 0, 0, null_mut()),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_despawn_all(bad_cmds, null(), 1),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_insert_if_new(bad_cmds, live_ffi, pos_h, pos_ptr, 8),
            FfiResult::INVALID_STATE
        );

        // 2. `host_cmd_spawn` / `host_cmd_spawn_empty` / `host_cmd_insert` / `insert_default` / `remove` / `despawn` guards
        assert_eq!(
            host_cmd_spawn(cmd_ptr, null(), null(), 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn(bad_cmds, null(), null(), 0, &raw mut out_e),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn(cmd_ptr, null(), null(), 1, &raw mut out_e),
            FfiResult::NULL_POINTER
        );
        let bad_comp_arr = [BamComponent::from_raw(9999)];
        let ptr_arr = [pos_ptr];
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                bad_comp_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                &raw mut out_e
            ),
            FfiResult::INVALID_COMPONENT
        );
        let res_arr = [res_h];
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                res_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                &raw mut out_e
            ),
            FfiResult::INVALID_COMPONENT
        );
        let null_payload_arr = [null::<u8>()];
        let pos_arr = [pos_h];
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                pos_arr.as_ptr(),
                null_payload_arr.as_ptr(),
                1,
                &raw mut out_e
            ),
            FfiResult::NULL_POINTER
        );

        // `host_cmd_spawn_batch` validation guards
        assert_eq!(
            host_cmd_spawn_batch(cmd_ptr, null(), null(), 1, 1, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                bad_comp_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                1,
                null_mut()
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                res_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                1,
                null_mut()
            ),
            FfiResult::INVALID_COMPONENT
        );
        let no_raw_arr = [no_raw_h];
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                no_raw_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                1,
                null_mut()
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                pos_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                usize::MAX,
                null_mut()
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                pos_arr.as_ptr(),
                null_payload_arr.as_ptr(),
                1,
                1,
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                pos_arr.as_ptr(),
                ptr_arr.as_ptr(),
                1,
                0,
                null_mut()
            ),
            FfiResult::OK
        );

        // `host_cmd_insert` with `ChildOf` invalid parent & self-parent
        let self_link = FfiChildOf::new(live_ffi);
        assert_eq!(
            host_cmd_insert(
                cmd_ptr,
                live_ffi,
                child_of_h,
                (&raw const self_link).cast::<u8>(),
                8
            ),
            FfiResult::INVALID_ARGUMENT
        );
        let bad_link = FfiChildOf::new(ph_ent);
        assert_eq!(
            host_cmd_insert(
                cmd_ptr,
                live_ffi,
                child_of_h,
                (&raw const bad_link).cast::<u8>(),
                8
            ),
            FfiResult::INVALID_ENTITY
        );

        // `host_cmd_insert_default` & `host_cmd_remove` guards
        assert_eq!(
            host_cmd_insert_default(bad_cmds, live_ffi, pos_h),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_insert_default(cmd_ptr, live_ffi, BamComponent::from_raw(9999)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_insert_default(cmd_ptr, live_ffi, res_h),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_insert_default(cmd_ptr, live_ffi, no_raw_h),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_cmd_remove(bad_cmds, live_ffi, pos_h),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_remove(cmd_ptr, live_ffi, BamComponent::from_raw(9999)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_remove(cmd_ptr, live_ffi, res_h),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_despawn(bad_cmds, live_ffi),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn_empty(bad_cmds, &raw mut out_e),
            FfiResult::INVALID_STATE
        );

        // `host_cmd_despawn_all` guards + `WITHOUT` in `or_group`
        let many_terms = [BamTermDesc::new(pos_h, AccessMode::FILTER_ONLY); 33];
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, many_terms.as_ptr(), 33),
            FfiResult::CAPACITY_EXCEEDED
        );
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, null(), 1),
            FfiResult::NULL_POINTER
        );
        let mut bad_term = BamTermDesc::new(pos_h, AccessMode::FILTER_ONLY);
        bad_term.flags = 0xFFFF;
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, &bad_term, 1),
            FfiResult::UNSUPPORTED
        );
        let unk_term = BamTermDesc::new(BamComponent::from_raw(9999), AccessMode::FILTER_ONLY);
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, &unk_term, 1),
            FfiResult::INVALID_COMPONENT
        );
        let res_term = BamTermDesc::new(res_h, AccessMode::FILTER_ONLY);
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, &res_term, 1),
            FfiResult::INVALID_COMPONENT
        );

        // Queue valid `WITHOUT` inside `or_group` for `despawn_all`
        let or_terms = [
            BamTermDesc::new(pos_h, AccessMode::FILTER_ONLY).with_or_group(1),
            BamTermDesc::new(vel_h, AccessMode::WITHOUT).with_or_group(1),
        ];
        assert_eq!(
            host_cmd_despawn_all(cmd_ptr, or_terms.as_ptr(), 2),
            FfiResult::OK
        );

        // 3. `host_cmd_trigger` exhaustive guards + happy paths
        let ev_val = CovGlobalEv { val: 7 };
        assert_eq!(
            host_cmd_trigger(cmd_ptr, global_ev_h, FfiEntity::NULL, null(), 4),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_trigger(
                bad_cmds,
                global_ev_h,
                FfiEntity::NULL,
                (&raw const ev_val).cast::<u8>(),
                4
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                BamComponent::from_raw(9999),
                FfiEntity::NULL,
                null(),
                0
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger(cmd_ptr, res_h, FfiEntity::NULL, null(), 0),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger(cmd_ptr, msg_h, FfiEntity::NULL, null(), 0),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger(cmd_ptr, dyn_drop_h, FfiEntity::NULL, null(), 0),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                global_ev_h,
                FfiEntity::NULL,
                (&raw const ev_val).cast::<u8>(),
                8
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                global_ev_h,
                bad_ent,
                (&raw const ev_val).cast::<u8>(),
                4
            ),
            FfiResult::INVALID_ENTITY
        );
        // Bridged `Ready` (`synthesizes_ffi_entity_payload`) with `target == NULL`: bad bits / placeholder / valid
        let bad_bits_u64 = bad_ent.to_bits();
        let ph_bits_u64 = ph_ent.to_bits();
        let live_bits_u64 = live_ffi.to_bits();
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ready_h,
                FfiEntity::NULL,
                (&raw const bad_bits_u64).cast::<u8>(),
                8
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ready_h,
                FfiEntity::NULL,
                (&raw const ph_bits_u64).cast::<u8>(),
                8
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ready_h,
                FfiEntity::NULL,
                (&raw const live_bits_u64).cast::<u8>(),
                8
            ),
            FfiResult::OK
        );
        // Typed `EntityEvent` with `target == NULL` extracting invalid/placeholder/valid target
        let ent_ev_ph = CovEntEv {
            target: TargetEntity(ph_ent),
            amount: 1,
        };
        let ent_ev_bad = CovEntEv {
            target: TargetEntity(bad_ent),
            amount: 1,
        };
        let ent_ev_ok = CovEntEv {
            target: TargetEntity(live_ffi),
            amount: 5,
        };
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ent_ev_h,
                FfiEntity::NULL,
                (&raw const ent_ev_ph).cast::<u8>(),
                core::mem::size_of::<CovEntEv>(),
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ent_ev_h,
                FfiEntity::NULL,
                (&raw const ent_ev_bad).cast::<u8>(),
                core::mem::size_of::<CovEntEv>(),
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                ent_ev_h,
                FfiEntity::NULL,
                (&raw const ent_ev_ok).cast::<u8>(),
                core::mem::size_of::<CovEntEv>(),
            ),
            FfiResult::OK
        );
        // Trigger on dead target (despawned before apply!)
        assert_eq!(
            host_cmd_trigger(
                cmd_ptr,
                global_ev_h,
                FfiEntity::from_bits(doomed_ev_target.to_bits()),
                (&raw const ev_val).cast::<u8>(),
                4,
            ),
            FfiResult::OK
        );

        // Queue `ChildOf` targeting `temp_parent` via `Insert`, `SpawnBatch`, and `SpawnSoaBatch`,
        // then despawn `temp_parent` BEFORE `apply_to_world` so all 3 dead-parent skips execute!
        let parent_link = FfiChildOf::new(temp_parent_ffi);
        assert_eq!(
            host_cmd_insert(
                cmd_ptr,
                live_ffi,
                child_of_h,
                (&raw const parent_link).cast::<u8>(),
                8
            ),
            FfiResult::OK
        );
        let co_comps = [child_of_h];
        let co_ptrs = [(&raw const parent_link).cast::<u8>()];
        let mut child1 = FfiEntity::NULL;
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                co_comps.as_ptr(),
                co_ptrs.as_ptr(),
                1,
                &raw mut child1
            ),
            FfiResult::OK
        );
        let mut child2 = [FfiEntity::NULL; 1];
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                co_comps.as_ptr(),
                co_ptrs.as_ptr(),
                1,
                1,
                child2.as_mut_ptr()
            ),
            FfiResult::OK
        );

        // Force 3 slabs in `CommandArena` (`3 * 70KB > 2 * 64KB`) so `reset()` truncates slabs > 2!
        let big_buf = vec![0xABu8; 70 * 1024];
        for _ in 0..3 {
            assert_eq!(
                host_cmd_insert(cmd_ptr, live_ffi, big_h, big_buf.as_ptr(), big_buf.len()),
                FfiResult::OK
            );
        }

        // Verify `MAX_SPAWN_BATCH_COMPONENTS` capacity exceeded
        let over_comps = [pos_h; MAX_SPAWN_BATCH_COMPONENTS + 1];
        let over_ptrs = [pos_ptr; MAX_SPAWN_BATCH_COMPONENTS + 1];
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                over_comps.as_ptr(),
                over_ptrs.as_ptr(),
                MAX_SPAWN_BATCH_COMPONENTS + 1,
                &raw mut out_e,
            ),
            FfiResult::CAPACITY_EXCEEDED
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmd_ptr,
                over_comps.as_ptr(),
                over_ptrs.as_ptr(),
                MAX_SPAWN_BATCH_COMPONENTS + 1,
                1,
                null_mut(),
            ),
            FfiResult::CAPACITY_EXCEEDED
        );

        drop(cmds);

        // Despawn `temp_parent` and `doomed_ev_target` before applying!
        world.despawn(temp_parent);
        world.despawn(doomed_ev_target);
        storage.apply_to_world(&mut world, &snap);

        // Now test `SpawnSoaBatch` stale unloaded dynamic `drop_fn` component skipped during `apply_to_world`
        let soa_drop_vals = [10u64, 20u64];
        let soa_drop_comps = [dyn_drop_h];
        let soa_drop_ptrs = [soa_drop_vals.as_ptr().cast::<u8>()];
        let mut soa_out = [FfiEntity::NULL; 2];
        let mut c2 = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(77)), None);
        assert_eq!(
            host_cmd_spawn_batch(
                c2.as_ffi_ptr(),
                soa_drop_comps.as_ptr(),
                soa_drop_ptrs.as_ptr(),
                1,
                2,
                soa_out.as_mut_ptr(),
            ),
            FfiResult::OK
        );
        drop(c2);
        // Replace world's registry with an empty `ModRegistry` so `apply_to_world` sees vanished
        // `dyn_drop_h` handles and invokes uncommitted destructors for both `SpawnSoaBatch` and `Insert`!
        let mut c3 = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let single_drop_val = 30u64;
        assert_eq!(
            host_cmd_insert(
                c3.as_ffi_ptr(),
                live_ffi,
                dyn_drop_h,
                (&raw const single_drop_val).cast::<u8>(),
                8,
            ),
            FfiResult::OK
        );
        drop(c3);
        world.insert_resource(ModRegistry::new());
        storage.apply_to_world(&mut world, &snap);
        assert_eq!(CMD_DROPS.load(Ordering::SeqCst), 3);
    }
}

// ============================================================================
// 6. `ffi/observers_ext.rs` & `ffi/register.rs` exhaustive coverage
// ============================================================================

#[test]
fn observers_ext_and_register_exhaustive_coverage() {
    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    ensure_dynamic_hook_map_in_world(&mut world);

    let pos_id = world.register_component::<CovPos>();
    let vel_id = world.register_component::<CovVel>();
    let zero_id = world.register_component::<CovZeroComp>();
    let res_id = world.register_component::<CovResVal>();
    let child_of_id = world.register_component::<ChildOf>();
    let global_ev_key = world.register_event_key::<CovGlobalEv>();
    let ent_ev_key = world.register_event_key::<CovEntEv>();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let _vel_h = registry
        .register_component::<CovVel>(vel_id, ComponentOptions::default())
        .unwrap();
    let zero_h = registry
        .register_component::<CovZeroComp>(zero_id, ComponentOptions::default())
        .unwrap();
    let res_h = registry.register_resource::<CovResVal>(res_id).unwrap();
    let child_of_h = registry.register_hierarchy(child_of_id, true).unwrap();
    let global_ev_h = registry
        .register_event::<CovGlobalEv>(global_ev_key)
        .unwrap();
    let ent_ev_h = registry
        .register_entity_event::<CovEntEv>(ent_ev_key)
        .unwrap();

    let mut poisoned = poisoned_header();
    let bad_load = (&raw mut poisoned).cast::<BamLoadCtx>();
    let bad_cmds = (&raw mut poisoned).cast::<BamCommands>();
    let bad_ent = invalid_entity_bits();
    let ph_ent = placeholder_entity();
    let bad_utf8_cstr = [0xFFu8, 0u8];
    let bad_utf8_ptr = bad_utf8_cstr.as_ptr().cast::<c_char>();

    // --- Part A: `ffi/register.rs` standalone `LoadContext`, resolve, relationships, components, systems, observers ---
    // SAFETY: Testing all registration validation and happy paths with valid/invalid pointers.
    unsafe {
        // Standalone `LoadContext::new` (no external world)
        let mut standalone = LoadContext::new(registry.clone());
        let mut stand_h = BamComponent::INVALID;
        let stand_desc = ComponentDesc::new(
            c"UnqualifiedStandalone".as_ptr(),
            ComponentUuid::from_parts("UnqualifiedStandalone", 4, 4, 0),
            4,
            4,
        );
        assert_eq!(
            host_register_component(standalone.as_ffi_ptr(), &stand_desc, &raw mut stand_h),
            FfiResult::OK
        );
        standalone.finish_init();
        // Inactive `LoadContext` rejected by `host_load_host_ctx`, `host_register_component`, `host_register_observer`
        assert!(host_load_host_ctx(standalone.as_ffi_ptr()).is_null());
        assert_eq!(
            host_register_component(standalone.as_ffi_ptr(), &stand_desc, &raw mut stand_h),
            FfiResult::INVALID_STATE
        );
        let obs_tmp = BamObserverDesc::new(global_ev_h, noop_observer, null_mut());
        assert_eq!(
            host_register_observer(standalone.as_ffi_ptr(), &obs_tmp),
            FfiResult::INVALID_STATE
        );

        // `host_resolve_component` edge branches
        let ctx_ptr = HostContext::new(registry.snapshot_arc());
        let mut out_c = BamComponent::INVALID;
        let pos_uuid = CovPos::UUID;
        let missing_uuid = ComponentUuid::from_canonical_name("cov::MissingUUID");
        assert_eq!(
            host_resolve_component(
                ctx_ptr.as_ffi_ptr(),
                c"cov::Pos".as_ptr(),
                null(),
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component(ctx_ptr.as_ffi_ptr(), bad_utf8_ptr, null(), &raw mut out_c),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_resolve_component(ctx_ptr.as_ffi_ptr(), c"".as_ptr(), null(), &raw mut out_c),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_resolve_component(
                ctx_ptr.as_ffi_ptr(),
                c"cov::WrongName".as_ptr(),
                &pos_uuid,
                &raw mut out_c,
            ),
            FfiResult::TYPE_MISMATCH
        );
        assert_eq!(
            host_resolve_component(
                ctx_ptr.as_ffi_ptr(),
                c"cov::WrongName".as_ptr(),
                &missing_uuid,
                &raw mut out_c,
            ),
            FfiResult::NOT_FOUND
        );
        assert_eq!(
            host_resolve_component(ctx_ptr.as_ffi_ptr(), null(), &missing_uuid, &raw mut out_c),
            FfiResult::NOT_FOUND
        );
        // `host_get_proc_address` check too
        assert!(host_get_proc_address(ctx_ptr.as_ffi_ptr(), null()).is_null());

        // Active `LoadContext` bound to `world`
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let lp = load_ctx.as_ffi_ptr();

        // Register dynamic relationship components + dynamic ZST event component
        let mut rel_src_h = BamComponent::INVALID;
        let mut rel_src2_h = BamComponent::INVALID;
        let mut rel_tgt_h = BamComponent::INVALID;
        let mut rel_tgt2_h = BamComponent::INVALID;
        let mut rel_bad_size_h = BamComponent::INVALID;
        let mut dyn_zst_ev_h = BamComponent::INVALID;
        let mut dyn_pod_ev_h = BamComponent::INVALID;
        for (name, size, align, out) in [
            (c"cov::RelSrc", 8usize, 8usize, &raw mut rel_src_h),
            (c"cov::RelSrc2", 8, 8, &raw mut rel_src2_h),
            (c"cov::RelTgt", 0, 1, &raw mut rel_tgt_h),
            (c"cov::RelTgt2", 0, 1, &raw mut rel_tgt2_h),
            (c"cov::RelBadSize", 4, 4, &raw mut rel_bad_size_h),
            (c"cov::DynZstEv", 0, 1, &raw mut dyn_zst_ev_h),
            (c"cov::DynPodEv", 4, 4, &raw mut dyn_pod_ev_h),
        ] {
            let s = name.to_str().unwrap();
            let d = ComponentDesc::new(
                name.as_ptr(),
                ComponentUuid::from_parts(s, size, align, 0),
                size,
                align,
            );
            assert_eq!(host_register_component(lp, &d, out), FfiResult::OK);
        }

        // `__bam_rel_bind::...` error branches
        let bind_try = |lp: *mut BamLoadCtx, spec: &str, size: usize, align: usize| -> FfiResult {
            let cname = CString::new(spec).unwrap();
            let d = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(spec, size, align, 0),
                size,
                align,
            );
            let mut out = BamComponent::INVALID;
            host_register_component(lp, &d, &raw mut out)
        };
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_tgt_h.0),
                4,
                4
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            bind_try(lp, "__bam_rel_bind::no_delim", 0, 1),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            bind_try(lp, "__bam_rel_bind::abc::def", 0, 1),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            bind_try(lp, "__bam_rel_bind::0::1", 0, 1),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            bind_try(lp, "__bam_rel_bind::9999::1", 0, 1),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            bind_try(lp, &format!("__bam_rel_bind::{}::9999", rel_src_h.0), 0, 1),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", pos_h.0, rel_tgt_h.0),
                0,
                1
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_bad_size_h.0, rel_tgt_h.0),
                0,
                1
            ),
            FfiResult::TYPE_MISMATCH
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_bad_size_h.0),
                0,
                1
            ),
            FfiResult::TYPE_MISMATCH
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_src_h.0),
                0,
                1
            ),
            FfiResult::TYPE_MISMATCH
        );
        // Valid bind + idempotent re-bind + conflicting re-binds!
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_tgt_h.0),
                0,
                1
            ),
            FfiResult::OK
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_tgt_h.0),
                0,
                1
            ),
            FfiResult::OK
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src_h.0, rel_tgt2_h.0),
                0,
                1
            ),
            FfiResult::TYPE_MISMATCH
        );
        assert_eq!(
            bind_try(
                lp,
                &format!("__bam_rel_bind::{}::{}", rel_src2_h.0, rel_tgt_h.0),
                0,
                1
            ),
            FfiResult::TYPE_MISMATCH
        );

        // `host_register_component` validation branches (`required_components`, `fields`, `required_init_descs`)
        let mut dummy_out = BamComponent::INVALID;
        let bad_layout = ComponentDesc::new(
            c"cov::BadLayout".as_ptr(),
            ComponentUuid::from_canonical_name("cov::BadLayout"),
            5,
            4,
        );
        assert_eq!(
            host_register_component(lp, &bad_layout, &raw mut dummy_out),
            FfiResult::INVALID_ARGUMENT
        );

        let req_unk = [BamComponent::from_raw(9999)];
        let d_req_unk = ComponentDesc::new(
            c"cov::ReqUnk".as_ptr(),
            ComponentUuid::from_canonical_name("cov::ReqUnk"),
            4,
            4,
        )
        .with_required_components(req_unk.as_ptr(), 1);
        assert_eq!(
            host_register_component(lp, &d_req_unk, &raw mut dummy_out),
            FfiResult::INVALID_COMPONENT
        );

        let req_res = [res_h];
        let d_req_res = ComponentDesc::new(
            c"cov::ReqRes".as_ptr(),
            ComponentUuid::from_canonical_name("cov::ReqRes"),
            4,
            4,
        )
        .with_required_components(req_res.as_ptr(), 1);
        assert_eq!(
            host_register_component(lp, &d_req_res, &raw mut dummy_out),
            FfiResult::INVALID_COMPONENT
        );

        let req_self = [pos_h];
        let d_req_self = ComponentDesc::new(c"cov::Pos".as_ptr(), CovPos::UUID, 8, 4)
            .with_required_components(req_self.as_ptr(), 1);
        assert_eq!(
            host_register_component(lp, &d_req_self, &raw mut dummy_out),
            FfiResult::INVALID_ARGUMENT
        );

        let bad_field = [BamFieldDesc::new(
            BamFieldKind::F32,
            StrView {
                ptr: bad_utf8_cstr.as_ptr(),
                len: 1,
            },
            StrView::from_str("f32"),
            0,
            4,
            4,
        )];
        let d_bad_field = ComponentDesc::new(
            c"cov::BadField".as_ptr(),
            ComponentUuid::from_canonical_name("cov::BadField"),
            4,
            4,
        )
        .with_fields(bad_field.as_ptr(), 1);
        assert_eq!(
            host_register_component(lp, &d_bad_field, &raw mut dummy_out),
            FfiResult::INVALID_UTF8
        );

        // `required_init_descs` validation branches
        let mut req_init = BamRequiredComponent::by_handle(pos_h);
        req_init.struct_size = 4;
        let mut d_ri = ComponentDesc::new(
            c"cov::ReqInit".as_ptr(),
            ComponentUuid::from_canonical_name("cov::ReqInit"),
            4,
            4,
        );
        d_ri.required_init_descs = &raw const req_init;
        d_ri.required_init_desc_count = 1;
        assert_eq!(
            host_register_component(lp, &d_ri, &raw mut dummy_out),
            FfiResult::INVALID_VERSION
        );

        let mut req_init2 = BamRequiredComponent::by_handle(pos_h);
        req_init2.flags = 0x8000_0000;
        d_ri.required_init_descs = &raw const req_init2;
        assert_eq!(
            host_register_component(lp, &d_ri, &raw mut dummy_out),
            FfiResult::UNSUPPORTED
        );

        let mut req_init3 = BamRequiredComponent::by_handle(pos_h);
        req_init3.reserved0 = 1;
        d_ri.required_init_descs = &raw const req_init3;
        assert_eq!(
            host_register_component(lp, &d_ri, &raw mut dummy_out),
            FfiResult::INVALID_ARGUMENT
        );

        let mut req_init4 = BamRequiredComponent::by_handle(pos_h);
        req_init4.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        req_init4.init_size = 4;
        req_init4.init_bytes = null();
        d_ri.required_init_descs = &raw const req_init4;
        assert_eq!(
            host_register_component(lp, &d_ri, &raw mut dummy_out),
            FfiResult::NULL_POINTER
        );

        let mut req_init5 = BamRequiredComponent::by_handle(pos_h);
        req_init5.flags = BAM_REQUIRED_FLAG_INIT_FN;
        req_init5.init_fn = None;
        d_ri.required_init_descs = &raw const req_init5;
        assert_eq!(
            host_register_component(lp, &d_ri, &raw mut dummy_out),
            FfiResult::NULL_POINTER
        );

        // `host_register_system` ordering error branches (`in_sets`, `after_weak`, `before_weak`, `ambiguous_with`, bad query[1])
        let bad_target = [c"".as_ptr()];
        let s_in = BamSystemDesc::new(c"cov::s1".as_ptr(), null(), 0, noop_system, null_mut())
            .with_in_sets(bad_target.as_ptr(), 1);
        assert_eq!(host_register_system(lp, &s_in), FfiResult::INVALID_ARGUMENT);
        let s_aw = BamSystemDesc::new(c"cov::s2".as_ptr(), null(), 0, noop_system, null_mut())
            .with_after_weak(bad_target.as_ptr(), 1);
        assert_eq!(host_register_system(lp, &s_aw), FfiResult::INVALID_ARGUMENT);
        let s_bw = BamSystemDesc::new(c"cov::s3".as_ptr(), null(), 0, noop_system, null_mut())
            .with_before_weak(bad_target.as_ptr(), 1);
        assert_eq!(host_register_system(lp, &s_bw), FfiResult::INVALID_ARGUMENT);
        let s_amb = BamSystemDesc::new(c"cov::s4".as_ptr(), null(), 0, noop_system, null_mut())
            .with_ambiguous_with(bad_target.as_ptr(), 1);
        assert_eq!(
            host_register_system(lp, &s_amb),
            FfiResult::INVALID_ARGUMENT
        );

        let mut bad_q = [BamQueryDesc::new(null(), 0), BamQueryDesc::new(null(), 0)];
        bad_q[1].struct_size = 4;
        let s_q2 = BamSystemDesc::new(
            c"cov::s5".as_ptr(),
            bad_q.as_ptr(),
            2,
            noop_system,
            null_mut(),
        );
        assert_eq!(host_register_system(lp, &s_q2), FfiResult::INVALID_VERSION);

        // `host_register_observer` exhaustive validation branches (`build_observer_binding_from_desc`)
        assert_eq!(
            host_register_observer(bad_load, &obs_tmp),
            FfiResult::INVALID_STATE
        );
        assert_eq!(host_register_observer(lp, null()), FfiResult::NULL_POINTER);
        let mut bad_obs_sz = obs_tmp;
        bad_obs_sz.struct_size = 4;
        assert_eq!(
            host_register_observer(lp, &bad_obs_sz),
            FfiResult::INVALID_VERSION
        );
        let mut bad_obs_res = obs_tmp;
        bad_obs_res.reserved0 = 1;
        assert_eq!(
            host_register_observer(lp, &bad_obs_res),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_name(bad_utf8_ptr)),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_name(c"".as_ptr())),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_entity(bad_ent)),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_entity(ph_ent)),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_propagate(BamComponent::from_raw(9999))),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_propagate(res_h)),
            FfiResult::INVALID_COMPONENT
        );
        let unk_w = [BamComponent::from_raw(9999)];
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_watched_components(unk_w.as_ptr(), 1)),
            FfiResult::INVALID_COMPONENT
        );
        let res_w = [res_h];
        assert_eq!(
            host_register_observer(lp, &obs_tmp.with_watched_components(res_w.as_ptr(), 1)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_register_observer(
                lp,
                &BamObserverDesc::new(BamComponent::from_raw(9999), noop_observer, null_mut())
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_register_observer(lp, &BamObserverDesc::new(res_h, noop_observer, null_mut())),
            FfiResult::INVALID_COMPONENT
        );
        // Component without `event_key` hits `EventKey::new` fallback!
        assert_eq!(
            host_register_observer(lp, &BamObserverDesc::new(pos_h, noop_observer, null_mut())),
            FfiResult::OK
        );
        // Lifecycle observer edge branches
        let mut lc_bad_ev =
            BamObserverDesc::new_lifecycle(BamEventKind::ADD, null(), 0, noop_observer, null_mut());
        lc_bad_ev.event = BamComponent::from_raw(9999);
        assert_eq!(
            host_register_observer(lp, &lc_bad_ev),
            FfiResult::INVALID_COMPONENT
        );
        lc_bad_ev.event = res_h;
        assert_eq!(
            host_register_observer(lp, &lc_bad_ev),
            FfiResult::INVALID_COMPONENT
        );
        // READY observer edge branches (before registering `"bevy_scene::Ready"`!)
        let ready_auto = BamObserverDesc::new_lifecycle(
            BamEventKind::READY,
            null(),
            0,
            noop_observer,
            null_mut(),
        );
        assert_eq!(
            host_register_observer(lp, &ready_auto),
            FfiResult::NOT_FOUND
        );
        let mut ready_explicit = ready_auto;
        ready_explicit.event = BamComponent::from_raw(9999);
        assert_eq!(
            host_register_observer(lp, &ready_explicit),
            FfiResult::INVALID_COMPONENT
        );
        ready_explicit.event = res_h;
        assert_eq!(
            host_register_observer(lp, &ready_explicit),
            FfiResult::INVALID_COMPONENT
        );
        ready_explicit.event = pos_h;
        assert_eq!(host_register_observer(lp, &ready_explicit), FfiResult::OK);

        registry = load_ctx.registry.clone();
        load_ctx.finish_init();

        // Register `"bevy_scene::Ready"` now!
        let ready_h = registry
            .register_bridged_entity_event::<CovReadyEv>(
                &mut world,
                "bevy_scene::Ready",
                Some("Ready"),
            )
            .unwrap();

        // --- Part B: `ffi/observers_ext.rs` (`host_cmd_observe` & `host_cmd_trigger_ex`) ---
        // Build a 3-node hierarchy: grandparent -> parent -> child via BOTH `ChildOf` and `rel_src_h`!
        let gp = world.spawn(CovPos::default()).id();
        let parent = world.spawn((CovPos::default(), ChildOf(gp))).id();
        let child = world.spawn((CovPos::default(), ChildOf(parent))).id();
        let cycle_a = world.spawn(CovPos::default()).id();
        let cycle_b = world.spawn(CovPos::default()).id();

        let gp_ffi = FfiEntity::from_bits(gp.to_bits());
        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let child_ffi = FfiEntity::from_bits(child.to_bits());
        let cycle_a_ffi = FfiEntity::from_bits(cycle_a.to_bits());
        let cycle_b_ffi = FfiEntity::from_bits(cycle_b.to_bits());

        // Attach dynamic `rel_src_h` pointers: child -> parent -> gp, and cycle_a <-> cycle_b!
        let snap = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            for (src, tgt) in [
                (child_ffi, parent_ffi),
                (parent_ffi, gp_ffi),
                (cycle_a_ffi, cycle_b_ffi),
                (cycle_b_ffi, cycle_a_ffi),
            ] {
                let bits = tgt.to_bits();
                assert_eq!(
                    host_cmd_insert(
                        cmds.as_ffi_ptr(),
                        src,
                        rel_src_h,
                        (&raw const bits).cast::<u8>(),
                        8
                    ),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(&mut world, &snap);

        // Now test binding dynamic relationship AFTER archetypes exist -> `INVALID_STATE`
        {
            let mut lc2 = LoadContext::with_manifest_name(
                registry.clone(),
                Arc::new(AtomicBool::new(true)),
                (&raw mut world).cast::<World>(),
                None,
                None,
            );
            assert_eq!(
                bind_try(
                    lc2.as_ffi_ptr(),
                    &format!("__bam_rel_bind::{}::{}", rel_src2_h.0, rel_tgt2_h.0),
                    0,
                    1
                ),
                FfiResult::OK
            );
            lc2.finish_init();
        }

        // Spawn observers via `host_cmd_observe`!
        static OBS_HITS: AtomicUsize = AtomicUsize::new(0);
        static STOP_PROP_ENTITY: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn counting_obs(
            _entity: FfiEntity,
            _data: *const c_void,
            _cmds: *mut BamCommands,
            _ud: *mut c_void,
        ) -> FfiResult {
            OBS_HITS.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        unsafe extern "C" fn stopping_mut_obs(
            entity: FfiEntity,
            _data: *mut u8,
            propagate: *mut FfiBool,
            _cmds: *mut BamCommands,
            _ud: *mut c_void,
        ) -> FfiResult {
            OBS_HITS.fetch_add(1, Ordering::SeqCst);
            if entity.to_bits() as usize == STOP_PROP_ENTITY.load(Ordering::SeqCst) {
                // SAFETY: `propagate` is valid writable `FfiBool` supplied by `guest_observer_runner`.
                unsafe {
                    *propagate = FfiBool::FALSE;
                }
            }
            FfiResult::OK
        }

        let unaligned_ent = core::ptr::without_provenance_mut::<FfiEntity>(1);
        let mut obs_e = FfiEntity::NULL;
        let active_flag = Arc::new(AtomicBool::new(true));
        let inactive_flag = Arc::new(AtomicBool::new(false));
        let watched_pos = [pos_h];

        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(None, Some(Arc::clone(&active_flag)))
                .with_registry(&registry);
            let cp = cmds.as_ffi_ptr();

            // `host_cmd_observe` error branches
            assert_eq!(
                host_cmd_observe(cp, &obs_tmp, unaligned_ent),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_observe(null_mut(), &obs_tmp, &raw mut obs_e),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_observe(bad_cmds, &obs_tmp, &raw mut obs_e),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_cmd_observe(cp, null(), &raw mut obs_e),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_observe(cp, &bad_obs_res, &raw mut obs_e),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_observe(
                    cp,
                    &BamObserverDesc::new(res_h, noop_observer, null_mut()),
                    &raw mut obs_e
                ),
                FfiResult::INVALID_COMPONENT
            );

            // Spawn live observers for:
            // 1) ZST custom event (global + targeted + component-watched)
            // 2) Global host event + EntityEvent + Ready
            // 3) All 5 lifecycle kinds (`ADD`, `INSERT`, `DISCARD`, `REMOVE`, `DESPAWN`) with `mut_callback`
            let zst_obs = BamObserverDesc::new(dyn_zst_ev_h, counting_obs, null_mut())
                .with_mut_callback(stopping_mut_obs);
            assert_eq!(
                host_cmd_observe(cp, &zst_obs, &raw mut obs_e),
                FfiResult::OK
            );
            let zst_w_obs = BamObserverDesc::new(dyn_zst_ev_h, counting_obs, null_mut())
                .with_watched_components(watched_pos.as_ptr(), 1)
                .with_entity(parent_ffi);
            assert_eq!(
                host_cmd_observe(cp, &zst_w_obs, &raw mut obs_e),
                FfiResult::OK
            );
            let g_obs = BamObserverDesc::new(global_ev_h, counting_obs, null_mut());
            assert_eq!(host_cmd_observe(cp, &g_obs, null_mut()), FfiResult::OK);
            let r_obs = BamObserverDesc::new_lifecycle(
                BamEventKind::READY,
                null(),
                0,
                counting_obs,
                null_mut(),
            )
            .with_mut_callback(stopping_mut_obs);
            assert_eq!(host_cmd_observe(cp, &r_obs, null_mut()), FfiResult::OK);
            for kind in [
                BamEventKind::ADD,
                BamEventKind::INSERT,
                BamEventKind::DISCARD,
                BamEventKind::REMOVE,
                BamEventKind::DESPAWN,
            ] {
                let lc_obs = BamObserverDesc::new_lifecycle(
                    kind,
                    watched_pos.as_ptr(),
                    1,
                    counting_obs,
                    null_mut(),
                )
                .with_mut_callback(stopping_mut_obs);
                assert_eq!(host_cmd_observe(cp, &lc_obs, null_mut()), FfiResult::OK);
            }
        }
        // Also queue an observer whose `is_active` is already `false` (covers `if !binding.is_active` skip in apply!)
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(None, Some(inactive_flag))
                .with_registry(&registry);
            assert_eq!(
                host_cmd_observe(cmds.as_ffi_ptr(), &obs_tmp, &raw mut obs_e),
                FfiResult::OK
            );
        }
        storage.apply_to_world(&mut world, &snap);

        // Now exhaustively exercise `host_cmd_trigger_ex`!
        let dead_t = world.spawn_empty().id();
        let dead_t_ffi = FfiEntity::from_bits(dead_t.to_bits());
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let cp = cmds.as_ffi_ptr();
        let mut ev_val = CovGlobalEv { val: 42 };
        let valid_trig = BamTriggerDesc::new(global_ev_h, (&raw mut ev_val).cast::<u8>(), 4);

        // Guard branches
        assert_eq!(
            host_cmd_trigger_ex(null_mut(), &valid_trig),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_trigger_ex(bad_cmds, &valid_trig),
            FfiResult::INVALID_STATE
        );
        assert_eq!(host_cmd_trigger_ex(cp, null()), FfiResult::NULL_POINTER);
        let mut bad_t = valid_trig;
        bad_t.reserved0 = 1;
        assert_eq!(host_cmd_trigger_ex(cp, &bad_t), FfiResult::INVALID_ARGUMENT);
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_entity(bad_ent)),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_entity(ph_ent)),
            FfiResult::INVALID_ENTITY
        );
        // `propagate_relationship` validation
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &valid_trig
                    .with_entity(child_ffi)
                    .with_propagate(BamComponent::from_raw(9999))
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_entity(child_ffi).with_propagate(res_h)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &valid_trig
                    .with_entity(child_ffi)
                    .with_propagate(rel_bad_size_h)
            ),
            FfiResult::INVALID_COMPONENT
        );
        // `watched_components` validation
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_watched_components(unk_w.as_ptr(), 1)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_watched_components(res_w.as_ptr(), 1)),
            FfiResult::INVALID_COMPONENT
        );
        // Custom event validation
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(BamComponent::from_raw(9999), null_mut(), 0)
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &BamTriggerDesc::new(res_h, null_mut(), 0)),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &BamTriggerDesc::new(global_ev_h, null_mut(), 0)),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_cmd_trigger_ex(cp, &valid_trig.with_propagate(BamComponent::INVALID)),
            FfiResult::INVALID_ENTITY
        );
        // Custom `EntityEvent` & `synthesizes_ffi_entity_payload` target extraction guards
        let mut ph_bits = ph_ent.to_bits();
        let mut bad_bits = bad_ent.to_bits();
        let mut child_bits = child_ffi.to_bits();
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(ready_h, (&raw mut bad_bits).cast::<u8>(), 8)
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(ready_h, (&raw mut ph_bits).cast::<u8>(), 8)
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(ready_h, (&raw mut child_bits).cast::<u8>(), 8)
            ),
            FfiResult::OK
        );
        let mut ent_ev_ph = CovEntEv {
            target: TargetEntity(ph_ent),
            amount: 1,
        };
        let mut ent_ev_bad = CovEntEv {
            target: TargetEntity(bad_ent),
            amount: 1,
        };
        let mut ent_ev_ok = CovEntEv {
            target: TargetEntity(child_ffi),
            amount: 2,
        };
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(
                    ent_ev_h,
                    (&raw mut ent_ev_ph).cast::<u8>(),
                    core::mem::size_of::<CovEntEv>()
                )
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(
                    ent_ev_h,
                    (&raw mut ent_ev_bad).cast::<u8>(),
                    core::mem::size_of::<CovEntEv>()
                )
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(
                    ent_ev_h,
                    (&raw mut ent_ev_ok).cast::<u8>(),
                    core::mem::size_of::<CovEntEv>()
                )
            ),
            FfiResult::OK
        );

        // Custom ZST global, custom ZST targeted + `watched_components`, custom host `trigger_fn` global,
        // and custom propagating (both `ChildOf` and dynamic `rel_src_h`, plus cycle!)
        assert_eq!(
            host_cmd_trigger_ex(cp, &BamTriggerDesc::new(dyn_zst_ev_h, null_mut(), 0)),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(dyn_zst_ev_h, null_mut(), 0)
                    .with_entity(parent_ffi)
                    .with_watched_components(watched_pos.as_ptr(), 1),
            ),
            FfiResult::OK
        );
        assert_eq!(host_cmd_trigger_ex(cp, &valid_trig), FfiResult::OK);
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(dyn_zst_ev_h, null_mut(), 0)
                    .with_entity(child_ffi)
                    .with_propagate(child_of_h),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(dyn_zst_ev_h, null_mut(), 0)
                    .with_entity(cycle_a_ffi)
                    .with_propagate(rel_src_h),
            ),
            FfiResult::OK
        );

        // Component lifecycle triggers (`ADD`, `INSERT`, `DISCARD`, `REMOVE`, `DESPAWN`): guards + non-prop + prop + stop + cycle!
        let lc_no_ent =
            BamTriggerDesc::new(pos_h, null_mut(), 0).with_event_kind(BamEventKind::ADD);
        assert_eq!(
            host_cmd_trigger_ex(cp, &lc_no_ent),
            FfiResult::INVALID_ENTITY
        );
        let lc_bad_len = BamTriggerDesc::new(pos_h, (&raw mut ev_val).cast::<u8>(), 4)
            .with_event_kind(BamEventKind::ADD)
            .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &lc_bad_len),
            FfiResult::INVALID_ARGUMENT
        );
        let lc_unk = BamTriggerDesc::new(BamComponent::from_raw(9999), null_mut(), 0)
            .with_event_kind(BamEventKind::ADD)
            .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &lc_unk),
            FfiResult::INVALID_COMPONENT
        );
        let lc_res = BamTriggerDesc::new(res_h, null_mut(), 0)
            .with_event_kind(BamEventKind::ADD)
            .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &lc_res),
            FfiResult::INVALID_COMPONENT
        );

        for kind in [
            BamEventKind::ADD,
            BamEventKind::INSERT,
            BamEventKind::DISCARD,
            BamEventKind::REMOVE,
            BamEventKind::DESPAWN,
        ] {
            let d = BamTriggerDesc::new(pos_h, null_mut(), 0)
                .with_event_kind(kind)
                .with_entity(child_ffi);
            assert_eq!(host_cmd_trigger_ex(cp, &d), FfiResult::OK);
        }
        // Propagating lifecycle along `ChildOf` and along dynamic `rel_src_h` cycle
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(pos_h, null_mut(), 0)
                    .with_event_kind(BamEventKind::INSERT)
                    .with_entity(child_ffi)
                    .with_propagate(BamComponent::INVALID),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(pos_h, null_mut(), 0)
                    .with_event_kind(BamEventKind::REMOVE)
                    .with_entity(cycle_a_ffi)
                    .with_propagate(rel_src_h),
            ),
            FfiResult::OK
        );

        // `READY` triggers: error paths + auto lookup + explicit handle + payload extraction + propagating!
        let r_unk = BamTriggerDesc::new(BamComponent::from_raw(9999), null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_unk),
            FfiResult::INVALID_COMPONENT
        );
        let r_res = BamTriggerDesc::new(res_h, null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_res),
            FfiResult::INVALID_COMPONENT
        );
        let r_bad_len =
            BamTriggerDesc::new(BamComponent::INVALID, (&raw mut ev_val).cast::<u8>(), 4)
                .with_event_kind(BamEventKind::READY)
                .with_entity(child_ffi);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_bad_len),
            FfiResult::INVALID_ARGUMENT
        );
        let r_no_target = BamTriggerDesc::new(BamComponent::INVALID, null_mut(), 0)
            .with_event_kind(BamEventKind::READY);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_no_target),
            FfiResult::INVALID_ENTITY
        );
        let r_bad_bits =
            BamTriggerDesc::new(BamComponent::INVALID, (&raw mut bad_bits).cast::<u8>(), 8)
                .with_event_kind(BamEventKind::READY);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_bad_bits),
            FfiResult::INVALID_ENTITY
        );
        let r_ph_bits =
            BamTriggerDesc::new(BamComponent::INVALID, (&raw mut ph_bits).cast::<u8>(), 8)
                .with_event_kind(BamEventKind::READY);
        assert_eq!(
            host_cmd_trigger_ex(cp, &r_ph_bits),
            FfiResult::INVALID_ENTITY
        );
        // Valid `READY` extracting target from 8-byte payload + explicit propagating + cycle!
        let r_from_payload =
            BamTriggerDesc::new(BamComponent::INVALID, (&raw mut child_bits).cast::<u8>(), 8)
                .with_event_kind(BamEventKind::READY);
        assert_eq!(host_cmd_trigger_ex(cp, &r_from_payload), FfiResult::OK);
        let r_prop = BamTriggerDesc::new(ready_h, null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(child_ffi)
            .with_propagate(BamComponent::INVALID);
        assert_eq!(host_cmd_trigger_ex(cp, &r_prop), FfiResult::OK);
        let r_cycle = BamTriggerDesc::new(ready_h, null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(cycle_a_ffi)
            .with_propagate(rel_src_h);
        assert_eq!(host_cmd_trigger_ex(cp, &r_cycle), FfiResult::OK);

        // Also queue dead-target custom/lifecycle/READY triggers to hit dead-start checks!
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(dyn_zst_ev_h, null_mut(), 0).with_entity(dead_t_ffi)
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(pos_h, null_mut(), 0)
                    .with_event_kind(BamEventKind::ADD)
                    .with_entity(dead_t_ffi),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_trigger_ex(
                cp,
                &BamTriggerDesc::new(ready_h, null_mut(), 0)
                    .with_event_kind(BamEventKind::READY)
                    .with_entity(dead_t_ffi),
            ),
            FfiResult::OK
        );
        drop(cmds);

        // Despawn `dead_t` and stop propagation on `parent`!
        world.despawn(dead_t);
        STOP_PROP_ENTITY.store(parent_ffi.to_bits() as usize, Ordering::SeqCst);
        OBS_HITS.store(0, Ordering::SeqCst);
        storage.apply_to_world(&mut world, &snap);
        assert!(OBS_HITS.load(Ordering::SeqCst) > 10);

        // Also test `READY` trigger when `"bevy_scene::Ready"` is NOT registered (`NOT_FOUND`)
        let empty_reg = ModRegistry::new();
        let empty_snap = empty_reg.snapshot_arc();
        let mut cmds_empty = HostCommands::new(&mut storage, world.entity_allocator(), &empty_snap);
        assert_eq!(
            host_cmd_trigger_ex(
                cmds_empty.as_ffi_ptr(),
                &BamTriggerDesc::new(BamComponent::INVALID, null_mut(), 0)
                    .with_event_kind(BamEventKind::READY)
                    .with_entity(child_ffi),
            ),
            FfiResult::NOT_FOUND
        );
        drop(cmds_empty);
        let _ = (zero_h, dyn_pod_ev_h, noop_observer_mut as *const ());
    }
}

// ============================================================================
// 7. `ffi/hierarchy_ext.rs` + `system/*` (`access_builder`, `error_policy`, `guest_system`)
// ============================================================================

#[test]
fn hierarchy_ext_and_system_exhaustive_coverage() {
    let mut world = World::new();
    world.insert_resource(ModHost::new());
    world.init_resource::<Messages<bevy_abi_mod_host_sys::ModSystemFaulted>>();
    let pos_id = world.register_component::<CovPos>();
    let vel_id = world.register_component::<CovVel>();
    let sparse_id = world.register_component::<CovSparseTag>();
    let imm_id = world.register_component::<CovImmComp>();
    let res_id = world.register_component::<CovResVal>();

    let mut registry = ModRegistry::new();
    let pos_h = registry
        .register_component::<CovPos>(pos_id, ComponentOptions::default())
        .unwrap();
    let vel_h = registry
        .register_component::<CovVel>(vel_id, ComponentOptions::default())
        .unwrap();
    let sparse_h = registry
        .register_component::<CovSparseTag>(sparse_id, ComponentOptions::default())
        .unwrap();
    let imm_h = registry
        .register_component::<CovImmComp>(imm_id, ComponentOptions::default())
        .unwrap();
    let res_h = registry.register_resource::<CovResVal>(res_id).unwrap();

    // Register a plain dynamic component (NOT bound as a relationship target) for hierarchy_ext tests.
    let mut plain_dyn_h = BamComponent::INVALID;
    {
        let mut lc = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let d = ComponentDesc::new(
            c"cov::PlainDyn".as_ptr(),
            ComponentUuid::from_parts("cov::PlainDyn", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid active `lc`, `d`, and writable `plain_dyn_h`.
        unsafe {
            assert_eq!(
                host_register_component(lc.as_ffi_ptr(), &d, &raw mut plain_dyn_h),
                FfiResult::OK
            );
        }
        registry = lc.registry.clone();
        lc.finish_init();
    }
    world.insert_resource(registry.clone());

    // 1. `ModQuarantine` (`system/error_policy.rs`)
    let mut q = ModQuarantine::new();
    let f1 = Arc::new(AtomicBool::new(false));
    let f2 = Arc::new(AtomicBool::new(true));
    q.register_system(ModId(1), Arc::from("sys_a"), Arc::clone(&f1));
    q.register_system(ModId(1), Arc::from("sys_b"), Arc::clone(&f2));
    assert!(q.is_quarantined(ModId(1), "sys_a"));
    assert!(!q.is_quarantined(ModId(1), "sys_b"));
    let quarantined: Vec<_> = q.quarantined_systems().collect();
    assert_eq!(quarantined, vec![(ModId(1), "sys_a")]);
    assert!(!q.reenable(ModId(99), "missing"));
    assert!(!q.reenable(ModId(1), "sys_b"));
    assert!(q.reenable(ModId(1), "sys_a"));
    assert!(!q.is_quarantined(ModId(1), "sys_a"));
    q.unregister_mod(ModId(1));
    assert_eq!(q.quarantined_systems().count(), 0);

    // 2. `ModDiagnosticsCollector` (when `diagnostics` feature is enabled)
    #[cfg(feature = "diagnostics")]
    {
        let collector = ModDiagnosticsCollector::new();
        collector.push(ModDiagnosticsSample {
            mod_id: Some(ModId(1)),
            mod_name: Arc::from("cov"),
            system_name: Arc::from("sys_a"),
            cpu_time_us: 12.5,
            commands_queued: 1,
            faulted: false,
        });
        assert_eq!(collector.drain().len(), 1);
        assert!(collector.drain().is_empty());
        world.insert_resource(collector);
    }

    // 3. `system/access_builder.rs` validation via `host_register_system` & `GuestSystem::initialize`
    {
        let mut lc = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        let lp = lc.as_ffi_ptr();
        // > 32 terms -> `CAPACITY_EXCEEDED`
        let terms_33 = [BamTermDesc::new(pos_h, AccessMode::READ); 33];
        let q33 = BamQueryDesc::new(terms_33.as_ptr(), 33);
        let s33 = BamSystemDesc::new(c"cov::t33".as_ptr(), &q33, 1, noop_system, null_mut());
        // Intra-query write-write conflict -> `ACCESS_CONFLICT`
        let terms_ww = [
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(pos_h, AccessMode::WRITE),
        ];
        let q_ww = BamQueryDesc::new(terms_ww.as_ptr(), 2);
        let s_ww = BamSystemDesc::new(c"cov::ww".as_ptr(), &q_ww, 1, noop_system, null_mut());
        // `or_group` with `WITHOUT` and `CHANGED` on written component -> `OK`
        let terms_or = [
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(vel_h, AccessMode::WITHOUT).with_or_group(1),
            BamTermDesc::new(pos_h, AccessMode::CHANGED).with_or_group(1),
            BamTermDesc::new(vel_h, AccessMode::FILTER_ONLY).with_or_group(2),
        ];
        let q_or = BamQueryDesc::new(terms_or.as_ptr(), terms_or.len());
        let s_or = BamSystemDesc::new(c"cov::or_ok".as_ptr(), &q_or, 1, noop_system, null_mut());
        // Resource read + resource write conflict across queries -> `ACCESS_CONFLICT`
        let q_r1_terms = [BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE)];
        let q_r2_terms = [BamTermDesc::new(res_h, AccessMode::RESOURCE_READ)];
        let q_res_conflict = [
            BamQueryDesc::new(q_r1_terms.as_ptr(), 1),
            BamQueryDesc::new(q_r2_terms.as_ptr(), 1),
        ];
        let s_rc = BamSystemDesc::new(
            c"cov::res_conflict".as_ptr(),
            q_res_conflict.as_ptr(),
            2,
            noop_system,
            null_mut(),
        );

        // SAFETY: Valid active `lp` and stack descriptors.
        unsafe {
            assert_eq!(host_register_system(lp, &s33), FfiResult::CAPACITY_EXCEEDED);
            assert_eq!(host_register_system(lp, &s_ww), FfiResult::ACCESS_CONFLICT);
            assert_eq!(host_register_system(lp, &s_or), FfiResult::OK);
            assert_eq!(host_register_system(lp, &s_rc), FfiResult::ACCESS_CONFLICT);
        }
        lc.finish_init();
    }

    // `GuestSystem::initialize` error branches: `ImmutableWrite` (`PERMISSION_DENIED`), `InvalidMode` in `or_group`,
    // `mod_id = Some(...)` recording fault, `validate_against_registry` `Err`, plus builder accessors!
    let mut imm_sys = GuestSystem::new(
        "cov::imm_write",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            imm_h,
            AccessMode::WRITE,
        )])],
        noop_system,
        null_mut(),
        registry.clone(),
    )
    .with_mod_id(ModId(55));
    assert_eq!(imm_sys.mod_id(), Some(ModId(55)));
    imm_sys.set_main_thread(true);
    assert!(imm_sys.is_main_thread());
    imm_sys.set_error_policy(ModErrorPolicy::Log);
    assert_eq!(imm_sys.error_policy(), ModErrorPolicy::Log);
    let act = Arc::new(AtomicBool::new(true));
    imm_sys.set_is_active(Arc::clone(&act));
    assert!(Arc::ptr_eq(&imm_sys.is_active(), &act));
    imm_sys.set_run_if(Some((noop_run_if, null_mut())));
    assert!(imm_sys.run_if().is_some());
    let _ = imm_sys.initialize(&mut world);
    assert!(matches!(
        imm_sys.init_error(),
        Some(SystemInitError::ImmutableWrite(_))
    ));

    let mut bad_or_sys = GuestSystem::new(
        "cov::bad_or",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::READ).with_or_group(1),
        ])],
        noop_system,
        null_mut(),
        registry.clone(),
    );
    let _ = bad_or_sys.initialize(&mut world);
    assert!(matches!(
        bad_or_sys.init_error(),
        Some(SystemInitError::InvalidMode { index: 0 })
    ));

    let bad_reg_sys = GuestSystem::new(
        "cov::missing_comp",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            BamComponent::from_raw(9999),
            AccessMode::READ,
        )])],
        noop_system,
        null_mut(),
        registry.clone(),
    );
    assert!(bad_reg_sys.validate_against_registry(&registry).is_err());

    // `GuestSystem::initialize` with `or_group` `CHANGED` on written component (covers `access_builder.rs:587`)
    let mut or_write_sys = GuestSystem::new(
        "cov::or_write_init",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(pos_h, AccessMode::CHANGED).with_or_group(1),
            BamTermDesc::new(vel_h, AccessMode::WITHOUT).with_or_group(1),
        ])],
        noop_system,
        null_mut(),
        registry.clone(),
    );
    let _ = or_write_sys.initialize(&mut world);
    assert!(or_write_sys.init_error().is_none());

    // 4. Standalone `HostSystemCtx::new` (no `world`) + `ffi/hierarchy_ext.rs` + `host_sys_*` guards!
    let mut poisoned = poisoned_header();
    let bad_sys_ctx_c = (&raw const poisoned).cast::<BamSystemCtx>();
    let bad_sys_ctx_m = (&raw mut poisoned).cast::<BamSystemCtx>();
    let live_e = world
        .spawn((CovPos { x: 1.0, y: 2.0 }, CovSparseTag { v: 5 }))
        .id();
    let live_ffi = FfiEntity::from_bits(live_e.to_bits());
    let bad_ent = invalid_entity_bits();
    let ph_ent = placeholder_entity();
    let unaligned_ptr = core::ptr::without_provenance_mut::<*const FfiEntity>(1);
    let unaligned_len = core::ptr::without_provenance_mut::<usize>(1);
    let unaligned_ticks = core::ptr::without_provenance_mut::<BamChangeTicks>(1);

    // SAFETY: Testing standalone `HostSystemCtx` and all `host_sys_*` / `host_sys_entity_children` guards.
    unsafe {
        let mut stand_ctx = HostSystemCtx::new(
            registry.snapshot_arc(),
            None,
            String::new(),
            [null_mut(); MAX_SYSTEM_QUERIES],
            1,
            null_mut(),
        );
        let sc = stand_ctx.as_ffi_ptr();

        assert!(host_sys_host_ctx(null()).is_null());
        assert!(host_sys_host_ctx(bad_sys_ctx_c).is_null());

        let mut out_it: *mut BamIter = null_mut();
        assert_eq!(host_sys_query(sc, 0, null_mut()), FfiResult::NULL_POINTER);
        assert_eq!(
            host_sys_query(null_mut(), 0, &raw mut out_it),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_query(bad_sys_ctx_m, 0, &raw mut out_it),
            FfiResult::INVALID_STATE
        );

        let mut out_cm: *mut BamCommands = null_mut();
        assert_eq!(host_sys_commands(sc, null_mut()), FfiResult::NULL_POINTER);
        assert_eq!(
            host_sys_commands(null_mut(), &raw mut out_cm),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_commands(bad_sys_ctx_m, &raw mut out_cm),
            FfiResult::INVALID_STATE
        );

        assert!(host_sys_entity_get(null(), 0, live_ffi, 0).is_null());
        assert!(host_sys_entity_get(bad_sys_ctx_c, 0, live_ffi, 0).is_null());
        assert!(host_sys_entity_get(sc, 99, live_ffi, 0).is_null());
        assert!(host_sys_entity_get(sc, 0, FfiEntity::NULL, 0).is_null());
        assert!(host_sys_entity_get(sc, 0, bad_ent, 0).is_null());
        assert!(host_sys_entity_get(sc, 0, ph_ent, 0).is_null());
        assert!(host_sys_entity_get(sc, 0, live_ffi, 0).is_null());

        assert!(host_sys_entity_get_mut(null_mut(), 0, live_ffi, 0).is_null());
        assert!(host_sys_entity_get_mut(bad_sys_ctx_m, 0, live_ffi, 0).is_null());
        assert!(host_sys_entity_get_mut(sc, 99, live_ffi, 0).is_null());
        assert!(host_sys_entity_get_mut(sc, 0, FfiEntity::NULL, 0).is_null());
        assert!(host_sys_entity_get_mut(sc, 0, bad_ent, 0).is_null());
        assert!(host_sys_entity_get_mut(sc, 0, ph_ent, 0).is_null());
        assert!(host_sys_entity_get_mut(sc, 0, live_ffi, 0).is_null());

        assert!(host_sys_res_ptr(null(), res_h, null_mut()).is_null());
        assert!(host_sys_res_ptr(bad_sys_ctx_c, res_h, null_mut()).is_null());
        assert!(host_sys_res_ptr(sc, res_h, unaligned_ticks).is_null());
        assert!(host_sys_res_ptr(sc, res_h, null_mut()).is_null());

        assert!(host_sys_res_mut_ptr(null_mut(), res_h, null_mut()).is_null());
        assert!(host_sys_res_mut_ptr(bad_sys_ctx_m, res_h, null_mut()).is_null());
        assert!(host_sys_res_mut_ptr(sc, res_h, unaligned_ticks).is_null());
        assert!(host_sys_res_mut_ptr(sc, res_h, null_mut()).is_null());

        // `host_sys_entity_children` guards
        let mut c_ptr: *const FfiEntity = null();
        let mut c_len = 0usize;
        assert_eq!(
            host_sys_entity_children(
                sc,
                live_ffi,
                BamComponent::INVALID,
                null_mut(),
                &raw mut c_len
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                live_ffi,
                BamComponent::INVALID,
                &raw mut c_ptr,
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                live_ffi,
                BamComponent::INVALID,
                unaligned_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                live_ffi,
                BamComponent::INVALID,
                &raw mut c_ptr,
                unaligned_len
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_sys_entity_children(
                null(),
                live_ffi,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_entity_children(
                bad_sys_ctx_c,
                live_ffi,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                FfiEntity::NULL,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                bad_ent,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_sys_entity_children(
                sc,
                ph_ent,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_ENTITY
        );
        // `HostSystemCtx` without `world` -> `INVALID_STATE`
        assert_eq!(
            host_sys_entity_children(
                sc,
                live_ffi,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len
            ),
            FfiResult::INVALID_STATE
        );
    }

    // 5. Run live `GuestSystem` exercising:
    //    - `host_sys_entity_get` / `get_mut` on sparse archetype + dense & sparse tick filters (`Added`/`Changed`),
    //      resource-only query (`!has_query_state`), out-of-range term, absent optional component!
    //    - `host_sys_res_ptr` / `host_sys_res_mut_ptr` with absent resource vs present resource + `out_ticks`!
    //    - `host_sys_entity_children` with missing `Children` (`NOT_FOUND`), unknown handle (`INVALID_COMPONENT`),
    //      dead entity (`INVALID_ENTITY`), plain dynamic component without/with hook map (`INVALID_COMPONENT`),
    //      and non-relationship static component (`INVALID_COMPONENT`)!
    //    - `GuestSystem::queue_deferred` with both queued commands AND `fault_queue`!
    struct SysProbe {
        live_sparse_e: FfiEntity,
        live_dense_e: FfiEntity,
        dead_e: FfiEntity,
        pos_h: BamComponent,
        plain_dyn_h: BamComponent,
        res_h: BamComponent,
        check_res_present: AtomicBool,
        return_fault: AtomicBool,
    }
    let live_dense_e = world.spawn(CovPos { x: 9.0, y: 9.0 }).id();
    let dead_e = world.spawn_empty().id();
    world.despawn(dead_e);

    unsafe extern "C" fn sys_probe_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to `SysProbe`, `sys_ctx` is active during `GuestSystem::run`.
        let p = unsafe { &*ud.cast::<SysProbe>() };
        // SAFETY: Valid `sys_ctx` and local stack out pointers.
        unsafe {
            // Query 0: `(Write<CovPos>, OptionalWrite<CovVel>, Added<CovPos>, Changed<CovSparseTag>, Read<CovSparseTag>)` (sparse!)
            assert!(host_sys_entity_get(sys_ctx, 0, p.live_sparse_e, 99).is_null());
            assert!(host_sys_entity_get_mut(sys_ctx, 0, p.live_sparse_e, 99).is_null());
            assert!(host_sys_entity_get(sys_ctx, 0, p.live_sparse_e, 1).is_null()); // absent optional
            assert!(host_sys_entity_get_mut(sys_ctx, 0, p.live_sparse_e, 1).is_null()); // absent optional
            let _ = host_sys_entity_get(sys_ctx, 0, p.live_sparse_e, 0);
            let _ = host_sys_entity_get_mut(sys_ctx, 0, p.live_sparse_e, 0);

            // Query 1: `(Read<CovPos>, Changed<CovPos>, Added<CovPos>, Without<CovSparseTag>)` (dense with tick filters!)
            let _ = host_sys_entity_get(sys_ctx, 1, p.live_dense_e, 0);

            // Query 2: resource-only `(ResourceWrite<CovResVal>)` (`!has_query_state`!)
            assert!(host_sys_entity_get(sys_ctx, 2, p.live_dense_e, 0).is_null());
            assert!(host_sys_entity_get_mut(sys_ctx, 2, p.live_dense_e, 0).is_null());

            let mut ticks = BamChangeTicks::default();
            let rp = host_sys_res_ptr(sys_ctx, p.res_h, &raw mut ticks);
            let rmp = host_sys_res_mut_ptr(sys_ctx, p.res_h, &raw mut ticks);
            if p.check_res_present.load(Ordering::Acquire) {
                assert!(!rp.is_null());
                assert!(!rmp.is_null());
            } else {
                assert!(rp.is_null());
                assert!(rmp.is_null());
            }

            // Query 3: `(Read<PlainDyn>)` for `host_sys_entity_children` error paths
            let mut c_ptr: *const FfiEntity = null();
            let mut c_len = 0usize;
            // `Children` not yet registered -> `NOT_FOUND`
            let _ = host_sys_entity_children(
                sys_ctx,
                p.live_dense_e,
                BamComponent::INVALID,
                &raw mut c_ptr,
                &raw mut c_len,
            );
            assert_eq!(
                host_sys_entity_children(
                    sys_ctx,
                    p.live_dense_e,
                    BamComponent::from_raw(9999),
                    &raw mut c_ptr,
                    &raw mut c_len
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_sys_entity_children(
                    sys_ctx,
                    p.dead_e,
                    p.pos_h,
                    &raw mut c_ptr,
                    &raw mut c_len
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_sys_entity_children(
                    sys_ctx,
                    p.live_dense_e,
                    p.pos_h,
                    &raw mut c_ptr,
                    &raw mut c_len
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_sys_entity_children(
                    sys_ctx,
                    p.live_dense_e,
                    p.plain_dyn_h,
                    &raw mut c_ptr,
                    &raw mut c_len
                ),
                FfiResult::INVALID_COMPONENT
            );

            // Queue a command so `queue_deferred` has non-empty commands!
            let mut cmds: *mut BamCommands = null_mut();
            assert_eq!(host_sys_commands(sys_ctx, &raw mut cmds), FfiResult::OK);
            let mut spawned = FfiEntity::NULL;
            assert_eq!(host_cmd_spawn_empty(cmds, &raw mut spawned), FfiResult::OK);
        }
        if p.return_fault.load(Ordering::Acquire) {
            FfiResult::INTERNAL_ERROR
        } else {
            FfiResult::OK
        }
    }

    let probe = SysProbe {
        live_sparse_e: live_ffi,
        live_dense_e: FfiEntity::from_bits(live_dense_e.to_bits()),
        dead_e: FfiEntity::from_bits(dead_e.to_bits()),
        pos_h,
        plain_dyn_h,
        res_h,
        check_res_present: AtomicBool::new(false),
        return_fault: AtomicBool::new(false),
    };
    let mut sys = GuestSystem::new(
        "cov::probe_sys",
        vec![
            BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(vel_h, AccessMode::OPTIONAL_WRITE),
                BamTermDesc::new(pos_h, AccessMode::ADDED),
                BamTermDesc::new(sparse_h, AccessMode::CHANGED),
                BamTermDesc::new(sparse_h, AccessMode::READ),
            ]),
            BamQuerySpecOwned::new(vec![
                BamTermDesc::new(pos_h, AccessMode::READ),
                BamTermDesc::new(pos_h, AccessMode::CHANGED),
                BamTermDesc::new(pos_h, AccessMode::ADDED),
                BamTermDesc::new(sparse_h, AccessMode::WITHOUT),
            ]),
            BamQuerySpecOwned::new(vec![BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE)]),
            BamQuerySpecOwned::new(vec![BamTermDesc::new(plain_dyn_h, AccessMode::READ)]),
        ],
        sys_probe_cb,
        (&raw const probe).cast_mut().cast::<c_void>(),
        registry.clone(),
    );
    let _ = sys.initialize(&mut world);
    assert!(sys.init_error().is_none());

    // Run #1: resource absent, no `DynamicComponentHookMap` yet, tick filters match!
    sys.run((), &mut world).unwrap();

    // Run #2: insert `CovResVal`, install `DynamicComponentHookMap` and `Children`, re-run
    // (tick filters now DO NOT match `live_dense_e` / `live_sparse_e` -> exercises `is_newer_than == false`!)
    // AND use `run_without_applying_deferred` + `queue_deferred`!
    world.insert_resource(CovResVal {
        tick: 1,
        scale: 2.0,
    });
    ensure_dynamic_hook_map_in_world(&mut world);
    ensure_children_registered_in_world(&mut world);
    let _ = world.components().component_id::<Children>().unwrap();
    probe.check_res_present.store(true, Ordering::Release);
    sys.run_without_applying_deferred((), &mut world).unwrap();
    {
        let dw: DeferredWorld<'_> = (&mut world).into();
        sys.queue_deferred(dw);
    }
    world.flush();

    // Run #3: trigger a fault and flush via `queue_deferred` (covers `fault_queue` path in `queue_deferred`!)
    probe.return_fault.store(true, Ordering::Release);
    sys.set_error_policy(ModErrorPolicy::Quarantine);
    sys.run_without_applying_deferred((), &mut world).unwrap();
    {
        let dw: DeferredWorld<'_> = (&mut world).into();
        sys.queue_deferred(dw);
    }
    world.flush();
    assert!(!sys.is_enabled());
}
