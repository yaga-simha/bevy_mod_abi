//! Foreign drop-trampoline pool conservation, saturation, slot reuse on reload,
//! paired active-guard fallback across multiple mods, and panic containment.

use bevy_abi_mod_host::ModRegistry;
use bevy_abi_mod_host_sys::{
    CommandQueueBridge, MAX_DROP_TRAMPOLINES, ModInitContext, host_cmd_insert_component,
    host_cmd_spawn_empty, host_register_component, reset_drop_trampolines_for_testing,
};
use bevy_abi_mod_sys::{ComponentUuid, FfiResult, ModComponentDescriptor, RawCommandQueue};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Serializes tests that saturate or reset the process-global `DROP_TRAMPOLINE_SLOTS` pool.
static DROP_TRAMPOLINE_LOCK: Mutex<()> = Mutex::new(());

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PodPowerData {
    power: f32,
    shield: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PodManaData {
    mana: f32,
    level: i32,
}

static FOREIGN_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRAMPOLINE_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static NON_POD_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static PANICKING_DROP_INVOKED: AtomicBool = AtomicBool::new(false);
static CROSS_TALK_DROP_A: AtomicUsize = AtomicUsize::new(0);
static CROSS_TALK_DROP_B: AtomicUsize = AtomicUsize::new(0);
static MOD1_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static MOD2_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn custom_foreign_drop_hook(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    FOREIGN_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn generic_drop_hook(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    TRAMPOLINE_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn non_pod_drop_hook(ptr: *mut c_void) {
    if !ptr.is_null() {
        NON_POD_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe extern "C" fn panicking_drop_trampoline_with_catch_unwind(ptr: *mut c_void) {
    if !ptr.is_null() {
        PANICKING_DROP_INVOKED.store(true, Ordering::SeqCst);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("Simulated non-POD drop hook panic in guest runtime");
        }));
    }
}

unsafe extern "C" fn foreign_drop_hook_mod_a(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    CROSS_TALK_DROP_A.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn foreign_drop_hook_mod_b(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    CROSS_TALK_DROP_B.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn mod1_drop_fn(_ptr: *mut c_void) {
    MOD1_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn mod2_drop_fn(_ptr: *mut c_void) {
    MOD2_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn test_dynamic_foreign_drop_trampoline_and_unload_safety() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let name = c"DynamicDroppable";
    let uuid = ComponentUuid::from_name("DynamicDroppable");
    let desc =
        ModComponentDescriptor::new(name.as_ptr(), uuid, 8, 4).with_drop(custom_foreign_drop_hook);

    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let e1 = unsafe { host_cmd_spawn_empty(raw_queue) };
    let e2 = unsafe { host_cmd_spawn_empty(raw_queue) };
    let dummy_payload = [42u32, 84u32];

    unsafe {
        host_cmd_insert_component(
            raw_queue,
            e1,
            name.as_ptr(),
            dummy_payload.as_ptr() as *const u8,
            8,
        );
        host_cmd_insert_component(
            raw_queue,
            e2,
            name.as_ptr(),
            dummy_payload.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    // 1. Despawn entity 1 while mod is ACTIVE: foreign drop hook MUST execute
    let initial_drops = FOREIGN_DROP_COUNT.load(Ordering::SeqCst);
    world.despawn(Entity::from_bits(e1.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Custom foreign drop hook must be called on despawn while active"
    );

    // 2. Simulate mod unload by setting atomic tombstone to false
    is_active.store(false, Ordering::Release);

    // 3. Despawn entity 2 while mod is INACTIVE: foreign drop hook MUST be safely skipped
    world.despawn(Entity::from_bits(e2.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Foreign drop hook must be safely skipped after mod deactivation without crashing"
    );

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_drop_trampoline_saturation_and_reclamation() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active_1 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_1 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_1),
        &mut world as *mut World,
    );
    let _guard_1 = init_ctx_1.make_active();

    let mut names = Vec::new();
    for i in 0..MAX_DROP_TRAMPOLINES {
        names.push(format!("DroppableComp_{i}\0"));
    }

    for (i, name_str) in names.iter().enumerate() {
        let uuid = ComponentUuid::from_name(&format!("DroppableComp_{i}"));
        let desc = ModComponentDescriptor::new(name_str.as_ptr().cast(), uuid, 8, 4)
            .with_drop(generic_drop_hook);

        let res = unsafe {
            host_register_component(&mut init_ctx_1 as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(res, FfiResult::OK, "Slot {i} should be allocated");
    }

    let extra_desc = ModComponentDescriptor::new(
        c"DroppableComp_Overflow".as_ptr(),
        ComponentUuid::from_name("DroppableComp_Overflow"),
        8,
        4,
    )
    .with_drop(generic_drop_hook);

    let res_overflow = unsafe {
        host_register_component(
            &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
            &extra_desc,
        )
    };
    assert_eq!(res_overflow, FfiResult::CAPACITY_EXCEEDED);

    // POD component (drop_fn == None) MUST still succeed
    let pod_desc = ModComponentDescriptor::new(
        c"PodComp_AfterSaturation".as_ptr(),
        ComponentUuid::from_name("PodComp_AfterSaturation"),
        8,
        4,
    );
    let res_pod = unsafe {
        host_register_component(
            &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
            &pod_desc,
        )
    };
    assert_eq!(res_pod, FfiResult::OK);
    drop(_guard_1);

    // Simulate mod 1 unload
    is_active_1.store(false, Ordering::Release);

    // Alien UUID cannot steal slots when pool is saturated
    let is_active_2 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_2 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_2),
        &mut world as *mut World,
    );
    let _guard_2 = init_ctx_2.make_active();

    let reclaim_desc = ModComponentDescriptor::new(
        c"ReclaimedDroppable".as_ptr(),
        ComponentUuid::from_name("ReclaimedDroppable"),
        8,
        4,
    )
    .with_drop(generic_drop_hook);

    let res_reclaim = unsafe {
        host_register_component(
            &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
            &reclaim_desc,
        )
    };
    assert_eq!(res_reclaim, FfiResult::CAPACITY_EXCEEDED);

    // Re-registering the same ComponentUuid (mod reload) succeeds by rebinding the slot
    let same_desc = ModComponentDescriptor::new(
        c"DroppableComp_0".as_ptr(),
        ComponentUuid::from_name("DroppableComp_0"),
        8,
        4,
    )
    .with_drop(generic_drop_hook);
    let res_same = unsafe {
        host_register_component(
            &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
            &same_desc,
        )
    };
    assert_eq!(res_same, FfiResult::OK);

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_drop_trampoline_slot_reuse_cross_talk() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active_base = Arc::new(AtomicBool::new(true));
    let mut init_ctx_base = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_base),
        &mut world as *mut World,
    );

    let is_active_a = Arc::new(AtomicBool::new(true));
    let mut init_ctx_a = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_a),
        &mut world as *mut World,
    );
    let desc_a = ModComponentDescriptor::new(
        c"CompA_SlotTest".as_ptr(),
        ComponentUuid::from_name("CompA_SlotTest"),
        8,
        4,
    )
    .with_drop(foreign_drop_hook_mod_a);
    {
        let _guard_a = init_ctx_a.make_active();
        assert_eq!(
            unsafe {
                host_register_component(
                    &mut init_ctx_a as *mut ModInitContext as *mut c_void,
                    &desc_a,
                )
            },
            FfiResult::OK
        );
    }

    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
    let e_a = unsafe { host_cmd_spawn_empty(raw_queue) };
    let payload_a = [0x1111u32, 0x2222u32];
    unsafe {
        host_cmd_insert_component(
            raw_queue,
            e_a,
            c"CompA_SlotTest".as_ptr(),
            payload_a.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    // Fill remaining slots up to MAX_DROP_TRAMPOLINES
    let mut dummy_names = Vec::new();
    for i in 0..(MAX_DROP_TRAMPOLINES - 1) {
        dummy_names.push(format!("DummyDropSlotComp_{i}\0"));
    }
    {
        let _guard_fill = init_ctx_base.make_active();
        for name_str in &dummy_names {
            let desc = ModComponentDescriptor::new(
                name_str.as_ptr().cast(),
                ComponentUuid::from_name(name_str),
                8,
                4,
            )
            .with_drop(foreign_drop_hook_mod_a);
            let _ = unsafe {
                host_register_component(
                    &mut init_ctx_base as *mut ModInitContext as *mut c_void,
                    &desc,
                )
            };
        }
    }

    // Unload Mod A
    is_active_a.store(false, Ordering::Release);

    // Mod B attempts to register a component with drop_fn_b; rejected with CAPACITY_EXCEEDED
    let is_active_b = Arc::new(AtomicBool::new(true));
    let mut init_ctx_b = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_b),
        &mut world as *mut World,
    );
    let desc_b = ModComponentDescriptor::new(
        c"CompB_ReusedSlot".as_ptr(),
        ComponentUuid::from_name("CompB_ReusedSlot"),
        8,
        4,
    )
    .with_drop(foreign_drop_hook_mod_b);
    {
        let _guard_b = init_ctx_b.make_active();
        assert_eq!(
            unsafe {
                host_register_component(
                    &mut init_ctx_b as *mut ModInitContext as *mut c_void,
                    &desc_b,
                )
            },
            FfiResult::CAPACITY_EXCEEDED
        );
    }

    CROSS_TALK_DROP_A.store(0, Ordering::SeqCst);
    CROSS_TALK_DROP_B.store(0, Ordering::SeqCst);
    world.despawn(Entity::from_bits(e_a.0));
    assert_eq!(CROSS_TALK_DROP_A.load(Ordering::SeqCst), 0);
    assert_eq!(CROSS_TALK_DROP_B.load(Ordering::SeqCst), 0);

    // Legitimate mod reload with same UUID succeeds and rebinds trampoline slot
    let is_active_a_reload = Arc::new(AtomicBool::new(true));
    let mut init_ctx_a_reload = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_a_reload),
        &mut world as *mut World,
    );
    {
        let _guard_a_reload = init_ctx_a_reload.make_active();
        assert_eq!(
            unsafe {
                host_register_component(
                    &mut init_ctx_a_reload as *mut ModInitContext as *mut c_void,
                    &desc_a,
                )
            },
            FfiResult::OK
        );
    }

    let mut bridge_r =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue_r = (&mut bridge_r as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
    let e_r = unsafe { host_cmd_spawn_empty(raw_queue_r) };
    unsafe {
        host_cmd_insert_component(
            raw_queue_r,
            e_r,
            c"CompA_SlotTest".as_ptr(),
            payload_a.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    CROSS_TALK_DROP_A.store(0, Ordering::SeqCst);
    world.despawn(Entity::from_bits(e_r.0));
    assert_eq!(CROSS_TALK_DROP_A.load(Ordering::SeqCst), 1);

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_pod_components_consume_zero_drop_trampoline_slots() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc_rust = ModComponentDescriptor::new(
        c"PodPowerData".as_ptr(),
        ComponentUuid::from_name("PodPowerData"),
        core::mem::size_of::<PodPowerData>(),
        core::mem::align_of::<PodPowerData>(),
    );
    assert_eq!(
        unsafe {
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_rust,
            )
        },
        FfiResult::OK
    );

    let desc_c = ModComponentDescriptor::new(
        c"PodManaData".as_ptr(),
        ComponentUuid::from_name("PodManaData"),
        core::mem::size_of::<PodManaData>(),
        core::mem::align_of::<PodManaData>(),
    );
    assert_eq!(
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_c)
        },
        FfiResult::OK
    );

    let mut names = Vec::new();
    for i in 0..100 {
        names.push(format!("PodComponent_{i}\0"));
    }
    for name in &names {
        let desc = ModComponentDescriptor::new(
            name.as_ptr().cast(),
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            8,
            4,
        );
        assert_eq!(
            unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            },
            FfiResult::OK
        );
    }

    let mut non_pod_names = Vec::new();
    for i in 0..MAX_DROP_TRAMPOLINES {
        non_pod_names.push(format!("NonPodComp_{i}\0"));
    }
    for name in &non_pod_names {
        let desc = ModComponentDescriptor::new(
            name.as_ptr().cast(),
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            16,
            8,
        )
        .with_drop(non_pod_drop_hook);
        assert_eq!(
            unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            },
            FfiResult::OK
        );
    }

    let desc_overflow = ModComponentDescriptor::new(
        c"NonPodComp_Overflow".as_ptr(),
        ComponentUuid::from_name("NonPodComp_Overflow"),
        16,
        8,
    )
    .with_drop(non_pod_drop_hook);
    assert_eq!(
        unsafe {
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_overflow,
            )
        },
        FfiResult::CAPACITY_EXCEEDED
    );

    let mut more_pod_names = Vec::new();
    for i in 100..110 {
        more_pod_names.push(format!("PodComponent_{i}\0"));
    }
    for name in &more_pod_names {
        let desc = ModComponentDescriptor::new(
            name.as_ptr().cast(),
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            8,
            4,
        );
        assert_eq!(
            unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            },
            FfiResult::OK
        );
    }

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_non_pod_panic_containment_in_drop_trampoline() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();
    PANICKING_DROP_INVOKED.store(false, Ordering::SeqCst);

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active),
        &mut world as *mut World,
    );
    let _guard = init_ctx.make_active();

    let desc = ModComponentDescriptor::new(
        c"PanickingNonPod".as_ptr(),
        ComponentUuid::from_name("PanickingNonPod"),
        8,
        4,
    )
    .with_drop(panicking_drop_trampoline_with_catch_unwind);

    assert_eq!(
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        },
        FfiResult::OK
    );

    let reg = world.resource::<ModRegistry>().clone();
    let comp_id = reg.get_by_name("PanickingNonPod").unwrap().component_id;

    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
    let val: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"PanickingNonPod".as_ptr(),
                val.as_ptr(),
                val.len(),
            )
        },
        FfiResult::OK
    );

    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).expect("Valid entity");
    assert!(world.entity(bevy_entity).contains_id(comp_id));

    world.despawn(bevy_entity);
    assert!(PANICKING_DROP_INVOKED.load(Ordering::SeqCst));

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_non_pod_slot_reuse_on_mod_reload() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();

    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    let is_active_1 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_1 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_1),
        &mut world as *mut World,
    );
    let _guard_1 = init_ctx_1.make_active();

    let uuid_shared = ComponentUuid::from_name("ReloadableComp");
    let desc_1 = ModComponentDescriptor::new(c"ReloadableComp".as_ptr(), uuid_shared, 16, 8)
        .with_drop(non_pod_drop_hook);
    assert_eq!(
        unsafe {
            host_register_component(
                &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
                &desc_1,
            )
        },
        FfiResult::OK
    );

    let mut names = Vec::new();
    for i in 1..MAX_DROP_TRAMPOLINES {
        names.push(format!("SlotComp_{i}\0"));
    }
    for name in &names {
        let desc = ModComponentDescriptor::new(
            name.as_ptr().cast(),
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            16,
            8,
        )
        .with_drop(non_pod_drop_hook);
        assert_eq!(
            unsafe {
                host_register_component(
                    &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
                    &desc,
                )
            },
            FfiResult::OK
        );
    }
    drop(_guard_1);

    is_active_1.store(false, Ordering::Release);

    let is_active_2 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_2 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_2),
        &mut world as *mut World,
    );
    let _guard_2 = init_ctx_2.make_active();

    let desc_reload = ModComponentDescriptor::new(c"ReloadableComp".as_ptr(), uuid_shared, 16, 8)
        .with_drop(non_pod_drop_hook);
    assert_eq!(
        unsafe {
            host_register_component(
                &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
                &desc_reload,
            )
        },
        FfiResult::OK
    );

    reset_drop_trampolines_for_testing();
}

#[test]
fn partial_unload_of_second_mod_never_invokes_unmapped_drop_fn() {
    let _lock = DROP_TRAMPOLINE_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();
    MOD1_DROP_COUNT.store(0, Ordering::SeqCst);
    MOD2_DROP_COUNT.store(0, Ordering::SeqCst);

    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    world.insert_resource((*reg).clone());

    let uuid = ComponentUuid::from_name("SharedMultiModDroppable");

    let active_1 = Arc::new(AtomicBool::new(true));
    {
        let mut ctx1 = ModInitContext::with_world(
            Arc::clone(&reg),
            Arc::clone(&active_1),
            &mut world as *mut World,
        );
        let _g1 = ctx1.make_active();
        let desc1 = ModComponentDescriptor::new(c"SharedMultiModDroppable".as_ptr(), uuid, 8, 8)
            .with_drop(mod1_drop_fn);
        assert_eq!(
            unsafe {
                host_register_component(&mut ctx1 as *mut ModInitContext as *mut c_void, &desc1)
            },
            FfiResult::OK
        );
    }

    let active_2 = Arc::new(AtomicBool::new(true));
    {
        let world_reg = Arc::new(world.resource::<ModRegistry>().clone());
        let mut ctx2 =
            ModInitContext::with_world(world_reg, Arc::clone(&active_2), &mut world as *mut World);
        let _g2 = ctx2.make_active();
        let desc2 = ModComponentDescriptor::new(c"SharedMultiModDroppable".as_ptr(), uuid, 8, 8)
            .with_drop(mod2_drop_fn);
        assert_eq!(
            unsafe {
                host_register_component(&mut ctx2 as *mut ModInitContext as *mut c_void, &desc2)
            },
            FfiResult::OK
        );
    }

    let reg_snap = world.resource::<ModRegistry>().clone();
    let mut queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg_snap);
    let raw_q = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
    let payload = 42u64.to_ne_bytes();
    let e1 = unsafe { host_cmd_spawn_empty(raw_q) };
    let e2 = unsafe { host_cmd_spawn_empty(raw_q) };
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_q,
                e1,
                c"SharedMultiModDroppable".as_ptr(),
                payload.as_ptr(),
                8,
            )
        },
        FfiResult::OK
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_q,
                e2,
                c"SharedMultiModDroppable".as_ptr(),
                payload.as_ptr(),
                8,
            )
        },
        FfiResult::OK
    );
    queue.apply(&mut world);

    // Unload Mod 2 while Mod 1 remains loaded
    active_2.store(false, Ordering::Release);

    world.despawn(Entity::try_from_bits(e1.to_bits()).unwrap());
    assert_eq!(MOD2_DROP_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(MOD1_DROP_COUNT.load(Ordering::SeqCst), 1);

    // Unload Mod 1 as well; despawning `e2` invokes neither
    active_1.store(false, Ordering::Release);
    world.despawn(Entity::try_from_bits(e2.to_bits()).unwrap());
    assert_eq!(MOD2_DROP_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(MOD1_DROP_COUNT.load(Ordering::SeqCst), 1);

    reset_drop_trampolines_for_testing();
}
