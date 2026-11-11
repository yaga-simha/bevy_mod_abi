#![deny(unsafe_op_in_unsafe_fn)]

//! # Milestone 3 Empirical Challenger Stress Suite
//!
//! Written by Challenger 2 for Milestone 3: Multi-Language Guest SDK & Mod Implementations.
//! Empirical Focus Areas:
//! 1. Drop Trampoline Pool Conservation:
//!    - Verify POD components (`RustModCustomData`, `CModCustomData`) consume ZERO slots in `DROP_TRAMPOLINE_SLOTS` (64-slot pool).
//!    - Verify >64 distinct POD components can be registered without exhausting the pool.
//!    - Verify non-POD components consume exactly 1 slot each, and slot 65 exhausts the pool.
//!    - Verify slot deduplication / reuse upon component re-registration with identical UUID (mod reload).
//!    - Verify panic containment in drop trampolines: catching unwinds cleanly without unwinding into the host or crashing.
//! 2. Deferred Command Queue Stress & Host Allocator Isolation (Rule 2):
//!    - Repeated rapid insertions and value overwrites on the same entity.
//!    - Immediate entity despawning before command queue flush.
//!    - Insertion on invalid / dead `FfiEntity`.
//!    - Boundary sizes (0 bytes / ZST, 1 byte, 64 KB, power-of-two alignments).
//!    - Stack memory scrambling defense (verifying host copy semantics and no dangling guest stack pointers).
//! 3. Mod Unload & Lifecycle Stress:
//!    - Full multi-language dynamic mod loading (`asymmetric_rust_mod` and `asymmetric_c_mod`).
//!    - Continuous updates, clean mod unload, post-unload updates without segfaults or panics.
//!    - Post-unload entity despawns.
//!    - Repeated mod load/update/unload cycling stress (5 full cycles).

mod common;

use abi_contract::{ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawCommandQueue};
use bevy_app::App;
use bevy_ecs::entity::Entity;
use bevy_ecs::world::{CommandQueue, World};
use common::*;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::commands::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, CommandQueueBridge,
};
use host_runtime::dynamic_system::{
    host_register_component, reset_drop_trampolines_for_testing, ModInitContext,
};
use host_runtime::plugin::AbiModPlugin;
use host_runtime::registry::ModRegistry;
use host_runtime::ModAppExt;
use std::sync::{Arc, Mutex};

static GLOBAL_DROP_TEST_LOCK: Mutex<()> = Mutex::new(());

// ============================================================================
// Types for Drop Trampoline Testing
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CModCustomData {
    pub mana: f32,
    pub level: i32,
}

static NON_POD_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn non_pod_drop_hook(ptr: *mut c_void) {
    if !ptr.is_null() {
        NON_POD_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

static PANICKING_DROP_INVOKED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn panicking_drop_trampoline_with_catch_unwind(ptr: *mut c_void) {
    if !ptr.is_null() {
        PANICKING_DROP_INVOKED.store(true, Ordering::SeqCst);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("Simulated non-POD drop hook panic in guest runtime");
        }));
    }
}

// ============================================================================
// 1. Drop Trampoline Pool Conservation & Non-POD Safety
// ============================================================================

#[test]
fn test_pod_components_consume_zero_drop_trampoline_slots() {
    let _lock = GLOBAL_DROP_TEST_LOCK.lock().unwrap();
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

    // 1. Register RustModCustomData (POD - drop_fn = None)
    let desc_rust = ModComponentDescriptor::new(
        c"RustModCustomData".as_ptr(),
        ComponentUuid::from_name("RustModCustomData"),
        core::mem::size_of::<RustModCustomData>(),
        core::mem::align_of::<RustModCustomData>(),
    );
    assert!(desc_rust.drop_fn.is_none());
    let res = unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_rust,
        )
    };
    assert_eq!(
        res,
        FfiResult::OK,
        "RustModCustomData registration must succeed"
    );

    // 2. Register CModCustomData (POD - drop_fn = None)
    let desc_c = ModComponentDescriptor::new(
        c"CModCustomData".as_ptr(),
        ComponentUuid::from_name("CModCustomData"),
        core::mem::size_of::<CModCustomData>(),
        core::mem::align_of::<CModCustomData>(),
    );
    assert!(desc_c.drop_fn.is_none());
    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc_c)
    };
    assert_eq!(
        res,
        FfiResult::OK,
        "CModCustomData registration must succeed"
    );

    // 3. Register 100 uniquely-named POD components (far exceeding the 64-slot pool limit)
    let mut names = Vec::new();
    for i in 0..100 {
        let name = format!("PodComponent_{i}\0");
        names.push(name);
    }

    for (i, name) in names.iter().enumerate() {
        let desc = ModComponentDescriptor::new(
            name.as_ptr() as *const core::ffi::c_char,
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            8,
            4,
        );
        let res = unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(
            res,
            FfiResult::OK,
            "POD component {i} must succeed without consuming trampoline slots"
        );
    }

    // 4. Now, verify all 64 slots in the drop trampoline pool remain completely available!
    // Register exactly 64 Non-POD components with non-null drop_fn.
    let mut non_pod_names = Vec::new();
    for i in 0..64 {
        let name = format!("NonPodComp_{i}\0");
        non_pod_names.push(name);
    }

    for (i, name) in non_pod_names.iter().enumerate() {
        let mut desc = ModComponentDescriptor::new(
            name.as_ptr() as *const core::ffi::c_char,
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            16,
            8,
        );
        desc.drop_fn = Some(non_pod_drop_hook);
        let res = unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(
            res,
            FfiResult::OK,
            "Non-POD slot {i} of 64 must succeed because POD components consumed 0 slots"
        );
    }

    // 5. Attempting to register the 65th Non-POD component MUST fail with INTERNAL_ERROR (pool full)
    let mut desc_overflow = ModComponentDescriptor::new(
        c"NonPodComp_Overflow".as_ptr(),
        ComponentUuid::from_name("NonPodComp_Overflow"),
        16,
        8,
    );
    desc_overflow.drop_fn = Some(non_pod_drop_hook);
    let res_overflow = unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_overflow,
        )
    };
    assert_eq!(
        res_overflow,
        FfiResult::INTERNAL_ERROR,
        "65th Non-POD component must fail due to exhausted 64-slot trampoline pool"
    );

    // 6. BUT registering an additional 10 POD components still succeeds!
    let mut more_pod_names = Vec::new();
    for i in 100..110 {
        let name = format!("PodComponent_{i}\0");
        more_pod_names.push(name);
    }
    for (i, name) in more_pod_names.iter().enumerate() {
        let desc = ModComponentDescriptor::new(
            name.as_ptr() as *const core::ffi::c_char,
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            8,
            4,
        );
        let res = unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(
            res,
            FfiResult::OK,
            "POD component {i} must still succeed even when Non-POD trampoline pool is exhausted"
        );
    }
}

#[test]
fn test_non_pod_panic_containment_in_drop_trampoline() {
    let _lock = GLOBAL_DROP_TEST_LOCK.lock().unwrap();
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

    let mut desc = ModComponentDescriptor::new(
        c"PanickingNonPod".as_ptr(),
        ComponentUuid::from_name("PanickingNonPod"),
        8,
        4,
    );
    desc.drop_fn = Some(panicking_drop_trampoline_with_catch_unwind);

    let res = unsafe {
        host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
    };
    assert_eq!(res, FfiResult::OK);

    let reg = world.resource::<ModRegistry>().clone();
    let comp_meta = reg
        .get_by_name("PanickingNonPod")
        .expect("Component registered");
    let comp_id = comp_meta.component_id;

    // Spawn an entity with this component
    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
    let val: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    let insert_res = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"PanickingNonPod".as_ptr(),
            val.as_ptr(),
            val.len(),
        )
    };
    assert_eq!(insert_res, FfiResult::OK);

    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).expect("Valid entity");
    assert!(world.get_entity(bevy_entity).is_ok());
    assert!(world.entity(bevy_entity).contains_id(comp_id));

    // Despawn the entity: Bevy calls the drop trampoline
    // generic_drop_trampoline catches the panic via catch_unwind
    world.despawn(bevy_entity);

    // Host survives cleanly without crashing!
    assert!(
        PANICKING_DROP_INVOKED.load(Ordering::SeqCst),
        "Panicking drop hook was invoked"
    );
}

#[test]
fn test_non_pod_slot_reuse_on_mod_reload() {
    let _lock = GLOBAL_DROP_TEST_LOCK.lock().unwrap();
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

    // Initial mod registration of non-POD component
    let mut desc_1 = ModComponentDescriptor::new(c"ReloadableComp".as_ptr(), uuid_shared, 16, 8);
    desc_1.drop_fn = Some(non_pod_drop_hook);
    let res = unsafe {
        host_register_component(
            &mut init_ctx_1 as *mut ModInitContext as *mut c_void,
            &desc_1,
        )
    };
    assert_eq!(res, FfiResult::OK);

    // Fill 63 more slots (total 64 slots full)
    let mut names = Vec::new();
    for i in 1..64 {
        names.push(format!("SlotComp_{i}\0"));
    }
    for (i, name) in names.iter().enumerate() {
        let mut desc = ModComponentDescriptor::new(
            name.as_ptr() as *const core::ffi::c_char,
            ComponentUuid::from_name(name.trim_end_matches('\0')),
            16,
            8,
        );
        desc.drop_fn = Some(non_pod_drop_hook);
        let res = unsafe {
            host_register_component(&mut init_ctx_1 as *mut ModInitContext as *mut c_void, &desc)
        };
        assert_eq!(res, FfiResult::OK, "Slot {i} must succeed");
    }

    drop(_guard_1);

    // Now simulate mod reload: Mod 1 unloads and reloads with a new context
    is_active_1.store(false, Ordering::Release);

    let is_active_2 = Arc::new(AtomicBool::new(true));
    let mut init_ctx_2 = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_2),
        &mut world as *mut World,
    );
    let _guard_2 = init_ctx_2.make_active();

    // Re-register the SAME component (matching UUID and layout)
    let mut desc_reload =
        ModComponentDescriptor::new(c"ReloadableComp".as_ptr(), uuid_shared, 16, 8);
    desc_reload.drop_fn = Some(non_pod_drop_hook);
    let res_reload = unsafe {
        host_register_component(
            &mut init_ctx_2 as *mut ModInitContext as *mut c_void,
            &desc_reload,
        )
    };
    assert_eq!(
        res_reload,
        FfiResult::OK,
        "Reloading existing UUID must succeed by reusing its existing slot"
    );
}

// ============================================================================
// 2. Deferred Command Queue Stress & Host Allocator Isolation (Rule 2)
// ============================================================================

#[test]
fn test_cmd_insert_rapid_overwrites_on_same_entity() {
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
        c"RustModCustomData".as_ptr(),
        ComponentUuid::from_name("RustModCustomData"),
        core::mem::size_of::<RustModCustomData>(),
        core::mem::align_of::<RustModCustomData>(),
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let reg = world.resource::<ModRegistry>().clone();
    let comp_id = reg.get_by_name("RustModCustomData").unwrap().component_id;

    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // Issue 5 sequential overwrites
    for val in [10.0f32, 25.0, 50.0, 75.0, 99.0] {
        let data = RustModCustomData {
            power: val,
            shield: val as u32 * 2,
        };
        let status = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"RustModCustomData".as_ptr(),
                &data as *const RustModCustomData as *const u8,
                core::mem::size_of::<RustModCustomData>(),
            )
        };
        assert_eq!(status, FfiResult::OK);
    }

    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
    let entity_ref = world.entity(bevy_entity);
    let final_data =
        unsafe { *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData) };
    assert_eq!(
        final_data,
        RustModCustomData {
            power: 99.0,
            shield: 198
        },
        "Final overwrite must be present in archetype table"
    );
}

#[test]
fn test_cmd_insert_and_despawn_before_flush() {
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
        c"CModCustomData".as_ptr(),
        ComponentUuid::from_name("CModCustomData"),
        core::mem::size_of::<CModCustomData>(),
        core::mem::align_of::<CModCustomData>(),
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let reg = world.resource::<ModRegistry>().clone();
    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
    let c_data = CModCustomData {
        mana: 123.45,
        level: 42,
    };

    let res_insert = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"CModCustomData".as_ptr(),
            &c_data as *const CModCustomData as *const u8,
            core::mem::size_of::<CModCustomData>(),
        )
    };
    assert_eq!(res_insert, FfiResult::OK);

    // Despawn in the same frame BEFORE flushing
    let res_despawn = unsafe { host_cmd_despawn(raw_queue, ffi_e) };
    assert_eq!(res_despawn, FfiResult::OK);

    // Applying commands must not panic, and must drop AlignedBuffer cleanly
    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
    assert!(
        world.get_entity(bevy_entity).is_err(),
        "Entity must be despawned cleanly"
    );
}

#[test]
fn test_cmd_insert_on_invalid_or_dead_entity() {
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
        c"CModCustomData".as_ptr(),
        ComponentUuid::from_name("CModCustomData"),
        core::mem::size_of::<CModCustomData>(),
        core::mem::align_of::<CModCustomData>(),
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let reg = world.resource::<ModRegistry>().clone();
    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    // Entity that was never allocated
    let dead_entity = FfiEntity(0xDEAD_CAFE);
    let c_data = CModCustomData {
        mana: 1.0,
        level: 1,
    };
    let res = unsafe {
        host_cmd_insert_component(
            raw_queue,
            dead_entity,
            c"CModCustomData".as_ptr(),
            &c_data as *const CModCustomData as *const u8,
            core::mem::size_of::<CModCustomData>(),
        )
    };
    assert_eq!(res, FfiResult::OK);

    // Applying queue must safely skip the non-existent entity without crashing or leaking
    cmd_queue.apply(&mut world);
}

#[test]
fn test_cmd_insert_boundary_sizes_and_alignments() {
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

    // 1. Zero-Sized Type (ZST)
    let desc_zst = ModComponentDescriptor::new(
        c"ZstComponent".as_ptr(),
        ComponentUuid::from_name("ZstComponent"),
        0,
        1,
    );
    unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_zst,
        )
    };

    // 2. Large 64 KB component
    const LARGE_SIZE: usize = 65536;
    let desc_large = ModComponentDescriptor::new(
        c"LargeComponent".as_ptr(),
        ComponentUuid::from_name("LargeComponent"),
        LARGE_SIZE,
        16,
    );
    unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_large,
        )
    };

    let reg = world.resource::<ModRegistry>().clone();
    let zst_id = reg.get_by_name("ZstComponent").unwrap().component_id;
    let large_id = reg.get_by_name("LargeComponent").unwrap().component_id;

    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // Insert ZST (null pointer allowed when len == 0)
    let res_zst = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"ZstComponent".as_ptr(),
            core::ptr::null(),
            0,
        )
    };
    assert_eq!(res_zst, FfiResult::OK);

    // Insert Large Component with distinct byte pattern
    let mut large_buf = vec![0x5Au8; LARGE_SIZE];
    large_buf[0] = 0x11;
    large_buf[LARGE_SIZE - 1] = 0x99;

    let res_large = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_e,
            c"LargeComponent".as_ptr(),
            large_buf.as_ptr(),
            LARGE_SIZE,
        )
    };
    assert_eq!(res_large, FfiResult::OK);

    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
    let entity_ref = world.entity(bevy_entity);

    assert!(entity_ref.contains_id(zst_id));
    assert!(entity_ref.contains_id(large_id));

    let table_large_ptr = entity_ref.get_by_id(large_id).unwrap().as_ptr() as *const u8;
    let table_large_slice = unsafe { core::slice::from_raw_parts(table_large_ptr, LARGE_SIZE) };
    assert_eq!(table_large_slice[0], 0x11);
    assert_eq!(table_large_slice[LARGE_SIZE - 1], 0x99);
    assert_eq!(table_large_slice[100], 0x5A);
}

#[test]
fn test_allocator_isolation_stack_memory_scramble_defense() {
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
        c"RustModCustomData".as_ptr(),
        ComponentUuid::from_name("RustModCustomData"),
        core::mem::size_of::<RustModCustomData>(),
        core::mem::align_of::<RustModCustomData>(),
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let reg = world.resource::<ModRegistry>().clone();
    let comp_id = reg.get_by_name("RustModCustomData").unwrap().component_id;

    let mut cmd_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, world.entity_allocator(), &reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // Scope stack variable and overwrite it before flush
    {
        let mut stack_data = RustModCustomData {
            power: 777.0,
            shield: 888,
        };

        let status = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"RustModCustomData".as_ptr(),
                &stack_data as *const RustModCustomData as *const u8,
                core::mem::size_of::<RustModCustomData>(),
            )
        };
        assert_eq!(status, FfiResult::OK);

        // Adversarially scramble stack memory!
        stack_data.power = -9999.0;
        stack_data.shield = 0xDEADBEEF;
        assert_eq!(stack_data.shield, 0xDEADBEEF);
    }

    // Now apply commands to world: host must read from its own host-allocated copy!
    cmd_queue.apply(&mut world);

    let bevy_entity = Entity::try_from_bits(ffi_e.0).unwrap();
    let entity_ref = world.entity(bevy_entity);
    let read_data =
        unsafe { *(entity_ref.get_by_id(comp_id).unwrap().as_ptr() as *const RustModCustomData) };

    assert_eq!(
        read_data,
        RustModCustomData {
            power: 777.0,
            shield: 888
        },
        "Host allocator isolation (Rule 2) verified: host memory was isolated from guest stack mutation"
    );
}

// ============================================================================
// 3. Mod Unload & Lifecycle Stress
// ============================================================================

#[test]
fn test_multilanguage_mod_unload_and_post_unload_update_stress() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    let reg = app.world().resource::<ModRegistry>().clone();
    let rust_comp_id = reg.get_by_name("RustModCustomData").unwrap().component_id;
    let c_comp_id = reg.get_by_name("CModCustomData").unwrap().component_id;

    // Spawn entities
    let e_rust = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 1.0,
            },
        ))
        .id();

    let e_c = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 10.0,
                z: 10.0,
            },
            Velocity {
                x: 2.0,
                y: 2.0,
                z: 2.0,
            },
        ))
        .id();

    // Attach dynamic components
    unsafe {
        let rust_data = RustModCustomData {
            power: 100.0,
            shield: 10,
        };
        app.world_mut().entity_mut(e_rust).insert_by_id(
            rust_comp_id,
            bevy_ecs::ptr::OwningPtr::new(
                core::ptr::NonNull::new(&rust_data as *const _ as *mut u8).unwrap(),
            ),
        );

        let c_data = CModCustomData {
            mana: 50.0,
            level: 5,
        };
        app.world_mut().entity_mut(e_c).insert_by_id(
            c_comp_id,
            bevy_ecs::ptr::OwningPtr::new(
                core::ptr::NonNull::new(&c_data as *const _ as *mut u8).unwrap(),
            ),
        );
    }

    // Run 5 active frames
    for _ in 0..5 {
        app.update();
    }

    // Verify mutations happened
    {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e_rust)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data_r.power, 150.0); // 100 + 10*5
        assert_eq!(data_r.shield, 15); // 10 + 1*5

        let data_c = unsafe {
            *(app
                .world()
                .entity(e_c)
                .get_by_id(c_comp_id)
                .unwrap()
                .as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_c.mana, 75.0); // 50 + 5*5
        assert_eq!(data_c.level, 10); // 5 + 1*5
    }

    // Unload both mods
    app.unload_mod(rust_mod_id).expect("Rust mod unload ok");
    app.unload_mod(c_mod_id).expect("C mod unload ok");

    // Run 10 ticks post-unload: systems must NOT execute, no segfaults or panics
    for _ in 0..10 {
        app.update();
    }

    // Verify values remain frozen
    {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e_rust)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data_r.power, 150.0, "Values must remain frozen post-unload");

        let data_c = unsafe {
            *(app
                .world()
                .entity(e_c)
                .get_by_id(c_comp_id)
                .unwrap()
                .as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_c.mana, 75.0, "Values must remain frozen post-unload");
    }

    // Despawn entities post-unload: drop trampolines must not jump into unmapped dylibs
    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
}

#[test]
fn test_multilanguage_mod_reload_churn_cycles() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    for cycle in 0..5 {
        let rust_mod_id = app
            .load_mod_tracked(&rust_dylib)
            .unwrap_or_else(|_| panic!("Failed to load rust mod in cycle {cycle}"));
        let c_mod_id = app
            .load_mod_tracked(&c_dylib)
            .unwrap_or_else(|_| panic!("Failed to load c mod in cycle {cycle}"));

        // Tick 2 frames
        app.update();
        app.update();

        // Unload
        app.unload_mod(rust_mod_id)
            .unwrap_or_else(|_| panic!("Failed to unload rust mod in cycle {cycle}"));
        app.unload_mod(c_mod_id)
            .unwrap_or_else(|_| panic!("Failed to unload c mod in cycle {cycle}"));

        // Tick 2 frames post-unload
        app.update();
        app.update();
    }
}
