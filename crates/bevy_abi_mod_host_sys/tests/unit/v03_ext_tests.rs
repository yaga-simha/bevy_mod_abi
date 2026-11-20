use super::*;
use crate::ffi::commands::{
    HostCommandsStorage, host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty,
};
use crate::ffi::iter::{QueryPlan, QueryTermMeta, host_iter_next_chunk};
use crate::ffi::register::{LoadContext, host_register_component};
use crate::loader::ModId;
use crate::registry::{ComponentOptions, ModRegistry};
use crate::{HostCommands, HostContext, HostIter};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{AccessMode, BamFieldDesc, BamFieldKind, FfiBool, ModComponent as _};
use bevy_ecs::change_detection::Tick;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Component;
use bevy_ecs::world::World;
use core::ffi::c_char;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "v03_test::Stats")]
struct TestStats {
    hp: i32,
    speed: f32,
}

#[test]
fn diagnostics_uuid_and_layout_introspection_work() {
    let mut world = World::new();
    let comp_id = world.register_component::<TestStats>();
    let mut registry = ModRegistry::new();
    let stats_handle = registry
        .register_component::<TestStats>(comp_id, ComponentOptions::default())
        .expect("register TestStats");
    let snap = registry.snapshot_arc();
    let host_ctx = HostContext::new(snap).with_namespace(Some(Arc::from("v03_test")));

    // SAFETY: `host_ctx` and local output buffers are valid and aligned for every FFI call below.
    unsafe {
        // 1. `log_diagnostic` must emit without overwriting `last_error`.
        host_ctx.set_last_error("prior error");
        let ok_res = host_log_diagnostic(
            host_ctx.as_ffi_ptr(),
            BAM_LOG_INFO,
            StrView::from_str("hello from guest"),
        );
        assert_eq!(ok_res, FfiResult::OK);
        assert_eq!(host_ctx.last_error_message(), "prior error");
        assert_eq!(
            host_log_diagnostic(host_ctx.as_ffi_ptr(), 99, StrView::from_str("bad level")),
            FfiResult::INVALID_ARGUMENT
        );

        // 2. `report_guest_fault` records into `last_error`.
        assert_eq!(
            host_report_guest_fault(host_ctx.as_ffi_ptr(), StrView::from_str("boom fault")),
            FfiResult::OK
        );
        assert_eq!(host_ctx.last_error_message(), "boom fault");

        // 3. `resolve_component_by_uuid`
        let mut resolved = BamComponent::INVALID;
        let uuid = TestStats::UUID;
        assert_eq!(
            host_resolve_component_by_uuid(host_ctx.as_ffi_ptr(), &uuid, &raw mut resolved),
            FfiResult::OK
        );
        assert_eq!(resolved, stats_handle);

        let missing_uuid = ComponentUuid::from_canonical_name("v03_test::DoesNotExist");
        assert_eq!(
            host_resolve_component_by_uuid(host_ctx.as_ffi_ptr(), &missing_uuid, &raw mut resolved,),
            FfiResult::NOT_FOUND
        );

        // 4. `component_layout_info` for static host `#[derive(ModComponent)]`
        let mut layout = BamLayoutInfo::empty();
        assert_eq!(
            host_component_layout_info(host_ctx.as_ffi_ptr(), stats_handle, &raw mut layout),
            FfiResult::OK
        );
        assert_eq!(layout.uuid, TestStats::UUID);
        assert_eq!(layout.schema_fingerprint, TestStats::SCHEMA_FINGERPRINT);
        assert_eq!(layout.size, core::mem::size_of::<TestStats>());
        assert_eq!(layout.align, core::mem::align_of::<TestStats>());
        assert_eq!(layout.field_count, 2);
        let fields_slice = core::slice::from_raw_parts(layout.fields, layout.field_count);
        assert_eq!(fields_slice[0].name.as_str().unwrap(), "hp");
        assert_eq!(fields_slice[0].kind, BamFieldKind::I32);
        assert_eq!(fields_slice[1].name.as_str().unwrap(), "speed");
        assert_eq!(fields_slice[1].kind, BamFieldKind::F32);

        // 5. Dynamic component with owned copied `BamFieldDesc` survives source buffer drop
        let mut load_ctx = LoadContext::with_manifest_name(
            registry,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(3)),
            Some(Arc::from("dyn_mod")),
        );
        let dyn_handle = {
            let local_name = String::from("mana");
            let local_tag = String::from("f32");
            let local_fields = [BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str(&local_name),
                StrView::from_str(&local_tag),
                0,
                4,
                4,
            )];
            let desc = ComponentDesc::new(
                c"DynMana".as_ptr(),
                ComponentUuid::from_parts("dyn_mod::DynMana", 4, 4, 0),
                4,
                4,
            )
            .with_schema_fingerprint(0xDEAD_BEEF_CAFE_BABEu64)
            .with_fields(local_fields.as_ptr(), local_fields.len());
            let mut out = BamComponent::INVALID;
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &desc, &raw mut out),
                FfiResult::OK
            );
            out
        };
        let mut dyn_layout = BamLayoutInfo::empty();
        assert_eq!(
            host_component_layout_info(
                load_ctx.host_ctx.as_ffi_ptr(),
                dyn_handle,
                &raw mut dyn_layout,
            ),
            FfiResult::OK
        );
        assert_eq!(dyn_layout.schema_fingerprint, 0xDEAD_BEEF_CAFE_BABEu64);
        assert_eq!(dyn_layout.field_count, 1);
        let dyn_fields = core::slice::from_raw_parts(dyn_layout.fields, dyn_layout.field_count);
        assert_eq!(dyn_fields[0].name.as_str().unwrap(), "mana");
        assert_eq!(dyn_fields[0].type_tag.as_str().unwrap(), "f32");
        load_ctx.finish_init();
    }
}

#[test]
fn iter_count_and_register_system_handle_work() {
    let mut world = World::new();
    let comp_id = world.register_component::<TestStats>();
    world.spawn(TestStats { hp: 10, speed: 1.0 });
    world.spawn(TestStats { hp: 20, speed: 2.0 });
    world.spawn(TestStats { hp: 30, speed: 3.0 });

    let mut registry = ModRegistry::new();
    let stats_handle = registry
        .register_component::<TestStats>(comp_id, ComponentOptions::default())
        .expect("register TestStats");

    let plan = QueryPlan::from_terms(vec![QueryTermMeta {
        component: stats_handle,
        component_id: comp_id,
        mode: AccessMode::READ,
        uuid: TestStats::UUID,
        size: core::mem::size_of::<TestStats>(),
        align: core::mem::align_of::<TestStats>(),
        storage_flags: 0,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);
    let table_id = world
        .archetypes()
        .iter()
        .find(|a| a.contains(comp_id))
        .expect("archetype")
        .table_id();
    let matched = [table_id];
    let mut iter = HostIter::new_dense(
        &world.storages().tables,
        &matched,
        &plan,
        Tick::new(0),
        Tick::new(1),
    );
    let mut count = 0usize;
    let mut chunk_len = 0usize;
    // SAFETY: `iter` and local stack outputs are valid and aligned.
    unsafe {
        assert_eq!(
            host_iter_count(iter.as_ffi_ptr(), &raw mut count),
            FfiResult::OK
        );
        assert_eq!(count, 3);
        // Non-destructive: `next_chunk` still yields all 3 entities.
        assert_eq!(
            host_iter_next_chunk(iter.as_ffi_ptr(), &raw mut chunk_len),
            FfiBool::TRUE
        );
        assert_eq!(chunk_len, 3);
        assert_eq!(
            host_iter_count(iter.as_ffi_ptr(), &raw mut count),
            FfiResult::OK
        );
        assert_eq!(count, 3);
    }
    drop(iter);

    // `register_system_handle` returns a valid 1-based handle.
    unsafe extern "C" fn noop_sys(
        _ctx: *mut bevy_abi_mod_sys::BamSystemCtx,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    let mut load_ctx = LoadContext::with_manifest_name(
        registry,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        None,
        None,
    );
    let sys_desc = BamSystemDesc::new(
        c"v03_test::count_sys".as_ptr(),
        null(),
        0,
        noop_sys,
        core::ptr::null_mut(),
    );
    let mut sys_handle = BamSystem::INVALID;
    // SAFETY: `load_ctx`, `sys_desc`, and `sys_handle` are valid aligned pointers.
    unsafe {
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &sys_desc, &raw mut sys_handle),
            FfiResult::OK
        );
    }
    assert_eq!(sys_handle, BamSystem::from_raw(1));
    assert_eq!(
        // SAFETY: No-op test callback touches neither world nor ctx pointers.
        unsafe { noop_sys(core::ptr::null_mut(), core::ptr::null_mut()) },
        FfiResult::OK
    );
    load_ctx.finish_init();
}

static DROP_COUNTER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn test_dyn_drop(_ptr: *mut c_void) {
    DROP_COUNTER.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn cmd_checkpoint_and_rollback_drop_uncommitted_payloads_once() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    DROP_COUNTER.store(0, Ordering::SeqCst);
    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let dyn_uuid = ComponentUuid::from_parts("rollback_test::DynBox", 8, 8, 0);
    let desc = ComponentDesc::new(c"rollback_test::DynBox".as_ptr(), dyn_uuid, 8, 8)
        .with_drop(test_dyn_drop);

    let mut dyn_handle = BamComponent::INVALID;
    {
        let mut load_ctx = LoadContext::with_manifest_name(
            registry,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            None,
        );
        // SAFETY: `load_ctx`, `desc`, and `dyn_handle` are valid aligned pointers.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &desc, &raw mut dyn_handle),
                FfiResult::OK
            );
        }
        registry = load_ctx.registry.clone();
        load_ctx.finish_init();
    }

    let e_keep = world.spawn_empty().id();
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    let cmd_ptr = cmds.as_ffi_ptr();

    let keep_val = 111u64;
    let roll_val = 222u64;
    let batch_val = 333u64;
    let batch_comps = [dyn_handle];
    let batch_ptrs = [(&raw const batch_val).cast::<u8>()];
    let mut cp = 0usize;
    let mut temp_ent = FfiEntity::NULL;
    let mut batch_ent = FfiEntity::NULL;

    // SAFETY: `cmd_ptr` and all stack component buffers and out-pointers are valid for the active `HostCommands`.
    unsafe {
        assert_eq!(
            host_cmd_insert(
                cmd_ptr,
                FfiEntity::from_bits(e_keep.to_bits()),
                dyn_handle,
                (&raw const keep_val).cast::<u8>(),
                8,
            ),
            FfiResult::OK
        );
        assert_eq!(host_cmd_checkpoint(cmd_ptr, &raw mut cp), FfiResult::OK);
        assert_eq!(cp, 1);

        assert_eq!(
            host_cmd_spawn_empty(cmd_ptr, &raw mut temp_ent),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_insert(
                cmd_ptr,
                temp_ent,
                dyn_handle,
                (&raw const roll_val).cast::<u8>(),
                8,
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_spawn(
                cmd_ptr,
                batch_comps.as_ptr(),
                batch_ptrs.as_ptr(),
                1,
                &raw mut batch_ent,
            ),
            FfiResult::OK
        );

        // Rolling back out-of-range checkpoint fails without mutating queue.
        assert_eq!(host_cmd_rollback(cmd_ptr, cp + 99), FfiResult::OUT_OF_RANGE);

        // Roll back to `cp`: drops `roll_val` and `batch_val` (2 payloads), leaves `keep_val` intact.
        assert_eq!(host_cmd_rollback(cmd_ptr, cp), FfiResult::OK);
    }
    assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 2);
    assert_eq!(cmds.checkpoint(), 1);
    drop(cmds);

    storage.apply_to_world(&mut world, &snap);
    assert!(
        world
            .get_entity(Entity::from_bits(batch_ent.to_bits()))
            .is_err()
    );
    assert!(
        world
            .get_entity(Entity::from_bits(temp_ent.to_bits()))
            .is_err()
    );
    let dyn_comp_id = snap.get_by_handle(dyn_handle).unwrap().component_id;
    assert!(world.get_entity(e_keep).unwrap().contains_id(dyn_comp_id));
    assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 2);
    assert!(world.despawn(e_keep));
    assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 3);
}

#[test]
fn reserved_v03_slots_validate_arguments_before_returning_unsupported() {
    let mut world = World::new();
    let comp_id = world.register_component::<TestStats>();
    let mut registry = ModRegistry::new();
    let stats_handle = registry
        .register_component::<TestStats>(comp_id, ComponentOptions::default())
        .expect("register TestStats");
    let live_ent = FfiEntity::from_bits(world.spawn_empty().id().to_bits());
    let mut load_ctx = LoadContext::with_manifest_name(
        registry,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        None,
        None,
    );
    let host_ptr = load_ctx.host_ctx.as_ffi_ptr();

    let rel_uuid = ComponentUuid::from_canonical_name("v03_test::Rel");
    let rel_desc = BamRelationDesc::new(c"v03_test::Rel".as_ptr(), rel_uuid);
    let mut rel_out = BamComponent::INVALID;
    let mut sym = BamSymbol::INVALID;
    let set_desc = BamScheduleSetDesc::new(c"v03_test::Set".as_ptr());
    let mut set_out = BamSystemSet::INVALID;

    unsafe extern "C" fn stub_migrate(
        _src: *const c_void,
        _dst: *mut c_void,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    let mig_desc = ComponentDesc::new(c"v03_test::Stats".as_ptr(), TestStats::UUID, 8, 4);

    // SAFETY: Valid `load_ctx`, `host_ctx`, descriptors, and stack out-pointers; intentional
    // null descriptors test FFI null-pointer rejection.
    unsafe {
        assert_eq!(
            host_register_relation(load_ctx.as_ffi_ptr(), &rel_desc, &raw mut rel_out),
            FfiResult::OK
        );
        assert!(rel_out.is_valid());
        assert_eq!(
            host_register_relation(load_ctx.as_ffi_ptr(), null(), &raw mut rel_out),
            FfiResult::NULL_POINTER
        );

        assert_eq!(
            host_intern_symbol(host_ptr, StrView::from_str(""), &raw mut sym),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_intern_symbol(host_ptr, StrView::from_str("hp"), &raw mut sym),
            FfiResult::OK
        );
        assert_eq!(sym, BamSymbol::from_raw(0));

        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), FfiEntity::NULL),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_bind_named_object(host_ptr, StrView::from_str("hero"), live_ent),
            FfiResult::OK
        );

        assert_eq!(
            host_configure_schedule_set(load_ctx.as_ffi_ptr(), &set_desc, &raw mut set_out),
            FfiResult::OK
        );
        assert_eq!(set_out, BamSystemSet::from_raw(0));
        assert_eq!(
            host_mount_system_in_schedule(
                load_ctx.as_ffi_ptr(),
                BamSystem::INVALID,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID,
            ),
            FfiResult::INVALID_ARGUMENT
        );

        assert_eq!(
            host_migrate_component_column(
                load_ctx.as_ffi_ptr(),
                stats_handle,
                TestStats::SCHEMA_FINGERPRINT,
                &mig_desc,
                None,
                core::ptr::null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_migrate_component_column(
                load_ctx.as_ffi_ptr(),
                stats_handle,
                TestStats::SCHEMA_FINGERPRINT,
                &mig_desc,
                Some(stub_migrate),
                core::ptr::null_mut(),
            ),
            FfiResult::PERMISSION_DENIED
        );
    }

    let mut storage = HostCommandsStorage::new();
    let snap = load_ctx.registry.snapshot_arc();
    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    // SAFETY: Valid `cmds` is active and valid on the stack; intentional null `data` with size 4 tests validation.
    unsafe {
        assert_eq!(
            host_cmd_insert_relation(
                cmds.as_ffi_ptr(),
                live_ent,
                stats_handle,
                live_ent,
                null(),
                4,
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_insert_relation(
                cmds.as_ffi_ptr(),
                live_ent,
                stats_handle,
                live_ent,
                null(),
                0,
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_insert_relation(cmds.as_ffi_ptr(), live_ent, rel_out, live_ent, null(), 0),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_remove_relation(cmds.as_ffi_ptr(), live_ent, stats_handle, live_ent),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_cmd_remove_relation(cmds.as_ffi_ptr(), live_ent, rel_out, live_ent),
            FfiResult::OK
        );
    }

    load_ctx.finish_init();
}

#[test]
fn v03_all_null_invalid_handle_struct_size_and_schedule_set_guards() {
    use crate::schedule::ModScheduleMap;
    use crate::system::error_policy::ActiveGuestFaultGuard;
    use bevy_abi_mod_sys::{BAM_SCHEDULE_SET_FLAG_SEQUENCE, BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK};
    use bevy_ecs::schedule::Schedule;
    use core::ptr::null_mut;

    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let comp_id = world.register_component::<TestStats>();
    let stats_handle = registry
        .register_component::<TestStats>(comp_id, ComponentOptions::default())
        .expect("register TestStats");
    let unbound_ctx = HostContext::new(registry.snapshot_arc());
    let mut poisoned_ctx = HostContext::new(registry.snapshot_arc());
    poisoned_ctx.header.poison();
    let poisoned_ctx_ptr = poisoned_ctx.as_ffi_ptr();

    let bad_utf8_bytes = [0xFFu8, 0xFE, 0];
    let bad_utf8_view = StrView {
        ptr: bad_utf8_bytes.as_ptr(),
        len: 2,
    };

    // 1. `log_diagnostic` (all 5 levels + guards) and `report_guest_fault` (with ActiveGuestFaultGuard).
    // SAFETY: Valid/null/poisoned handles tested safely by FFI magic validation.
    unsafe {
        for lvl in [
            BAM_LOG_TRACE,
            BAM_LOG_DEBUG,
            BAM_LOG_INFO,
            BAM_LOG_WARN,
            BAM_LOG_ERROR,
        ] {
            assert_eq!(
                host_log_diagnostic(
                    unbound_ctx.as_ffi_ptr(),
                    lvl,
                    StrView::from_str("diag level")
                ),
                FfiResult::OK
            );
        }
        assert_eq!(
            host_log_diagnostic(null(), BAM_LOG_INFO, StrView::from_str("x")),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_log_diagnostic(poisoned_ctx_ptr, BAM_LOG_INFO, StrView::from_str("x")),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_log_diagnostic(unbound_ctx.as_ffi_ptr(), BAM_LOG_INFO, bad_utf8_view),
            FfiResult::INVALID_UTF8
        );

        assert_eq!(
            host_report_guest_fault(null(), StrView::from_str("x")),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_report_guest_fault(poisoned_ctx_ptr, StrView::from_str("x")),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_report_guest_fault(unbound_ctx.as_ffi_ptr(), bad_utf8_view),
            FfiResult::INVALID_UTF8
        );

        let guard = ActiveGuestFaultGuard::enter(
            unbound_ctx.as_ffi_ptr(),
            Some(ModId(1)),
            Some(Arc::from("fault_ns")),
            2,
            Arc::from("fault_sys"),
        );
        assert_eq!(
            host_report_guest_fault(unbound_ctx.as_ffi_ptr(), StrView::from_str("inside system")),
            FfiResult::OK
        );
        assert_eq!(guard.finish().as_deref(), Some("inside system"));

        // 2. `resolve_component_by_uuid`, `component_layout_info`, `iter_count` guards.
        let mut out_comp = BamComponent::INVALID;
        let uuid = TestStats::UUID;
        assert_eq!(
            host_resolve_component_by_uuid(unbound_ctx.as_ffi_ptr(), &uuid, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component_by_uuid(null(), &uuid, &raw mut out_comp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_resolve_component_by_uuid(unbound_ctx.as_ffi_ptr(), null(), &raw mut out_comp),
            FfiResult::NULL_POINTER
        );

        let mut out_info = BamLayoutInfo::empty();
        assert_eq!(
            host_component_layout_info(unbound_ctx.as_ffi_ptr(), stats_handle, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_component_layout_info(null(), stats_handle, &raw mut out_info),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_component_layout_info(
                unbound_ctx.as_ffi_ptr(),
                BamComponent::from_raw(999),
                &raw mut out_info
            ),
            FfiResult::INVALID_COMPONENT
        );

        let mut out_count = 0usize;
        assert_eq!(
            host_iter_count(core::ptr::null_mut(), null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_count(core::ptr::null_mut(), &raw mut out_count),
            FfiResult::NULL_POINTER
        );

        // 3. `intern_symbol` and `bind_named_object` unbound-world & null guards.
        let mut sym = BamSymbol::INVALID;
        assert_eq!(
            host_intern_symbol(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str("hp"),
                null_mut()
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_intern_symbol(null(), StrView::from_str("hp"), &raw mut sym),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_intern_symbol(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str("hp"),
                &raw mut sym
            ),
            FfiResult::INVALID_STATE
        );

        assert_eq!(
            host_bind_named_object(null(), StrView::from_str("hero"), FfiEntity::NULL),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_bind_named_object(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str(""),
                FfiEntity::NULL
            ),
            FfiResult::INVALID_ARGUMENT
        );
        let live_e = world.spawn_empty().id();
        assert_eq!(
            host_bind_named_object(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str("hero"),
                FfiEntity::from_bits(live_e.to_bits())
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_bind_named_object(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str("bad_bits"),
                FfiEntity::from_bits(1u64 << 32)
            ),
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            host_bind_named_object(
                unbound_ctx.as_ffi_ptr(),
                StrView::from_str("placeholder"),
                FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits())
            ),
            FfiResult::INVALID_ENTITY
        );

        // 4. `cmd_checkpoint` & `cmd_rollback` null/invalid handle + nested rollback.
        let mut cp = 0usize;
        assert_eq!(
            host_cmd_checkpoint(core::ptr::null_mut(), null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_checkpoint(core::ptr::null_mut(), &raw mut cp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_rollback(core::ptr::null_mut(), 0),
            FfiResult::NULL_POINTER
        );
    }

    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    let mut cp1 = 0usize;
    let mut cp2 = 0usize;
    let mut e_out = FfiEntity::NULL;
    // SAFETY: Valid `cmds` and stack variables.
    unsafe {
        assert_eq!(
            host_cmd_spawn_empty(cmds.as_ffi_ptr(), &raw mut e_out),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_checkpoint(cmds.as_ffi_ptr(), &raw mut cp1),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_spawn_empty(cmds.as_ffi_ptr(), &raw mut e_out),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_checkpoint(cmds.as_ffi_ptr(), &raw mut cp2),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_spawn_empty(cmds.as_ffi_ptr(), &raw mut e_out),
            FfiResult::OK
        );
        assert_eq!(cp1, 1);
        assert_eq!(cp2, 2);
        assert_eq!(host_cmd_rollback(cmds.as_ffi_ptr(), cp2), FfiResult::OK);
        assert_eq!(cmds.checkpoint(), 2);
        assert_eq!(host_cmd_rollback(cmds.as_ffi_ptr(), cp1), FfiResult::OK);
        assert_eq!(cmds.checkpoint(), 1);
    }
    drop(cmds);

    // 5. `register_system_handle`, `configure_schedule_set`, `mount_system_in_schedule`, `migrate_component_column`
    unsafe extern "C" fn test_sys_cb(
        _ctx: *mut bevy_abi_mod_sys::BamSystemCtx,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn test_mig_cb(
        src: *const c_void,
        dst: *mut c_void,
        _ud: *mut c_void,
    ) -> FfiResult {
        // SAFETY: Caller passes valid 4-byte source and 8-byte destination buffers.
        unsafe {
            let v = core::ptr::read_unaligned(src.cast::<u32>());
            core::ptr::write_unaligned(dst.cast::<u64>(), u64::from(v) * 10);
        }
        FfiResult::OK
    }

    // Standalone LoadContext (`world_ptr.is_null()`)
    let mut standalone = LoadContext::new(registry.clone());
    let sys_desc = BamSystemDesc::new(c"sa::sys".as_ptr(), null(), 0, test_sys_cb, null_mut());
    let mut sys_h = BamSystem::INVALID;
    let set_desc = BamScheduleSetDesc::new(c"sa::Set".as_ptr());
    let mut set_out = BamSystemSet::INVALID;

    // SAFETY: Valid standalone `LoadContext` and stack descriptors/outputs; null/short checks test FFI validation.
    unsafe {
        assert_eq!(
            host_register_system_handle(standalone.as_ffi_ptr(), &sys_desc, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_system_handle(null_mut(), &sys_desc, &raw mut sys_h),
            FfiResult::NULL_POINTER
        );
        let mut short_sys = sys_desc;
        short_sys.struct_size = 4;
        assert_eq!(
            host_register_system_handle(standalone.as_ffi_ptr(), &short_sys, &raw mut sys_h),
            FfiResult::INVALID_VERSION
        );
        assert_eq!(
            host_register_system_handle(standalone.as_ffi_ptr(), &sys_desc, &raw mut sys_h),
            FfiResult::OK
        );

        // Standalone `mount_system_in_schedule`: built-in schedule + INVALID set succeeds (`787-794`),
        // while custom schedule or valid set returns `INVALID_STATE` (`681-684`).
        assert_eq!(
            host_mount_system_in_schedule(
                standalone.as_ffi_ptr(),
                sys_h,
                ScheduleId::FIXED_UPDATE,
                BamSystemSet::INVALID,
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_mount_system_in_schedule(
                standalone.as_ffi_ptr(),
                sys_h,
                ScheduleId::custom(2048),
                BamSystemSet::INVALID,
            ),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_mount_system_in_schedule(
                standalone.as_ffi_ptr(),
                sys_h,
                ScheduleId::UPDATE,
                BamSystemSet::from_raw(0),
            ),
            FfiResult::INVALID_STATE
        );

        // `configure_schedule_set` with null `world_ptr` -> `INVALID_STATE` (`571-574`) + header/name guards.
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &set_desc, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_configure_schedule_set(null_mut(), &set_desc, &raw mut set_out),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), null(), &raw mut set_out),
            FfiResult::NULL_POINTER
        );
        let mut short_set = set_desc;
        short_set.struct_size = 4;
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &short_set, &raw mut set_out),
            FfiResult::INVALID_VERSION
        );
        let bad_flags_set = BamScheduleSetDesc::new(c"sa::Set".as_ptr()).with_flags(1 << 30);
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &bad_flags_set, &raw mut set_out),
            FfiResult::UNSUPPORTED
        );
        let empty_name_set = BamScheduleSetDesc::new(c"".as_ptr());
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &empty_name_set, &raw mut set_out),
            FfiResult::INVALID_ARGUMENT
        );
        let bad_utf8_set = BamScheduleSetDesc::new(bad_utf8_bytes.as_ptr().cast::<c_char>());
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &bad_utf8_set, &raw mut set_out),
            FfiResult::INVALID_UTF8
        );
        let bad_syntax_set = BamScheduleSetDesc::new(c"a::b::c".as_ptr());
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &bad_syntax_set, &raw mut set_out),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_configure_schedule_set(standalone.as_ffi_ptr(), &set_desc, &raw mut set_out),
            FfiResult::INVALID_STATE
        );
    }
    standalone.finish_init();

    // World-bound `LoadContext`: exercise `configure_schedule_set` (`before_sets`, `after_sets`,
    // `BEFORE_WEAK`, `AFTER_WEAK`, missing schedule, unknown dependency set), `mount_system_in_schedule`
    // (missing `ModScheduleSetRegistry`, mismatched schedule set, remounting across schedules),
    // and `migrate_component_column` (null/version/dynamic 4B -> 8B migration).
    world.insert_resource(ModScheduleMap::new());

    let mut world_ctx = LoadContext::with_manifest_name(
        registry,
        Arc::new(AtomicBool::new(true)),
        (&raw mut world).cast::<World>(),
        Some(ModId(8)),
        Some(Arc::from("sched_mod")),
    );
    let mut world_sys = BamSystem::INVALID;
    let mut set_a = BamSystemSet::INVALID;
    let mut set_b = BamSystemSet::INVALID;
    let mut dyn_h = BamComponent::INVALID;

    let dyn_uuid = ComponentUuid::from_parts("sched_mod::DynV1", 4, 4, 0);
    let dyn_v1 = ComponentDesc::new(c"sched_mod::DynV".as_ptr(), dyn_uuid, 4, 4)
        .with_schema_fingerprint(0x1111);
    let dyn_v2 = ComponentDesc::new(c"sched_mod::DynV".as_ptr(), dyn_uuid, 8, 8)
        .with_schema_fingerprint(0x2222);

    // SAFETY: Valid world-bound `LoadContext` and stack descriptors/outputs.
    unsafe {
        assert_eq!(
            host_register_system_handle(world_ctx.as_ffi_ptr(), &sys_desc, &raw mut world_sys),
            FfiResult::OK
        );

        // Missing `ModScheduleSetRegistry` -> `NOT_FOUND` (`723-727`)
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::UPDATE,
                BamSystemSet::from_raw(0),
            ),
            FfiResult::NOT_FOUND
        );
        // Invalid `ScheduleId` / unregistered custom schedule / invalid `LoadContext`
        assert_eq!(
            host_mount_system_in_schedule(
                null_mut(),
                world_sys,
                ScheduleId::UPDATE,
                BamSystemSet::INVALID,
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId(100),
                BamSystemSet::INVALID,
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::custom(2049),
                BamSystemSet::INVALID,
            ),
            FfiResult::NOT_FOUND
        );

        // Configure `set_a` (SEQUENCE on UPDATE) and `set_b` (SEQUENCE_WEAK + BEFORE_WEAK + AFTER_WEAK on FIXED_UPDATE)
        let mut unknown_sched_set = BamScheduleSetDesc::new(c"MissingSchedSet".as_ptr());
        unknown_sched_set.schedule = ScheduleId::custom(2049);
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &unknown_sched_set, &raw mut set_a),
            FfiResult::NOT_FOUND
        );

        let mut set_a_desc =
            BamScheduleSetDesc::new(c"PhaseA".as_ptr()).with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
        set_a_desc.schedule = ScheduleId::UPDATE;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &set_a_desc, &raw mut set_a),
            FfiResult::OK
        );

        let dep_bad = [BamSystemSet::from_raw(999)];
        let mut bad_dep_desc = BamScheduleSetDesc::new(c"BadDep".as_ptr());
        bad_dep_desc.schedule = ScheduleId::UPDATE;
        bad_dep_desc.before_sets = dep_bad.as_ptr();
        bad_dep_desc.before_count = 1;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &bad_dep_desc, &raw mut set_b),
            FfiResult::NOT_FOUND
        );

        let dep_a = [set_a];
        let mut cycle_desc = BamScheduleSetDesc::new(c"PhaseCycle".as_ptr());
        cycle_desc.schedule = ScheduleId::UPDATE;
        cycle_desc.before_sets = dep_a.as_ptr();
        cycle_desc.before_count = 1;
        cycle_desc.after_sets = dep_a.as_ptr();
        cycle_desc.after_count = 1;
        let mut set_cycle = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &cycle_desc, &raw mut set_cycle),
            FfiResult::CYCLE_DETECTED
        );

        let mut set_c_desc = BamScheduleSetDesc::new(c"PhaseC".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK | BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK);
        set_c_desc.schedule = ScheduleId::UPDATE;
        set_c_desc.before_sets = dep_a.as_ptr();
        set_c_desc.before_count = 1;
        let mut set_c = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &set_c_desc, &raw mut set_c),
            FfiResult::OK
        );

        let dep_c = [set_c];
        let mut set_d_desc = BamScheduleSetDesc::new(c"PhaseD".as_ptr())
            .with_flags(BAM_SCHEDULE_SET_FLAG_AFTER_WEAK);
        set_d_desc.schedule = ScheduleId::UPDATE;
        set_d_desc.after_sets = dep_c.as_ptr();
        set_d_desc.after_count = 1;
        let mut set_d = BamSystemSet::INVALID;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &set_d_desc, &raw mut set_d),
            FfiResult::OK
        );

        let mut set_b_desc = BamScheduleSetDesc::new(c"PhaseB".as_ptr());
        set_b_desc.schedule = ScheduleId::FIXED_UPDATE;
        assert_eq!(
            host_configure_schedule_set(world_ctx.as_ffi_ptr(), &set_b_desc, &raw mut set_b),
            FfiResult::OK
        );

        // Unknown set handle / cross-schedule set mismatch
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::UPDATE,
                BamSystemSet::from_raw(999),
            ),
            FfiResult::NOT_FOUND
        );
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::UPDATE,
                set_b,
            ),
            FfiResult::INVALID_ARGUMENT
        );

        // Mount into `set_a` on UPDATE, then remount onto `set_b` on FIXED_UPDATE (`schedule_changed` path).
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::UPDATE,
                set_a,
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_mount_system_in_schedule(
                world_ctx.as_ffi_ptr(),
                world_sys,
                ScheduleId::FIXED_UPDATE,
                set_b,
            ),
            FfiResult::OK
        );

        // `migrate_component_column` guards & live dynamic 4B -> 8B migration (`test_mig_cb`).
        assert_eq!(
            host_migrate_component_column(
                null_mut(),
                stats_handle,
                0,
                &dyn_v2,
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_migrate_component_column(
                world_ctx.as_ffi_ptr(),
                BamComponent::from_raw(999),
                0,
                &dyn_v2,
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            host_migrate_component_column(
                world_ctx.as_ffi_ptr(),
                stats_handle,
                0,
                null(),
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
        let mut short_mig = dyn_v2;
        short_mig.struct_size = 4;
        assert_eq!(
            host_migrate_component_column(
                world_ctx.as_ffi_ptr(),
                stats_handle,
                0,
                &short_mig,
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::INVALID_VERSION
        );
        let bad_mig = ComponentDesc::new(null(), dyn_uuid, 8, 8);
        assert_eq!(
            host_migrate_component_column(
                world_ctx.as_ffi_ptr(),
                stats_handle,
                0,
                &bad_mig,
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::NULL_POINTER
        );

        assert_eq!(
            host_register_component(world_ctx.as_ffi_ptr(), &dyn_v1, &raw mut dyn_h),
            FfiResult::OK
        );
    }

    // Spawn an entity carrying `dyn_h` (4B `u32(7)`), then migrate it to 8B `u64(70)`.
    let dyn_cid = world_ctx
        .registry
        .snapshot()
        .get_by_handle(dyn_h)
        .unwrap()
        .component_id;
    let mig_e = world.spawn_empty().id();
    let mut v1_val: u32 = 7;
    // SAFETY: `dyn_cid` has `(4, 4)` layout matching `u32`.
    unsafe {
        world.entity_mut(mig_e).insert_by_id(
            dyn_cid,
            bevy_ecs::ptr::OwningPtr::new(core::ptr::NonNull::from(&mut v1_val).cast()),
        );
        assert_eq!(
            host_migrate_component_column(
                world_ctx.as_ffi_ptr(),
                dyn_h,
                0x1111,
                &dyn_v2,
                Some(test_mig_cb),
                null_mut(),
            ),
            FfiResult::OK
        );
    }
    let v2_cid = world_ctx
        .registry
        .snapshot()
        .get_by_handle(dyn_h)
        .unwrap()
        .component_id;
    let migrated_ptr = world.entity(mig_e).get_by_id(v2_cid).unwrap();
    // SAFETY: Migrated component now has 8-byte `u64` layout holding `70`.
    let migrated_val = unsafe { core::ptr::read_unaligned(migrated_ptr.as_ptr().cast::<u64>()) };
    assert_eq!(migrated_val, 70);

    // Run the registered system once so `test_sys_cb` executes as well.
    let sys = world_ctx.registered_systems.pop().unwrap();
    world_ctx.finish_init();
    let mut sched = Schedule::default();
    sched.add_systems(sys);
    sched.run(&mut world);
}
