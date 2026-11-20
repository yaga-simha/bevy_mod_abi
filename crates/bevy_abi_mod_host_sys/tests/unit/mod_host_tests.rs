use super::*;
use crate::registry::{ModRegistry, RegisteredComponent};
use crate::sync_ptr::SymbolAddress;
use bevy_abi_mod_sys::{BAM_STORAGE_FLAG_RESOURCE, BamComponent, ComponentUuid};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, StorageType};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::resource::IsResource;
use core::alloc::Layout;

#[test]
fn excise_unload_removes_mod_owned_is_resource_and_preserves_host_resources() {
    #[derive(Resource)]
    struct HostRes(u32);

    static SHUTDOWN_COUNT: AtomicU64 = AtomicU64::new(0);
    unsafe extern "C" fn test_shutdown(_inst: *mut BamInstance) {
        SHUTDOWN_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    SHUTDOWN_COUNT.store(0, Ordering::SeqCst);
    let mut world = World::new();
    world.insert_resource(HostRes(77));
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModQuarantine::new());

    // Register a mod-owned `IsResource` singleton resource in Bevy `World`.
    // SAFETY: Valid non-drop `(8, 8)` layout matching `u64`.
    let mod_res_id = unsafe {
        let desc = ComponentDescriptor::new_with_layout(
            "test_mod::ModOwnedRes".to_string(),
            StorageType::Table,
            Layout::from_size_align(8, 8).unwrap(),
            None,
            true,
            true,
            ComponentCloneBehavior::Default,
            None,
        );
        world.register_component_with_descriptor(desc)
    };
    let res_ent = world.spawn(IsResource::new(mod_res_id)).id();
    // SAFETY: `mod_res_id` has `(8, 8)` layout matching `u64`.
    unsafe {
        OwningPtr::make(1234u64, |ptr| {
            world.entity_mut(res_ent).insert_by_id(mod_res_id, ptr);
        });
    }
    assert!(world.contains_resource_by_id(mod_res_id));

    let mod_id = ModId::allocate();
    let mod_name: Arc<str> = Arc::from("test_mod");

    // Record `mod_res_id` in `ModRegistry` as owned by `mod_id` with `BAM_STORAGE_FLAG_RESOURCE`.
    {
        let mut reg = world.resource_mut::<ModRegistry>();
        let mut snap = (*reg.snapshot_arc()).clone();
        let handle = BamComponent::from_raw((snap.entries.len() + 1) as u32);
        let res_name: Arc<str> = Arc::from("test_mod::ModOwnedRes");
        let uuid = ComponentUuid::from_parts("test_mod::ModOwnedRes", 8, 8, 0);
        snap.entries.push(RegisteredComponent {
            handle,
            name: Arc::clone(&res_name),
            uuid,
            component_id: mod_res_id,
            size: 8,
            align: 8,
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
            storage_flags: BAM_STORAGE_FLAG_RESOURCE,
            is_abi_pod: true,
            is_dynamic: true,
            vtable_ptr: SymbolAddress::NULL,
            insert_raw_fn: None,
            insert_default_fn: None,
            drop_fn: None,
            drop_slot: None,
            has_on_add_hook: false,
            has_on_remove_hook: false,
            create_msg_cursor_fn: None,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
            validates_entity_payload: false,
            synthesizes_ffi_entity_payload: false,
            event_key: None,
            entity_event_trigger_fn: None,
            trigger_event_fn: None,
            extract_event_target_fn: None,
            extract_relationship_targets_fn: None,
            owner: Some(mod_id),
        });
        snap.by_name.insert(res_name, handle);
        snap.by_uuid.insert(uuid, handle);
        snap.by_component_id.insert(mod_res_id, handle);
        reg.restore_snapshot(Arc::new(snap));
    }

    let sys_lib_path = Path::new("in_memory_test_mod.dylib");
    let loaded_path = sys_lib_path.to_path_buf();
    let loaded_lib = LoadedLibrary::mock_empty_for_test();
    #[cfg(feature = "signature-verification")]
    let verified_signature = None;

    let lib_state = shared_lib_state_for_path(&loaded_path);
    lib_state.lock().unwrap().active_count += 1;
    let pre_snapshot = world.resource::<ModRegistry>().snapshot_arc();
    world
        .resource_mut::<ModHost>()
        .loaded_libraries
        .push(LoadedMod {
            id: mod_id,
            name: mod_name,
            version: Arc::from("0.1.0"),
            source_path: sys_lib_path.to_path_buf(),
            loaded_path,
            system_count: 0,
            fault_count: 0,
            entity: None,
            generation: 0,
            pre_snapshot,
            staged_drop_bindings: Vec::new(),
            library: loaded_lib,
            shutdown_fn: test_shutdown,
            save_state_fn: None,
            restore_state_fn: None,
            resolve_symbol_fn: None,
            instance: GuestInstancePtr::new(core::ptr::null_mut()),
            is_active: Arc::new(AtomicBool::new(true)),
            lib_state,
            #[cfg(feature = "signature-verification")]
            verified_signature,
            registered_named_sets: Vec::new(),
            observer_entities: Vec::new(),
        });

    // Unload via `excise_and_unload_mod_in_world` and verify mod-owned `IsResource` is evicted!
    excise_and_unload_mod_in_world(&mut world, mod_id, |_| {})
        .expect("excise_and_unload_mod_in_world succeeds");

    assert!(!world.contains_resource_by_id(mod_res_id));
    assert!(world.contains_resource::<HostRes>());
    assert_eq!(world.resource::<HostRes>().0, 77);
    assert!(!world.resource::<ModHost>().contains_mod(mod_id));
    assert_eq!(SHUTDOWN_COUNT.load(Ordering::SeqCst), 1);
}
