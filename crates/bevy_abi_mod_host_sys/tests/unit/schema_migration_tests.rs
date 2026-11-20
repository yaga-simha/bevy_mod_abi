use super::*;
use crate::drop_trampoline::WorldDropGuard;
use crate::ffi::register::{LoadContext, host_register_component};
use crate::ffi::validation::BamExportFieldsSinkFn;
use crate::loader::mod_host::excise_and_unload_mod_for_reload_in_world;
use crate::system::ModQuarantine;
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{BamInstance, ComponentDesc, ModComponent};
use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn snapshot_migrate_and_restore_v1_to_v2_under_miri() {
    // Serialize against other global `DROP_SLAB` LIFO/free-count assertions.
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static V1_DROPS: AtomicUsize = AtomicUsize::new(0);
    static V2_DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Default)]
    struct DropV1;
    impl Drop for DropV1 {
        fn drop(&mut self) {
            V1_DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    struct DropV2;
    impl Drop for DropV2 {
        fn drop(&mut self) {
            V2_DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default, ModComponent)]
    #[mod_component(name = "sim::UnitState")]
    struct UnitV1 {
        hp: i32,
        tag: String,
        scores: Vec<u32>,
        legacy: u64,
        #[mod_field(skip)]
        _probe: DropV1,
    }

    #[derive(Clone, ModComponent)]
    #[mod_component(name = "sim::UnitState")]
    struct UnitV2 {
        shield: f32,
        scores: Vec<u32>,
        hp: i64,
        tag: String,
        #[mod_field(skip)]
        _probe: DropV2,
    }

    impl Default for UnitV2 {
        fn default() -> Self {
            Self {
                shield: 12.5,
                scores: Vec::new(),
                hp: 0,
                tag: String::new(),
                _probe: DropV2,
            }
        }
    }

    unsafe extern "C" fn drop_v1(ptr: *mut c_void) {
        // SAFETY: `ptr` points to a live aligned `UnitV1`.
        unsafe { core::ptr::drop_in_place(ptr.cast::<UnitV1>()) };
    }
    unsafe extern "C" fn drop_v2(ptr: *mut c_void) {
        // SAFETY: `ptr` points to a live aligned `UnitV2`.
        unsafe { core::ptr::drop_in_place(ptr.cast::<UnitV2>()) };
    }
    unsafe extern "C" fn init_v2(dst: *mut c_void) -> FfiResult {
        // SAFETY: `dst` points to writable `UnitV2`-aligned storage.
        unsafe { core::ptr::write(dst.cast::<UnitV2>(), UnitV2::default()) };
        FfiResult::OK
    }
    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn resolve_v1(
        _inst: *const BamInstance,
        name: *const c_char,
    ) -> *const c_void {
        // SAFETY: `name` is a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return core::ptr::null();
        };
        UnitV1::resolve_symbol(s).unwrap_or(core::ptr::null())
    }
    unsafe extern "C" fn resolve_v2(
        _inst: *const BamInstance,
        name: *const c_char,
    ) -> *const c_void {
        // SAFETY: `name` is a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return core::ptr::null();
        };
        if s == "bam.v0.sim::UnitState.init_default" {
            return init_v2 as *const c_void;
        }
        UnitV2::resolve_symbol(s).unwrap_or(core::ptr::null())
    }

    V1_DROPS.store(0, Ordering::SeqCst);
    V2_DROPS.store(0, Ordering::SeqCst);

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModQuarantine::new());

    let old_active = Arc::new(AtomicBool::new(true));
    let old_id = LoadedMod::register_mock_for_test(
        &mut world,
        "sim_mod",
        Arc::clone(&old_active),
        noop_shutdown,
        Some(resolve_v1),
        core::ptr::null_mut(),
    );
    let mut old_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, old_active, world_ptr, Some(old_id));
        let desc = ComponentDesc::new(
            UnitV1::COMPONENT_CSTR.as_ptr(),
            UnitV1::UUID,
            core::mem::size_of::<UnitV1>(),
            core::mem::align_of::<UnitV1>(),
        )
        .with_drop(drop_v1)
        .with_vtable(UnitV1::erased_vtable_ptr());
        let rc_v1 =
            // SAFETY: `load_ctx`, `desc`, and `&mut old_handle` are valid for this test registration.
            unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const desc,
                    &raw mut old_handle,
                )
            };
        assert_eq!(rc_v1, FfiResult::OK);
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    let comp_v1_id = world
        .resource::<ModRegistry>()
        .get_by_handle(old_handle)
        .unwrap()
        .component_id;

    let e_active = world.spawn(ModScoped(old_id)).id();
    let e_disabled = world.spawn((ModScoped(old_id), Disabled)).id();
    // SAFETY: `comp_v1_id` has exact `UnitV1` layout.
    unsafe {
        OwningPtr::make(
            UnitV1 {
                hp: -7,
                tag: String::from("knight"),
                scores: vec![4, 5, 6],
                legacy: 999,
                _probe: DropV1,
            },
            |ptr| {
                world.entity_mut(e_active).insert_by_id(comp_v1_id, ptr);
            },
        );
        OwningPtr::make(
            UnitV1 {
                hp: 88,
                tag: String::from("archer"),
                scores: vec![1, 2],
                legacy: 111,
                _probe: DropV1,
            },
            |ptr| {
                world.entity_mut(e_disabled).insert_by_id(comp_v1_id, ptr);
            },
        );
    }

    // Snapshot, remove V1 (dropping on V1 allocator), and unload for reload preserving entities.
    let snap =
        snapshot_mod_dynamic_columns_for_reload(&mut world, old_id).expect("snapshot succeeds");
    assert_eq!(V1_DROPS.load(Ordering::SeqCst), 2);
    assert!(world.entities().contains(e_active));
    assert!(world.entities().contains(e_disabled));

    excise_and_unload_mod_for_reload_in_world(&mut world, old_id, |_| {})
        .expect("unload for reload preserves ModScoped entities");
    assert!(world.entities().contains(e_active));
    assert!(world.entities().contains(e_disabled));

    // Load V2 with evolved layout & restore columns + migrate ModScoped.
    let new_active = Arc::new(AtomicBool::new(true));
    let new_id = LoadedMod::register_mock_for_test(
        &mut world,
        "sim_mod",
        Arc::clone(&new_active),
        noop_shutdown,
        Some(resolve_v2),
        core::ptr::null_mut(),
    );
    let mut new_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, new_active, world_ptr, Some(new_id));
        let desc = ComponentDesc::new(
            UnitV2::COMPONENT_CSTR.as_ptr(),
            UnitV2::UUID,
            core::mem::size_of::<UnitV2>(),
            core::mem::align_of::<UnitV2>(),
        )
        .with_drop(drop_v2)
        .with_vtable(UnitV2::erased_vtable_ptr());
        let rc_v2 =
            // SAFETY: `load_ctx`, `desc`, and `&mut new_handle` are valid for this test registration.
            unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const desc,
                    &raw mut new_handle,
                )
            };
        assert_eq!(rc_v2, FfiResult::OK);
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    restore_mod_dynamic_columns_after_reload(&mut world, new_id, snap)
        .expect("restore V2 columns succeeds");
    migrate_mod_scoped_entities(&mut world, old_id, new_id);

    assert_eq!(world.get::<ModScoped>(e_active), Some(&ModScoped(new_id)));
    assert_eq!(world.get::<ModScoped>(e_disabled), Some(&ModScoped(new_id)));

    let comp_v2_id = world
        .resource::<ModRegistry>()
        .get_by_handle(new_handle)
        .unwrap()
        .component_id;
    // SAFETY: `comp_v2_id` holds live `UnitV2` values constructed by `init_v2` + `import_field`.
    unsafe {
        let p1 = world
            .entity(e_active)
            .get_by_id(comp_v2_id)
            .unwrap()
            .deref::<UnitV2>();
        assert_eq!(p1.hp, -7i64);
        assert!((p1.shield - 12.5).abs() < f32::EPSILON);
        assert_eq!(p1.tag, "knight");
        assert_eq!(p1.scores, vec![4, 5, 6]);

        let p2 = world
            .entity(e_disabled)
            .get_by_id(comp_v2_id)
            .unwrap()
            .deref::<UnitV2>();
        assert_eq!(p2.hp, 88i64);
        assert!((p2.shield - 12.5).abs() < f32::EPSILON);
        assert_eq!(p2.tag, "archer");
        assert_eq!(p2.scores, vec![1, 2]);
    }

    // Permanent unload despawns both ModScoped(new_id) entities (including Disabled) and drops V2!
    crate::loader::mod_host::excise_and_unload_mod_in_world(&mut world, new_id, |_| {})
        .expect("permanent unload succeeds");
    assert_eq!(V2_DROPS.load(Ordering::SeqCst), 2);
    assert!(!world.entities().contains(e_active));
    assert!(!world.entities().contains(e_disabled));
}

#[test]
fn zero_copy_fingerprint_retention_reload_under_miri() {
    static EXPORT_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct FastPod {
        x: i32,
        y: i32,
    }

    const FAST_UUID: ComponentUuid = ComponentUuid::from_parts("sim::FastPod", 8, 4, 0);
    const FAST_FP: u64 = 0x51A0_F1A6_0000_0001;
    const CHANGED_FP: u64 = 0x51A0_F1A6_0000_0002;

    unsafe extern "C" fn forbidden_export_fields(
        _src: *const c_void,
        _sink: Option<BamExportFieldsSinkFn>,
        _sink_ctx: *mut c_void,
    ) -> FfiResult {
        EXPORT_CALLS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }
    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn resolve_sym(
        _inst: *const BamInstance,
        name: *const c_char,
    ) -> *const c_void {
        // SAFETY: `name` is a valid NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return core::ptr::null();
        };
        if s == "bam.v0.sim::FastPod.export_fields" {
            return forbidden_export_fields as *const c_void;
        }
        core::ptr::null()
    }

    EXPORT_CALLS.store(0, Ordering::SeqCst);

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModQuarantine::new());

    let old_active = Arc::new(AtomicBool::new(true));
    let old_id = LoadedMod::register_mock_for_test(
        &mut world,
        "fast_pod_mod",
        Arc::clone(&old_active),
        noop_shutdown,
        Some(resolve_sym),
        core::ptr::null_mut(),
    );

    let mut old_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, old_active, world_ptr, Some(old_id));
        let desc = ComponentDesc::new(
            c"sim::FastPod".as_ptr(),
            FAST_UUID,
            core::mem::size_of::<FastPod>(),
            core::mem::align_of::<FastPod>(),
        )
        .with_schema_fingerprint(FAST_FP);
        // SAFETY: `load_ctx`, `desc`, and `&mut old_handle` are valid for this call.
        let rc = unsafe {
            host_register_component(load_ctx.as_ffi_ptr(), &raw const desc, &raw mut old_handle)
        };
        assert_eq!(rc, FfiResult::OK);
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    let old_comp_id = world
        .resource::<ModRegistry>()
        .get_by_handle(old_handle)
        .unwrap()
        .component_id;

    let e_active = world.spawn(ModScoped(old_id)).id();
    let e_disabled = world.spawn((ModScoped(old_id), Disabled)).id();
    let e_persistent = world.spawn_empty().id();
    // SAFETY: `old_comp_id` has exact `FastPod` layout.
    unsafe {
        OwningPtr::make(FastPod { x: 10, y: 20 }, |p| {
            world.entity_mut(e_active).insert_by_id(old_comp_id, p);
        });
        OwningPtr::make(FastPod { x: -30, y: 40 }, |p| {
            world.entity_mut(e_disabled).insert_by_id(old_comp_id, p);
        });
        OwningPtr::make(FastPod { x: 77, y: 88 }, |p| {
            world.entity_mut(e_persistent).insert_by_id(old_comp_id, p);
        });
    }

    // Capture raw column data pointer for `e_active`'s table prior to reload.
    let before_table_id = world.entity(e_active).location().table_id;
    let before_col_ptr = {
        let table = world.storages().tables.get(before_table_id).unwrap();
        let col = table.get_column(old_comp_id).unwrap();
        // SAFETY: Table has at least 1 entity so row 0 is in bounds.
        unsafe {
            col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                .as_ptr() as usize
        }
    };

    // Snapshot: must retain zero-copy without invoking `export_fields` or copying any bytes.
    let snap =
        snapshot_mod_dynamic_columns_for_reload(&mut world, old_id).expect("snapshot succeeds");
    assert_eq!(snap.bytes_copied, 0);
    assert!(snap.columns.is_empty());
    assert_eq!(snap.retained_zero_copy.len(), 1);
    assert_eq!(snap.retained_zero_copy[0].component_id, old_comp_id);
    assert_eq!(snap.retained_zero_copy[0].schema_fingerprint, FAST_FP);
    assert_eq!(EXPORT_CALLS.load(Ordering::SeqCst), 0);

    excise_and_unload_mod_for_reload_in_world(&mut world, old_id, |_| {})
        .expect("unload for reload succeeds");

    let new_active = Arc::new(AtomicBool::new(true));
    let new_id = LoadedMod::register_mock_for_test(
        &mut world,
        "fast_pod_mod",
        Arc::clone(&new_active),
        noop_shutdown,
        Some(resolve_sym),
        core::ptr::null_mut(),
    );

    let mut new_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, new_active, world_ptr, Some(new_id));

        // Mismatched non-zero fingerprint must be rejected with TYPE_MISMATCH.
        let bad_desc = ComponentDesc::new(
            c"sim::FastPod".as_ptr(),
            FAST_UUID,
            core::mem::size_of::<FastPod>(),
            core::mem::align_of::<FastPod>(),
        )
        .with_schema_fingerprint(CHANGED_FP);
        let mut bad_handle = BamComponent::INVALID;
        // SAFETY: Valid test pointers.
        let rc_bad = unsafe {
            host_register_component(
                load_ctx.as_ffi_ptr(),
                &raw const bad_desc,
                &raw mut bad_handle,
            )
        };
        assert_eq!(rc_bad, FfiResult::TYPE_MISMATCH);

        // Matching fingerprint retains exact handle, ComponentId, and BlobVec storage.
        let good_desc = ComponentDesc::new(
            c"sim::FastPod".as_ptr(),
            FAST_UUID,
            core::mem::size_of::<FastPod>(),
            core::mem::align_of::<FastPod>(),
        )
        .with_schema_fingerprint(FAST_FP);
        // SAFETY: Valid test pointers.
        let rc_good = unsafe {
            host_register_component(
                load_ctx.as_ffi_ptr(),
                &raw const good_desc,
                &raw mut new_handle,
            )
        };
        assert_eq!(rc_good, FfiResult::OK);
        assert_eq!(new_handle, old_handle);
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    restore_mod_dynamic_columns_after_reload(&mut world, new_id, snap).expect("restore succeeds");
    migrate_mod_scoped_entities(&mut world, old_id, new_id);

    let retained_entry = world
        .resource::<ModRegistry>()
        .get_by_handle(new_handle)
        .unwrap();
    assert_eq!(retained_entry.component_id, old_comp_id);
    assert_eq!(retained_entry.schema_fingerprint, FAST_FP);
    assert_eq!(retained_entry.owner, Some(new_id));

    // Exact same Bevy BlobVec backing allocation pointer — 0 bytes copied!
    let after_table_id = world.entity(e_active).location().table_id;
    assert_eq!(after_table_id, before_table_id);
    let after_col_ptr = {
        let table = world.storages().tables.get(after_table_id).unwrap();
        let col = table.get_column(old_comp_id).unwrap();
        // SAFETY: Table still holds `e_active` at row 0.
        unsafe {
            col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                .as_ptr() as usize
        }
    };
    assert_eq!(after_col_ptr, before_col_ptr);
    assert_eq!(EXPORT_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `old_comp_id` holds live `FastPod` values.
    unsafe {
        assert_eq!(
            *world
                .entity(e_active)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<FastPod>(),
            FastPod { x: 10, y: 20 }
        );
        assert_eq!(
            *world
                .entity(e_disabled)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<FastPod>(),
            FastPod { x: -30, y: 40 }
        );
        assert_eq!(
            *world
                .entity(e_persistent)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<FastPod>(),
            FastPod { x: 77, y: 88 }
        );
    }
    assert_eq!(world.get::<ModScoped>(e_active), Some(&ModScoped(new_id)));
    assert_eq!(world.get::<ModScoped>(e_disabled), Some(&ModScoped(new_id)));
}

#[test]
fn migrate_component_column_8b_to_16b_dense_sparse_and_error_rollback_under_miri() {
    use crate::ffi::v03_ext::host_migrate_component_column;
    use bevy_abi_mod_sys::{BAM_COMPONENT_FLAG_SPARSE_SET, BamFieldDesc, BamFieldKind};

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct PosV1 {
        x: f32,
        y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct PosV2 {
        x: f64,
        y: f64,
    }

    const UUID_V1: ComponentUuid = ComponentUuid::from_parts("sim::PosMig", 8, 4, 1);
    const UUID_V2: ComponentUuid = ComponentUuid::from_parts("sim::PosMig", 16, 8, 2);
    const FP_V1: u64 = 0x1111_2222_3333_4444;
    const FP_V2: u64 = 0x5555_6666_7777_8888;
    const V2_FIELDS: [BamFieldDesc; 2] = [
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("x"),
            StrView::from_str("f64"),
            0,
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("y"),
            StrView::from_str("f64"),
            8,
            8,
            8,
        ),
    ];

    unsafe extern "C" fn widen_pos(
        src_row: *const c_void,
        dst_row: *mut c_void,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `src_row` points to initialized `PosV1`; `dst_row` is aligned `PosV2` storage.
        let old = unsafe { core::ptr::read(src_row.cast::<PosV1>()) };
        let bias = if user_data.is_null() {
            0.0
        } else {
            // SAFETY: `user_data` points to an `f64` stack local when non-null.
            unsafe { *user_data.cast::<f64>() }
        };
        // SAFETY: `dst_row` is valid writable `PosV2` storage.
        unsafe {
            core::ptr::write(
                dst_row.cast::<PosV2>(),
                PosV2 {
                    x: f64::from(old.x) + bias,
                    y: f64::from(old.y) - bias,
                },
            );
        }
        FfiResult::OK
    }

    unsafe extern "C" fn failing_migrate(
        src_row: *const c_void,
        dst_row: *mut c_void,
        counter: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `counter` points to an `AtomicUsize` for this synchronous test call.
        let seen = unsafe { &*counter.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
        if seen == 2 {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        // SAFETY: `widen_pos` handles valid `PosV1 -> PosV2` pointers with null user_data.
        unsafe { widen_pos(src_row, dst_row, core::ptr::null_mut()) }
    }

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());

    let active = Arc::new(AtomicBool::new(true));
    let mut dense_handle = BamComponent::INVALID;
    let mut sparse_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);

        let dense_desc = ComponentDesc::new(
            c"sim::PosMig".as_ptr(),
            UUID_V1,
            core::mem::size_of::<PosV1>(),
            core::mem::align_of::<PosV1>(),
        )
        .with_schema_fingerprint(FP_V1);
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const dense_desc,
                    &raw mut dense_handle,
                )
            },
            FfiResult::OK
        );

        let sparse_desc = ComponentDesc::new(
            c"sim::SparsePosMig".as_ptr(),
            ComponentUuid::from_parts("sim::SparsePosMig", 8, 4, 1),
            core::mem::size_of::<PosV1>(),
            core::mem::align_of::<PosV1>(),
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
        .with_schema_fingerprint(FP_V1);
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const sparse_desc,
                    &raw mut sparse_handle,
                )
            },
            FfiResult::OK
        );

        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    let old_dense_id = world
        .resource::<ModRegistry>()
        .get_by_handle(dense_handle)
        .unwrap()
        .component_id;
    let old_sparse_id = world
        .resource::<ModRegistry>()
        .get_by_handle(sparse_handle)
        .unwrap()
        .component_id;

    let mut dense_entities = Vec::new();
    for i in 0..4u32 {
        let e = if i == 3 {
            world.spawn(Disabled).id()
        } else {
            world.spawn_empty().id()
        };
        // SAFETY: `old_dense_id` has exact `PosV1` layout.
        unsafe {
            OwningPtr::make(
                PosV1 {
                    x: i as f32 * 2.0,
                    y: i as f32 * -3.0,
                },
                |p| {
                    world.entity_mut(e).insert_by_id(old_dense_id, p);
                },
            );
        }
        dense_entities.push(e);
    }

    let mut sparse_entities = Vec::new();
    for i in 0..3u32 {
        let e = world.spawn_empty().id();
        // SAFETY: `old_sparse_id` has exact `PosV1` layout.
        unsafe {
            OwningPtr::make(
                PosV1 {
                    x: 10.0 + i as f32,
                    y: 20.0 + i as f32,
                },
                |p| {
                    world.entity_mut(e).insert_by_id(old_sparse_id, p);
                },
            );
        }
        sparse_entities.push(e);
    }

    let v2_dense_desc = ComponentDesc::new(
        c"sim::PosMig".as_ptr(),
        UUID_V2,
        core::mem::size_of::<PosV2>(),
        core::mem::align_of::<PosV2>(),
    )
    .with_schema_fingerprint(FP_V2)
    .with_fields(V2_FIELDS.as_ptr(), V2_FIELDS.len());

    let v1_same_desc = ComponentDesc::new(
        c"sim::PosMig".as_ptr(),
        UUID_V1,
        core::mem::size_of::<PosV1>(),
        core::mem::align_of::<PosV1>(),
    )
    .with_schema_fingerprint(FP_V1);

    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);

        // 1. Mismatched `old_fingerprint` -> TYPE_MISMATCH.
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    dense_handle,
                    0xDEAD_BEEF,
                    &raw const v2_dense_desc,
                    Some(widen_pos),
                    core::ptr::null_mut(),
                )
            },
            FfiResult::TYPE_MISMATCH
        );

        // 2. Unchanged non-zero fingerprint fast-path -> OK with 0 migrate_fn calls.
        let fail_counter = AtomicUsize::new(0);
        assert_eq!(
            // SAFETY: Valid test pointers; fast-path must not invoke `failing_migrate`.
            unsafe {
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    dense_handle,
                    FP_V1,
                    &raw const v1_same_desc,
                    Some(failing_migrate),
                    (&raw const fail_counter).cast_mut().cast::<c_void>(),
                )
            },
            FfiResult::OK
        );
        assert_eq!(fail_counter.load(Ordering::SeqCst), 0);

        // 3. Error mid-table aborts atomically without modifying registry or entity data.
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    dense_handle,
                    FP_V1,
                    &raw const v2_dense_desc,
                    Some(failing_migrate),
                    (&raw const fail_counter).cast_mut().cast::<c_void>(),
                )
            },
            FfiResult::CAPACITY_EXCEEDED
        );
        assert_eq!(
            load_ctx
                .registry
                .get_by_handle(dense_handle)
                .unwrap()
                .component_id,
            old_dense_id
        );

        // 4. Successful dense 8B -> 16B migration across active + Disabled tables.
        let mut bias = 0.5_f64;
        assert_eq!(
            // SAFETY: Valid test pointers; `bias` lives for the call.
            unsafe {
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    dense_handle,
                    FP_V1,
                    &raw const v2_dense_desc,
                    Some(widen_pos),
                    (&raw mut bias).cast::<c_void>(),
                )
            },
            FfiResult::OK
        );

        // 5. SparseSet 8B -> Table 16B migration.
        let sparse_to_table_desc = ComponentDesc::new(
            c"sim::SparsePosMig".as_ptr(),
            ComponentUuid::from_parts("sim::SparsePosMig", 16, 8, 2),
            core::mem::size_of::<PosV2>(),
            core::mem::align_of::<PosV2>(),
        )
        .with_schema_fingerprint(FP_V2);
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_migrate_component_column(
                    load_ctx.as_ffi_ptr(),
                    sparse_handle,
                    FP_V1,
                    &raw const sparse_to_table_desc,
                    Some(widen_pos),
                    core::ptr::null_mut(),
                )
            },
            FfiResult::OK
        );

        load_ctx.finish_init();
    }

    let reg = world.resource::<ModRegistry>();
    let new_dense_entry = reg.get_by_handle(dense_handle).unwrap().clone();
    assert_ne!(new_dense_entry.component_id, old_dense_id);
    assert_eq!(new_dense_entry.size, 16);
    assert_eq!(new_dense_entry.align, 8);
    assert_eq!(new_dense_entry.schema_fingerprint, FP_V2);
    assert_eq!(new_dense_entry.uuid, UUID_V2);
    assert_eq!(new_dense_entry.fields.len(), 2);
    assert_eq!(reg.get_by_uuid(&UUID_V2).unwrap().handle, dense_handle);
    assert!(reg.get_by_uuid(&UUID_V1).is_none());

    for (i, &e) in dense_entities.iter().enumerate() {
        assert!(world.entity(e).get_by_id(old_dense_id).is_err());
        // SAFETY: `new_dense_entry.component_id` holds initialized `PosV2` values.
        let val = unsafe {
            *world
                .entity(e)
                .get_by_id(new_dense_entry.component_id)
                .unwrap()
                .deref::<PosV2>()
        };
        assert_eq!(
            val,
            PosV2 {
                x: (i as f64) * 2.0 + 0.5,
                y: (i as f64) * -3.0 - 0.5,
            }
        );
    }

    let new_sparse_entry = reg.get_by_handle(sparse_handle).unwrap().clone();
    assert_ne!(new_sparse_entry.component_id, old_sparse_id);
    assert!(!new_sparse_entry.is_sparse_set());
    assert_eq!(new_sparse_entry.size, 16);
    for (i, &e) in sparse_entities.iter().enumerate() {
        assert!(world.entity(e).get_by_id(old_sparse_id).is_err());
        // SAFETY: `new_sparse_entry.component_id` holds initialized `PosV2` values.
        let val = unsafe {
            *world
                .entity(e)
                .get_by_id(new_sparse_entry.component_id)
                .unwrap()
                .deref::<PosV2>()
        };
        assert_eq!(
            val,
            PosV2 {
                x: 10.0 + i as f64,
                y: 20.0 + i as f64,
            }
        );
    }
}
