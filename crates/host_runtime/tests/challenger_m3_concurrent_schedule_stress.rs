mod common;

use bevy_app::App;
use bevy_ecs::ptr::OwningPtr;
use common::*;
use host_runtime::{AbiModPlugin, ModAppExt, ModRegistry};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct CModCustomData {
    pub mana: f32,
    pub level: i32,
}

#[test]
fn test_concurrent_dual_mod_schedule_stress() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Register host static exposable components required by both mods
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // 1. Load both mods simultaneously
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    // 2. Query component IDs from registry
    let (rust_comp_id, c_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();
        let r_meta = registry
            .get_by_name("RustModCustomData")
            .expect("RustModCustomData must be registered");
        let c_meta = registry
            .get_by_name("CModCustomData")
            .expect("CModCustomData must be registered");
        (r_meta.component_id, c_meta.component_id)
    };

    // 3. Spawn 3,000 entities across 3 groups (1,000 per group)
    const PER_GROUP: usize = 1_000;
    let mut group_a = Vec::with_capacity(PER_GROUP); // Rust dynamic only
    let mut group_b = Vec::with_capacity(PER_GROUP); // C dynamic only
    let mut group_c = Vec::with_capacity(PER_GROUP); // Both dynamic (shared table)

    // Group A: Rust only
    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_data = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        group_a.push(e);
    }

    // Group B: C only
    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_data = CModCustomData {
            mana: 20.0,
            level: 1,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        group_b.push(e);
    }

    // Group C: Both Rust and C on same entity (heterogeneous dynamic archetype table)
    for _ in 0..PER_GROUP {
        let e = app.world_mut().spawn_empty().id();
        let init_r = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let init_c = CModCustomData {
            mana: 20.0,
            level: 1,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_r, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(init_c, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        group_c.push(e);
    }

    // 4. Run 50 ticks of Bevy schedule with both mods actively querying and mutating
    for _ in 0..50 {
        app.update();
    }

    // 5. Verify mathematical precision across all entities:
    // Rust: +10.0 power, +1 shield per tick * 50 ticks = power: 600.0, shield: 100
    // C: +5.0 mana, +1 level per tick * 50 ticks = mana: 270.0, level: 51

    for &e in &group_a {
        let data = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data.power, 600.0, "Group A power must be 600.0");
        assert_eq!(data.shield, 100, "Group A shield must be 100");
    }

    for &e in &group_b {
        let data = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 270.0, "Group B mana must be 270.0");
        assert_eq!(data.level, 51, "Group B level must be 51");
    }

    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 600.0, "Group C power must be 600.0");
        assert_eq!(data_r.shield, 100, "Group C shield must be 100");
        assert_eq!(data_c.mana, 270.0, "Group C mana must be 270.0");
        assert_eq!(data_c.level, 51, "Group C level must be 51");
    }

    // 6. Asymmetric Unload: Unload Rust mod first, run 10 more ticks
    app.unload_mod(rust_mod_id).expect("Rust mod unload failed");

    for _ in 0..10 {
        app.update();
    }

    // Post-Rust-unload verification:
    // Rust values must be FROZEN (power: 600.0, shield: 100)
    // C values must ADVANCE by 10 more ticks (mana: 270 + 50 = 320.0, level: 51 + 10 = 61)
    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data_r.power, 600.0,
            "Rust values must remain frozen after Rust mod unload"
        );
        assert_eq!(
            data_r.shield, 100,
            "Rust values must remain frozen after Rust mod unload"
        );
        assert_eq!(
            data_c.mana, 320.0,
            "C values must advance after Rust mod unload"
        );
        assert_eq!(
            data_c.level, 61,
            "C values must advance after Rust mod unload"
        );
    }

    // 7. Unload C mod
    app.unload_mod(c_mod_id).expect("C mod unload failed");

    for _ in 0..10 {
        app.update();
    }

    // Both frozen
    for &e in &group_c {
        let data_r = unsafe {
            *(app
                .world()
                .entity(e)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(app.world().entity(e).get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data_r.power, 600.0,
            "Rust values must remain frozen after both mods unloaded"
        );
        assert_eq!(
            data_c.mana, 320.0,
            "C values must remain frozen after both mods unloaded"
        );
    }

    // 8. Despawn all 3,000 entities
    for e in group_a.into_iter().chain(group_b).chain(group_c) {
        app.world_mut().despawn(e);
    }
}
