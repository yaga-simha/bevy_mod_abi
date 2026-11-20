use super::*;
use bevy_abi_mod_sys::{
    BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK, ModComponent,
};
use bevy_ecs::prelude::Component;
use core::sync::atomic::AtomicUsize;
use std::ffi::CString;

struct HookLog {
    events: Mutex<Vec<(&'static str, FfiEntity, u32)>>,
}

unsafe extern "C" fn test_hook_add(entity: FfiEntity, data: *const c_void, user_data: *mut c_void) {
    // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
    let val = unsafe { *data.cast::<u32>() };
    // SAFETY: `user_data` points to `HookLog`.
    let log = unsafe { &*user_data.cast::<HookLog>() };
    log.events.lock().unwrap().push(("add", entity, val));
}

unsafe extern "C" fn test_hook_insert(
    entity: FfiEntity,
    data: *const c_void,
    user_data: *mut c_void,
) {
    // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
    let val = unsafe { *data.cast::<u32>() };
    // SAFETY: `user_data` points to `HookLog`.
    let log = unsafe { &*user_data.cast::<HookLog>() };
    log.events.lock().unwrap().push(("insert", entity, val));
}

unsafe extern "C" fn test_hook_discard(
    entity: FfiEntity,
    data: *const c_void,
    user_data: *mut c_void,
) {
    // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
    let val = unsafe { *data.cast::<u32>() };
    // SAFETY: `user_data` points to `HookLog`.
    let log = unsafe { &*user_data.cast::<HookLog>() };
    log.events.lock().unwrap().push(("discard", entity, val));
}

unsafe extern "C" fn test_hook_remove(
    entity: FfiEntity,
    data: *const c_void,
    user_data: *mut c_void,
) {
    // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
    let val = unsafe { *data.cast::<u32>() };
    // SAFETY: `user_data` points to `HookLog`.
    let log = unsafe { &*user_data.cast::<HookLog>() };
    log.events.lock().unwrap().push(("remove", entity, val));
}

unsafe extern "C" fn test_hook_despawn(
    entity: FfiEntity,
    data: *const c_void,
    user_data: *mut c_void,
) {
    // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
    let val = unsafe { *data.cast::<u32>() };
    // SAFETY: `user_data` points to `HookLog`.
    let log = unsafe { &*user_data.cast::<HookLog>() };
    log.events.lock().unwrap().push(("despawn", entity, val));
}

unsafe extern "C" fn test_clone_u32_doubled(
    src: *const c_void,
    dst: *mut c_void,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `src` is readable `u32`, `dst` is writable `u32`.
    let val = unsafe { *src.cast::<u32>() };
    // SAFETY: `dst` is writable `u32`.
    unsafe { *dst.cast::<u32>() = val.saturating_mul(2) };
    if !user_data.is_null() {
        // SAFETY: `user_data` points to `AtomicUsize`.
        unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
    }
    FfiResult::OK
}

unsafe extern "C" fn test_clone_zst(
    _src: *const c_void,
    _dst: *mut c_void,
    user_data: *mut c_void,
) -> FfiResult {
    if !user_data.is_null() {
        // SAFETY: `user_data` points to `AtomicUsize`.
        unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
    }
    FfiResult::OK
}

#[derive(Component)]
struct OpaqueHostComp(#[allow(dead_code)] String);

// Opaque host component metadata only; raw insert disabled.
impl ModComponent for OpaqueHostComp {
    const CANONICAL_NAME: &'static str = "reg_test::OpaqueHostComp";
    const COMPONENT_CSTR: &'static CStr = c"reg_test::OpaqueHostComp";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("reg_test::OpaqueHostComp");
    const IS_RAW_INSERTABLE: bool = false;
    const IS_ABI_POD: bool = false;
    type VTable = ();

    fn vtable() -> &'static Self::VTable {
        &()
    }

    fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
        &[]
    }
}

#[test]
fn summary_tick_flag_sets_component_info_and_rejects_sparse_set() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let mut h_summary = BamComponent::INVALID;
    let mut h_plain = BamComponent::INVALID;
    let mut h_bad = BamComponent::INVALID;

    let mut ctx =
        LoadContext::with_world_and_mod_id(reg, active, &mut world as *mut World, Some(ModId(10)));

    let desc_summary = ComponentDesc::new(
        c"reg_test::WithSummary".as_ptr(),
        ComponentUuid::from_parts("reg_test::WithSummary", 4, 4, 0),
        4,
        4,
    )
    .with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK);
    let desc_plain = ComponentDesc::new(
        c"reg_test::PlainTable".as_ptr(),
        ComponentUuid::from_parts("reg_test::PlainTable", 4, 4, 0),
        4,
        4,
    );
    let desc_bad = ComponentDesc::new(
        c"reg_test::BadSparseSummary".as_ptr(),
        ComponentUuid::from_parts("reg_test::BadSparseSummary", 4, 4, 0),
        4,
        4,
    )
    .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_SUMMARY_TICK);

    // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
    unsafe {
        assert_eq!(
            host_register_component(ctx.as_ffi_ptr(), &desc_summary, &mut h_summary),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(ctx.as_ffi_ptr(), &desc_plain, &mut h_plain),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(ctx.as_ffi_ptr(), &desc_bad, &mut h_bad),
            FfiResult::INVALID_ARGUMENT
        );
    }

    let reg_after = ctx.registry.clone();
    ctx.finish_init();

    let m_sum = reg_after.get_by_handle(h_summary).unwrap();
    let m_plain = reg_after.get_by_handle(h_plain).unwrap();
    assert!(m_sum.is_summary_tick());
    assert!(!m_plain.is_summary_tick());
    assert!(
        world
            .components()
            .get_info(m_sum.component_id)
            .unwrap()
            .summary_tick()
    );
    assert!(
        !world
            .components()
            .get_info(m_plain.component_id)
            .unwrap()
            .summary_tick()
    );
}

#[test]
fn all_five_lifecycle_hooks_post_unload_disarm_and_beyond_1024_cap() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let log = Box::new(HookLog {
        events: Mutex::new(Vec::new()),
    });
    let log_ptr = (&*log as *const HookLog).cast_mut().cast::<c_void>();

    let mut h_hooked = BamComponent::INVALID;
    let reg_after = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(11)),
        );
        let desc = ComponentDesc::new(
            c"reg_test::FullHooks".as_ptr(),
            ComponentUuid::from_parts("reg_test::FullHooks", 4, 4, 0),
            4,
            4,
        )
        .with_hooks(Some(test_hook_add), Some(test_hook_remove), log_ptr)
        .with_on_insert(test_hook_insert)
        .with_on_discard(test_hook_discard)
        .with_on_despawn(test_hook_despawn);
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let r_main = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h_hooked) };
        assert_eq!(r_main, FfiResult::OK);

        // Register 1,100 additional hooked components in the same World to prove zero 1,024-slot cap.
        let extra_count = if cfg!(miri) { 8 } else { 1100 };
        for idx in 0..extra_count {
            let name = format!("reg_test::ManyHooked_{idx}");
            let cname = CString::new(name.as_str()).unwrap();
            let mut h = BamComponent::INVALID;
            let extra = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(&name, 4, 4, 0),
                4,
                4,
            )
            .with_on_insert(test_hook_insert)
            .with_hook_user_data(log_ptr);
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let r_extra = unsafe { host_register_component(ctx.as_ffi_ptr(), &extra, &mut h) };
            assert_eq!(r_extra, FfiResult::OK);
        }

        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let cid = reg_after.get_by_handle(h_hooked).unwrap().component_id;
    let e1 = world.spawn_empty().id();
    let e1_ffi = FfiEntity::from_bits(e1.to_bits());

    let mut v1: u32 = 10;
    // SAFETY: `cid` matches 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e1).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut v1).cast()),
        );
    }
    let mut v2: u32 = 20;
    // SAFETY: `cid` matches 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e1).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut v2).cast()),
        );
    }
    world.entity_mut(e1).remove_by_id(cid);

    let mut v3: u32 = 30;
    // SAFETY: `cid` matches 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e1).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut v3).cast()),
        );
    }
    world.entity_mut(e1).despawn();

    let recorded = log.events.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            ("add", e1_ffi, 10),
            ("insert", e1_ffi, 10),
            ("discard", e1_ffi, 10),
            ("insert", e1_ffi, 20),
            ("discard", e1_ffi, 20),
            ("remove", e1_ffi, 20),
            ("add", e1_ffi, 30),
            ("insert", e1_ffi, 30),
            ("despawn", e1_ffi, 30),
            ("discard", e1_ffi, 30),
            ("remove", e1_ffi, 30),
        ]
    );
    // Disarm `is_active` and verify zero foreign hooks run afterwards.
    active.store(false, Ordering::Release);
    let e2 = world.spawn_empty().id();
    let mut v4: u32 = 40;
    // SAFETY: `cid` matches 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e2).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut v4).cast()),
        );
    }
    world.entity_mut(e2).despawn();
    assert_eq!(log.events.lock().unwrap().len(), 11);
}

#[test]
fn entity_cloner_custom_clone_fn_non_zst_zst_and_post_unload_disarm() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let clone_count = Box::new(AtomicUsize::new(0));
    let count_ptr = (&*clone_count as *const AtomicUsize)
        .cast_mut()
        .cast::<c_void>();

    let mut h_val = BamComponent::INVALID;
    let mut h_zst = BamComponent::INVALID;
    let reg_after = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(12)),
        );
        let desc_val = ComponentDesc::new(
            c"reg_test::CloneVal".as_ptr(),
            ComponentUuid::from_parts("reg_test::CloneVal", 4, 4, 0),
            4,
            4,
        )
        .with_clone_fn(test_clone_u32_doubled)
        .with_hook_user_data(count_ptr);
        let desc_zst = ComponentDesc::new(
            c"reg_test::CloneZst".as_ptr(),
            ComponentUuid::from_parts("reg_test::CloneZst", 0, 1, 0),
            0,
            1,
        )
        .with_clone_fn(test_clone_zst)
        .with_hook_user_data(count_ptr);
        // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &desc_val, &mut h_val),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &desc_zst, &mut h_zst),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let cid_val = reg_after.get_by_handle(h_val).unwrap().component_id;
    let cid_zst = reg_after.get_by_handle(h_zst).unwrap().component_id;
    let e = world.spawn_empty().id();
    let mut raw_val: u32 = 21;
    // SAFETY: `cid_val` has 4-byte `u32` layout and `cid_zst` is ZST.
    unsafe {
        world.entity_mut(e).insert_by_id(
            cid_val,
            OwningPtr::new(core::ptr::NonNull::from(&mut raw_val).cast()),
        );
        world.entity_mut(e).insert_by_id(
            cid_zst,
            OwningPtr::new(core::ptr::NonNull::<u8>::dangling()),
        );
    }

    let cloned = world.entity_mut(e).clone_and_spawn();
    let cloned_ref = world.entity(cloned);
    assert!(cloned_ref.contains_id(cid_zst));
    let cloned_ptr = cloned_ref.get_by_id(cid_val).unwrap();
    // SAFETY: `cloned_ptr` points to 4-byte `u32` initialized by `test_clone_u32_doubled`.
    let cloned_val = unsafe { core::ptr::read_unaligned(cloned_ptr.as_ptr().cast::<u32>()) };
    assert_eq!(cloned_val, 42);
    assert_eq!(clone_count.load(Ordering::Relaxed), 2);

    // Disarm `is_active` and clone again: `clone_fn` must NOT execute!
    active.store(false, Ordering::Release);
    let cloned2 = world.entity_mut(e).clone_and_spawn();
    assert!(!world.entity(cloned2).contains_id(cid_val));
    assert!(!world.entity(cloned2).contains_id(cid_zst));
    assert_eq!(clone_count.load(Ordering::Relaxed), 2);
}

#[test]
fn transitive_required_components_preserves_existing_and_validates_ctors() {
    let mut world = World::new();
    let mut reg = ModRegistry::new();
    let opaque_cid = world.register_component::<OpaqueHostComp>();
    reg.register_component::<OpaqueHostComp>(opaque_cid, Default::default())
        .unwrap();
    let h_opaque = reg.get_by_id(opaque_cid).unwrap().handle;

    let active = Arc::new(AtomicBool::new(true));
    let mut h_c = BamComponent::INVALID;
    let mut h_b = BamComponent::INVALID;
    let mut h_a = BamComponent::INVALID;
    let mut h_fail = BamComponent::INVALID;

    let reg_after = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            active,
            &mut world as *mut World,
            Some(ModId(13)),
        );
        let desc_c = ComponentDesc::new(
            c"req_test::CompC".as_ptr(),
            ComponentUuid::from_parts("req_test::CompC", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_c, &mut h_c) };
        assert_eq!(rc, FfiResult::OK);

        let reqs_b = [h_c];
        let desc_b = ComponentDesc::new(
            c"req_test::CompB".as_ptr(),
            ComponentUuid::from_parts("req_test::CompB", 4, 4, 0),
            4,
            4,
        )
        .with_required_components(reqs_b.as_ptr(), reqs_b.len());
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let rb = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut h_b) };
        assert_eq!(rb, FfiResult::OK);

        let reqs_a = [h_b];
        let desc_a = ComponentDesc::new(
            c"req_test::CompA".as_ptr(),
            ComponentUuid::from_parts("req_test::CompA", 4, 4, 0),
            4,
            4,
        )
        .with_required_components(reqs_a.as_ptr(), reqs_a.len());
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let ra = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut h_a) };
        assert_eq!(ra, FfiResult::OK);

        // Requiring an opaque host component without a default constructor must fail with PERMISSION_DENIED.
        let bad_reqs = [h_opaque];
        let desc_bad = ComponentDesc::new(
            c"req_test::BadReq".as_ptr(),
            ComponentUuid::from_parts("req_test::BadReq", 4, 4, 0),
            4,
            4,
        )
        .with_required_components(bad_reqs.as_ptr(), bad_reqs.len());
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let rbad = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_bad, &mut h_fail) };
        assert_eq!(rbad, FfiResult::PERMISSION_DENIED);

        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    assert_eq!(reg_after.required_components(h_a), &[h_b]);
    assert_eq!(reg_after.required_components(h_b), &[h_c]);

    let cid_a = reg_after.get_by_handle(h_a).unwrap().component_id;
    let cid_b = reg_after.get_by_handle(h_b).unwrap().component_id;
    let cid_c = reg_after.get_by_handle(h_c).unwrap().component_id;

    // 1. Inserting A automatically inserts transitive requirements B and C.
    let e1 = world.spawn_empty().id();
    let mut val_a: u32 = 7;
    // SAFETY: `cid_a` has 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e1).insert_by_id(
            cid_a,
            OwningPtr::new(core::ptr::NonNull::from(&mut val_a).cast()),
        );
    }
    assert!(world.entity(e1).contains_id(cid_a));
    assert!(world.entity(e1).contains_id(cid_b));
    assert!(world.entity(e1).contains_id(cid_c));
    let b_ptr = world.entity(e1).get_by_id(cid_b).unwrap().as_ptr();
    let c_ptr = world.entity(e1).get_by_id(cid_c).unwrap().as_ptr();
    // SAFETY: `cid_b` and `cid_c` are zero-initialized `u32`s.
    unsafe {
        assert_eq!(core::ptr::read_unaligned(b_ptr.cast::<u32>()), 0);
        assert_eq!(core::ptr::read_unaligned(c_ptr.cast::<u32>()), 0);
    }

    // 2. Pre-existing B(99) is preserved when A is inserted, while missing C is still inserted.
    let e2 = world.spawn_empty().id();
    let mut val_b: u32 = 99;
    // SAFETY: `cid_b` has 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e2).insert_by_id(
            cid_b,
            OwningPtr::new(core::ptr::NonNull::from(&mut val_b).cast()),
        );
        world.entity_mut(e2).insert_by_id(
            cid_a,
            OwningPtr::new(core::ptr::NonNull::from(&mut val_a).cast()),
        );
    }
    assert!(world.entity(e2).contains_id(cid_c));
    let e2_b_ptr = world.entity(e2).get_by_id(cid_b).unwrap().as_ptr();
    // SAFETY: `cid_b` holds 99.
    unsafe {
        assert_eq!(core::ptr::read_unaligned(e2_b_ptr.cast::<u32>()), 99);
    }
}

unsafe extern "C" fn test_drop_atomic_ptr(ptr: *mut c_void) {
    // SAFETY: `ptr` stores an aligned `*const AtomicUsize` pointing to a live counter.
    let counter_ptr = unsafe { core::ptr::read_unaligned(ptr.cast::<*const AtomicUsize>()) };
    if !counter_ptr.is_null() {
        // SAFETY: Test keeps the `Box<AtomicUsize>` alive for the test duration.
        unsafe { (*counter_ptr).fetch_add(1, Ordering::Relaxed) };
    }
}

unsafe extern "C" fn test_on_insert_atomic(
    _entity: FfiEntity,
    _data: *const c_void,
    user_data: *mut c_void,
) {
    if !user_data.is_null() {
        // SAFETY: `user_data` points to a live `AtomicUsize`.
        unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
    }
}

#[test]
fn drop_fn_plus_on_insert_active_and_disarmed_never_underflows_live_values() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let drop_count = Box::new(AtomicUsize::new(0));
    let insert_count = Box::new(AtomicUsize::new(0));
    let drop_ptr = &*drop_count as *const AtomicUsize;
    let insert_ptr = (&*insert_count as *const AtomicUsize)
        .cast_mut()
        .cast::<c_void>();

    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let mut handle = BamComponent::INVALID;

    let reg_after = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(21)),
        );
        let desc = ComponentDesc::new(
            c"reg_test::DropPlusInsert".as_ptr(),
            ComponentUuid::from_parts(
                "reg_test::DropPlusInsert",
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
                0,
            ),
            core::mem::size_of::<*const AtomicUsize>(),
            core::mem::align_of::<*const AtomicUsize>(),
        )
        .with_drop(test_drop_atomic_ptr)
        .with_on_insert(test_on_insert_atomic)
        .with_hook_user_data(insert_ptr);
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut handle) };
        assert_eq!(rc, FfiResult::OK);
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let meta = reg_after.get_by_handle(handle).unwrap();
    let cid = meta.component_id;
    let slot = meta.drop_slot.expect("drop_fn allocates a drop slot");
    assert_eq!(
        world
            .get_resource::<DynamicComponentHookMap>()
            .and_then(|m| m.drop_slot_by_id.get(&cid).copied()),
        Some(slot)
    );

    // 1. Active insert + despawn runs both `on_insert` and `drop_fn`.
    let e1 = world.spawn_empty().id();
    let mut payload1 = drop_ptr;
    // SAFETY: `cid` matches `*const AtomicUsize` layout.
    unsafe {
        world.entity_mut(e1).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut payload1).cast()),
        );
    }
    assert_eq!(insert_count.load(Ordering::Relaxed), 1);
    assert!(world.despawn(e1));
    assert_eq!(drop_count.load(Ordering::Relaxed), 1);

    // 2. Disarm `is_active` and insert/despawn again: callbacks do NOT run, and `live_values`
    // must still increment on insert so `drop_trampoline` does not underflow on despawn.
    active.store(false, Ordering::Release);
    let e2 = world.spawn_empty().id();
    let mut payload2 = drop_ptr;
    // SAFETY: `cid` matches `*const AtomicUsize` layout.
    unsafe {
        world.entity_mut(e2).insert_by_id(
            cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut payload2).cast()),
        );
    }
    assert_eq!(insert_count.load(Ordering::Relaxed), 1);
    assert!(world.despawn(e2));
    assert_eq!(drop_count.load(Ordering::Relaxed), 1);
    drop(world);
}

#[test]
fn clone_fn_supports_over_1024_components_and_failed_inits_in_one_world() {
    let mut world = World::new();
    let reg = ModRegistry::new();

    // 1. 60 failed/rolled-back inits with `clone_fn` + 1,100 live registrations in one World.
    let (rollback_iters, live_iters) = if cfg!(miri) { (4, 8) } else { (60, 1100) };
    for idx in 0..rollback_iters {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            Some(ModId(300 + idx)),
        );
        let name = format!("clone_cap_test::RolledBack_{idx}");
        let cname = CString::new(name.as_str()).unwrap();
        let mut h = BamComponent::INVALID;
        let desc = ComponentDesc::new(
            cname.as_ptr(),
            ComponentUuid::from_parts(&name, 4, 4, 0),
            4,
            4,
        )
        .with_clone_fn(test_clone_u32_doubled);
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) };
        assert_eq!(rc, FfiResult::OK);
        ctx.rollback();
    }

    let active = Arc::new(AtomicBool::new(true));
    let mut last_handle = BamComponent::INVALID;
    let reg_after = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            active,
            &mut world as *mut World,
            Some(ModId(400)),
        );
        for idx in 0..live_iters {
            let name = format!("clone_cap_test::Live_{idx}");
            let cname = CString::new(name.as_str()).unwrap();
            let mut h = BamComponent::INVALID;
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(&name, 4, 4, 0),
                4,
                4,
            )
            .with_clone_fn(test_clone_u32_doubled);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) };
            assert_eq!(rc, FfiResult::OK);
            last_handle = h;
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let cid_last = reg_after.get_by_handle(last_handle).unwrap().component_id;
    let e = world.spawn_empty().id();
    let mut val: u32 = 21;
    // SAFETY: `cid_last` has 4-byte `u32` layout.
    unsafe {
        world.entity_mut(e).insert_by_id(
            cid_last,
            OwningPtr::new(core::ptr::NonNull::from(&mut val).cast()),
        );
    }
    let cloned = world.entity_mut(e).clone_and_spawn();
    let cloned_ptr = world.entity(cloned).get_by_id(cid_last).unwrap();
    // SAFETY: `cloned_ptr` holds `u32` produced by `test_clone_u32_doubled`.
    let cloned_val = unsafe { core::ptr::read_unaligned(cloned_ptr.as_ptr().cast::<u32>()) };
    assert_eq!(cloned_val, 42);
}

#[test]
fn recycled_drop_slot_reregistration_allocates_fresh_component_id() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let drop_a_count = Box::new(AtomicUsize::new(0));
    let drop_b_count = Box::new(AtomicUsize::new(0));
    let drop_a_ptr = &*drop_a_count as *const AtomicUsize;
    let drop_b_ptr = &*drop_b_count as *const AtomicUsize;

    let mut world = World::new();
    let reg = ModRegistry::new();
    let active_a1 = Arc::new(AtomicBool::new(true));
    let mut h_a1 = BamComponent::INVALID;

    let reg_after_a1 = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active_a1),
            &mut world as *mut World,
            Some(ModId(501)),
        );
        let desc_a = ComponentDesc::new(
            c"recycle_test::CompA".as_ptr(),
            ComponentUuid::from_parts(
                "recycle_test::CompA",
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
                0,
            ),
            core::mem::size_of::<*const AtomicUsize>(),
            core::mem::align_of::<*const AtomicUsize>(),
        )
        .with_drop(test_drop_atomic_ptr);
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut h_a1) };
        assert_eq!(rc, FfiResult::OK);
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let meta_a1 = reg_after_a1.get_by_handle(h_a1).unwrap();
    let cid_a1 = meta_a1.component_id;
    let slot_a1 = meta_a1.drop_slot.unwrap();

    // Simulate Mod A unloading with 0 live values and its drop slot being recycled cleanly.
    active_a1.store(false, Ordering::Release);
    prune_drop_trampolines_for_guard(&active_a1);
    drop(world.remove_resource::<WorldDropGuard>());
    world.insert_resource(WorldDropGuard::new());

    // Mod B claims the recycled slot (`slot_a1`).
    let active_b = Arc::new(AtomicBool::new(true));
    let mut h_b = BamComponent::INVALID;
    let reg_after_b = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg_after_a1,
            active_b,
            &mut world as *mut World,
            Some(ModId(502)),
        );
        let desc_b = ComponentDesc::new(
            c"recycle_test::CompB".as_ptr(),
            ComponentUuid::from_parts(
                "recycle_test::CompB",
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
                0,
            ),
            core::mem::size_of::<*const AtomicUsize>(),
            core::mem::align_of::<*const AtomicUsize>(),
        )
        .with_drop(test_drop_atomic_ptr);
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut h_b) };
        assert_eq!(rc, FfiResult::OK);
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };
    let _slot_b = reg_after_b.get_by_handle(h_b).unwrap().drop_slot.unwrap();

    // Mod A reloads and gets a DIFFERENT drop slot than `slot_a1` (when Mod B took `slot_a1`).
    let active_a2 = Arc::new(AtomicBool::new(true));
    let mut h_a2 = BamComponent::INVALID;
    let reg_after_a2 = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg_after_b,
            active_a2,
            &mut world as *mut World,
            Some(ModId(503)),
        );
        let desc_a2 = ComponentDesc::new(
            c"recycle_test::CompA".as_ptr(),
            ComponentUuid::from_parts(
                "recycle_test::CompA",
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
                0,
            ),
            core::mem::size_of::<*const AtomicUsize>(),
            core::mem::align_of::<*const AtomicUsize>(),
        )
        .with_drop(test_drop_atomic_ptr);
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a2, &mut h_a2) };
        assert_eq!(rc, FfiResult::OK);
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    assert_eq!(h_a2, h_a1);
    let meta_a2 = reg_after_a2.get_by_handle(h_a2).unwrap();
    let slot_a2 = meta_a2.drop_slot.unwrap();
    assert_ne!(slot_a2, slot_a1);
    assert_ne!(
        meta_a2.component_id, cid_a1,
        "re-registering onto a different drop slot must allocate a fresh ComponentId"
    );
    assert_eq!(
        reg_after_a2.get_by_id(meta_a2.component_id).unwrap().handle,
        h_a2
    );
    assert!(reg_after_a2.get_by_id(cid_a1).is_none());

    let ea = world.spawn_empty().id();
    let mut pa = drop_a_ptr;
    let eb = world.spawn_empty().id();
    let mut pb = drop_b_ptr;
    let cid_b = reg_after_a2.get_by_handle(h_b).unwrap().component_id;
    // SAFETY: Layouts match `*const AtomicUsize`.
    unsafe {
        world.entity_mut(ea).insert_by_id(
            meta_a2.component_id,
            OwningPtr::new(core::ptr::NonNull::from(&mut pa).cast()),
        );
        world.entity_mut(eb).insert_by_id(
            cid_b,
            OwningPtr::new(core::ptr::NonNull::from(&mut pb).cast()),
        );
    }
    assert!(world.despawn(ea));
    assert!(world.despawn(eb));
    assert_eq!(drop_a_count.load(Ordering::Relaxed), 1);
    assert_eq!(drop_b_count.load(Ordering::Relaxed), 1);
}

#[test]
fn required_init_descs_custom_bytes_init_fn_uuid_and_disarm_under_miri() {
    use bevy_abi_mod_sys::{
        BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN,
        BAM_REQUIRED_FLAG_RESOLVE_BY_UUID, BamRequiredComponent,
    };

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Stats {
        hp: i32,
        armor: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Shield {
        cap: u32,
        regen: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct BaseBadge {
        rank: u64,
    }

    const STATS_UUID: ComponentUuid = ComponentUuid::from_parts("req::Stats", 8, 4, 0);
    const SHIELD_UUID: ComponentUuid = ComponentUuid::from_parts("req::Shield", 8, 4, 0);
    const BADGE_UUID: ComponentUuid = ComponentUuid::from_parts("req::BaseBadge", 8, 8, 0);
    const UNIT_UUID: ComponentUuid = ComponentUuid::from_parts("req::Unit", 0, 1, 0);

    struct InitCtx {
        calls: AtomicUsize,
        rank_value: u64,
    }

    unsafe extern "C" fn init_badge_fn(dst: *mut c_void, user_data: *mut c_void) -> FfiResult {
        // SAFETY: `user_data` points to `InitCtx` pinned on the test stack while `active` is true.
        let ctx = unsafe { &*user_data.cast::<InitCtx>() };
        ctx.calls.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `dst` is valid 8-byte aligned storage for `BaseBadge`.
        unsafe {
            core::ptr::write(
                dst.cast::<BaseBadge>(),
                BaseBadge {
                    rank: ctx.rank_value,
                },
            );
        }
        FfiResult::OK
    }

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));
    let init_ctx = InitCtx {
        calls: AtomicUsize::new(0),
        rank_value: 0xABCD_1234_5678_90EF,
    };

    let mut h_stats = BamComponent::INVALID;
    let mut h_shield = BamComponent::INVALID;
    let mut h_badge = BamComponent::INVALID;
    let mut h_unit = BamComponent::INVALID;

    let stats_custom = Stats { hp: 250, armor: 42 };
    let stats_bytes: [u8; 8] = {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&stats_custom.hp.to_ne_bytes());
        b[4..8].copy_from_slice(&stats_custom.armor.to_ne_bytes());
        b
    };
    let shield_custom = Shield {
        cap: 120,
        regen: 15,
    };
    let shield_bytes: [u8; 8] = {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&shield_custom.cap.to_ne_bytes());
        b[4..8].copy_from_slice(&shield_custom.regen.to_ne_bytes());
        b
    };

    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(601)),
        );

        let badge_desc = ComponentDesc::new(
            c"req::BaseBadge".as_ptr(),
            BADGE_UUID,
            core::mem::size_of::<BaseBadge>(),
            core::mem::align_of::<BaseBadge>(),
        );
        assert_eq!(
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &badge_desc, &mut h_badge) },
            FfiResult::OK
        );

        // `Shield` transitively requires `BaseBadge` via `INIT_FN` + `RESOLVE_BY_UUID`.
        let badge_req = BamRequiredComponent::by_uuid(BADGE_UUID).with_init_fn(
            init_badge_fn,
            core::mem::size_of::<BaseBadge>(),
            (&raw const init_ctx).cast_mut().cast::<c_void>(),
        );
        assert_eq!(
            badge_req.flags,
            BAM_REQUIRED_FLAG_RESOLVE_BY_UUID | BAM_REQUIRED_FLAG_INIT_FN
        );
        let shield_desc = ComponentDesc::new(
            c"req::Shield".as_ptr(),
            SHIELD_UUID,
            core::mem::size_of::<Shield>(),
            core::mem::align_of::<Shield>(),
        )
        .with_required_init_descs(&badge_req, 1);
        assert_eq!(
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &shield_desc, &mut h_shield) },
            FfiResult::OK
        );

        let stats_desc = ComponentDesc::new(
            c"req::Stats".as_ptr(),
            STATS_UUID,
            core::mem::size_of::<Stats>(),
            core::mem::align_of::<Stats>(),
        );
        assert_eq!(
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &stats_desc, &mut h_stats) },
            FfiResult::OK
        );

        // Mismatched `init_size` (4 != 8) must fail with `TYPE_MISMATCH`.
        let bad_bytes = [0u8; 4];
        let mut bad_req = BamRequiredComponent::by_handle(h_stats);
        bad_req.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        bad_req.init_bytes = bad_bytes.as_ptr();
        bad_req.init_size = bad_bytes.len();
        let bad_unit_desc = ComponentDesc::new(c"req::BadUnit".as_ptr(), UNIT_UUID, 0, 1)
            .with_required_init_descs(&bad_req, 1);
        let mut h_bad = BamComponent::INVALID;
        assert_eq!(
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &bad_unit_desc, &mut h_bad) },
            FfiResult::TYPE_MISMATCH
        );

        // `Unit` requires `Stats` (by handle + CUSTOM_BYTES) and `Shield` (by UUID + CUSTOM_BYTES).
        let mut req_stats = BamRequiredComponent::by_handle(h_stats);
        req_stats.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        req_stats.init_bytes = stats_bytes.as_ptr();
        req_stats.init_size = stats_bytes.len();

        let mut req_shield = BamRequiredComponent::by_uuid(SHIELD_UUID);
        req_shield.flags |= BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        req_shield.init_bytes = shield_bytes.as_ptr();
        req_shield.init_size = shield_bytes.len();

        let unit_reqs = [req_stats, req_shield];
        let unit_desc = ComponentDesc::new(c"req::Unit".as_ptr(), UNIT_UUID, 0, 1)
            .with_required_init_descs(unit_reqs.as_ptr(), unit_reqs.len());
        assert_eq!(
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &unit_desc, &mut h_unit) },
            FfiResult::OK
        );

        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let cid_stats = reg.get_by_handle(h_stats).unwrap().component_id;
    let cid_shield = reg.get_by_handle(h_shield).unwrap().component_id;
    let cid_badge = reg.get_by_handle(h_badge).unwrap().component_id;
    let cid_unit = reg.get_by_handle(h_unit).unwrap().component_id;

    // Spawn entity and insert `Unit`: must materialize `Stats`, `Shield`, and transitively `BaseBadge`!
    let e1 = world.spawn_empty().id();
    // SAFETY: `cid_unit` is a ZST component registered in `world`.
    unsafe {
        world
            .entity_mut(e1)
            .insert_by_id(cid_unit, OwningPtr::new(core::ptr::NonNull::dangling()));
    }
    assert_eq!(init_ctx.calls.load(Ordering::SeqCst), 1);
    // SAFETY: Components hold initialized POD values matching their registered layouts.
    unsafe {
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(cid_stats)
                .unwrap()
                .deref::<Stats>(),
            stats_custom
        );
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(cid_shield)
                .unwrap()
                .deref::<Shield>(),
            shield_custom
        );
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(cid_badge)
                .unwrap()
                .deref::<BaseBadge>(),
            BaseBadge {
                rank: 0xABCD_1234_5678_90EF
            }
        );
    }

    // Existing `Stats` must NOT be overwritten when `Unit` is added!
    let e2 = world.spawn_empty().id();
    let existing_stats = Stats { hp: 999, armor: 7 };
    // SAFETY: `cid_stats` and `cid_unit` match registered layouts.
    unsafe {
        OwningPtr::make(existing_stats, |p| {
            world.entity_mut(e2).insert_by_id(cid_stats, p);
        });
        world
            .entity_mut(e2)
            .insert_by_id(cid_unit, OwningPtr::new(core::ptr::NonNull::dangling()));
    }
    assert_eq!(init_ctx.calls.load(Ordering::SeqCst), 2);
    // SAFETY: `cid_stats` holds `Stats`.
    unsafe {
        assert_eq!(
            *world
                .entity(e2)
                .get_by_id(cid_stats)
                .unwrap()
                .deref::<Stats>(),
            existing_stats
        );
    }

    // Disarm the mod (`is_active = false`): inserting `Shield` must NOT call `init_badge_fn`!
    active.store(false, Ordering::SeqCst);
    let e3 = world.spawn_empty().id();
    // SAFETY: `cid_shield` matches `Shield` layout.
    unsafe {
        OwningPtr::make(shield_custom, |p| {
            world.entity_mut(e3).insert_by_id(cid_shield, p);
        });
    }
    assert_eq!(
        init_ctx.calls.load(Ordering::SeqCst),
        2,
        "disarmed mod tombstone must suppress `init_fn` after unload"
    );
    assert!(world.entity(e3).get_by_id(cid_badge).is_err());
}

#[test]
fn dynamic_rel_bind_opt_in_allow_self_referential_flag_and_default_false() {
    use std::ffi::CString;

    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));

    let mut rel_default = BamComponent::INVALID;
    let mut tgt_default = BamComponent::INVALID;
    let mut rel_self = BamComponent::INVALID;
    let mut tgt_self = BamComponent::INVALID;
    let mut rel_self_prefix = BamComponent::INVALID;
    let mut tgt_self_prefix = BamComponent::INVALID;

    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(77)),
        );
        for (name, size, align, out) in [
            ("rel_test::DefaultRel", 8, 8, &mut rel_default),
            ("rel_test::DefaultTgt", 0, 1, &mut tgt_default),
            ("rel_test::SelfRel", 8, 8, &mut rel_self),
            ("rel_test::SelfTgt", 0, 1, &mut tgt_self),
            ("rel_test::SelfPrefixRel", 8, 8, &mut rel_self_prefix),
            ("rel_test::SelfPrefixTgt", 0, 1, &mut tgt_self_prefix),
        ] {
            let cname = CString::new(name).unwrap();
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(name, size, align, 0),
                size,
                align,
            );
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let res = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, out) };
            assert_eq!(res, FfiResult::OK);
        }

        // Default `__bam_rel_bind::<rel>::<tgt>` has `allow_self_referential = false`.
        let bind_default = CString::new(format!(
            "__bam_rel_bind::{}::{}",
            rel_default.0, tgt_default.0
        ))
        .unwrap();
        let bind_default_desc = ComponentDesc::new(
            bind_default.as_ptr(),
            ComponentUuid::from_parts(bind_default.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        let mut bound_out = BamComponent::INVALID;
        // SAFETY: Valid bind descriptor.
        let res = unsafe {
            host_register_component(ctx.as_ffi_ptr(), &bind_default_desc, &mut bound_out)
        };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(bound_out, tgt_default);

        // Conflicting re-bind with `::self` on already-bound non-self pair must fail `TYPE_MISMATCH`.
        let conflict_bind = CString::new(format!(
            "__bam_rel_bind::{}::{}::self",
            rel_default.0, tgt_default.0
        ))
        .unwrap();
        let conflict_desc = ComponentDesc::new(
            conflict_bind.as_ptr(),
            ComponentUuid::from_parts(conflict_bind.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid bind descriptor testing policy mismatch.
        let res =
            unsafe { host_register_component(ctx.as_ffi_ptr(), &conflict_desc, &mut bound_out) };
        assert_eq!(res, FfiResult::TYPE_MISMATCH);

        // Opt-in via `::self` suffix (`__bam_rel_bind::<rel>::<tgt>::self`).
        let bind_self = CString::new(format!(
            "__bam_rel_bind::{}::{}::self",
            rel_self.0, tgt_self.0
        ))
        .unwrap();
        let bind_self_desc = ComponentDesc::new(
            bind_self.as_ptr(),
            ComponentUuid::from_parts(bind_self.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid self-referential bind descriptor.
        let res =
            unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_self_desc, &mut bound_out) };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(bound_out, tgt_self);

        // Idempotent re-bind with same `::self` policy succeeds.
        // SAFETY: Valid idempotent re-bind.
        let res =
            unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_self_desc, &mut bound_out) };
        assert_eq!(res, FfiResult::OK);

        // Opt-in via `__bam_rel_bind_self::<rel>::<tgt>` prefix.
        let bind_prefix = CString::new(format!(
            "__bam_rel_bind_self::{}::{}",
            rel_self_prefix.0, tgt_self_prefix.0
        ))
        .unwrap();
        let bind_prefix_desc = ComponentDesc::new(
            bind_prefix.as_ptr(),
            ComponentUuid::from_parts(bind_prefix.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid self-referential bind descriptor.
        let res =
            unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_prefix_desc, &mut bound_out) };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(bound_out, tgt_self_prefix);

        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let rel_def_cid = reg.get_by_handle(rel_default).unwrap().component_id;
    let tgt_def_cid = reg.get_by_handle(tgt_default).unwrap().component_id;
    let rel_self_cid = reg.get_by_handle(rel_self).unwrap().component_id;
    let tgt_self_cid = reg.get_by_handle(tgt_self).unwrap().component_id;
    let rel_pref_cid = reg.get_by_handle(rel_self_prefix).unwrap().component_id;
    let tgt_pref_cid = reg.get_by_handle(tgt_self_prefix).unwrap().component_id;

    // 1. Default binding (`allow_self_referential = false`) rejects self-referential insert.
    let e_def = world.spawn_empty().id();
    let e_def_ffi = FfiEntity::from_bits(e_def.to_bits());
    // SAFETY: `rel_def_cid` has 8-byte `FfiEntity` layout.
    unsafe {
        OwningPtr::make(e_def_ffi, |p| {
            world.entity_mut(e_def).insert_by_id(rel_def_cid, p);
        });
    }
    world.flush();
    assert!(!world.entity(e_def).contains_id(rel_def_cid));
    assert!(!world.entity(e_def).contains_id(tgt_def_cid));

    // 2. Opt-in `::self` binding permits `e -> e`, attaches target marker, cleans up on remove,
    //    and despawns cleanly when still self-referencing.
    let e_self = world.spawn_empty().id();
    let e_self_ffi = FfiEntity::from_bits(e_self.to_bits());
    // SAFETY: `rel_self_cid` has 8-byte `FfiEntity` layout.
    unsafe {
        OwningPtr::make(e_self_ffi, |p| {
            world.entity_mut(e_self).insert_by_id(rel_self_cid, p);
        });
    }
    world.flush();
    assert!(world.entity(e_self).contains_id(rel_self_cid));
    assert!(world.entity(e_self).contains_id(tgt_self_cid));
    assert_eq!(
        world
            .resource::<DynamicComponentHookMap>()
            .relationship_targets_for(tgt_self_cid, e_self),
        &[e_self_ffi]
    );

    // Removing the self-referential source cleans up the target marker on `e_self`.
    world.entity_mut(e_self).remove_by_id(rel_self_cid);
    world.flush();
    assert!(!world.entity(e_self).contains_id(tgt_self_cid));
    assert!(
        world
            .resource::<DynamicComponentHookMap>()
            .relationship_targets_for(tgt_self_cid, e_self)
            .is_empty()
    );

    // Re-insert via `__bam_rel_bind_self::` and despawn `e_self` while self-linked.
    // SAFETY: `rel_pref_cid` has 8-byte `FfiEntity` layout.
    unsafe {
        OwningPtr::make(e_self_ffi, |p| {
            world.entity_mut(e_self).insert_by_id(rel_pref_cid, p);
        });
    }
    world.flush();
    assert!(world.entity(e_self).contains_id(tgt_pref_cid));
    assert!(world.despawn(e_self));
    world.flush();
    assert!(
        world
            .resource::<DynamicComponentHookMap>()
            .relationship_targets_for(tgt_pref_cid, e_self)
            .is_empty()
    );
}

#[test]
fn summary_tick_dynamic_and_typed_options_flow_into_iter_column_info() {
    use crate::ffi::iter::{HostIter, QueryPlan, QueryTermMeta, host_iter_column_info};
    use crate::registry::ComponentOptions;
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{AccessMode, BAM_STORAGE_FLAG_SUMMARY_TICK, BamColumnInfo};
    use bevy_ecs::component::Component;

    #[derive(Component, Clone, Copy, Debug, ModComponent)]
    #[component(summary_tick)]
    #[mod_component(name = "sum_test::HostSummaryComp")]
    #[repr(C)]
    struct HostSummaryComp(u32);

    #[derive(Component, Clone, Copy, Debug, ModComponent)]
    #[mod_component(name = "sum_test::HostPlainComp")]
    #[repr(C)]
    struct HostPlainComp(u32);

    // Typed `ComponentOptions::<T>::new()` inherits `T::HAS_SUMMARY_TICK` automatically,
    // and `.with_summary_tick()` / `.summary_tick()` set the flag explicitly.
    let opts_auto = ComponentOptions::<HostSummaryComp>::new();
    assert_ne!(
        opts_auto.extra_storage_flags() & BAM_STORAGE_FLAG_SUMMARY_TICK,
        0
    );
    let opts_plain = ComponentOptions::<HostPlainComp>::new();
    assert_eq!(
        opts_plain.extra_storage_flags() & BAM_STORAGE_FLAG_SUMMARY_TICK,
        0
    );
    let opts_explicit = ComponentOptions::<HostPlainComp>::new().with_summary_tick();
    assert_ne!(
        opts_explicit.extra_storage_flags() & BAM_STORAGE_FLAG_SUMMARY_TICK,
        0
    );
    assert_eq!(
        opts_explicit.extra_storage_flags(),
        ComponentOptions::<HostPlainComp>::new()
            .summary_tick()
            .extra_storage_flags()
    );

    // Dynamic registration with `BAM_COMPONENT_FLAG_SUMMARY_TICK` sets `BAM_STORAGE_FLAG_SUMMARY_TICK`
    // in `ModRegistry`, Bevy `ComponentInfo::summary_tick()`, and `host_iter_column_info`.
    let mut world = World::new();
    let mut reg = ModRegistry::new();
    let host_cid = world.register_component::<HostSummaryComp>();
    let host_h = reg.register_component(host_cid, opts_auto).unwrap();
    assert!(reg.get_by_handle(host_h).unwrap().is_summary_tick());

    let mut dyn_h = BamComponent::INVALID;
    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            Some(ModId(88)),
        );
        let desc = ComponentDesc::new(
            c"sum_test::DynSummary".as_ptr(),
            ComponentUuid::from_parts("sum_test::DynSummary", 4, 4, 0),
            4,
            4,
        )
        .with_summary_tick();
        // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
        let res = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut dyn_h) };
        assert_eq!(res, FfiResult::OK);
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let meta = reg.get_by_handle(dyn_h).unwrap();
    assert!(meta.is_summary_tick());
    assert_ne!(meta.storage_flags & BAM_STORAGE_FLAG_SUMMARY_TICK, 0);
    assert!(
        world
            .components()
            .get_info(meta.component_id)
            .unwrap()
            .summary_tick()
    );

    let plan = QueryPlan::from_terms(vec![QueryTermMeta {
        component: dyn_h,
        component_id: meta.component_id,
        mode: AccessMode::READ,
        uuid: meta.uuid,
        size: meta.size,
        align: meta.align,
        storage_flags: meta.storage_flags,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);
    let tick = world.change_tick();
    let mut iter = HostIter::new_dense(&world.storages().tables, &[], &plan, tick, tick);
    let mut col_info = BamColumnInfo {
        component: BamComponent::INVALID,
        mode: AccessMode::READ,
        uuid: ComponentUuid::ZERO,
        size: 0,
        align: 0,
        storage_flags: 0,
        reserved0: 0,
    };
    // SAFETY: Valid `HostIter` and aligned writable `BamColumnInfo`.
    let res = unsafe { host_iter_column_info(iter.as_ffi_ptr().cast_const(), 0, &mut col_info) };
    assert_eq!(res, FfiResult::OK);
    assert_ne!(col_info.storage_flags & BAM_STORAGE_FLAG_SUMMARY_TICK, 0);
}
