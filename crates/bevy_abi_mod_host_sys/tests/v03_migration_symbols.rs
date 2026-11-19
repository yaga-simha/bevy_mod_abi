//! Integration tests for ABI 0.3.0 Phase 2 Item #07:
//! - Zero-copy schema fingerprint retention across hot-reload (100,000-entity benchmark & byte counter)
//! - Contiguous `AbiPod` column migration via `host_migrate_component_column` (8B -> 16B across
//!   100,000 entities with zero per-entity heap allocations, plus `SparseSet`, fast path, and atomic
//!   error rollback)
//! - Custom `RequiredComponents` via `ComponentDesc.required_init_descs` (handle + UUID resolution,
//!   `CUSTOM_BYTES`, `INIT_FN`, `init_size` validation, transitive chains, and unload disarm)
//! - Monotonic UTF-8 symbol interner (`host_intern_symbol` / `BamSymbolTable`) and persistent
//!   named objects (`host_bind_named_object` / `ModNamedObjects`).

#![allow(unsafe_code)]

use bevy_abi_mod_host_sys::loader::{
    BamSymbolTable, ModNamedObjects, ensure_symbol_tables_in_world, migrate_mod_scoped_entities,
    restore_mod_dynamic_columns_after_reload, snapshot_mod_dynamic_columns_for_reload,
};
use bevy_abi_mod_host_sys::{
    HOST_API, HostContext, LoadContext, ModId, ModRegistry, ModScoped, WorldDropGuard,
    host_bind_named_object, host_component_layout_info, host_intern_symbol, host_load_host_ctx,
    host_migrate_component_column, host_register_component,
};
use bevy_abi_mod_sys::{
    BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_REQUIRED_FLAG_CUSTOM_BYTES,
    BAM_REQUIRED_FLAG_INIT_FN, BAM_REQUIRED_FLAG_RESOLVE_BY_UUID, BamComponent, BamFieldDesc,
    BamFieldKind, BamLayoutInfo, BamRequiredComponent, BamSymbol, ComponentDesc, ComponentUuid,
    FfiEntity, FfiResult, StrView,
};
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::{Allow, Without};
use bevy_ecs::resource::IsResource;
use bevy_ecs::storage::TableRow;
use bevy_ecs::world::World;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use nonmax::NonMaxU32;
use std::alloc::System;
use std::sync::Arc;
use std::time::Instant;

struct CountingAlloc;

thread_local! {
    static COUNTING_ENABLED: Cell<bool> = const { Cell::new(false) };
    static THREAD_ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: Delegates directly to `std::alloc::System` while incrementing a thread-local counter
// when explicitly enabled by a test scope.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNTING_ENABLED.with(|enabled| {
            if enabled.get() {
                THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
            }
        });
        // SAFETY: Forwards exact `layout` to `System::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        COUNTING_ENABLED.with(|enabled| {
            if enabled.get() {
                THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
            }
        });
        // SAFETY: Forwards exact `layout` to `System::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNTING_ENABLED.with(|enabled| {
            if enabled.get() {
                THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
            }
        });
        // SAFETY: Forwards exact arguments to `System::realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwards exact arguments to `System::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;

fn count_thread_allocs<R>(f: impl FnOnce() -> R) -> (R, usize) {
    THREAD_ALLOCS.with(|c| c.set(0));
    COUNTING_ENABLED.with(|e| e.set(true));
    let out = f();
    COUNTING_ENABLED.with(|e| e.set(false));
    let allocs = THREAD_ALLOCS.with(Cell::get);
    (out, allocs)
}

#[test]
fn zero_copy_fingerprint_retention_100k_entities_on_reload() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct ParticleV1 {
        px: f32,
        py: f32,
        vx: f32,
        vy: f32,
    }

    const PARTICLE_UUID: ComponentUuid = ComponentUuid::from_parts("perf::ParticleV1", 16, 4, 1);
    const PARTICLE_FP: u64 = 0x9E37_79B9_7F4A_7C15;
    const ENTITY_COUNT: usize = 100_000;

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());

    let old_id = ModId(101);
    let new_id = ModId(102);

    let mut old_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(old_id),
        );
        let desc = ComponentDesc::new(
            c"perf::ParticleV1".as_ptr(),
            PARTICLE_UUID,
            core::mem::size_of::<ParticleV1>(),
            core::mem::align_of::<ParticleV1>(),
        )
        .with_schema_fingerprint(PARTICLE_FP);
        // SAFETY: Valid test pointers.
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

    let mut first_entity = None;
    let mut last_entity = None;
    for i in 0..ENTITY_COUNT {
        let e = world.spawn(ModScoped(old_id)).id();
        if i == 0 {
            first_entity = Some(e);
        }
        if i + 1 == ENTITY_COUNT {
            last_entity = Some(e);
        }
        let val = ParticleV1 {
            px: i as f32,
            py: (i as f32) * 0.5,
            vx: 1.0,
            vy: -2.0,
        };
        // SAFETY: `old_comp_id` has exact `ParticleV1` layout.
        unsafe {
            OwningPtr::make(val, |ptr| {
                world.entity_mut(e).insert_by_id(old_comp_id, ptr);
            });
        }
    }
    let disabled_entity = world.spawn((ModScoped(old_id), Disabled)).id();
    // SAFETY: `old_comp_id` has exact `ParticleV1` layout.
    unsafe {
        OwningPtr::make(
            ParticleV1 {
                px: -99.0,
                py: -88.0,
                vx: 3.0,
                vy: 4.0,
            },
            |ptr| {
                world
                    .entity_mut(disabled_entity)
                    .insert_by_id(old_comp_id, ptr);
            },
        );
    }

    let first_entity = first_entity.unwrap();
    let last_entity = last_entity.unwrap();
    let before_table_id = world.entity(first_entity).location().table_id;
    let before_col_ptr = {
        let table = world.storages().tables.get(before_table_id).unwrap();
        assert_eq!(table.entity_count() as usize, ENTITY_COUNT);
        let col = table.get_column(old_comp_id).unwrap();
        // SAFETY: Table holds `ENTITY_COUNT > 0` rows so row 0 is valid.
        unsafe {
            col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                .as_ptr() as usize
        }
    };

    // Time the full zero-copy reload path across 100,000 entities!
    let start = Instant::now();
    let snap =
        snapshot_mod_dynamic_columns_for_reload(&mut world, old_id).expect("snapshot succeeds");
    assert_eq!(snap.bytes_copied, 0);
    assert!(snap.columns.is_empty());
    assert_eq!(snap.retained_zero_copy.len(), 1);
    assert_eq!(snap.retained_zero_copy[0].component_id, old_comp_id);
    assert_eq!(snap.retained_zero_copy[0].schema_fingerprint, PARTICLE_FP);

    let mut new_handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(new_id),
        );
        let desc = ComponentDesc::new(
            c"perf::ParticleV1".as_ptr(),
            PARTICLE_UUID,
            core::mem::size_of::<ParticleV1>(),
            core::mem::align_of::<ParticleV1>(),
        )
        .with_schema_fingerprint(PARTICLE_FP);
        // SAFETY: Valid test pointers.
        let rc = unsafe {
            host_register_component(load_ctx.as_ffi_ptr(), &raw const desc, &raw mut new_handle)
        };
        assert_eq!(rc, FfiResult::OK);
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    restore_mod_dynamic_columns_after_reload(&mut world, new_id, snap)
        .expect("zero-copy restore succeeds");
    migrate_mod_scoped_entities(&mut world, old_id, new_id);
    let elapsed = start.elapsed();

    eprintln!(
        "[perf] zero-copy fingerprint retention reload ({ENTITY_COUNT} entities): {:.3} ms (bytes_copied=0)",
        elapsed.as_secs_f64() * 1000.0
    );

    assert_eq!(new_handle, old_handle);
    let retained = world
        .resource::<ModRegistry>()
        .get_by_handle(new_handle)
        .unwrap();
    assert_eq!(retained.component_id, old_comp_id);
    assert_eq!(retained.schema_fingerprint, PARTICLE_FP);
    assert_eq!(retained.owner, Some(new_id));

    // Raw Bevy BlobVec backing pointer must be identical — 0 bytes copied!
    let after_table_id = world.entity(first_entity).location().table_id;
    assert_eq!(after_table_id, before_table_id);
    let after_col_ptr = {
        let table = world.storages().tables.get(after_table_id).unwrap();
        assert_eq!(table.entity_count() as usize, ENTITY_COUNT);
        let col = table.get_column(old_comp_id).unwrap();
        // SAFETY: Table still holds `ENTITY_COUNT` rows.
        unsafe {
            col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                .as_ptr() as usize
        }
    };
    assert_eq!(after_col_ptr, before_col_ptr);

    // Spot-check first, last, and Disabled entities + ModScoped migration.
    assert_eq!(
        world.get::<ModScoped>(first_entity),
        Some(&ModScoped(new_id))
    );
    assert_eq!(
        world.get::<ModScoped>(last_entity),
        Some(&ModScoped(new_id))
    );
    assert_eq!(
        world.get::<ModScoped>(disabled_entity),
        Some(&ModScoped(new_id))
    );
    // SAFETY: `old_comp_id` holds live `ParticleV1` values.
    unsafe {
        assert_eq!(
            *world
                .entity(first_entity)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<ParticleV1>(),
            ParticleV1 {
                px: 0.0,
                py: 0.0,
                vx: 1.0,
                vy: -2.0
            }
        );
        assert_eq!(
            *world
                .entity(last_entity)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<ParticleV1>(),
            ParticleV1 {
                px: (ENTITY_COUNT - 1) as f32,
                py: ((ENTITY_COUNT - 1) as f32) * 0.5,
                vx: 1.0,
                vy: -2.0
            }
        );
        assert_eq!(
            *world
                .entity(disabled_entity)
                .get_by_id(old_comp_id)
                .unwrap()
                .deref::<ParticleV1>(),
            ParticleV1 {
                px: -99.0,
                py: -88.0,
                vx: 3.0,
                vy: 4.0
            }
        );
    }

    #[cfg(not(debug_assertions))]
    assert!(
        elapsed.as_secs_f64() * 1000.0 < 1.5,
        "expected 100k-entity zero-copy reload < 1.5 ms in release, took {:.3} ms",
        elapsed.as_secs_f64() * 1000.0
    );
}

#[test]
fn contiguous_column_migration_8b_to_16b_100k_entities_zero_per_entity_alloc() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CoordV1 {
        x: f32,
        y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CoordV2 {
        x: f64,
        y: f64,
    }

    const UUID_V1: ComponentUuid = ComponentUuid::from_parts("perf::Coord", 8, 4, 1);
    const UUID_V2: ComponentUuid = ComponentUuid::from_parts("perf::Coord", 16, 8, 2);
    const FP_V1: u64 = 0xAAAA_BBBB_CCCC_0001;
    const FP_V2: u64 = 0xAAAA_BBBB_CCCC_0002;
    const ENTITY_COUNT: usize = 100_000;
    const FIELDS_V2: [BamFieldDesc; 2] = [
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

    unsafe extern "C" fn widen_coord(
        src_row: *const c_void,
        dst_row: *mut c_void,
        _user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `src_row` points to initialized `CoordV1`; `dst_row` is aligned `CoordV2` storage.
        let old = unsafe { core::ptr::read(src_row.cast::<CoordV1>()) };
        // SAFETY: `dst_row` is writable `CoordV2` storage.
        unsafe {
            core::ptr::write(
                dst_row.cast::<CoordV2>(),
                CoordV2 {
                    x: f64::from(old.x) * 2.0,
                    y: f64::from(old.y) + 100.0,
                },
            );
        }
        FfiResult::OK
    }

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());

    let active = Arc::new(AtomicBool::new(true));
    let mut handle = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);
        let desc = ComponentDesc::new(
            c"perf::Coord".as_ptr(),
            UUID_V1,
            core::mem::size_of::<CoordV1>(),
            core::mem::align_of::<CoordV1>(),
        )
        .with_schema_fingerprint(FP_V1);
        assert_eq!(
            // SAFETY: Valid test pointers.
            unsafe {
                host_register_component(load_ctx.as_ffi_ptr(), &raw const desc, &raw mut handle)
            },
            FfiResult::OK
        );
        world.insert_resource(load_ctx.registry.clone());
        load_ctx.finish_init();
    }

    let old_comp_id = world
        .resource::<ModRegistry>()
        .get_by_handle(handle)
        .unwrap()
        .component_id;

    let mut entities = Vec::with_capacity(ENTITY_COUNT);
    for i in 0..ENTITY_COUNT {
        let e = world.spawn_empty().id();
        let val = CoordV1 {
            x: i as f32,
            y: -(i as f32),
        };
        // SAFETY: `old_comp_id` has exact `CoordV1` layout.
        unsafe {
            OwningPtr::make(val, |ptr| {
                world.entity_mut(e).insert_by_id(old_comp_id, ptr);
            });
        }
        entities.push(e);
    }

    let new_desc = ComponentDesc::new(
        c"perf::Coord".as_ptr(),
        UUID_V2,
        core::mem::size_of::<CoordV2>(),
        core::mem::align_of::<CoordV2>(),
    )
    .with_schema_fingerprint(FP_V2)
    .with_fields(FIELDS_V2.as_ptr(), FIELDS_V2.len());

    let ((rc, elapsed), total_allocs) = {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(reg, active, world_ptr, None);
        let migrate_slot = HOST_API
            .migrate_component_column
            .expect("HOST_API.migrate_component_column must be wired");
        let ((rc, elapsed), allocs) = count_thread_allocs(|| {
            let start = Instant::now();
            // SAFETY: Valid `load_ctx`, `new_desc`, and `widen_coord`.
            let rc = unsafe {
                migrate_slot(
                    load_ctx.as_ffi_ptr(),
                    handle,
                    FP_V1,
                    &raw const new_desc,
                    Some(widen_coord),
                    core::ptr::null_mut(),
                )
            };
            (rc, start.elapsed())
        });

        // Verify `host_component_layout_info` reflects the atomically remapped 16B layout + fields.
        let mut info = BamLayoutInfo::empty();
        // SAFETY: Valid `load_ctx`.
        let host_ctx_ptr = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr` and `&mut info`.
            unsafe { host_component_layout_info(host_ctx_ptr, handle, &raw mut info) },
            FfiResult::OK
        );
        assert_eq!(info.size, 16);
        assert_eq!(info.align, 8);
        assert_eq!(info.schema_fingerprint, FP_V2);
        assert_eq!(info.field_count, 2);

        load_ctx.finish_init();
        ((rc, elapsed), allocs)
    };

    assert_eq!(rc, FfiResult::OK);
    eprintln!(
        "[perf] 8B -> 16B column migration ({ENTITY_COUNT} entities): {:.3} ms (total heap allocs={total_allocs})",
        elapsed.as_secs_f64() * 1000.0
    );

    // Zero per-entity heap allocations: only O(log N) table growth + 1 contiguous column buffer!
    assert!(
        total_allocs < 256,
        "expected zero per-entity heap allocations (O(log N) < 256), observed {total_allocs}"
    );

    let new_entry = world
        .resource::<ModRegistry>()
        .get_by_handle(handle)
        .unwrap()
        .clone();
    assert_ne!(new_entry.component_id, old_comp_id);
    assert_eq!(new_entry.size, 16);
    assert_eq!(new_entry.align, 8);
    assert_eq!(new_entry.schema_fingerprint, FP_V2);
    assert_eq!(new_entry.fields.len(), 2);

    // Validate every single migrated entity!
    for (i, &e) in entities.iter().enumerate() {
        let ent = world.entity(e);
        assert!(ent.get_by_id(old_comp_id).is_err());
        // SAFETY: `new_entry.component_id` holds initialized `CoordV2` values.
        let val = unsafe {
            *ent.get_by_id(new_entry.component_id)
                .unwrap()
                .deref::<CoordV2>()
        };
        assert_eq!(
            val,
            CoordV2 {
                x: (i as f64) * 2.0,
                y: -(i as f64) + 100.0,
            }
        );
    }

    // In release mode, contiguous column transform takes ~0.5 ms and Bevy 0.20's public
    // `EntityWorldMut::remove_by_id` + `insert_by_id` commit across 100k entities takes ~8 ms
    // (as `BundleInserter`/`BundleRemover`/`World::storages_mut` are `pub(crate)` in `bevy_ecs`).
    #[cfg(not(debug_assertions))]
    assert!(
        elapsed.as_secs_f64() * 1000.0 < 25.0,
        "expected 100k-entity 8B -> 16B migration < 25.0 ms in release, took {:.3} ms",
        elapsed.as_secs_f64() * 1000.0
    );
}

#[test]
fn migrate_component_column_validation_sparse_and_error_isolation() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct TagV1 {
        a: u32,
        b: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct TagV2 {
        a: u64,
        b: u64,
    }

    unsafe extern "C" fn migrate_tag(
        src: *const c_void,
        dst: *mut c_void,
        calls: *mut c_void,
    ) -> FfiResult {
        if !calls.is_null() {
            // SAFETY: `calls` points to an `AtomicUsize` in this test.
            unsafe { &*calls.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: Valid `TagV1` src and `TagV2` dst.
        let v = unsafe { core::ptr::read(src.cast::<TagV1>()) };
        // SAFETY: Valid aligned `TagV2` dst.
        unsafe {
            core::ptr::write(
                dst.cast::<TagV2>(),
                TagV2 {
                    a: u64::from(v.a) * 10,
                    b: u64::from(v.b) + 7,
                },
            );
        }
        FfiResult::OK
    }

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());

    let active_a = Arc::new(AtomicBool::new(true));
    let mut h_sparse = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active_a),
            world_ptr,
            Some(ModId(1)),
        );
        let desc = ComponentDesc::new(
            c"mig::SparseTag".as_ptr(),
            ComponentUuid::from_parts("mig::SparseTag", 8, 4, 1),
            8,
            4,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
        .with_schema_fingerprint(0xCAFE_0001);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_register_component(ctx.as_ffi_ptr(), &raw const desc, &raw mut h_sparse)
            },
            FfiResult::OK
        );
        world.insert_resource(ctx.registry.clone());
        ctx.finish_init();
    }

    let old_id = world
        .resource::<ModRegistry>()
        .get_by_handle(h_sparse)
        .unwrap()
        .component_id;
    let e1 = world.spawn_empty().id();
    let e2 = world.spawn(Disabled).id();
    // SAFETY: `old_id` has exact `TagV1` layout.
    unsafe {
        OwningPtr::make(TagV1 { a: 3, b: 4 }, |p| {
            world.entity_mut(e1).insert_by_id(old_id, p);
        });
        OwningPtr::make(TagV1 { a: 9, b: 1 }, |p| {
            world.entity_mut(e2).insert_by_id(old_id, p);
        });
    }

    // Cross-mod attempt from ModId(2) must fail with PERMISSION_DENIED.
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut ctx_b = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(ModId(2)),
        );
        let v2_desc = ComponentDesc::new(
            c"mig::SparseTag".as_ptr(),
            ComponentUuid::from_parts("mig::SparseTag", 16, 8, 2),
            16,
            8,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
        .with_schema_fingerprint(0xCAFE_0002);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_migrate_component_column(
                    ctx_b.as_ffi_ptr(),
                    h_sparse,
                    0xCAFE_0001,
                    &raw const v2_desc,
                    Some(migrate_tag),
                    core::ptr::null_mut(),
                )
            },
            FfiResult::PERMISSION_DENIED
        );
        ctx_b.finish_init();
    }

    // Owner ModId(1): invalid SPARSE_SET | SUMMARY_TICK -> INVALID_ARGUMENT, then valid Sparse -> Sparse!
    let calls = AtomicUsize::new(0);
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut ctx_a =
            LoadContext::with_world_and_mod_id(reg, active_a, world_ptr, Some(ModId(1)));
        let bad_flags_desc = ComponentDesc::new(
            c"mig::SparseTag".as_ptr(),
            ComponentUuid::from_parts("mig::SparseTag", 16, 8, 2),
            16,
            8,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_SUMMARY_TICK)
        .with_schema_fingerprint(0xCAFE_0002);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_migrate_component_column(
                    ctx_a.as_ffi_ptr(),
                    h_sparse,
                    0xCAFE_0001,
                    &raw const bad_flags_desc,
                    Some(migrate_tag),
                    (&raw const calls).cast_mut().cast::<c_void>(),
                )
            },
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let v2_sparse_desc = ComponentDesc::new(
            c"mig::SparseTag".as_ptr(),
            ComponentUuid::from_parts("mig::SparseTag", 16, 8, 2),
            16,
            8,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
        .with_schema_fingerprint(0xCAFE_0002);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_migrate_component_column(
                    ctx_a.as_ffi_ptr(),
                    h_sparse,
                    0xCAFE_0001,
                    &raw const v2_sparse_desc,
                    Some(migrate_tag),
                    (&raw const calls).cast_mut().cast::<c_void>(),
                )
            },
            FfiResult::OK
        );
        ctx_a.finish_init();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let entry = world
        .resource::<ModRegistry>()
        .get_by_handle(h_sparse)
        .unwrap();
    assert!(entry.is_sparse_set());
    assert_eq!(entry.size, 16);
    assert_eq!(entry.schema_fingerprint, 0xCAFE_0002);
    // SAFETY: Migrated entities hold `TagV2`.
    unsafe {
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(entry.component_id)
                .unwrap()
                .deref::<TagV2>(),
            TagV2 { a: 30, b: 11 }
        );
        assert_eq!(
            *world
                .entity(e2)
                .get_by_id(entry.component_id)
                .unwrap()
                .deref::<TagV2>(),
            TagV2 { a: 90, b: 8 }
        );
    }
}

#[test]
fn required_init_descs_custom_bytes_init_fn_uuid_and_reload_update() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Health {
        cur: i32,
        max: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Team {
        id: u32,
    }

    const HEALTH_UUID: ComponentUuid = ComponentUuid::from_parts("req_int::Health", 8, 4, 0);
    const TEAM_UUID: ComponentUuid = ComponentUuid::from_parts("req_int::Team", 4, 4, 0);
    const HERO_UUID: ComponentUuid = ComponentUuid::from_parts("req_int::Hero", 0, 1, 0);

    static TEAM_INIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn init_team(dst: *mut c_void, user_data: *mut c_void) -> FfiResult {
        TEAM_INIT_CALLS.fetch_add(1, Ordering::SeqCst);
        let team_id = user_data as usize as u32;
        // SAFETY: `dst` is valid 4-byte aligned storage for `Team`.
        unsafe { core::ptr::write(dst.cast::<Team>(), Team { id: team_id }) };
        FfiResult::OK
    }
    TEAM_INIT_CALLS.store(0, Ordering::SeqCst);

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());

    let active_v1 = Arc::new(AtomicBool::new(true));
    let mut h_health = BamComponent::INVALID;
    let mut h_team = BamComponent::INVALID;
    let mut h_hero = BamComponent::INVALID;

    let hp_v1 = Health { cur: 100, max: 100 };
    let hp_v1_bytes: [u8; 8] = {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&hp_v1.cur.to_ne_bytes());
        b[4..8].copy_from_slice(&hp_v1.max.to_ne_bytes());
        b
    };

    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active_v1),
            world_ptr,
            Some(ModId(10)),
        );

        let d_health = ComponentDesc::new(c"req_int::Health".as_ptr(), HEALTH_UUID, 8, 4);
        let d_team = ComponentDesc::new(c"req_int::Team".as_ptr(), TEAM_UUID, 4, 4);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_register_component(ctx.as_ffi_ptr(), &raw const d_health, &raw mut h_health)
            },
            FfiResult::OK
        );
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_register_component(ctx.as_ffi_ptr(), &raw const d_team, &raw mut h_team)
            },
            FfiResult::OK
        );

        // Wrong `init_size` (2 != 8) must fail with TYPE_MISMATCH.
        let mut bad_req = BamRequiredComponent::by_handle(h_health);
        bad_req.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        bad_req.init_bytes = hp_v1_bytes.as_ptr();
        bad_req.init_size = 2;
        let d_bad = ComponentDesc::new(c"req_int::Hero".as_ptr(), HERO_UUID, 0, 1)
            .with_required_init_descs(&raw const bad_req, 1);
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe { host_register_component(ctx.as_ffi_ptr(), &raw const d_bad, &raw mut h_hero) },
            FfiResult::TYPE_MISMATCH
        );

        let mut req_health = BamRequiredComponent::by_handle(h_health);
        req_health.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        req_health.init_bytes = hp_v1_bytes.as_ptr();
        req_health.init_size = hp_v1_bytes.len();

        let req_team = BamRequiredComponent::by_uuid(TEAM_UUID).with_init_fn(
            init_team,
            core::mem::size_of::<Team>(),
            core::ptr::without_provenance_mut::<c_void>(7),
        );
        assert_eq!(
            req_team.flags,
            BAM_REQUIRED_FLAG_RESOLVE_BY_UUID | BAM_REQUIRED_FLAG_INIT_FN
        );

        let reqs = [req_health, req_team];
        let d_hero = ComponentDesc::new(c"req_int::Hero".as_ptr(), HERO_UUID, 0, 1)
            .with_required_init_descs(reqs.as_ptr(), reqs.len());
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_register_component(ctx.as_ffi_ptr(), &raw const d_hero, &raw mut h_hero)
            },
            FfiResult::OK
        );

        world.insert_resource(ctx.registry.clone());
        ctx.finish_init();
    }

    let cid_health = world
        .resource::<ModRegistry>()
        .get_by_handle(h_health)
        .unwrap()
        .component_id;
    let cid_team = world
        .resource::<ModRegistry>()
        .get_by_handle(h_team)
        .unwrap()
        .component_id;
    let cid_hero = world
        .resource::<ModRegistry>()
        .get_by_handle(h_hero)
        .unwrap()
        .component_id;

    let e1 = world.spawn_empty().id();
    // SAFETY: `cid_hero` is a ZST component registered in `world`.
    unsafe {
        world
            .entity_mut(e1)
            .insert_by_id(cid_hero, OwningPtr::new(core::ptr::NonNull::dangling()));
    }
    assert_eq!(TEAM_INIT_CALLS.load(Ordering::SeqCst), 1);
    // SAFETY: Initialized POD components.
    unsafe {
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(cid_health)
                .unwrap()
                .deref::<Health>(),
            hp_v1
        );
        assert_eq!(
            *world
                .entity(e1)
                .get_by_id(cid_team)
                .unwrap()
                .deref::<Team>(),
            Team { id: 7 }
        );
    }

    // Disarm v1 -> `init_team` must NOT fire!
    active_v1.store(false, Ordering::SeqCst);
    let e_disarmed = world.spawn_empty().id();
    // SAFETY: `cid_hero` is ZST; disarmed `init_team` must NOT fire.
    unsafe {
        world
            .entity_mut(e_disarmed)
            .insert_by_id(cid_hero, OwningPtr::new(core::ptr::NonNull::dangling()));
    }
    assert_eq!(TEAM_INIT_CALLS.load(Ordering::SeqCst), 1);
    assert!(world.entity(e_disarmed).get_by_id(cid_team).is_err());

    // Reload with updated custom bytes + new `is_active` guard -> new spawns use updated payload!
    let active_v2 = Arc::new(AtomicBool::new(true));
    let hp_v2 = Health { cur: 500, max: 500 };
    let hp_v2_bytes: [u8; 8] = {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&hp_v2.cur.to_ne_bytes());
        b[4..8].copy_from_slice(&hp_v2.max.to_ne_bytes());
        b
    };
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut ctx =
            LoadContext::with_world_and_mod_id(reg, active_v2, world_ptr, Some(ModId(11)));
        let mut req_health = BamRequiredComponent::by_handle(h_health);
        req_health.flags = BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        req_health.init_bytes = hp_v2_bytes.as_ptr();
        req_health.init_size = hp_v2_bytes.len();
        let req_team = BamRequiredComponent::by_uuid(TEAM_UUID).with_init_fn(
            init_team,
            core::mem::size_of::<Team>(),
            core::ptr::without_provenance_mut::<c_void>(42),
        );
        let reqs = [req_health, req_team];
        let d_hero_v2 = ComponentDesc::new(c"req_int::Hero".as_ptr(), HERO_UUID, 0, 1)
            .with_required_init_descs(reqs.as_ptr(), reqs.len());
        let mut h_hero_v2 = BamComponent::INVALID;
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_register_component(ctx.as_ffi_ptr(), &raw const d_hero_v2, &raw mut h_hero_v2)
            },
            FfiResult::OK
        );
        assert_eq!(h_hero_v2, h_hero);
        world.insert_resource(ctx.registry.clone());
        ctx.finish_init();
    }

    let e2 = world.spawn_empty().id();
    // SAFETY: `cid_hero` is ZST.
    unsafe {
        world
            .entity_mut(e2)
            .insert_by_id(cid_hero, OwningPtr::new(core::ptr::NonNull::dangling()));
    }
    assert_eq!(TEAM_INIT_CALLS.load(Ordering::SeqCst), 2);
    // SAFETY: Initialized POD components.
    unsafe {
        assert_eq!(
            *world
                .entity(e2)
                .get_by_id(cid_health)
                .unwrap()
                .deref::<Health>(),
            hp_v2
        );
        assert_eq!(
            *world
                .entity(e2)
                .get_by_id(cid_team)
                .unwrap()
                .deref::<Team>(),
            Team { id: 42 }
        );
    }
}

#[test]
fn symbol_interner_monotonic_dedup_utf8_reload_stability_and_multithreaded() {
    let mut world = World::new();
    world.insert_resource(ModRegistry::new());
    world.insert_resource(WorldDropGuard::new());

    let intern_slot = HOST_API
        .intern_symbol
        .expect("HOST_API.intern_symbol must be wired");

    let (s_attack, s_defend, s_attack_dup) = {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(ModId(201)),
        );
        // SAFETY: `load_ctx` is live.
        let host_ctx_ptr = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        let mut out0 = BamSymbol::INVALID;
        let mut out1 = BamSymbol::INVALID;
        let mut out_dup = BamSymbol::INVALID;

        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr`, UTF-8 `StrView`, and out pointers.
            unsafe {
                intern_slot(
                    host_ctx_ptr,
                    StrView::from_str("action::attack"),
                    &raw mut out0,
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                intern_slot(
                    host_ctx_ptr,
                    StrView::from_str("action::defend"),
                    &raw mut out1,
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_intern_symbol(
                    host_ctx_ptr,
                    StrView::from_str("action::attack"),
                    &raw mut out_dup,
                )
            },
            FfiResult::OK
        );

        // Invalid UTF-8 rejected with INVALID_UTF8.
        let bad_utf8: [u8; 2] = [0xFF, 0xFE];
        let mut out_bad = BamSymbol::INVALID;
        assert_eq!(
            // SAFETY: `bad_utf8` is a valid 2-byte slice with non-UTF-8 content.
            unsafe {
                host_intern_symbol(
                    host_ctx_ptr,
                    StrView::from_slice(&bad_utf8),
                    &raw mut out_bad,
                )
            },
            FfiResult::INVALID_UTF8
        );
        assert_eq!(out_bad, BamSymbol::INVALID);

        // Empty string is rejected with INVALID_ARGUMENT per ABI 0.3.0 symbol contract.
        let mut out_empty = BamSymbol::INVALID;
        assert_eq!(
            // SAFETY: Empty `StrView` is valid.
            unsafe { host_intern_symbol(host_ctx_ptr, StrView::empty(), &raw mut out_empty) },
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(out_empty, BamSymbol::INVALID);

        load_ctx.finish_init();
        (out0, out1, out_dup)
    };

    assert_eq!(s_attack, BamSymbol(0));
    assert_eq!(s_defend, BamSymbol(1));
    assert_eq!(s_attack_dup, s_attack);

    let sym_table = world.resource::<BamSymbolTable>();
    assert_eq!(
        sym_table.resolve_symbol_str(s_attack),
        Some("action::attack")
    );
    assert_eq!(
        sym_table.resolve_symbol_str(s_defend),
        Some("action::defend")
    );
    assert_eq!(sym_table.resolve_symbol_str(BamSymbol(999)), None);

    // Reloaded mod session (ModId(202)) resolves the exact same symbol IDs!
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(ModId(202)),
        );
        // SAFETY: `load_ctx` is live.
        let host_ctx_ptr = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        let mut out_again = BamSymbol::INVALID;
        let mut out_new = BamSymbol::INVALID;
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_intern_symbol(
                    host_ctx_ptr,
                    StrView::from_str("action::attack"),
                    &raw mut out_again,
                )
            },
            FfiResult::OK
        );
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_intern_symbol(
                    host_ctx_ptr,
                    StrView::from_str("action::parry"),
                    &raw mut out_new,
                )
            },
            FfiResult::OK
        );
        assert_eq!(out_again, s_attack);
        assert_eq!(out_new, BamSymbol(2));
        load_ctx.finish_init();
    }

    // Multi-threaded stress: 8 threads concurrently intern overlapping & distinct symbols via HostContext.
    let snap = world.resource::<ModRegistry>().snapshot_arc();
    let handles: Vec<_> = (0..8usize)
        .map(|tid| {
            let snap = Arc::clone(&snap);
            std::thread::spawn(move || {
                let ctx = HostContext::new(snap);
                let mut ids = Vec::with_capacity(128);
                for i in 0..64usize {
                    let s = format!("shared::sym_{}", i % 16);
                    let mut out = BamSymbol::INVALID;
                    // SAFETY: `ctx` and `s` are valid for the call.
                    let rc = unsafe {
                        host_intern_symbol(ctx.as_ffi_ptr(), StrView::from_str(&s), &raw mut out)
                    };
                    assert_eq!(rc, FfiResult::OK);
                    ids.push((s, out));

                    let unique = format!("thread::{tid}::{i}");
                    let mut u_out = BamSymbol::INVALID;
                    // SAFETY: `ctx` and `unique` are valid for the call.
                    let rc2 = unsafe {
                        host_intern_symbol(
                            ctx.as_ffi_ptr(),
                            StrView::from_str(&unique),
                            &raw mut u_out,
                        )
                    };
                    assert_eq!(rc2, FfiResult::OK);
                    ids.push((unique, u_out));
                }
                ids
            })
        })
        .collect();

    let sym_table = world.resource::<BamSymbolTable>();
    let mut shared_map = std::collections::HashMap::new();
    for h in handles {
        for (name, sym) in h.join().unwrap() {
            assert_eq!(sym_table.resolve_symbol_str(sym), Some(name.as_str()));
            if name.starts_with("shared::") {
                let prev = shared_map.entry(name).or_insert(sym);
                assert_eq!(
                    *prev, sym,
                    "concurrent dedup must yield identical BamSymbol"
                );
            }
        }
    }
    // 3 initial + 16 shared + 8*64 unique = 531 total distinct symbols.
    assert_eq!(sym_table.len(), 3 + 16 + 8 * 64);
}

#[test]
fn named_objects_persistent_binding_uuid_rebind_and_reload_survival() {
    let mut world = World::new();
    world.insert_resource(ModRegistry::new());
    world.insert_resource(WorldDropGuard::new());
    let _world_id = ensure_symbol_tables_in_world(&mut world);

    let mod_1 = ModId(301);
    let mod_2 = ModId(302);

    // Spawn a ModScoped entity that gets bound as a persistent named object, plus an unbound one.
    let hero_entity = world.spawn(ModScoped(mod_1)).id();
    let temp_entity = world.spawn(ModScoped(mod_1)).id();
    let hero_ffi = FfiEntity::from_bits(hero_entity.to_bits());

    let bind_slot = HOST_API
        .bind_named_object
        .expect("HOST_API.bind_named_object must be wired");

    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(mod_1),
        );
        // SAFETY: `load_ctx` is live.
        let host_ctx_ptr = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        // Validation: NULL entity, empty name, invalid UTF-8.
        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr`.
            unsafe {
                bind_slot(
                    host_ctx_ptr,
                    StrView::from_str("prefab::hero"),
                    FfiEntity::NULL,
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr`.
            unsafe { host_bind_named_object(host_ctx_ptr, StrView::empty(), hero_ffi) },
            FfiResult::INVALID_ARGUMENT
        );
        let bad_utf8: [u8; 2] = [0xC3, 0x28];
        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr` and slice.
            unsafe {
                host_bind_named_object(host_ctx_ptr, StrView::from_slice(&bad_utf8), hero_ffi)
            },
            FfiResult::INVALID_UTF8
        );

        // Bind `prefab::hero` -> `hero_entity`.
        assert_eq!(
            // SAFETY: Valid `host_ctx_ptr` and entity.
            unsafe { bind_slot(host_ctx_ptr, StrView::from_str("prefab::hero"), hero_ffi) },
            FfiResult::OK
        );

        load_ctx.finish_init();
    }

    let expected_uuid = ComponentUuid::from_parts("prefab::hero", 0, 0, 0);
    {
        let named = world.resource::<ModNamedObjects>();
        assert_eq!(named.get("prefab::hero"), Some(hero_entity));
        assert_eq!(named.get_ffi("prefab::hero"), Some(hero_ffi));
        assert_eq!(named.get_by_uuid(expected_uuid), Some(hero_entity));
        assert_eq!(named.get_alive(&world, "prefab::hero"), Some(hero_entity));
        assert_eq!(
            named.get_alive_by_uuid(&world, expected_uuid),
            Some(hero_entity)
        );
        assert_eq!(
            named.get_name_by_uuid(expected_uuid).as_deref(),
            Some("prefab::hero")
        );
    }

    // Simulate mod_1 unload teardown: strip ModScoped from bound objects, then despawn ModScoped(mod_1).
    ModNamedObjects::strip_mod_scoped_in_world(&mut world);
    world.despawn_all_where::<(&ModScoped,), (Allow<Disabled>, Without<IsResource>)>(|(s,)| {
        s.0 == mod_1
    });

    assert!(
        world.entities().contains(hero_entity),
        "bound named object must survive mod unload"
    );
    assert!(
        world.get::<ModScoped>(hero_entity).is_none(),
        "bound named object must be promoted to persistent (!ModScoped)"
    );
    assert!(!world.entities().contains(temp_entity));

    // Reloaded `mod_2` resolves the exact same `hero_entity` and can rebind to a new entity!
    let hero_v2_entity = world.spawn(ModScoped(mod_2)).id();
    let hero_v2_ffi = FfiEntity::from_bits(hero_v2_entity.to_bits());
    {
        let named = world.resource::<ModNamedObjects>();
        assert_eq!(named.get_alive(&world, "prefab::hero"), Some(hero_entity));
        assert_eq!(
            named.get_alive_by_uuid(&world, expected_uuid),
            Some(hero_entity)
        );
    }
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(mod_2),
        );
        // SAFETY: `load_ctx` is live.
        let host_ctx_ptr = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        // Rebind `prefab::hero` -> `hero_v2_entity`!
        assert_eq!(
            // SAFETY: Valid pointers.
            unsafe {
                host_bind_named_object(host_ctx_ptr, StrView::from_str("prefab::hero"), hero_v2_ffi)
            },
            FfiResult::OK
        );
        load_ctx.finish_init();
    }

    assert_eq!(
        world.resource::<ModNamedObjects>().get("prefab::hero"),
        Some(hero_v2_entity)
    );
    // `get_or_spawn_persistent` returns existing live entity without spawning a duplicate,
    // and spawns a fresh persistent entity for an unbound name.
    assert_eq!(
        ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::hero"),
        hero_v2_entity
    );
    let spawned_boss = ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::boss");
    assert!(world.entities().contains(spawned_boss));
    assert!(world.get::<ModScoped>(spawned_boss).is_none());
    assert_eq!(
        world.resource::<ModNamedObjects>().get("prefab::boss"),
        Some(spawned_boss)
    );

    // Despawn `spawned_boss` and verify `get_alive` returns `None` and `prune_despawned` cleans it.
    assert!(world.despawn(spawned_boss));
    assert_eq!(
        world
            .resource::<ModNamedObjects>()
            .get_alive(&world, "prefab::boss"),
        None
    );
    let named_clone = world.resource::<ModNamedObjects>().clone();
    assert_eq!(named_clone.prune_despawned(&world), 1);
    assert!(!named_clone.contains_name("prefab::boss"));
    assert_eq!(named_clone.len(), 1);
}
