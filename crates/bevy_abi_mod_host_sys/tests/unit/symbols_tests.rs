use super::*;
use crate::loader::mod_host::ModId;

#[test]
fn symbol_table_monotonic_dedup_and_lock_free_resolve() {
    let table = BamSymbolTable::new();
    assert!(table.is_empty());
    assert_eq!(table.intern(""), Err(FfiResult::INVALID_ARGUMENT));
    assert_eq!(table.resolve_symbol_str(BamSymbol::INVALID), None);
    assert_eq!(table.resolve_symbol_str(BamSymbol::from_raw(0)), None);

    let s0 = table.intern("combat::crit").expect("intern s0");
    let s1 = table.intern("combat::burn").expect("intern s1");
    let s0_dup = table.intern("combat::crit").expect("dedup s0");
    assert_eq!(s0, BamSymbol::from_raw(0));
    assert_eq!(s1, BamSymbol::from_raw(1));
    assert_eq!(s0_dup, s0);
    assert_eq!(table.len(), 2);

    // Borrow `&str` directly while concurrently interning additional symbols.
    let borrowed = table.resolve_symbol_str(s0).expect("resolve s0");
    let s2 = table.intern("ai::patrol").expect("intern s2");
    assert_eq!(borrowed, "combat::crit");
    assert_eq!(table.resolve_symbol_str(s1), Some("combat::burn"));
    assert_eq!(table.resolve_symbol_str(s2), Some("ai::patrol"));
    assert_eq!(table.lookup("ai::patrol"), Some(s2));
    assert_eq!(table.lookup("missing"), None);
}

#[test]
fn named_objects_uuid_binding_rebind_and_mod_scoped_stripping() {
    let mut world = World::new();
    let e1 = world.spawn(ModScoped(ModId(7))).id();
    let e2 = world.spawn_empty().id();

    let uuid = ModNamedObjects::bind_persistent(&mut world, "prefab::Hero", e1)
        .expect("bind_persistent succeeds");
    assert_eq!(uuid, ComponentUuid::from_parts("prefab::Hero", 0, 0, 0));
    assert!(
        !world.entity(e1).contains::<ModScoped>(),
        "bind_persistent must strip ModScoped"
    );

    let objs = world.resource::<ModNamedObjects>();
    assert_eq!(objs.get("prefab::Hero"), Some(e1));
    assert_eq!(objs.get_by_uuid(uuid), Some(e1));
    assert_eq!(objs.get_alive(&world, "prefab::Hero"), Some(e1));
    assert_eq!(objs.get_name_by_uuid(uuid).as_deref(), Some("prefab::Hero"));

    // Rebind to e2.
    objs.bind("prefab::Hero", e2);
    assert_eq!(objs.get("prefab::Hero"), Some(e2));
    assert_eq!(objs.get_by_uuid(uuid), Some(e2));

    // Despawning e2 makes `get_alive` return None and `prune_despawned` cleans it up.
    assert!(world.despawn(e2));
    let objs = world.resource::<ModNamedObjects>();
    assert_eq!(objs.get_alive(&world, "prefab::Hero"), None);
    assert_eq!(objs.prune_despawned(&world), 1);
    assert!(objs.is_empty());

    // `get_or_spawn_persistent` materializes a fresh persistent entity.
    let spawned = ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::Boss");
    assert!(world.entities().contains(spawned));
    assert!(!world.entity(spawned).contains::<ModScoped>());
    assert_eq!(
        ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::Boss"),
        spawned
    );
}

#[test]
fn world_drop_cleans_up_registry_slot_without_leaks() {
    let world_id = {
        let mut world = World::new();
        world.insert_resource(ModRegistry::new());
        let id = ensure_symbol_tables_in_world(&mut world);
        assert!(lookup_symbol_table_by_id(id).is_some());
        assert!(lookup_named_objects_by_id(id).is_some());
        id
    };
    assert!(lookup_symbol_table_by_id(world_id).is_none());
    assert!(lookup_named_objects_by_id(world_id).is_none());
}

#[test]
fn named_object_survives_full_excise_unload_and_symbols_persist_across_reload() {
    use crate::drop_trampoline::WorldDropGuard;
    use crate::ffi::register::{LoadContext, host_load_host_ctx};
    use crate::ffi::v03_ext::{host_bind_named_object, host_intern_symbol};
    use crate::loader::mod_host::{LoadedMod, ModHost, excise_and_unload_mod_in_world};
    use crate::system::ModQuarantine;
    use bevy_abi_mod_sys::{BamInstance, FfiEntity, StrView};
    use core::sync::atomic::AtomicBool;

    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModQuarantine::new());

    let active_1 = Arc::new(AtomicBool::new(true));
    let mod_1 = LoadedMod::register_mock_for_test(
        &mut world,
        "prefab_unload_mod",
        Arc::clone(&active_1),
        noop_shutdown,
        None,
        core::ptr::null_mut(),
    );

    let bound_ent = world.spawn(ModScoped(mod_1)).id();
    let unbound_ent = world.spawn(ModScoped(mod_1)).id();

    let mut sym_before = BamSymbol::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, active_1, world_ptr, Some(mod_1));
        // SAFETY: `load_ctx` is live.
        let host_ctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
        assert_eq!(
            // SAFETY: Valid `host_ctx`, UTF-8 string, and entity bits.
            unsafe {
                host_bind_named_object(
                    host_ctx,
                    StrView::from_str("world::spawn_point"),
                    FfiEntity::from_bits(bound_ent.to_bits()),
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            // SAFETY: Valid `host_ctx`, UTF-8 string, and out pointer.
            unsafe {
                host_intern_symbol(
                    host_ctx,
                    StrView::from_str("event::portal"),
                    &raw mut sym_before,
                )
            },
            FfiResult::OK
        );
        load_ctx.finish_init();
    }

    // Full unload despawns unbound ModScoped entities, while bound entity is stripped of
    // ModScoped and survives!
    excise_and_unload_mod_in_world(&mut world, mod_1, |_| {}).expect("unload succeeds");
    assert!(world.entities().contains(bound_ent));
    assert!(world.get::<ModScoped>(bound_ent).is_none());
    assert!(!world.entities().contains(unbound_ent));

    // Reloaded mod sees the exact same symbol ID and named entity!
    let active_2 = Arc::new(AtomicBool::new(true));
    let mod_2 = LoadedMod::register_mock_for_test(
        &mut world,
        "prefab_unload_mod",
        Arc::clone(&active_2),
        noop_shutdown,
        None,
        core::ptr::null_mut(),
    );
    let mut sym_after = BamSymbol::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, active_2, world_ptr, Some(mod_2));
        // SAFETY: `load_ctx` is live.
        let host_ctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
        assert_eq!(
            // SAFETY: Valid `host_ctx`, UTF-8 string, and out pointer.
            unsafe {
                host_intern_symbol(
                    host_ctx,
                    StrView::from_str("event::portal"),
                    &raw mut sym_after,
                )
            },
            FfiResult::OK
        );
        load_ctx.finish_init();
    }
    assert_eq!(sym_after, sym_before);
    assert_eq!(
        world
            .resource::<ModNamedObjects>()
            .get_alive(&world, "world::spawn_point"),
        Some(bound_ent)
    );
}
