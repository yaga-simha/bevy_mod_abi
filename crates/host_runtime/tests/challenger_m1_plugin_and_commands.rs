#![allow(clippy::default_constructed_unit_structs, clippy::clone_on_copy)]

use abi_contract::FfiResult;
use bevy_app::App;
use bevy_ecs::component::Component;
use host_runtime::{
    commands::CommandQueueBridge, host_cmd_insert_component, host_cmd_spawn_empty, AbiModPlugin,
    ModAppExt, ModHost, ModRegistry,
};
use mod_derive::ModExposable;

#[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq)]
#[repr(C)]
struct Health {
    hp: f32,
    max_hp: f32,
}

#[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq)]
#[repr(C)]
struct Velocity {
    vx: f32,
    vy: f32,
}

#[test]
fn test_abi_mod_plugin_traits() {
    let plugin = AbiModPlugin::default();
    let debug_str = format!("{:?}", plugin);
    assert_eq!(debug_str, "AbiModPlugin");

    // Test Copy
    let plugin_copy = plugin;
    // Test Clone
    let plugin_clone = plugin.clone();

    let mut app = App::new();
    app.add_plugins(plugin_copy);
    assert!(app.world().get_resource::<ModRegistry>().is_some());
    assert!(app.world().get_resource::<ModHost>().is_some());

    // Adding plugin or running update works
    app.update();
    let _ = plugin_clone;
}

#[test]
fn test_abi_mod_plugin_default_construction_and_app_lifecycle() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());

    // Verify resources were inserted
    assert!(
        app.world().contains_resource::<ModRegistry>(),
        "ModRegistry must be initialized by AbiModPlugin::default()"
    );
    assert!(
        app.world().contains_resource::<ModHost>(),
        "ModHost must be initialized by AbiModPlugin::default()"
    );

    // Register mod components via ModAppExt
    app.register_mod_component::<Health>();
    app.register_mod_component::<Velocity>();

    {
        let registry = app.world().resource::<ModRegistry>();
        assert!(registry.get_by_name("Health").is_some());
        assert!(registry.get_by_name("Velocity").is_some());
        assert_eq!(
            registry.get_by_name("Health").unwrap().size,
            core::mem::size_of::<Health>()
        );
    }

    // Run multiple update cycles
    for _ in 0..100 {
        app.update();
    }
}

#[test]
fn test_deferred_command_bridge_with_plugin_registered_types() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Health>();

    let mut queue = bevy_ecs::world::CommandQueue::default();
    let world = app.world_mut();

    let registry = world.resource::<ModRegistry>().clone();
    let cell = world.as_unsafe_world_cell();

    let mut bridge = CommandQueueBridge::new(&mut queue, cell.entity_allocator(), &registry);
    let raw_queue = &mut bridge as *mut _ as *mut abi_contract::RawCommandQueue;

    // Spawn entity deferred
    let spawned_ffi = unsafe { host_cmd_spawn_empty(raw_queue) };
    assert_ne!(spawned_ffi.0, 0);

    // Insert Health component deferred
    let health = Health {
        hp: 75.0,
        max_hp: 100.0,
    };
    let insert_res = unsafe {
        host_cmd_insert_component(
            raw_queue,
            spawned_ffi,
            c"Health".as_ptr(),
            &health as *const Health as *const u8,
            core::mem::size_of::<Health>(),
        )
    };
    assert_eq!(insert_res, FfiResult::OK);

    // Flush commands to World
    queue.apply(world);

    // Verify entity was spawned and component initialized correctly
    let bevy_entity = bevy_ecs::entity::Entity::from_bits(spawned_ffi.to_bits());
    let world_health = world.get::<Health>(bevy_entity);
    assert!(world_health.is_some());
    assert_eq!(world_health.unwrap(), &health);
}
