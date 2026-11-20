use super::*;
use core::ffi::c_void;
use core::sync::atomic::Ordering;
use std::sync::Mutex;

#[test]
fn queue_custom_runs_in_fifo_order_and_clear_skips_execution() {
    let mut world = World::new();
    let registry = ModRegistry::new();
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let log = Arc::new(Mutex::new(Vec::<u32>::new()));
    let active = Arc::new(AtomicBool::new(true));
    let mid = ModId(7);

    let reserved = {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(mid), Some(Arc::clone(&active)))
            .with_registry(&registry);
        assert_eq!(cmds.owner_mod_id(), Some(mid));
        assert!(cmds.is_active().unwrap().load(Ordering::Relaxed));
        assert!(cmds.registry().is_some());
        let _ = cmds.snapshot();

        let e = cmds.reserve_entity();
        let log1 = Arc::clone(&log);
        cmds.queue_custom(move |w, _| {
            let _ = w.spawn_empty_at(e);
            log1.lock().unwrap().push(1);
        });
        let log2 = Arc::clone(&log);
        cmds.queue_custom(move |w, _| {
            assert!(w.entities().contains(e));
            log2.lock().unwrap().push(2);
        });
        e
    };

    storage.apply_to_world(&mut world, &snap);
    assert!(world.entities().contains(reserved));
    assert_eq!(*log.lock().unwrap(), vec![1, 2]);

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let log3 = Arc::clone(&log);
        cmds.queue_custom(move |_, _| {
            log3.lock().unwrap().push(99);
        });
    }
    storage.clear();
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(*log.lock().unwrap(), vec![1, 2]);
}

#[test]
fn stale_unloaded_drop_handle_rejected_and_queued_commands_skip_recycled_drop_slot() {
    use crate::drop_trampoline::{
        WorldDropGuard, available_drop_trampoline_slots, prune_drop_trampolines_for_guard,
    };
    use crate::ffi::commands_ext::host_cmd_spawn_persistent;
    use crate::ffi::register::{LoadContext, host_register_component};
    use bevy_abi_mod_sys::{ComponentDesc, ComponentUuid};
    use core::sync::atomic::AtomicUsize;

    static DROP_A_COUNT: AtomicUsize = AtomicUsize::new(0);
    static DROP_B_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn drop_a(_ptr: *mut c_void) {
        DROP_A_COUNT.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn drop_b(ptr: *mut c_void) {
        // SAFETY: Validating Mod B's 24-byte magic sentinel to catch any cross-mod type confusion.
        let sentinel = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) };
        assert_eq!(sentinel, 0xBBBB_BBBB_BBBB_BBBB);
        DROP_B_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    DROP_A_COUNT.store(0, Ordering::SeqCst);
    DROP_B_COUNT.store(0, Ordering::SeqCst);
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());
    let mod_a = ModId(101);
    let mod_b = ModId(102);

    // 1. Mod A registers `CompA` (`size = 8`, `drop_a`).
    let active_a = Arc::new(AtomicBool::new(true));
    let mut comp_a_h = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active_a),
            &mut world as *mut World,
            Some(mod_a),
        );
        let desc_a = ComponentDesc::new(
            c"stale_drop::CompA".as_ptr(),
            ComponentUuid::from_parts("stale_drop::CompA", 8, 8, 1),
            8,
            8,
        )
        .with_drop(drop_a);
        // SAFETY: Valid active `ctx`, `desc_a`, and writable `comp_a_h`.
        let st = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut comp_a_h) };
        assert_eq!(st, FfiResult::OK);
        let updated = ctx.registry.clone();
        ctx.finish_init();
        world.insert_resource(updated);
    }
    let comp_a_id = world
        .resource::<ModRegistry>()
        .get_by_handle(comp_a_h)
        .unwrap()
        .component_id;
    let slot_a = world
        .resource::<ModRegistry>()
        .get_by_handle(comp_a_h)
        .unwrap()
        .drop_slot
        .unwrap();

    // 2. Queue `cmd_spawn` and `cmd_insert` of `CompA` BEFORE Mod A unloads.
    let target_ent = world.spawn_empty().id();
    let target_ffi = FfiEntity::from_bits(target_ent.to_bits());
    let pre_unload_snap = world.resource::<ModRegistry>().snapshot_arc();
    let mut pre_unload_storage = HostCommandsStorage::new();
    let mut pre_unload_clear_storage = HostCommandsStorage::new();
    let payload_a: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    let mut queued_spawn_ent = FfiEntity::NULL;
    {
        let mut cmds = HostCommands::new(
            &mut pre_unload_storage,
            world.entity_allocator(),
            &pre_unload_snap,
        )
        .with_mod_context(Some(mod_a), Some(Arc::clone(&active_a)));
        let comps = [comp_a_h];
        let ptrs = [(&raw const payload_a).cast::<u8>()];
        // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `payload_a`.
        unsafe {
            assert_eq!(
                host_cmd_spawn(
                    cmds.as_ffi_ptr(),
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    1,
                    &mut queued_spawn_ent,
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert(
                    cmds.as_ffi_ptr(),
                    target_ffi,
                    comp_a_h,
                    (&raw const payload_a).cast::<u8>(),
                    8,
                ),
                FfiResult::OK
            );
        }
    }
    {
        let mut cmds = HostCommands::new(
            &mut pre_unload_clear_storage,
            world.entity_allocator(),
            &pre_unload_snap,
        )
        .with_mod_context(Some(mod_a), Some(Arc::clone(&active_a)));
        // SAFETY: Valid `cmds` and `payload_a`.
        unsafe {
            assert_eq!(
                host_cmd_insert(
                    cmds.as_ffi_ptr(),
                    target_ffi,
                    comp_a_h,
                    (&raw const payload_a).cast::<u8>(),
                    8,
                ),
                FfiResult::OK
            );
        }
    }

    // 3. Unload Mod A (`live_values == 0`): disarms `active_a`, recycles `slot_a` (`gen + 1`),
    //    and evicts Mod A from `ModRegistry` (`drop_fn = None`, `owner = None`).
    prune_drop_trampolines_for_guard(&active_a);
    let recycled = world
        .resource_mut::<WorldDropGuard>()
        .recycle_slots_for_guard(&active_a);
    assert_eq!(recycled, 1);
    world.resource_mut::<ModRegistry>().evict_mod(mod_a);

    // 4. Mod B loads and registers `CompB` (`size = 24`, `drop_b`) — reuses `slot_a`!
    let active_b = Arc::new(AtomicBool::new(true));
    let mut comp_b_h = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            active_b,
            &mut world as *mut World,
            Some(mod_b),
        );
        let desc_b = ComponentDesc::new(
            c"stale_drop::CompB".as_ptr(),
            ComponentUuid::from_parts("stale_drop::CompB", 24, 8, 2),
            24,
            8,
        )
        .with_drop(drop_b);
        // SAFETY: Valid active `ctx`, `desc_b`, and writable `comp_b_h`.
        let st = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut comp_b_h) };
        assert_eq!(st, FfiResult::OK);
        let updated = ctx.registry.clone();
        ctx.finish_init();
        world.insert_resource(updated);
    }
    let slot_b = world
        .resource::<ModRegistry>()
        .get_by_handle(comp_b_h)
        .unwrap()
        .drop_slot
        .unwrap();
    assert_eq!(slot_b, slot_a);

    // 5. Post-unload calls with stale `comp_a_h` MUST fail with `INVALID_STATE`.
    let post_unload_snap = world.resource::<ModRegistry>().snapshot_arc();
    let mut post_storage = HostCommandsStorage::new();
    let mut ignored_ent = FfiEntity::NULL;
    {
        let mut cmds = HostCommands::new(
            &mut post_storage,
            world.entity_allocator(),
            &post_unload_snap,
        );
        let comps = [comp_a_h];
        let ptrs = [(&raw const payload_a).cast::<u8>()];
        // SAFETY: Valid `cmds` and `payload_a`; verifying stale handle rejection.
        unsafe {
            assert_eq!(
                host_cmd_spawn(
                    cmds.as_ffi_ptr(),
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    1,
                    &mut ignored_ent,
                ),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_cmd_spawn_persistent(
                    cmds.as_ffi_ptr(),
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    1,
                    &mut ignored_ent,
                ),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_cmd_insert(
                    cmds.as_ffi_ptr(),
                    target_ffi,
                    comp_a_h,
                    (&raw const payload_a).cast::<u8>(),
                    8,
                ),
                FfiResult::INVALID_STATE
            );
        }
    }
    assert!(post_storage.is_empty());

    // 6. Applying & clearing commands queued BEFORE unload MUST NOT insert `comp_a_id` or invoke `drop_b`!
    pre_unload_storage.apply_to_world(&mut world, &pre_unload_snap);
    pre_unload_clear_storage.clear();
    assert!(!world.entity(target_ent).contains_id(comp_a_id));
    let spawned_bevy = Entity::from_bits(queued_spawn_ent.to_bits());
    assert!(!world.entity(spawned_bevy).contains_id(comp_a_id));
    assert_eq!(DROP_A_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(DROP_B_COUNT.load(Ordering::SeqCst), 0);

    let _ = available_drop_trampoline_slots();
}

#[test]
fn command_quotas_enforce_arena_and_spawn_caps_and_rewind_cleanly() {
    use crate::ffi::assets_scene::{HostSceneBackend, host_cmd_spawn_scene};
    use crate::ffi::commands_ext::{
        host_cmd_insert_if_new, host_cmd_spawn_batch, host_cmd_spawn_persistent,
    };
    use crate::ffi::register::{LoadContext, host_register_component};
    use crate::ffi::v03_ext::{host_cmd_insert_relation, host_register_relation};
    use crate::{ComponentOptions, WorldDropGuard};
    use bevy_abi_mod_sys::{
        AbiPod, BamRelationDesc, ComponentDesc, ComponentUuid, FfiAssetHandle, ModComponent,
        RawInsertable, SymbolEntry, VTableHeader,
    };
    use bevy_ecs::component::Component;
    use bevy_ecs::event::Event;
    use core::ffi::CStr;

    #[repr(C)]
    #[derive(Component, Clone, Copy, Default)]
    struct TinyByte(u8);
    // SAFETY: `u8` has universal bit validity.
    unsafe impl RawInsertable for TinyByte {}
    // SAFETY: `#[repr(C)]` `u8` POD.
    unsafe impl AbiPod for TinyByte {}
    impl ModComponent for TinyByte {
        const CANONICAL_NAME: &'static str = "cmd_quota::TinyByte";
        const COMPONENT_CSTR: &'static CStr = c"cmd_quota::TinyByte";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cmd_quota::TinyByte");
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader = VTableHeader::new::<VTableHeader, TinyByte>(TinyByte::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    #[repr(C)]
    #[derive(Component, Clone, Copy, Default)]
    struct AlignedWord(u64);
    // SAFETY: `u64` has universal bit validity.
    unsafe impl RawInsertable for AlignedWord {}
    // SAFETY: `#[repr(C)]` `u64` POD.
    unsafe impl AbiPod for AlignedWord {}
    impl ModComponent for AlignedWord {
        const CANONICAL_NAME: &'static str = "cmd_quota::AlignedWord";
        const COMPONENT_CSTR: &'static CStr = c"cmd_quota::AlignedWord";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cmd_quota::AlignedWord");
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader =
                VTableHeader::new::<VTableHeader, AlignedWord>(AlignedWord::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    #[repr(C)]
    #[derive(Event, Clone, Copy, Default)]
    struct QuotaEvt(u64);
    // SAFETY: `u64` has universal bit validity.
    unsafe impl RawInsertable for QuotaEvt {}
    // SAFETY: `#[repr(C)]` `u64` POD.
    unsafe impl AbiPod for QuotaEvt {}
    impl ModComponent for QuotaEvt {
        const CANONICAL_NAME: &'static str = "cmd_quota::QuotaEvt";
        const COMPONENT_CSTR: &'static CStr = c"cmd_quota::QuotaEvt";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cmd_quota::QuotaEvt");
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader = VTableHeader::new::<VTableHeader, QuotaEvt>(QuotaEvt::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    struct MockSceneBackend;
    impl HostSceneBackend for MockSceneBackend {
        fn spawn_scene(
            &self,
            _world: &mut World,
            _snapshot: &RegistrySnapshot,
            _root_entity: Entity,
            _parent: Option<Entity>,
            _scene_handle: &FfiAssetHandle,
        ) -> Result<(), FfiResult> {
            Ok(())
        }
    }

    static REWIND_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn rewind_drop(_ptr: *mut c_void) {
        REWIND_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
    REWIND_DROP_COUNT.store(0, Ordering::SeqCst);

    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    let tiny_id = world.register_component::<TinyByte>();
    let word_id = world.register_component::<AlignedWord>();
    let evt_key = world.register_event_key::<QuotaEvt>();
    crate::registry::catalog::ensure_relation_graph_in_world(&mut world);

    let mut reg = ModRegistry::new();
    let tiny_h = reg
        .register_component::<TinyByte>(tiny_id, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let word_h = reg
        .register_component::<AlignedWord>(word_id, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let evt_h = reg.register_event::<QuotaEvt>(evt_key).unwrap();
    reg.set_scene_backend(Arc::new(MockSceneBackend));
    world.insert_resource(reg);

    // Register a 1-byte dynamic drop component and an 8-byte relation payload for rewind & relation tests.
    let active = Arc::new(AtomicBool::new(true));
    let mut dyn_tiny_h = BamComponent::INVALID;
    let mut rel_h = BamComponent::INVALID;
    {
        let reg_clone = world.resource::<ModRegistry>().clone();
        let mut lctx = LoadContext::with_world_and_mod_id(
            reg_clone,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(77)),
        );
        let desc = ComponentDesc::new(
            c"cmd_quota::DynTiny".as_ptr(),
            ComponentUuid::from_parts("cmd_quota::DynTiny", 1, 1, 1),
            1,
            1,
        )
        .with_drop(rewind_drop);
        // SAFETY: Valid active `LoadContext`, `ComponentDesc`, and output handle.
        let st_comp = unsafe { host_register_component(lctx.as_ffi_ptr(), &desc, &mut dyn_tiny_h) };
        assert_eq!(st_comp, FfiResult::OK);
        let rel_desc = BamRelationDesc::new(
            c"cmd_quota::Edge".as_ptr(),
            ComponentUuid::from_parts("cmd_quota::Edge", 8, 8, 1),
        )
        .with_payload_layout(8, 8);
        // SAFETY: Valid active `LoadContext`, `BamRelationDesc`, and output handle.
        let st_rel = unsafe { host_register_relation(lctx.as_ffi_ptr(), &rel_desc, &mut rel_h) };
        assert_eq!(st_rel, FfiResult::OK);
        let updated = lctx.registry.clone();
        lctx.finish_init();
        world.insert_resource(updated);
    }

    let target = world.spawn_empty().id();
    let target_ffi = FfiEntity::from_bits(target.to_bits());
    let snap = world.resource::<ModRegistry>().snapshot_arc();
    let mut storage = HostCommandsStorage::new();

    // 1. Arena cap of 0 bytes rejects `insert`, `insert_if_new`, `trigger`, `insert_relation`
    //    without allocating any slab!
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_quotas(
                CommandQuotaLimits {
                    max_arena_bytes: Some(0),
                    max_spawns_per_frame: None,
                },
                None,
            );
        let word = AlignedWord(123);
        let word_ptr = (&raw const word).cast::<u8>();
        // SAFETY: Valid `HostCommands` and readable `AlignedWord` bytes.
        unsafe {
            assert_eq!(
                host_cmd_insert(cmds.as_ffi_ptr(), target_ffi, word_h, word_ptr, 8),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_insert_if_new(cmds.as_ffi_ptr(), target_ffi, word_h, word_ptr, 8),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_trigger(cmds.as_ffi_ptr(), evt_h, FfiEntity::NULL, word_ptr, 8),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_insert_relation(
                    cmds.as_ffi_ptr(),
                    target_ffi,
                    rel_h,
                    target_ffi,
                    word_ptr.cast::<c_void>(),
                    8,
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
        }
    }
    assert_eq!(storage.arena_allocated_bytes(), 0);
    assert_eq!(storage.arena_bytes_used(), 0);
    assert_eq!(storage.arena_breaches(), 4);
    assert!(storage.is_empty());
    // Empty `apply_to_world` must still reset invocation counters cleanly.
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.arena_breaches(), 0);

    // 2. Alignment-padding Pass 2 overflow in `cmd_spawn` and `cmd_spawn_batch` rewinds arena,
    //    invokes uncommitted `drop_fn` on `dyn_tiny_h`, and releases reserved spawn quota!
    let shared_spawns = AtomicUsize::new(0);
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_quotas(
                CommandQuotaLimits {
                    // 1 byte + 8 bytes = 9 payload bytes (passes Pass 1 sum check <= 10),
                    // but 7 bytes of alignment padding for `u64` makes slab consumption 16 > 10!
                    max_arena_bytes: Some(10),
                    max_spawns_per_frame: Some(3),
                },
                Some(&shared_spawns),
            );
        let tiny_b = 7u8;
        let word = AlignedWord(42);
        let comps = [dyn_tiny_h, word_h];
        let ptrs = [
            (&raw const tiny_b).cast::<u8>(),
            (&raw const word).cast::<u8>(),
        ];
        let mut out_e = FfiEntity::NULL;
        // SAFETY: Valid `HostCommands`, component handles, and readable payload pointers.
        unsafe {
            assert_eq!(
                host_cmd_spawn(
                    cmds.as_ffi_ptr(),
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    2,
                    &mut out_e
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
            // Same alignment-padding rewind check on `host_cmd_spawn_batch`!
            let mut out_batch = [FfiEntity::NULL; 1];
            assert_eq!(
                host_cmd_spawn_batch(
                    cmds.as_ffi_ptr(),
                    comps.as_ptr(),
                    ptrs.as_ptr(),
                    2,
                    1,
                    out_batch.as_mut_ptr(),
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
        }
        assert_eq!(shared_spawns.load(Ordering::Acquire), 0);
        assert_eq!(REWIND_DROP_COUNT.load(Ordering::SeqCst), 2);

        // 3. Spawn quota across `spawn_empty`, `spawn(count=0)`, `spawn_persistent`,
        //    `spawn_scene`, and `spawn_batch` (cap = 3).
        let mut e1 = FfiEntity::NULL;
        let mut e2 = FfiEntity::NULL;
        let mut e3 = FfiEntity::NULL;
        let mut e4 = FfiEntity::NULL;
        let scene_handle = FfiAssetHandle::from_index(BamComponent::from_raw(1), 1, 0);
        // SAFETY: Valid `HostCommands` and writable `FfiEntity` outputs.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e1),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn_persistent(
                    cmds.as_ffi_ptr(),
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                    &mut e2,
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn_scene(cmds.as_ffi_ptr(), &scene_handle, FfiEntity::NULL, &mut e3),
                FfiResult::OK
            );
            // Now at 3/3 spawns: every spawn variant must return `CAPACITY_EXCEEDED`!
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e4),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_spawn(
                    cmds.as_ffi_ptr(),
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                    &mut e4,
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
            let tb = TinyByte(1);
            let c_one = [tiny_h];
            let p_one = [(&raw const tb).cast::<u8>()];
            assert_eq!(
                host_cmd_spawn(
                    cmds.as_ffi_ptr(),
                    c_one.as_ptr(),
                    p_one.as_ptr(),
                    1,
                    &mut e4
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_spawn_batch(
                    cmds.as_ffi_ptr(),
                    c_one.as_ptr(),
                    p_one.as_ptr(),
                    1,
                    1,
                    &mut e4,
                ),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_cmd_spawn_scene(cmds.as_ffi_ptr(), &scene_handle, FfiEntity::NULL, &mut e4,),
                FfiResult::CAPACITY_EXCEEDED
            );
        }
    }
    assert_eq!(shared_spawns.load(Ordering::Acquire), 3);
    assert_eq!(storage.spawns_queued(), 3);
    assert_eq!(storage.spawn_breaches(), 5);
    assert_eq!(storage.arena_bytes_used(), 0);
    storage.clear();

    // 4. Local-only spawn cap (no shared counter) and unlimited-with-shared-counter path.
    let shared_unbounded = AtomicUsize::new(0);
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_quotas(
                CommandQuotaLimits {
                    max_arena_bytes: None,
                    max_spawns_per_frame: Some(1),
                },
                None,
            );
        let mut e = FfiEntity::NULL;
        // SAFETY: Valid `HostCommands` and writable `e`.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e),
                FfiResult::CAPACITY_EXCEEDED
            );
        }
    }
    assert_eq!(storage.spawns_queued(), 1);
    assert_eq!(storage.spawn_breaches(), 1);
    storage.clear();

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_quotas(CommandQuotaLimits::default(), Some(&shared_unbounded));
        let mut e = FfiEntity::NULL;
        // SAFETY: Valid `HostCommands` and writable `e`.
        unsafe {
            assert_eq!(
                host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut e),
                FfiResult::OK
            );
        }
    }
    assert_eq!(shared_unbounded.load(Ordering::Acquire), 1);
    storage.clear();

    // Clean up dynamic slot registered by this test.
    crate::drop_trampoline::prune_drop_trampolines_for_guard(&active);
    let _ = world
        .resource_mut::<WorldDropGuard>()
        .recycle_slots_for_guard(&active);
}
