#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Milestone 2 Empirical Adversarial Stress Suite
//!
//! Written by Challenger 2 for Milestone 2: Host Runtime Dynamic ECS Integration & Concurrency.
//! Focus Areas:
//! 1. Zero-Copy Pointer Arithmetic & Alignment (1B, 2B, 4B, 8B, 16B, 32B, 64B, and ZSTs).
//! 2. Contiguous Table Column Verification & Entity ID Alignment across Multiple Archetype Chunks.
//! 3. Native Bevy Change Detection Parity (Row-level vs Chunk Bulk Ticks).
//! 4. Allocator Isolation & Foreign Memory Decoupling (Stack overwrite defense, queue discard leak test).
//! 5. Drop Trampoline Lifecycle, Unload Safety, and Slot Reuse Challenge.

use abi_contract::{
    ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter,
    RawCommandQueue,
};
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::system::System;
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::chunk_iter::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_iter_reset,
};
use host_runtime::commands::{host_cmd_insert_component, host_cmd_spawn_empty, CommandQueueBridge};
use host_runtime::dynamic_system::{
    host_register_component, reset_drop_trampolines_for_testing, ComponentAccessDescOwned,
    DynamicCSystem, ModInitContext,
};
use host_runtime::plugin::AbiModPlugin;
use host_runtime::registry::ModRegistry;
use std::sync::{Arc, Mutex};

// ============================================================================
// Focus 1: Alignment and Size Matrix Stress Test
// ============================================================================

#[repr(C, align(1))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign1 {
    val: u8,
}

#[repr(C, align(2))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign2 {
    val: u16,
}

#[repr(C, align(4))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign4 {
    val: u32,
}

#[repr(C, align(8))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign8 {
    val: u64,
}

#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign16 {
    val: [u64; 2],
}

#[repr(C, align(32))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign32 {
    val: [u64; 4],
}

#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CompAlign64 {
    val: [u64; 8],
}

#[test]
fn test_adversarial_alignment_and_pointer_arithmetic_matrix() {
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

    // Register all alignments
    macro_rules! register_type {
        ($t:ty, $name:literal) => {{
            let name_cstr = concat!($name, "\0").as_bytes();
            let desc = ModComponentDescriptor::new(
                name_cstr.as_ptr() as *const core::ffi::c_char,
                ComponentUuid::from_name($name),
                core::mem::size_of::<$t>(),
                core::mem::align_of::<$t>(),
            );
            let res = unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            };
            assert_eq!(res, FfiResult::OK, "Failed to register {}", $name);
        }};
    }

    register_type!(CompAlign1, "CompAlign1");
    register_type!(CompAlign2, "CompAlign2");
    register_type!(CompAlign4, "CompAlign4");
    register_type!(CompAlign8, "CompAlign8");
    register_type!(CompAlign16, "CompAlign16");
    register_type!(CompAlign32, "CompAlign32");
    register_type!(CompAlign64, "CompAlign64");

    // Spawn 16 entities with all these components
    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    const ENTITY_COUNT: usize = 16;
    let mut entities = Vec::new();

    for i in 0..ENTITY_COUNT {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        entities.push(ffi_e);

        let c1 = CompAlign1 { val: i as u8 };
        let c2 = CompAlign2 { val: i as u16 * 10 };
        let c4 = CompAlign4 {
            val: i as u32 * 100,
        };
        let c8 = CompAlign8 {
            val: i as u64 * 1000,
        };
        let c16 = CompAlign16 {
            val: [i as u64, i as u64 * 2],
        };
        let c32 = CompAlign32 { val: [i as u64; 4] };
        let c64 = CompAlign64 { val: [i as u64; 8] };

        macro_rules! insert_val {
            ($c:ident, $t:ty, $name:literal) => {{
                let name_cstr = concat!($name, "\0").as_bytes();
                let res = unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        ffi_e,
                        name_cstr.as_ptr() as *const core::ffi::c_char,
                        &$c as *const $t as *const u8,
                        core::mem::size_of::<$t>(),
                    )
                };
                assert_eq!(res, FfiResult::OK, "Failed to insert {}", $name);
            }};
        }

        insert_val!(c1, CompAlign1, "CompAlign1");
        insert_val!(c2, CompAlign2, "CompAlign2");
        insert_val!(c4, CompAlign4, "CompAlign4");
        insert_val!(c8, CompAlign8, "CompAlign8");
        insert_val!(c16, CompAlign16, "CompAlign16");
        insert_val!(c32, CompAlign32, "CompAlign32");
        insert_val!(c64, CompAlign64, "CompAlign64");
    }

    command_queue.apply(&mut world);

    // Verify all components in Bevy World are strictly aligned and equal to inserted data
    let reg = world.resource::<ModRegistry>();
    for (i, ffi_e) in entities.iter().enumerate().take(ENTITY_COUNT) {
        let bevy_e = Entity::from_bits(ffi_e.0);
        let e_ref = world.entity(bevy_e);

        macro_rules! check_val {
            ($t:ty, $name:literal, $expected:expr) => {{
                let comp_id = reg.get_by_name($name).unwrap().component_id;
                let ptr = e_ref.get_by_id(comp_id).expect($name).as_ptr();
                assert_eq!(
                    ptr as usize % core::mem::align_of::<$t>(),
                    0,
                    "Pointer for {} is misaligned: {:p} (expected align {})",
                    $name,
                    ptr,
                    core::mem::align_of::<$t>()
                );
                let read_val = unsafe { *(ptr as *const $t) };
                assert_eq!(read_val, $expected, "Value mismatch for {}", $name);
            }};
        }

        check_val!(CompAlign1, "CompAlign1", CompAlign1 { val: i as u8 });
        check_val!(CompAlign2, "CompAlign2", CompAlign2 { val: i as u16 * 10 });
        check_val!(
            CompAlign4,
            "CompAlign4",
            CompAlign4 {
                val: i as u32 * 100
            }
        );
        check_val!(
            CompAlign8,
            "CompAlign8",
            CompAlign8 {
                val: i as u64 * 1000
            }
        );
        check_val!(
            CompAlign16,
            "CompAlign16",
            CompAlign16 {
                val: [i as u64, i as u64 * 2]
            }
        );
        check_val!(
            CompAlign32,
            "CompAlign32",
            CompAlign32 { val: [i as u64; 4] }
        );
        check_val!(
            CompAlign64,
            "CompAlign64",
            CompAlign64 { val: [i as u64; 8] }
        );
    }
}

// ============================================================================
// Focus 2: Dynamic System Chunk Iteration Pointer Arithmetic & Reset
// ============================================================================

static ITER_VERIFIED_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn dynamic_chunk_pointer_arithmetic_verifier(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let count = unsafe { host_iter_next_chunk(iter) };
    if count > 0 {
        // Columns: 0: CompAlign1, 1: CompAlign16, 2: CompAlign64
        let p1 = unsafe { host_iter_column_mutable(iter, 0) };
        let p16 = unsafe { host_iter_column_mutable(iter, 1) };
        let p64 = unsafe { host_iter_column_mutable(iter, 2) };

        assert!(!p1.is_null());
        assert!(!p16.is_null());
        assert!(!p64.is_null());

        let stride1 = unsafe { host_iter_column_stride(iter, 0) };
        let stride16 = unsafe { host_iter_column_stride(iter, 1) };
        let stride64 = unsafe { host_iter_column_stride(iter, 2) };

        assert_eq!(stride1, core::mem::size_of::<CompAlign1>());
        assert_eq!(stride16, core::mem::size_of::<CompAlign16>());
        assert_eq!(stride64, core::mem::size_of::<CompAlign64>());

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null());

        for row in 0..count {
            // Check natural alignment for each row
            let ptr1_row = unsafe { p1.add(row * stride1) };
            let ptr16_row = unsafe { p16.add(row * stride16) };
            let ptr64_row = unsafe { p64.add(row * stride64) };

            assert_eq!(ptr1_row as usize % core::mem::align_of::<CompAlign1>(), 0);
            assert_eq!(ptr16_row as usize % core::mem::align_of::<CompAlign16>(), 0);
            assert_eq!(ptr64_row as usize % core::mem::align_of::<CompAlign64>(), 0);

            // Mutate each component via raw pointer
            unsafe {
                let c1 = &mut *(ptr1_row as *mut CompAlign1);
                c1.val = c1.val.wrapping_add(1);

                let c16 = &mut *(ptr16_row as *mut CompAlign16);
                c16.val[0] += 500;

                let c64 = &mut *(ptr64_row as *mut CompAlign64);
                c64.val[0] += 1000;
            }

            ITER_VERIFIED_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // Test iterator reset capability: reset and verify values reflect the mutation
        let reset_res = unsafe { host_iter_reset(iter) };
        assert_eq!(reset_res, FfiResult::OK);

        let re_count = unsafe { host_iter_next_chunk(iter) };
        assert_eq!(re_count, count);

        let re_p1 = unsafe { host_iter_column_readonly(iter, 0) };
        assert_eq!(re_p1, p1 as *const u8);
    }

    FfiResult::OK
}

#[test]
fn test_adversarial_chunk_pointer_arithmetic_and_reset() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();

    // Register CompAlign1, CompAlign16, CompAlign64
    macro_rules! register_type {
        ($t:ty, $name:literal) => {{
            let name_cstr = concat!($name, "\0").as_bytes();
            let desc = ModComponentDescriptor::new(
                name_cstr.as_ptr() as *const core::ffi::c_char,
                ComponentUuid::from_name($name),
                core::mem::size_of::<$t>(),
                core::mem::align_of::<$t>(),
            );
            unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            };
        }};
    }
    register_type!(CompAlign1, "ChunkIterComp1");
    register_type!(CompAlign16, "ChunkIterComp16");
    register_type!(CompAlign64, "ChunkIterComp64");

    // Spawn 8 entities
    let mut command_queue = CommandQueue::default();
    let world_reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let mut entities = Vec::new();
    for i in 0..8 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        entities.push(ffi_e);

        let c1 = CompAlign1 { val: i as u8 };
        let c16 = CompAlign16 {
            val: [i as u64, i as u64 * 2],
        };
        let c64 = CompAlign64 { val: [i as u64; 8] };

        macro_rules! insert_val {
            ($c:ident, $t:ty, $name:literal) => {{
                let name_cstr = concat!($name, "\0").as_bytes();
                unsafe {
                    host_cmd_insert_component(
                        raw_queue,
                        ffi_e,
                        name_cstr.as_ptr() as *const core::ffi::c_char,
                        &$c as *const $t as *const u8,
                        core::mem::size_of::<$t>(),
                    );
                }
            }};
        }
        insert_val!(c1, CompAlign1, "ChunkIterComp1");
        insert_val!(c16, CompAlign16, "ChunkIterComp16");
        insert_val!(c64, CompAlign64, "ChunkIterComp64");
    }
    command_queue.apply(app.world_mut());

    let sys = DynamicCSystem::new(
        "pointer_arith_verifier".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "ChunkIterComp1".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "ChunkIterComp16".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "ChunkIterComp64".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ],
        dynamic_chunk_pointer_arithmetic_verifier,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    app.add_systems(Update, sys);

    ITER_VERIFIED_COUNT.store(0, Ordering::SeqCst);
    app.update();

    assert_eq!(ITER_VERIFIED_COUNT.load(Ordering::SeqCst), 8);

    // Verify mutated values in Bevy World
    let reg = app.world().resource::<ModRegistry>();
    let id1 = reg.get_by_name("ChunkIterComp1").unwrap().component_id;
    let id16 = reg.get_by_name("ChunkIterComp16").unwrap().component_id;
    let id64 = reg.get_by_name("ChunkIterComp64").unwrap().component_id;

    for (i, &ffi_e) in entities.iter().enumerate() {
        let bevy_e = Entity::from_bits(ffi_e.0);
        let e_ref = app.world().entity(bevy_e);

        let v1 = unsafe { *(e_ref.get_by_id(id1).unwrap().as_ptr() as *const CompAlign1) };
        let v16 = unsafe { *(e_ref.get_by_id(id16).unwrap().as_ptr() as *const CompAlign16) };
        let v64 = unsafe { *(e_ref.get_by_id(id64).unwrap().as_ptr() as *const CompAlign64) };

        assert_eq!(v1.val, (i as u8).wrapping_add(1));
        assert_eq!(v16.val[0], (i as u64) + 500);
        assert_eq!(v64.val[0], (i as u64) + 1000);
    }
}

// ============================================================================
// Focus 3: Zero-Sized Types (ZST) Stress Test
// ============================================================================

static ZST_CHUNK_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn zst_system_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        // Col 0: ZST align 1, Col 1: ZST align 16, Col 2: CompAlign4
        let p_zst1 = unsafe { host_iter_column_readonly(iter, 0) };
        let p_zst16 = unsafe { host_iter_column_mutable(iter, 1) };
        let p_comp4 = unsafe { host_iter_column_mutable(iter, 2) };

        assert!(!p_zst1.is_null());
        assert!(!p_zst16.is_null());
        assert!(!p_comp4.is_null());

        let stride_zst1 = unsafe { host_iter_column_stride(iter, 0) };
        let stride_zst16 = unsafe { host_iter_column_stride(iter, 1) };
        let stride_comp4 = unsafe { host_iter_column_stride(iter, 2) };

        assert_eq!(stride_zst1, 0, "ZST stride must be 0");
        assert_eq!(stride_zst16, 0, "ZST stride must be 0");
        assert_eq!(stride_comp4, 4);

        ZST_CHUNK_COUNT.fetch_add(count, Ordering::Relaxed);
    }
    FfiResult::OK
}

#[test]
fn test_adversarial_zst_components_lifecycle_and_iteration() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();

    // Register ZSTs: size 0, align 1 and size 0, align 16
    let desc_zst1 = ModComponentDescriptor::new(
        c"ZstAlign1".as_ptr(),
        ComponentUuid::from_name("ZstAlign1"),
        0,
        1,
    );
    let desc_zst16 = ModComponentDescriptor::new(
        c"ZstAlign16".as_ptr(),
        ComponentUuid::from_name("ZstAlign16"),
        0,
        16,
    );
    let desc_c4 = ModComponentDescriptor::new(
        c"PayloadC4".as_ptr(),
        ComponentUuid::from_name("PayloadC4"),
        4,
        4,
    );

    unsafe {
        assert_eq!(
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_zst1
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_zst16
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_register_component(
                &mut init_ctx as *mut ModInitContext as *mut c_void,
                &desc_c4
            ),
            FfiResult::OK
        );
    }

    // Spawn 5 entities possessing the ZST components and payload
    let mut command_queue = CommandQueue::default();
    let world_reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    for i in 0..5 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

        // For ZST, data can be non-null dangling pointer or null with len=0
        let dummy_byte = 0u8;
        let res1 = unsafe {
            host_cmd_insert_component(raw_queue, ffi_e, c"ZstAlign1".as_ptr(), &dummy_byte, 0)
        };
        assert_eq!(res1, FfiResult::OK);

        let res16 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"ZstAlign16".as_ptr(),
                core::ptr::null(),
                0,
            )
        };
        assert_eq!(res16, FfiResult::OK);

        let payload = i as u32 * 7;
        let resc4 = unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"PayloadC4".as_ptr(),
                &payload as *const u32 as *const u8,
                4,
            )
        };
        assert_eq!(resc4, FfiResult::OK);
    }
    command_queue.apply(app.world_mut());

    // Register system querying both ZSTs and Payload
    let sys = DynamicCSystem::new(
        "zst_system".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "ZstAlign1".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "ZstAlign16".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "PayloadC4".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ],
        zst_system_callback,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    app.add_systems(Update, sys);

    ZST_CHUNK_COUNT.store(0, Ordering::SeqCst);
    app.update();

    assert_eq!(ZST_CHUNK_COUNT.load(Ordering::SeqCst), 5);
}

// ============================================================================
// Focus 4: Multi-Archetype / Multi-Chunk Contiguity Verification
// ============================================================================

static MULTI_CHUNK_ENTITY_RECORDS: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());

unsafe extern "C" fn multi_chunk_collector_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        let p_shared = unsafe { host_iter_column_readonly(iter, 0) };
        assert!(!p_shared.is_null());
        let stride = unsafe { host_iter_column_stride(iter, 0) };
        assert_eq!(stride, 4);

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null());

        for row in 0..count {
            let entity_id = unsafe { (*entities_ptr.add(row)).0 };
            let val = unsafe { *(p_shared.add(row * stride) as *const u32) };
            MULTI_CHUNK_ENTITY_RECORDS
                .lock()
                .unwrap()
                .push((entity_id, val));
        }
    }
    FfiResult::OK
}

#[test]
fn test_adversarial_multi_archetype_chunks_contiguous_traversal() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx = ModInitContext::with_world(
        Arc::new(app.world().resource::<ModRegistry>().clone()),
        Arc::clone(&is_active),
        app.world_mut() as *mut World,
    );
    let _guard = init_ctx.make_active();

    // Register SharedComp, MarkerA, MarkerB, MarkerC
    let desc_shared = ModComponentDescriptor::new(
        c"SharedComp".as_ptr(),
        ComponentUuid::from_name("SharedComp"),
        4,
        4,
    );
    let desc_ma = ModComponentDescriptor::new(
        c"MarkerA".as_ptr(),
        ComponentUuid::from_name("MarkerA"),
        4,
        4,
    );
    let desc_mb = ModComponentDescriptor::new(
        c"MarkerB".as_ptr(),
        ComponentUuid::from_name("MarkerB"),
        4,
        4,
    );
    let desc_mc = ModComponentDescriptor::new(
        c"MarkerC".as_ptr(),
        ComponentUuid::from_name("MarkerC"),
        4,
        4,
    );

    unsafe {
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_shared,
        );
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_ma,
        );
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_mb,
        );
        host_register_component(
            &mut init_ctx as *mut ModInitContext as *mut c_void,
            &desc_mc,
        );
    }

    // Spawn 3 groups of entities into 3 different tables:
    // Group 1 (3 entities): [SharedComp, MarkerA]
    // Group 2 (4 entities): [SharedComp, MarkerB]
    // Group 3 (5 entities): [SharedComp, MarkerC]
    let mut command_queue = CommandQueue::default();
    let world_reg = app.world().resource::<ModRegistry>().clone();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let mut expected_map = std::collections::HashMap::new();

    // Group 1
    for i in 0..3 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let val: u32 = 100 + i;
        expected_map.insert(ffi_e.0, val);
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"SharedComp".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"MarkerA".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
        }
    }

    // Group 2
    for i in 0..4 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let val: u32 = 200 + i;
        expected_map.insert(ffi_e.0, val);
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"SharedComp".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"MarkerB".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
        }
    }

    // Group 3
    for i in 0..5 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let val: u32 = 300 + i;
        expected_map.insert(ffi_e.0, val);
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"SharedComp".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"MarkerC".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
        }
    }
    command_queue.apply(app.world_mut());

    // Register DynamicCSystem querying SharedComp (will visit 3 separate archetype tables)
    let sys = DynamicCSystem::new(
        "multi_chunk_collector".to_string(),
        vec![ComponentAccessDescOwned {
            name: "SharedComp".to_string(),
            mode: ComponentAccessMode::READ,
        }],
        multi_chunk_collector_system,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    app.add_systems(Update, sys);

    MULTI_CHUNK_ENTITY_RECORDS.lock().unwrap().clear();
    app.update();

    let records = MULTI_CHUNK_ENTITY_RECORDS.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        12,
        "Must visit all 12 entities across 3 tables"
    );

    for (e_id, val) in records {
        let expected = expected_map
            .get(&e_id)
            .expect("Entity must be in expected map");
        assert_eq!(val, *expected, "Entity value mismatch in chunk traversal");
    }
}

// ============================================================================
// Focus 5: Native Bevy Change Detection Parity (Row Ticks vs Bulk Ticks)
// ============================================================================

unsafe extern "C" fn fine_grained_change_marker_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        // Use readonly pointer (does NOT touch change ticks)
        let _ptr = unsafe { host_iter_column_readonly(iter, 0) };

        // Mark only ROW 1 as changed
        for row in 0..count {
            if row == 1 {
                let res = unsafe { host_iter_mark_changed(iter, 0, row) };
                assert_eq!(res, FfiResult::OK);
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_adversarial_native_bevy_change_detection_parity() {
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
        c"TickTrackComp".as_ptr(),
        ComponentUuid::from_name("TickTrackComp"),
        4,
        4,
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let mut entities = Vec::new();
    for i in 0..4 {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        let val = i as u32;
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                ffi_e,
                c"TickTrackComp".as_ptr(),
                &val as *const u32 as *const u8,
                4,
            );
        }
        entities.push(Entity::from_bits(ffi_e.0));
    }
    command_queue.apply(&mut world);

    let comp_id = world
        .resource::<ModRegistry>()
        .get_by_name("TickTrackComp")
        .unwrap()
        .component_id;

    // Advance world change tick to simulate frame 1
    let frame1_tick = world.change_tick();

    // Create dynamic system that only marks row 1 as changed
    let mut sys = DynamicCSystem::new(
        "tick_marker".to_string(),
        vec![ComponentAccessDescOwned {
            name: "TickTrackComp".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        fine_grained_change_marker_system,
        core::ptr::null_mut(),
        Arc::new(world_reg),
    );
    sys.initialize(&mut world);

    // Advance world change tick for frame 2
    world.increment_change_tick();
    let frame2_tick = world.change_tick();
    unsafe {
        sys.run_unsafe((), world.as_unsafe_world_cell()).unwrap();
    }

    // Inspect change ticks directly via Bevy ECS entity component ticks accessor!
    let ticks_e0 = world
        .entity(entities[0])
        .get_change_ticks_by_id(comp_id)
        .unwrap();
    let ticks_e1 = world
        .entity(entities[1])
        .get_change_ticks_by_id(comp_id)
        .unwrap();
    let ticks_e2 = world
        .entity(entities[2])
        .get_change_ticks_by_id(comp_id)
        .unwrap();
    let ticks_e3 = world
        .entity(entities[3])
        .get_change_ticks_by_id(comp_id)
        .unwrap();

    // Row 1 (entities[1]) MUST have changed_tick == frame2_tick
    assert_eq!(
        ticks_e1.changed, frame2_tick,
        "Row 1 was explicitly marked changed; must match frame 2 tick"
    );

    // Other rows (0, 2, 3) must NOT have been updated to frame2_tick!
    assert_ne!(
        ticks_e0.changed, frame2_tick,
        "Row 0 was NOT marked changed"
    );
    assert_ne!(
        ticks_e2.changed, frame2_tick,
        "Row 2 was NOT marked changed"
    );
    assert_ne!(
        ticks_e3.changed, frame2_tick,
        "Row 3 was NOT marked changed"
    );

    assert!(
        ticks_e1.is_changed(frame1_tick, frame2_tick),
        "Bevy is_changed must return true for entity 1"
    );
    assert!(
        !ticks_e0.is_changed(frame1_tick, frame2_tick),
        "Bevy is_changed must return false for entity 0"
    );
}

// ============================================================================
// Focus 6: Allocator Isolation & Foreign Stack Memory Decoupling (Rule 2)
// ============================================================================

#[test]
fn test_adversarial_foreign_stack_memory_overwrite_decoupling() {
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
        c"StackDecoupleComp".as_ptr(),
        ComponentUuid::from_name("StackDecoupleComp"),
        32,
        16,
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };

    // Push component from a temporary stack buffer, then immediately smash the stack
    fn insert_and_smash_stack(queue: *mut RawCommandQueue, entity: FfiEntity) {
        let mut stack_buf = [0x55u8; 32];
        stack_buf[0] = 0xAA;
        stack_buf[31] = 0xBB;

        let res = unsafe {
            host_cmd_insert_component(
                queue,
                entity,
                c"StackDecoupleComp".as_ptr(),
                stack_buf.as_ptr(),
                32,
            )
        };
        assert_eq!(res, FfiResult::OK);

        // Intentionally overwrite stack memory with zeroes
        stack_buf.fill(0x00);
    }

    insert_and_smash_stack(raw_queue, ffi_e);

    // Apply command queue to World
    command_queue.apply(&mut world);

    // Verify Bevy World has the unpoisoned original data [0xAA, 0x55..., 0xBB]
    let comp_id = world
        .resource::<ModRegistry>()
        .get_by_name("StackDecoupleComp")
        .unwrap()
        .component_id;
    let ptr = world
        .entity(Entity::from_bits(ffi_e.0))
        .get_by_id(comp_id)
        .unwrap()
        .as_ptr();

    let slice = unsafe { core::slice::from_raw_parts(ptr, 32) };
    assert_eq!(slice[0], 0xAA);
    assert_eq!(slice[1], 0x55);
    assert_eq!(slice[31], 0xBB);
}

#[test]
fn test_adversarial_discarded_command_queue_memory_leak_defense() {
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
        c"HeavyAllocComp".as_ptr(),
        ComponentUuid::from_name("HeavyAllocComp"),
        256,
        32,
    );
    unsafe { host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc) };

    // Queue 500 insertions of 256-byte components, then drop the CommandQueue WITHOUT applying
    {
        let mut command_queue = CommandQueue::default();
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut bridge =
            CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let dummy_data = [0x77u8; 256];
        for _ in 0..500 {
            let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"HeavyAllocComp".as_ptr(),
                    dummy_data.as_ptr(),
                    256,
                );
            }
        }
        // command_queue is dropped here without apply
    }
    // If AlignedBuffer leaked or panicked on drop, this would fail or leak under Valgrind/Miri
}

// ============================================================================
// Focus 7: Drop Trampoline Lifecycle, Unload Safety, and Slot Reuse Challenge
// ============================================================================

static ADVERSARIAL_DROP_INVOCATIONS_A: AtomicUsize = AtomicUsize::new(0);
static ADVERSARIAL_DROP_INVOCATIONS_B: AtomicUsize = AtomicUsize::new(0);
static DROP_TEST_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" fn foreign_drop_hook_mod_a(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    ADVERSARIAL_DROP_INVOCATIONS_A.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn foreign_drop_hook_mod_b(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    ADVERSARIAL_DROP_INVOCATIONS_B.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn test_adversarial_drop_trampoline_unload_and_slot_reuse_behavior() {
    let _test_lock = DROP_TEST_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();
    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    // Mod A initialization
    let is_active_a = Arc::new(AtomicBool::new(true));
    let mut init_ctx_a = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_a),
        &mut world as *mut World,
    );
    let _guard_a = init_ctx_a.make_active();

    let desc_a = ModComponentDescriptor::new(
        c"DropCompA".as_ptr(),
        ComponentUuid::from_name("DropCompA"),
        8,
        4,
    )
    .with_drop(foreign_drop_hook_mod_a);
    let res_a = unsafe {
        host_register_component(
            &mut init_ctx_a as *mut ModInitContext as *mut c_void,
            &desc_a,
        )
    };
    assert_eq!(res_a, FfiResult::OK);

    // Spawn entity possessing DropCompA
    let mut command_queue = CommandQueue::default();
    let world_reg = world.resource::<ModRegistry>().clone();
    let mut bridge =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let e_a = unsafe { host_cmd_spawn_empty(raw_queue) };
    let payload_a = [11u32, 22u32];
    unsafe {
        host_cmd_insert_component(
            raw_queue,
            e_a,
            c"DropCompA".as_ptr(),
            payload_a.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    // Despawn entity A while Mod A is active: drop hook A MUST be called
    let drops_a_start = ADVERSARIAL_DROP_INVOCATIONS_A.load(Ordering::SeqCst);
    world.despawn(Entity::from_bits(e_a.0));
    assert_eq!(
        ADVERSARIAL_DROP_INVOCATIONS_A.load(Ordering::SeqCst),
        drops_a_start + 1
    );

    // Now spawn another entity for Mod A
    let mut bridge2 =
        CommandQueueBridge::new(&mut command_queue, world.entity_allocator(), &world_reg);
    let raw_queue2 = (&mut bridge2 as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
    let e_a2 = unsafe { host_cmd_spawn_empty(raw_queue2) };
    unsafe {
        host_cmd_insert_component(
            raw_queue2,
            e_a2,
            c"DropCompA".as_ptr(),
            payload_a.as_ptr() as *const u8,
            8,
        );
    }
    command_queue.apply(&mut world);

    // Unload Mod A (is_active_a = false)
    is_active_a.store(false, Ordering::Release);

    // Despawn e_a2 while Mod A is inactive: drop hook A MUST NOT be called!
    world.despawn(Entity::from_bits(e_a2.0));
    assert_eq!(
        ADVERSARIAL_DROP_INVOCATIONS_A.load(Ordering::SeqCst),
        drops_a_start + 1,
        "Drop hook A must not be called after Mod A is unloaded"
    );

    reset_drop_trampolines_for_testing();
}

#[test]
fn test_adversarial_drop_trampoline_slot_reuse_cross_talk() {
    let _test_lock = DROP_TEST_LOCK.lock().unwrap();
    reset_drop_trampolines_for_testing();
    let mut world = World::new();
    let registry = Arc::new(ModRegistry::new());
    world.insert_resource((*registry).clone());

    // 1. Fill all 64 slots.
    // First, let's register 64 distinct components with drop hooks.
    let is_active_base = Arc::new(AtomicBool::new(true));
    let mut init_ctx_base = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_base),
        &mut world as *mut World,
    );
    let _guard = init_ctx_base.make_active();

    // Register Mod A's component (which will take one slot)
    let is_active_a = Arc::new(AtomicBool::new(true));
    let mut init_ctx_a = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_a),
        &mut world as *mut World,
    );
    let _guard_a = init_ctx_a.make_active();

    let desc_a = ModComponentDescriptor::new(
        c"CompA_SlotTest".as_ptr(),
        ComponentUuid::from_name("CompA_SlotTest"),
        8,
        4,
    )
    .with_drop(foreign_drop_hook_mod_a);
    let res_a = unsafe {
        host_register_component(
            &mut init_ctx_a as *mut ModInitContext as *mut c_void,
            &desc_a,
        )
    };
    assert_eq!(res_a, FfiResult::OK);

    println!("res_a: {:?}", res_a);

    // Spawn an entity with CompA_SlotTest
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
    for i in 0..(host_runtime::MAX_DROP_TRAMPOLINES - 1) {
        let name_str = format!("DummyDropSlotComp_{}\0", i);
        dummy_names.push(name_str);
    }

    let _guard_fill = init_ctx_base.make_active();
    for name_str in &dummy_names {
        let desc = ModComponentDescriptor::new(
            name_str.as_ptr() as *const core::ffi::c_char,
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
    drop(_guard_fill);

    // Now, all 64 slots are full!
    // Unload Mod A (is_active_a = false)
    is_active_a.store(false, Ordering::Release);

    // Now Mod B attempts to register a component with drop_fn_b.
    // Since all 64 slots are occupied by distinct ComponentUuids, Mod B must be rejected!
    let is_active_b = Arc::new(AtomicBool::new(true));
    let mut init_ctx_b = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_b),
        &mut world as *mut World,
    );
    let _guard_b = init_ctx_b.make_active();

    let desc_b = ModComponentDescriptor::new(
        c"CompB_ReusedSlot".as_ptr(),
        ComponentUuid::from_name("CompB_ReusedSlot"),
        8,
        4,
    )
    .with_drop(foreign_drop_hook_mod_b);
    let res_b = unsafe {
        host_register_component(
            &mut init_ctx_b as *mut ModInitContext as *mut c_void,
            &desc_b,
        )
    };
    println!("res_b: {:?}", res_b);
    assert_eq!(
        res_b,
        FfiResult::INTERNAL_ERROR,
        "Alien component cannot steal slot when pool is saturated"
    );

    // Now, entity e_a (which has CompA_SlotTest) is STILL in the world.
    // Despawn e_a while Mod A is unloaded. Neither Mod A nor Mod B should be invoked!
    ADVERSARIAL_DROP_INVOCATIONS_A.store(0, Ordering::SeqCst);
    ADVERSARIAL_DROP_INVOCATIONS_B.store(0, Ordering::SeqCst);

    let comp_a_id = world
        .resource::<ModRegistry>()
        .get_by_name("CompA_SlotTest")
        .unwrap()
        .component_id;
    println!(
        "e_a has CompA_SlotTest before despawn? {}",
        world
            .entity(Entity::from_bits(e_a.0))
            .contains_id(comp_a_id)
    );
    println!(
        "comp_a drop fn is_some? {}",
        world
            .components()
            .get_info(comp_a_id)
            .unwrap()
            .drop()
            .is_some()
    );

    world.despawn(Entity::from_bits(e_a.0));

    let drops_a = ADVERSARIAL_DROP_INVOCATIONS_A.load(Ordering::SeqCst);
    let drops_b = ADVERSARIAL_DROP_INVOCATIONS_B.load(Ordering::SeqCst);

    println!(
        "Slot reuse test results: drops_a={}, drops_b={}",
        drops_a, drops_b
    );
    assert_eq!(
        drops_a, 0,
        "Mod A drop hook must not be called after Mod A is unloaded"
    );
    assert_eq!(
        drops_b, 0,
        "Mod B drop hook must not be called on Mod A's component"
    );

    // Verify legitimate mod reload with same UUID succeeds and rebinds trampoline slot
    let is_active_a_reload = Arc::new(AtomicBool::new(true));
    let mut init_ctx_a_reload = ModInitContext::with_world(
        Arc::clone(&registry),
        Arc::clone(&is_active_a_reload),
        &mut world as *mut World,
    );
    let _guard_a_reload = init_ctx_a_reload.make_active();
    let res_a_reload = unsafe {
        host_register_component(
            &mut init_ctx_a_reload as *mut ModInitContext as *mut c_void,
            &desc_a,
        )
    };
    assert_eq!(
        res_a_reload,
        FfiResult::OK,
        "Reloading mod with identical ComponentUuid must succeed"
    );

    // Spawn entity under reloaded Mod A and despawn to verify drop hook is active
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

    ADVERSARIAL_DROP_INVOCATIONS_A.store(0, Ordering::SeqCst);
    world.despawn(Entity::from_bits(e_r.0));
    assert_eq!(
        ADVERSARIAL_DROP_INVOCATIONS_A.load(Ordering::SeqCst),
        1,
        "Reloaded mod drop hook must execute on entity despawn"
    );

    reset_drop_trampolines_for_testing();
}
