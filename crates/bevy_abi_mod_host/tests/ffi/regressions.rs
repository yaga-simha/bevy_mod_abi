//! Wave 1 FFI regressions (`S-04` padded struct insertion, `S-10` entity bit layout).

use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
use bevy_abi_mod_host_sys::{
    CommandQueueBridge, ModInitContext, host_cmd_insert_component, host_cmd_spawn_empty,
    host_register_component,
};
use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::{
    ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawCommandQueue,
};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicBool;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq, Eq)]
struct PaddedPair {
    tag: u8,
    // 7 padding bytes at offsets 1..8
    value: u64,
}

// ============================================================================
// S-04: Padded struct insertion with uninitialized padding bytes
// ============================================================================

#[test]
fn regression_s04_padded_struct_insert() {
    assert_eq!(core::mem::size_of::<PaddedPair>(), 16);
    assert_eq!(core::mem::align_of::<PaddedPair>(), 8);

    // Construct a `PaddedPair` leaving its 7 padding bytes genuinely uninitialized.
    let mut raw_slot = MaybeUninit::<PaddedPair>::uninit();
    unsafe {
        let base = raw_slot.as_mut_ptr();
        core::ptr::addr_of_mut!((*base).tag).write(0x5A);
        core::ptr::addr_of_mut!((*base).value).write(0xDEAD_BEEF_CAFE_BABEu64);
    }
    let raw_bytes_ptr = raw_slot.as_ptr().cast::<u8>();

    // 1. Static host-registered component insert path
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<PaddedPair>();

    let e_static = {
        let world = app.world_mut();
        let registry = world.resource::<ModRegistry>().clone();
        let mut queue = CommandQueue::default();
        let entity_bits = {
            let mut bridge =
                CommandQueueBridge::new(&mut queue, world.entity_allocator(), &registry);
            let bridge_ptr = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
            let e = unsafe { host_cmd_spawn_empty(bridge_ptr) };
            let rc = unsafe {
                host_cmd_insert_component(
                    bridge_ptr,
                    e,
                    c"PaddedPair".as_ptr(),
                    raw_bytes_ptr,
                    core::mem::size_of::<PaddedPair>(),
                )
            };
            assert_eq!(rc, FfiResult::OK);
            e
        };
        queue.apply(world);
        Entity::from_bits(entity_bits.to_bits())
    };

    let got = app
        .world()
        .get::<PaddedPair>(e_static)
        .expect("PaddedPair inserted via static command path");
    assert_eq!(got.tag, 0x5A);
    assert_eq!(got.value, 0xDEAD_BEEF_CAFE_BABEu64);

    // 2. Dynamic guest-registered component insert path
    {
        let world = app.world_mut();
        let reg_arc = Arc::new(world.resource::<ModRegistry>().clone());
        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx = ModInitContext::with_world(reg_arc, is_active, world as *mut World);
        let _guard = init_ctx.make_active();

        let desc = ModComponentDescriptor::new(
            c"DynamicPaddedPair".as_ptr(),
            ComponentUuid::from_name("DynamicPaddedPair"),
            16,
            8,
        );
        let rc = unsafe {
            host_register_component(
                (&mut init_ctx as *mut ModInitContext).cast::<c_void>(),
                &desc,
            )
        };
        assert_eq!(rc, FfiResult::OK);
    }

    let dyn_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("DynamicPaddedPair")
        .expect("DynamicPaddedPair registered")
        .component_id;

    let e_dyn = {
        let world = app.world_mut();
        let registry = world.resource::<ModRegistry>().clone();
        let mut queue = CommandQueue::default();
        let entity_bits = {
            let mut bridge =
                CommandQueueBridge::new(&mut queue, world.entity_allocator(), &registry);
            let bridge_ptr = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
            let e = unsafe { host_cmd_spawn_empty(bridge_ptr) };
            let rc = unsafe {
                host_cmd_insert_component(
                    bridge_ptr,
                    e,
                    c"DynamicPaddedPair".as_ptr(),
                    raw_bytes_ptr,
                    16,
                )
            };
            assert_eq!(rc, FfiResult::OK);
            e
        };
        queue.apply(world);
        Entity::from_bits(entity_bits.to_bits())
    };

    let entity_ref = app.world().entity(e_dyn);
    let untyped_ptr = entity_ref
        .get_by_id(dyn_comp_id)
        .expect("DynamicPaddedPair present on entity");
    unsafe {
        let base = untyped_ptr.as_ptr();
        assert_eq!(core::ptr::read(base), 0x5A);
        assert_eq!(
            core::ptr::read_unaligned(base.add(8).cast::<u64>()),
            0xDEAD_BEEF_CAFE_BABEu64
        );
    }
}

// ============================================================================
// S-10: Entity <-> FfiEntity bit-representation layout guard
// ============================================================================

#[test]
fn regression_s10_entity_bits_layout() {
    assert_eq!(
        core::mem::size_of::<Entity>(),
        core::mem::size_of::<FfiEntity>()
    );
    assert_eq!(
        core::mem::align_of::<Entity>(),
        core::mem::align_of::<FfiEntity>()
    );

    let mut world = World::new();
    let mut entities = Vec::new();
    for _ in 0..32 {
        entities.push(world.spawn_empty().id());
    }
    for e in entities.drain(..16).collect::<Vec<_>>() {
        world.despawn(e);
    }
    for _ in 0..32 {
        entities.push(world.spawn_empty().id());
    }

    for entity in entities {
        let raw_transmuted = unsafe { core::mem::transmute_copy::<Entity, u64>(&entity) };
        assert_eq!(raw_transmuted, entity.to_bits());
        let ffi = FfiEntity::from_bits(entity.to_bits());
        assert_eq!(ffi.to_bits(), raw_transmuted);
        assert_eq!(Entity::try_from_bits(ffi.to_bits()), Some(entity));
    }
}
