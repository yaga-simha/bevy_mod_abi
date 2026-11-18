//! Foreign drop-trampoline pool conservation, saturation, `WorldDropGuard` reclamation,
//! slot reuse on reload, paired active-guard fallback across multiple mods, and panic containment.

use crate::DROP_SLOT_POOL_RWLOCK;
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::drop_trampoline::available_drop_trampoline_slots;
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, host_cmd_insert, host_cmd_spawn_empty,
    host_register_component,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::ffi::CString;
use std::sync::Arc;

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

fn reg_comp(ctx: &mut LoadContext, desc: &ComponentDesc, out: &mut BamComponent) -> FfiResult {
    // SAFETY: `ctx` is an active stack-owned `LoadContext`, `desc` has valid C-string name and
    // layout fields, and `out` is a writable stack `BamComponent`.
    unsafe { host_register_component(ctx.as_ffi_ptr(), desc, out) }
}

fn spawn_and_insert_via_host_commands(
    world: &mut World,
    comp: BamComponent,
    payload: &[u8],
) -> FfiEntity {
    let snap = world.resource::<ModRegistry>().snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let mut ent = FfiEntity::NULL;
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let raw_cmds = cmds.as_ffi_ptr();
        // SAFETY: Valid `raw_cmds` and writable `ent`.
        let spawn_res = unsafe { host_cmd_spawn_empty(raw_cmds, &mut ent) };
        assert_eq!(spawn_res, FfiResult::OK);
        // SAFETY: Valid `raw_cmds`, reserved `ent`, valid `comp`, and readable `payload`.
        let insert_res =
            unsafe { host_cmd_insert(raw_cmds, ent, comp, payload.as_ptr(), payload.len()) };
        assert_eq!(insert_res, FfiResult::OK);
    }
    storage.apply_to_world(world, &snap);
    ent
}

#[test]
fn test_dynamic_foreign_drop_trampoline_and_unload_safety() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());

    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        registry,
        Arc::clone(&is_active),
        &mut world as *mut World,
        None,
    );

    let name = c"drop_test::DynamicDroppable";
    let uuid = ComponentUuid::from_parts("drop_test::DynamicDroppable", 8, 4, 0);
    let desc = ComponentDesc::new(name.as_ptr(), uuid, 8, 4).with_drop(custom_foreign_drop_hook);

    let mut comp_handle = BamComponent::INVALID;
    // SAFETY: Valid active `load_ctx`, `desc`, and `comp_handle`.
    let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut comp_handle) };
    assert_eq!(res, FfiResult::OK);
    load_ctx.finish_init();

    let dummy_payload = [42u32, 84u32];
    // SAFETY: Reinterpreting `[u32; 2]` as 8 initialized bytes.
    let bytes = unsafe { core::slice::from_raw_parts(dummy_payload.as_ptr().cast::<u8>(), 8) };
    let e1 = spawn_and_insert_via_host_commands(&mut world, comp_handle, bytes);
    let e2 = spawn_and_insert_via_host_commands(&mut world, comp_handle, bytes);

    // 1. Despawn entity 1 while mod is ACTIVE: foreign drop hook MUST execute.
    let initial_drops = FOREIGN_DROP_COUNT.load(Ordering::SeqCst);
    world.despawn(Entity::from_bits(e1.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Custom foreign drop hook must be called on despawn while active"
    );

    // 2. Simulate mod unload by setting atomic liveness flag to false.
    is_active.store(false, Ordering::Release);

    // 3. Despawn entity 2 while mod is INACTIVE: foreign drop hook MUST be safely skipped.
    world.despawn(Entity::from_bits(e2.0));
    assert_eq!(
        FOREIGN_DROP_COUNT.load(Ordering::SeqCst),
        initial_drops + 1,
        "Foreign drop hook must be safely skipped after mod deactivation without crashing"
    );
}

#[test]
fn test_drop_trampoline_saturation_and_reclamation() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let is_active_1 = Arc::new(AtomicBool::new(true));
        let mut load_ctx_1 = LoadContext::with_world_and_mod_id(
            registry,
            Arc::clone(&is_active_1),
            &mut world as *mut World,
            None,
        );

        let to_fill = available_drop_trampoline_slots();
        let mut names = Vec::with_capacity(to_fill);
        for i in 0..to_fill {
            names.push(CString::new(format!("sat::DroppableComp_{i}")).unwrap());
        }

        for (i, cstr) in names.iter().enumerate() {
            let uuid = ComponentUuid::from_parts(cstr.to_str().unwrap(), 8, 4, i as u64);
            let desc = ComponentDesc::new(cstr.as_ptr(), uuid, 8, 4).with_drop(generic_drop_hook);
            let mut h = BamComponent::INVALID;
            // SAFETY: Valid active `load_ctx_1` and `desc`.
            let res = unsafe { host_register_component(load_ctx_1.as_ffi_ptr(), &desc, &mut h) };
            assert_eq!(res, FfiResult::OK, "Slot {i} should be allocated");
        }
        assert_eq!(available_drop_trampoline_slots(), 0);

        let extra_desc = ComponentDesc::new(
            c"sat::DroppableComp_Overflow".as_ptr(),
            ComponentUuid::from_parts("sat::DroppableComp_Overflow", 8, 4, 9999),
            8,
            4,
        )
        .with_drop(generic_drop_hook);
        let mut h_over = BamComponent::INVALID;
        // SAFETY: Valid active `load_ctx_1` and `extra_desc`.
        let res_overflow =
            unsafe { host_register_component(load_ctx_1.as_ffi_ptr(), &extra_desc, &mut h_over) };
        assert_eq!(res_overflow, FfiResult::CAPACITY_EXCEEDED);

        // POD component (`drop_fn == None`) MUST still succeed even at 0 free drop slots!
        let pod_desc = ComponentDesc::new(
            c"sat::PodComp_AfterSaturation".as_ptr(),
            ComponentUuid::from_parts("sat::PodComp_AfterSaturation", 8, 4, 0),
            8,
            4,
        );
        let mut h_pod = BamComponent::INVALID;
        // SAFETY: Valid active `load_ctx_1` and `pod_desc`.
        let res_pod =
            unsafe { host_register_component(load_ctx_1.as_ffi_ptr(), &pod_desc, &mut h_pod) };
        assert_eq!(res_pod, FfiResult::OK);
        load_ctx_1.finish_init();

        // Simulate mod 1 unload while `world` still holds `ComponentId` bindings.
        is_active_1.store(false, Ordering::Release);

        // Alien UUID cannot steal slots owned by `world`'s `ComponentId`s when pool is saturated.
        let is_active_2 = Arc::new(AtomicBool::new(true));
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut load_ctx_2 = LoadContext::with_world_and_mod_id(
            world_reg,
            is_active_2,
            &mut world as *mut World,
            None,
        );

        let reclaim_desc = ComponentDesc::new(
            c"sat::ReclaimedDroppable".as_ptr(),
            ComponentUuid::from_parts("sat::ReclaimedDroppable", 8, 4, 0),
            8,
            4,
        )
        .with_drop(generic_drop_hook);
        let mut h_reclaim = BamComponent::INVALID;
        // SAFETY: Valid active `load_ctx_2` and `reclaim_desc`.
        let res_reclaim = unsafe {
            host_register_component(load_ctx_2.as_ffi_ptr(), &reclaim_desc, &mut h_reclaim)
        };
        assert_eq!(res_reclaim, FfiResult::CAPACITY_EXCEEDED);

        // Re-registering the same `ComponentUuid` (mod hot-reload) succeeds by rebinding the slot!
        let same_desc = ComponentDesc::new(
            c"sat::DroppableComp_0".as_ptr(),
            ComponentUuid::from_parts("sat::DroppableComp_0", 8, 4, 0),
            8,
            4,
        )
        .with_drop(generic_drop_hook);
        let mut h_same = BamComponent::INVALID;
        // SAFETY: Valid active `load_ctx_2` and `same_desc`.
        let res_same =
            unsafe { host_register_component(load_ctx_2.as_ffi_ptr(), &same_desc, &mut h_same) };
        assert_eq!(res_same, FfiResult::OK);
        load_ctx_2.finish_init();
    }

    // `WorldDropGuard` MUST reclaim every allocated slot once `world` drops.
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn test_drop_trampoline_slot_reuse_cross_talk() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        world.insert_resource(ModRegistry::new());

        let is_active_a = Arc::new(AtomicBool::new(true));
        let desc_a = ComponentDesc::new(
            c"xtalk::CompA_SlotTest".as_ptr(),
            ComponentUuid::from_parts("xtalk::CompA_SlotTest", 8, 4, 0),
            8,
            4,
        )
        .with_drop(foreign_drop_hook_mod_a);
        let mut comp_a = BamComponent::INVALID;
        {
            let reg_a = world.resource::<ModRegistry>().clone();
            let mut ctx_a = LoadContext::with_world_and_mod_id(
                reg_a,
                Arc::clone(&is_active_a),
                &mut world as *mut World,
                None,
            );
            assert_eq!(reg_comp(&mut ctx_a, &desc_a, &mut comp_a), FfiResult::OK);
            ctx_a.finish_init();
        }

        let payload_a = [0x1111u32, 0x2222u32];
        // SAFETY: `[u32; 2]` is 8 initialized bytes.
        let bytes_a = unsafe { core::slice::from_raw_parts(payload_a.as_ptr().cast::<u8>(), 8) };
        let e_a = spawn_and_insert_via_host_commands(&mut world, comp_a, bytes_a);

        // Fill remaining slots up to 0 free slots.
        let remaining = available_drop_trampoline_slots();
        let mut dummy_names = Vec::with_capacity(remaining);
        for i in 0..remaining {
            dummy_names.push(CString::new(format!("xtalk::DummySlot_{i}")).unwrap());
        }
        {
            let is_active_fill = Arc::new(AtomicBool::new(true));
            let reg_fill = world.resource::<ModRegistry>().clone();
            let mut ctx_fill = LoadContext::with_world_and_mod_id(
                reg_fill,
                is_active_fill,
                &mut world as *mut World,
                None,
            );
            for (i, cname) in dummy_names.iter().enumerate() {
                let desc = ComponentDesc::new(
                    cname.as_ptr(),
                    ComponentUuid::from_parts(cname.to_str().unwrap(), 8, 4, i as u64),
                    8,
                    4,
                )
                .with_drop(foreign_drop_hook_mod_a);
                let mut h = BamComponent::INVALID;
                assert_eq!(reg_comp(&mut ctx_fill, &desc, &mut h), FfiResult::OK);
            }
            ctx_fill.finish_init();
        }
        assert_eq!(available_drop_trampoline_slots(), 0);

        // Unload Mod A.
        is_active_a.store(false, Ordering::Release);

        // Mod B attempts to register a new component; rejected with `CAPACITY_EXCEEDED` (cannot hijack Mod A's slot!).
        {
            let is_active_b = Arc::new(AtomicBool::new(true));
            let reg_b = world.resource::<ModRegistry>().clone();
            let mut ctx_b = LoadContext::with_world_and_mod_id(
                reg_b,
                is_active_b,
                &mut world as *mut World,
                None,
            );
            let desc_b = ComponentDesc::new(
                c"xtalk::CompB_ReusedSlot".as_ptr(),
                ComponentUuid::from_parts("xtalk::CompB_ReusedSlot", 8, 4, 0),
                8,
                4,
            )
            .with_drop(foreign_drop_hook_mod_b);
            let mut h_b = BamComponent::INVALID;
            assert_eq!(
                reg_comp(&mut ctx_b, &desc_b, &mut h_b),
                FfiResult::CAPACITY_EXCEEDED
            );
            ctx_b.finish_init();
        }

        CROSS_TALK_DROP_A.store(0, Ordering::SeqCst);
        CROSS_TALK_DROP_B.store(0, Ordering::SeqCst);
        world.despawn(Entity::from_bits(e_a.0));
        assert_eq!(CROSS_TALK_DROP_A.load(Ordering::SeqCst), 0);
        assert_eq!(CROSS_TALK_DROP_B.load(Ordering::SeqCst), 0);

        // Legitimate mod reload with same UUID succeeds and rebinds trampoline slot!
        let is_active_a_reload = Arc::new(AtomicBool::new(true));
        {
            let reg_reload = world.resource::<ModRegistry>().clone();
            let mut ctx_reload = LoadContext::with_world_and_mod_id(
                reg_reload,
                is_active_a_reload,
                &mut world as *mut World,
                None,
            );
            let mut comp_r = BamComponent::INVALID;
            assert_eq!(
                reg_comp(&mut ctx_reload, &desc_a, &mut comp_r),
                FfiResult::OK
            );
            assert_eq!(comp_r, comp_a);
            ctx_reload.finish_init();
        }

        let e_r = spawn_and_insert_via_host_commands(&mut world, comp_a, bytes_a);
        CROSS_TALK_DROP_A.store(0, Ordering::SeqCst);
        world.despawn(Entity::from_bits(e_r.0));
        assert_eq!(CROSS_TALK_DROP_A.load(Ordering::SeqCst), 1);
    }

    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn test_pod_components_consume_zero_drop_trampoline_slots() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let is_active = Arc::new(AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(registry, is_active, &mut world as *mut World, None);

        let desc_rust = ComponentDesc::new(
            c"pod::PodPowerData".as_ptr(),
            ComponentUuid::from_parts(
                "pod::PodPowerData",
                core::mem::size_of::<PodPowerData>(),
                core::mem::align_of::<PodPowerData>(),
                0,
            ),
            core::mem::size_of::<PodPowerData>(),
            core::mem::align_of::<PodPowerData>(),
        );
        let mut h = BamComponent::INVALID;
        assert_eq!(reg_comp(&mut load_ctx, &desc_rust, &mut h), FfiResult::OK);

        let desc_c = ComponentDesc::new(
            c"pod::PodManaData".as_ptr(),
            ComponentUuid::from_parts(
                "pod::PodManaData",
                core::mem::size_of::<PodManaData>(),
                core::mem::align_of::<PodManaData>(),
                0,
            ),
            core::mem::size_of::<PodManaData>(),
            core::mem::align_of::<PodManaData>(),
        );
        assert_eq!(reg_comp(&mut load_ctx, &desc_c, &mut h), FfiResult::OK);

        for i in 0..100 {
            let cname = CString::new(format!("pod::PodComponent_{i}")).unwrap();
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(cname.to_str().unwrap(), 8, 4, 0),
                8,
                4,
            );
            assert_eq!(reg_comp(&mut load_ctx, &desc, &mut h), FfiResult::OK);
        }

        // Zero drop slots consumed by 102 POD dynamic components!
        assert_eq!(available_drop_trampoline_slots(), baseline_free);

        for i in 0..baseline_free {
            let cname = CString::new(format!("pod::NonPodComp_{i}")).unwrap();
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(cname.to_str().unwrap(), 16, 8, 0),
                16,
                8,
            )
            .with_drop(non_pod_drop_hook);
            assert_eq!(reg_comp(&mut load_ctx, &desc, &mut h), FfiResult::OK);
        }

        let desc_overflow = ComponentDesc::new(
            c"pod::NonPodComp_Overflow".as_ptr(),
            ComponentUuid::from_parts("pod::NonPodComp_Overflow", 16, 8, 0),
            16,
            8,
        )
        .with_drop(non_pod_drop_hook);
        assert_eq!(
            reg_comp(&mut load_ctx, &desc_overflow, &mut h),
            FfiResult::CAPACITY_EXCEEDED
        );

        for i in 100..110 {
            let cname = CString::new(format!("pod::PodComponent_{i}")).unwrap();
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(cname.to_str().unwrap(), 8, 4, 0),
                8,
                4,
            );
            assert_eq!(reg_comp(&mut load_ctx, &desc, &mut h), FfiResult::OK);
        }
        load_ctx.finish_init();
    }

    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn test_non_pod_panic_containment_in_drop_trampoline() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    PANICKING_DROP_INVOKED.store(false, Ordering::SeqCst);

    let mut world = World::new();
    let registry = ModRegistry::new();
    world.insert_resource(registry.clone());

    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx =
        LoadContext::with_world_and_mod_id(registry, is_active, &mut world as *mut World, None);

    let desc = ComponentDesc::new(
        c"drop_test::PanickingNonPod".as_ptr(),
        ComponentUuid::from_parts("drop_test::PanickingNonPod", 8, 4, 0),
        8,
        4,
    )
    .with_drop(panicking_drop_trampoline_with_catch_unwind);

    let mut comp_handle = BamComponent::INVALID;
    assert_eq!(
        reg_comp(&mut load_ctx, &desc, &mut comp_handle),
        FfiResult::OK
    );
    load_ctx.finish_init();

    let comp_id = world
        .resource::<ModRegistry>()
        .get_by_handle(comp_handle)
        .unwrap()
        .component_id;

    let val: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    let ffi_e = spawn_and_insert_via_host_commands(&mut world, comp_handle, &val);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).expect("Valid entity");
    assert!(world.entity(bevy_entity).contains_id(comp_id));

    world.despawn(bevy_entity);
    assert!(PANICKING_DROP_INVOKED.load(Ordering::SeqCst));
}

#[test]
fn test_non_pod_slot_reuse_on_mod_reload() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let baseline_free = available_drop_trampoline_slots();

    {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let is_active_1 = Arc::new(AtomicBool::new(true));
        let mut load_ctx_1 = LoadContext::with_world_and_mod_id(
            registry,
            Arc::clone(&is_active_1),
            &mut world as *mut World,
            None,
        );

        let uuid_shared = ComponentUuid::from_parts("reload::ReloadableComp", 16, 8, 0);
        let desc_1 = ComponentDesc::new(c"reload::ReloadableComp".as_ptr(), uuid_shared, 16, 8)
            .with_drop(non_pod_drop_hook);
        let mut h = BamComponent::INVALID;
        assert_eq!(reg_comp(&mut load_ctx_1, &desc_1, &mut h), FfiResult::OK);

        let remaining = available_drop_trampoline_slots();
        for i in 0..remaining {
            let cname = CString::new(format!("reload::SlotComp_{i}")).unwrap();
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(cname.to_str().unwrap(), 16, 8, i as u64),
                16,
                8,
            )
            .with_drop(non_pod_drop_hook);
            assert_eq!(reg_comp(&mut load_ctx_1, &desc, &mut h), FfiResult::OK);
        }
        load_ctx_1.finish_init();

        is_active_1.store(false, Ordering::Release);

        let is_active_2 = Arc::new(AtomicBool::new(true));
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut load_ctx_2 = LoadContext::with_world_and_mod_id(
            world_reg,
            is_active_2,
            &mut world as *mut World,
            None,
        );

        let desc_reload =
            ComponentDesc::new(c"reload::ReloadableComp".as_ptr(), uuid_shared, 16, 8)
                .with_drop(non_pod_drop_hook);
        assert_eq!(
            reg_comp(&mut load_ctx_2, &desc_reload, &mut h),
            FfiResult::OK
        );
        load_ctx_2.finish_init();
    }

    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}

#[test]
fn partial_unload_of_second_mod_never_invokes_unmapped_drop_fn() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    MOD1_DROP_COUNT.store(0, Ordering::SeqCst);
    MOD2_DROP_COUNT.store(0, Ordering::SeqCst);

    let mut world = World::new();
    world.insert_resource(ModRegistry::new());

    let uuid = ComponentUuid::from_parts("shared::MultiModDroppable", 8, 8, 0);
    let mut comp_handle = BamComponent::INVALID;

    let active_1 = Arc::new(AtomicBool::new(true));
    {
        let reg1 = world.resource::<ModRegistry>().clone();
        let mut ctx1 = LoadContext::with_world_and_mod_id(
            reg1,
            Arc::clone(&active_1),
            &mut world as *mut World,
            None,
        );
        let desc1 = ComponentDesc::new(c"shared::MultiModDroppable".as_ptr(), uuid, 8, 8)
            .with_drop(mod1_drop_fn);
        assert_eq!(reg_comp(&mut ctx1, &desc1, &mut comp_handle), FfiResult::OK);
        ctx1.finish_init();
    }

    let active_2 = Arc::new(AtomicBool::new(true));
    {
        let reg2 = world.resource::<ModRegistry>().clone();
        let mut ctx2 = LoadContext::with_world_and_mod_id(
            reg2,
            Arc::clone(&active_2),
            &mut world as *mut World,
            None,
        );
        let desc2 = ComponentDesc::new(c"shared::MultiModDroppable".as_ptr(), uuid, 8, 8)
            .with_drop(mod2_drop_fn);
        let mut comp_handle_2 = BamComponent::INVALID;
        assert_eq!(
            reg_comp(&mut ctx2, &desc2, &mut comp_handle_2),
            FfiResult::OK
        );
        assert_eq!(comp_handle_2, comp_handle);
        ctx2.finish_init();
    }

    let payload = 42u64.to_ne_bytes();
    let e1 = spawn_and_insert_via_host_commands(&mut world, comp_handle, &payload);
    let e2 = spawn_and_insert_via_host_commands(&mut world, comp_handle, &payload);

    // Unload Mod 2 while Mod 1 remains loaded: falls back to Mod 1's active drop hook!
    active_2.store(false, Ordering::Release);

    world.despawn(Entity::try_from_bits(e1.to_bits()).unwrap());
    assert_eq!(MOD2_DROP_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(MOD1_DROP_COUNT.load(Ordering::SeqCst), 1);

    // Unload Mod 1 as well; despawning `e2` invokes neither.
    active_1.store(false, Ordering::Release);
    world.despawn(Entity::try_from_bits(e2.to_bits()).unwrap());
    assert_eq!(MOD2_DROP_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(MOD1_DROP_COUNT.load(Ordering::SeqCst), 1);
}
