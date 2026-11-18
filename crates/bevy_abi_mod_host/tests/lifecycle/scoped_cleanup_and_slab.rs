//! Mod-scoped entity cleanup (`ModScoped`, `Disabled`, `IsResource`, `HostAssetBackend`),
//! persistent entity opt-out (`host_cmd_spawn_persistent`), and 2,600+ cycle generation-recycled
//! paged drop-slab stress tests.

use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{Position, TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host::world_ext::ModScoped;
use bevy_abi_mod_host_sys::drop_trampoline::{
    MAX_DROP_TRAMPOLINES, allocated_drop_pages, available_drop_trampoline_slots,
    drop_slot_generation, drop_slot_live_values,
};
use bevy_abi_mod_host_sys::{
    HOST_API, HostAssetBackend, HostCommands, HostCommandsStorage, LoadContext, LoadOptions,
    ModHost, RegistrySnapshot, WorldDropGuard, host_cmd_spawn, host_cmd_spawn_empty,
    host_cmd_spawn_persistent, host_register_component,
};
use bevy_abi_mod_sys::{BAM_STORAGE_FLAG_RESOURCE, FfiAssetHandle, FfiAssetLoadState, FfiResult};
use bevy_app::App;
use bevy_ecs::component::{Component, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::resource::IsResource;
use bevy_ecs::world::World;
use core::alloc::Layout;
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;
use std::ffi::CString;
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "scoped_test::HostMarker")]
struct HostMarker(u32);

#[test]
fn mod_scoped_entities_including_disabled_despawn_on_unload_while_persistent_survives() {
    let ordered_dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "scoped_cleanup");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .unwrap();
    let marker_h = app
        .register_mod_component::<HostMarker>(ComponentOptions::new().with_raw_insert())
        .unwrap();

    let mod_a = app
        .load_mod_with(
            ordered_dylib.path(),
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(
            ordered_dylib.path(),
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("load mod_b");

    let snap = app.world().resource::<ModRegistry>().snapshot_arc();
    let active_a = Arc::new(AtomicBool::new(true));
    let active_b = Arc::new(AtomicBool::new(true));

    let mut storage = HostCommandsStorage::new();
    let mut e_a_empty = FfiEntity::NULL;
    let mut e_a_batch = FfiEntity::NULL;
    let mut e_a_disabled = FfiEntity::NULL;
    let mut e_a_persistent_empty = FfiEntity::NULL;
    let mut e_a_persistent_batch = FfiEntity::NULL;
    let mut e_b_scoped = FfiEntity::NULL;
    let mut e_host_unscoped = FfiEntity::NULL;

    // 1. Mod A spawns scoped empty, scoped batch, scoped disabled, and persistent entities.
    {
        let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap)
            .with_mod_context(Some(mod_a), Some(Arc::clone(&active_a)));
        let raw_cmds = cmds.as_ffi_ptr();

        let pos = Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        let marker = HostMarker(42);
        let comps = [pos_h, marker_h];
        let ptrs = [
            (&raw const pos).cast::<u8>(),
            (&raw const marker).cast::<u8>(),
        ];

        // SAFETY: Valid active `raw_cmds`, writable output pointers, and matching component payloads.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(raw_cmds, &mut e_a_empty),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn(
                    raw_cmds,
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    comps.len(),
                    &mut e_a_batch
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn(
                    raw_cmds,
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    comps.len(),
                    &mut e_a_disabled
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn_persistent(
                    raw_cmds,
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                    &mut e_a_persistent_empty
                ),
                FfiResult::OK
            );
            let persistent_fn = HOST_API
                .cmd_spawn_persistent
                .expect("HOST_API.cmd_spawn_persistent must be wired");
            assert_eq!(
                persistent_fn(
                    raw_cmds,
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    comps.len(),
                    &mut e_a_persistent_batch
                ),
                FfiResult::OK
            );
        }
    }

    // 2. Mod B spawns a scoped entity (must NOT be despawned when Mod A unloads).
    {
        let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap)
            .with_mod_context(Some(mod_b), Some(Arc::clone(&active_b)));
        // SAFETY: Valid `cmds` and output entity.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e_b_scoped),
                FfiResult::OK
            );
        }
    }

    // 3. Host unscoped `HostCommands` spawns without `ModScoped`.
    {
        let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and output entity.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e_host_unscoped),
                FfiResult::OK
            );
        }
    }

    storage.apply_to_world(app.world_mut(), &snap);

    let ent_a_empty = Entity::from_bits(e_a_empty.to_bits());
    let ent_a_batch = Entity::from_bits(e_a_batch.to_bits());
    let ent_a_disabled = Entity::from_bits(e_a_disabled.to_bits());
    let ent_a_pers_empty = Entity::from_bits(e_a_persistent_empty.to_bits());
    let ent_a_pers_batch = Entity::from_bits(e_a_persistent_batch.to_bits());
    let ent_b_scoped = Entity::from_bits(e_b_scoped.to_bits());
    let ent_host = Entity::from_bits(e_host_unscoped.to_bits());

    // Mark `ent_a_disabled` as Bevy `Disabled` to verify `Allow<Disabled>` cleanup!
    app.world_mut().entity_mut(ent_a_disabled).insert(Disabled);

    assert_eq!(
        app.world().entity(ent_a_empty).get::<ModScoped>(),
        Some(&ModScoped(mod_a))
    );
    assert_eq!(
        app.world().entity(ent_a_batch).get::<ModScoped>(),
        Some(&ModScoped(mod_a))
    );
    assert_eq!(
        app.world().entity(ent_a_disabled).get::<ModScoped>(),
        Some(&ModScoped(mod_a))
    );
    assert_eq!(
        app.world().entity(ent_a_pers_empty).get::<ModScoped>(),
        None
    );
    assert_eq!(
        app.world().entity(ent_a_pers_batch).get::<ModScoped>(),
        None
    );
    assert_eq!(
        app.world().entity(ent_a_pers_batch).get::<HostMarker>(),
        Some(&HostMarker(42))
    );
    assert_eq!(
        app.world().entity(ent_b_scoped).get::<ModScoped>(),
        Some(&ModScoped(mod_b))
    );
    assert_eq!(app.world().entity(ent_host).get::<ModScoped>(), None);

    // Unload Mod A: all ModScoped(mod_a) entities (including Disabled) MUST despawn;
    // persistent entities, Mod B entities, and host entities MUST survive!
    app.unload_mod(mod_a).expect("unload mod_a");

    assert!(!app.world().entities().contains(ent_a_empty));
    assert!(!app.world().entities().contains(ent_a_batch));
    assert!(!app.world().entities().contains(ent_a_disabled));

    assert!(app.world().entities().contains(ent_a_pers_empty));
    assert!(app.world().entities().contains(ent_a_pers_batch));
    assert_eq!(
        app.world().entity(ent_a_pers_batch).get::<HostMarker>(),
        Some(&HostMarker(42))
    );
    assert!(app.world().entities().contains(ent_b_scoped));
    assert!(app.world().entities().contains(ent_host));

    // Unload Mod B: Mod B's entity despawns, persistent & host entities still survive!
    app.unload_mod(mod_b).expect("unload mod_b");
    assert!(!app.world().entities().contains(ent_b_scoped));
    assert!(app.world().entities().contains(ent_a_pers_empty));
    assert!(app.world().entities().contains(ent_a_pers_batch));
    assert!(app.world().entities().contains(ent_host));
}

struct MockAssetBackend {
    released_owners: Mutex<Vec<String>>,
}

impl HostAssetBackend for MockAssetBackend {
    fn load_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        _owner_namespace: Option<&str>,
        _asset_type: BamComponent,
        _path: &str,
    ) -> Result<FfiAssetHandle, (FfiResult, String)> {
        Ok(FfiAssetHandle::NONE)
    }

    fn get_load_state(
        &self,
        _snapshot: &RegistrySnapshot,
        _handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, (FfiResult, String)> {
        Ok(FfiAssetLoadState::NOT_LOADED)
    }

    fn release_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        _owner_namespace: Option<&str>,
        _handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        Ok(())
    }

    fn release_all_for_owner(&self, owner_namespace: &str) {
        self.released_owners
            .lock()
            .unwrap()
            .push(owner_namespace.to_string());
    }
}

#[test]
fn is_resource_excluded_from_despawn_all_and_assets_released_on_unload() {
    let ordered_dylib =
        TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "is_resource_cleanup");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    #[derive(Resource)]
    struct HostOwnedSentinel(u64);
    app.insert_resource(HostOwnedSentinel(99));

    let asset_backend = Arc::new(MockAssetBackend {
        released_owners: Mutex::new(Vec::new()),
    });
    app.world_mut()
        .resource_mut::<ModRegistry>()
        .set_asset_backend(Arc::clone(&asset_backend) as Arc<dyn HostAssetBackend>);

    let mod_id = app
        .load_mod_with(
            ordered_dylib.path(),
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("load mod");

    // Create an `IsResource` singleton entity and also tag it `ModScoped(mod_id)` to verify
    // `despawn_all_where` (`Without<IsResource>`) never accidentally despawns `IsResource` entities.
    // SAFETY: Valid non-drop `(8, 8)` layout for a POD singleton resource.
    let res_comp_id: ComponentId = unsafe {
        let bevy_desc = ComponentDescriptor::new_with_layout(
            "ordered_sets_mod::HostIsResourceGuard".to_string(),
            StorageType::Table,
            Layout::from_size_align(8, 8).unwrap(),
            None,
            true,
            true,
            bevy_ecs::component::ComponentCloneBehavior::Default,
            None,
        );
        app.world_mut()
            .register_component_with_descriptor(bevy_desc)
    };
    let res_entity = app.world_mut().spawn(IsResource::new(res_comp_id)).id();
    // SAFETY: `res_comp_id` was registered above with `(8, 8)` layout matching `u64`.
    unsafe {
        OwningPtr::make(0xCAFE_BABEu64, |ptr| {
            app.world_mut()
                .entity_mut(res_entity)
                .insert_by_id(res_comp_id, ptr);
        });
    }
    app.world_mut()
        .entity_mut(res_entity)
        .insert(ModScoped(mod_id));
    assert!(app.world().contains_resource_by_id(res_comp_id));

    let mod_name = app
        .world()
        .resource::<ModHost>()
        .get_mod(mod_id)
        .unwrap()
        .name()
        .to_string();

    app.unload_mod(mod_id).expect("unload mod");

    assert_eq!(app.world().resource::<HostOwnedSentinel>().0, 99);
    assert!(app.world().contains_resource_by_id(res_comp_id));
    assert_eq!(
        *asset_backend.released_owners.lock().unwrap(),
        vec![mod_name]
    );
    assert_ne!(BAM_STORAGE_FLAG_RESOURCE, 0);
}

#[test]
fn real_mod_2600_load_spawn_unload_cycles_immediately_recycle_generation_stamped_drop_slots() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let vtable_dylib = TempDylibCopy::new(&fixture_dylib("vtable_eviction_mod"), "recycle_2600");
    let baseline_free = available_drop_trampoline_slots();

    const CYCLES: usize = 2_600;
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let comp_uuid = ComponentUuid::from_parts("vtable_eviction_mod::GuestVTableComp", 8, 8, 0);
    let mut tracked_slot: Option<usize> = None;
    let mut prev_gen: Option<u64> = None;
    let opts = LoadOptions::new().with_shadow_copy(false);

    for cycle in 0..CYCLES {
        let mod_id = app
            .load_mod_with(vtable_dylib.path(), opts.clone())
            .unwrap_or_else(|e| panic!("cycle {cycle}: load failed: {e:?}"));

        let (slot, slot_gen) = app
            .world()
            .resource::<WorldDropGuard>()
            .binding_for_uuid(&comp_uuid)
            .expect("GuestVTableComp must bind a drop slot");

        if let Some(expected_slot) = tracked_slot {
            assert_eq!(
                slot, expected_slot,
                "LIFO recycling should reuse the exact freed slot"
            );
            assert_eq!(
                slot_gen,
                prev_gen.unwrap() + 1,
                "recycled slot generation must increment monotonically on every unload"
            );
        } else {
            tracked_slot = Some(slot);
        }
        prev_gen = Some(slot_gen);

        assert_eq!(available_drop_trampoline_slots(), baseline_free - 1);

        // Spawn a `ModScoped(mod_id)` entity carrying `GuestVTableComp` (`live_values` -> 1).
        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let comp_h = snap
            .get_by_name("vtable_eviction_mod::GuestVTableComp")
            .expect("GuestVTableComp registered")
            .handle;
        let payload = (cycle as u64).to_ne_bytes();
        let mut storage = HostCommandsStorage::new();
        let mut out_ent = FfiEntity::NULL;
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap)
                .with_mod_context(Some(mod_id), None);
            let comps = [comp_h];
            let ptrs = [payload.as_ptr()];
            // SAFETY: Valid `cmds`, component handle, and 8-byte payload.
            unsafe {
                assert_eq!(
                    host_cmd_spawn(
                        cmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        &mut out_ent
                    ),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);
        assert_eq!(drop_slot_live_values(slot), Some(1));

        // Unloading immediately despawns `ModScoped(mod_id)` -> `live_values == 0` -> slot
        // recycles immediately with `gen + 1` back to `free_list` BEFORE `World::drop`!
        app.unload_mod(mod_id)
            .unwrap_or_else(|e| panic!("cycle {cycle}: unload failed: {e:?}"));

        assert_eq!(drop_slot_live_values(slot), Some(0));
        assert_eq!(drop_slot_generation(slot), Some(slot_gen + 1));
        assert_eq!(
            available_drop_trampoline_slots(),
            baseline_free,
            "cycle {cycle}: drop slot must be recycled immediately on unload_mod"
        );
    }

    assert_eq!(available_drop_trampoline_slots(), baseline_free);
    assert!(
        allocated_drop_pages() >= 1,
        "at least one 64-slot page was lazily allocated"
    );
}

#[test]
fn paged_slab_grows_lazily_past_1024_up_to_3000_distinct_live_slots_in_one_world() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    unsafe extern "C" fn noop_drop(_ptr: *mut c_void) {}

    let baseline_free = available_drop_trampoline_slots();
    const { assert!(MAX_DROP_TRAMPOLINES >= 4096) };

    if baseline_free >= 3_000 {
        let mut world = World::new();
        world.insert_resource(ModRegistry::new());

        let active = Arc::new(AtomicBool::new(true));
        let world_ptr = &mut world as *mut World;
        let reg = world.resource::<ModRegistry>().clone();
        let mut ctx = LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);

        let mut names = Vec::with_capacity(3_000);
        for i in 0..3_000 {
            names.push(CString::new(format!("paged3k::Comp_{i}")).unwrap());
        }
        for (i, cname) in names.iter().enumerate() {
            let uuid = ComponentUuid::from_parts(cname.to_str().unwrap(), 8, 8, i as u64);
            let desc = ComponentDesc::new(cname.as_ptr(), uuid, 8, 8).with_drop(noop_drop);
            let mut h = BamComponent::INVALID;
            // SAFETY: Valid active `ctx`, `desc`, and writable `h`.
            let st = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) };
            assert_eq!(st, FfiResult::OK, "slot {i} must succeed in 4096-slot slab");
        }
        ctx.finish_init();
        assert_eq!(available_drop_trampoline_slots(), baseline_free - 3_000);
        assert!(allocated_drop_pages() >= 47);

        drop(world);
        assert_eq!(available_drop_trampoline_slots(), baseline_free);
    }
}
