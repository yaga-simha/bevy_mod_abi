#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::schedule::{LogLevel, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{System, SystemAccess};
use bevy_ecs::world::World;
use common::*;
use host_runtime::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_mark_changed, host_iter_next_chunk, AbiModPlugin, ComponentAccessDescOwned,
    DynamicCSystem, ModAppExt, ModRegistry,
};
use libloading::{Library, Symbol};
use mod_derive::ModExposable;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ============================================================================
// Alignment Verification Components
// ============================================================================

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C, align(1))]
pub struct AlignByte {
    pub val: u8,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C, align(4))]
pub struct AlignWord {
    pub val: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C, align(8))]
pub struct AlignDWord {
    pub val: u64,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C, align(16))]
pub struct AlignSimd {
    pub val: [f32; 4],
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C, align(64))]
pub struct AlignCacheLine {
    pub val: [u64; 8],
}

static ALIGNMENT_VERIFIED_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn alignment_verifier_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut core::ffi::c_void,
) -> FfiResult {
    let alignments = [1, 4, 8, 16, 64];

    loop {
        // SAFETY: C-ABI iterator call within dynamic system execution
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        for (col_idx, &align) in alignments.iter().enumerate() {
            // SAFETY: Valid column indices 0..5
            let ptr = unsafe { host_iter_column_readonly(iter, col_idx) };
            let stride = unsafe { host_iter_column_stride(iter, col_idx) };

            assert!(!ptr.is_null(), "Column {col_idx} pointer must not be null");
            assert_eq!(
                (ptr as usize) % align,
                0,
                "Base column pointer must satisfy {align}-byte alignment"
            );
            assert_eq!(
                stride % align,
                0,
                "Stride {stride} must be multiple of alignment {align}"
            );

            for i in 0..count {
                let elem_ptr = unsafe { ptr.add(i * stride) };
                assert_eq!(
                    (elem_ptr as usize) % align,
                    0,
                    "Element {i} in column {col_idx} must satisfy {align}-byte alignment"
                );
            }
        }
        ALIGNMENT_VERIFIED_COUNT.fetch_add(count, Ordering::SeqCst);
    }
    FfiResult::OK
}

/// Test 1: Alignment verification across 1, 4, 8, 16, 64 bytes.
#[test]
fn test_heterogeneous_memory_alignment() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<AlignByte>();
    app.register_mod_component::<AlignWord>();
    app.register_mod_component::<AlignDWord>();
    app.register_mod_component::<AlignSimd>();
    app.register_mod_component::<AlignCacheLine>();

    const COUNT: usize = 128;
    for i in 0..COUNT {
        app.world_mut().spawn((
            AlignByte {
                val: (i % 255) as u8,
            },
            AlignWord { val: i as u32 },
            AlignDWord {
                val: 0xDEADBEEFCAFE0000 + i as u64,
            },
            AlignSimd {
                val: [1.0, 2.0, 3.0, i as f32],
            },
            AlignCacheLine { val: [i as u64; 8] },
        ));
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        ComponentAccessDescOwned {
            name: "AlignByte".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "AlignWord".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "AlignDWord".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "AlignSimd".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "AlignCacheLine".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    ALIGNMENT_VERIFIED_COUNT.store(0, Ordering::SeqCst);
    let sys = DynamicCSystem::new(
        "alignment_verifier".to_string(),
        query_descs,
        alignment_verifier_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(bevy_app::Update, sys);
    app.update();

    assert_eq!(
        ALIGNMENT_VERIFIED_COUNT.load(Ordering::SeqCst),
        COUNT,
        "All 128 entities must have their 1B, 4B, 8B, 16B, and 64B alignments verified"
    );
}

static BOUNDS_CALLBACK_RUNS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn bounds_checker_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut core::ffi::c_void,
) -> FfiResult {
    // Next chunk on empty world returns 0 immediately
    unsafe {
        let c1 = host_iter_next_chunk(iter);
        assert_eq!(c1, 0, "Empty table must yield 0 entities");

        // Repeated next chunk calls past exhaustion must return 0 idempotently
        let c2 = host_iter_next_chunk(iter);
        assert_eq!(c2, 0, "Exhausted iterator must return 0");
        let c3 = host_iter_next_chunk(iter);
        assert_eq!(c3, 0, "Exhausted iterator must return 0");

        // Requesting columns on exhausted chunk returns null
        let col = host_iter_column_readonly(iter, 0);
        assert!(col.is_null(), "Exhausted column pointer must be null");

        let col_mut = host_iter_column_mutable(iter, 0);
        assert!(
            col_mut.is_null(),
            "Exhausted mutable column pointer must be null"
        );

        // Invalid row index mark changed returns error without panic
        let err = host_iter_mark_changed(iter, 0, 999);
        assert_eq!(err, FfiResult::INTERNAL_ERROR);
    }

    BOUNDS_CALLBACK_RUNS.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

/// Test 2: Bounds checking on empty tables (0 entities) and iterator exhaustion.
#[test]
fn test_chunk_iterator_bounds_and_empty_tables() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();

    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
    let query_descs = vec![ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];

    BOUNDS_CALLBACK_RUNS.store(0, Ordering::SeqCst);
    let sys = DynamicCSystem::new(
        "bounds_checker".to_string(),
        query_descs.clone(),
        bounds_checker_callback,
        core::ptr::null_mut(),
        Arc::clone(&registry),
    );

    app.add_systems(bevy_app::Update, sys);

    // 1. Completely empty world (0 entities)
    app.update();
    assert_eq!(BOUNDS_CALLBACK_RUNS.load(Ordering::SeqCst), 1);

    // 2. Spawn and immediately despawn -> creates an empty table with 0 entities in storages
    let e = app
        .world_mut()
        .spawn(Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        })
        .id();
    app.world_mut().despawn(e);

    app.update();
    assert_eq!(BOUNDS_CALLBACK_RUNS.load(Ordering::SeqCst), 2);
}

/// Test 3: FFI transparent enum and newtype safety.
#[test]
fn test_ffi_transparent_type_safety() {
    let valid_ok = FfiResult::OK;
    assert!(valid_ok.is_ok());
    assert!(!valid_ok.is_err());

    let panic_res = FfiResult::PANIC_CAUGHT;
    assert!(!panic_res.is_ok());
    assert!(panic_res.is_err());

    // Foreign arbitrary u32 values
    let foreign_code = FfiResult(0xCAFEBABE);
    assert!(foreign_code.is_err());
    assert!(!foreign_code.is_ok());

    let foreign_zero = FfiResult(0);
    assert!(foreign_zero.is_ok());

    // ComponentAccessMode transparent u32
    let read_mode = ComponentAccessMode::READ;
    assert_eq!(read_mode.0, 0);
    let write_mode = ComponentAccessMode::WRITE;
    assert_eq!(write_mode.0, 1);
    let filter_mode = ComponentAccessMode::FILTER_ONLY;
    assert_eq!(filter_mode.0, 2);

    // FfiEntity bits transparency
    let ent = FfiEntity(0x0000000100000005);
    assert_eq!(ent.0, 0x0000000100000005);
    assert_eq!(FfiEntity::PLACEHOLDER.0, u64::MAX);
}

/// Test 4: Bevy ECS Borrow conflict detection across guest systems.
#[test]
fn test_borrow_conflict_detection_across_guest_systems() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);
    let reg_arc = Arc::new(registry);

    unsafe extern "C" fn sys_cb(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut core::ffi::c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    let query_write = vec![ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let query_read = vec![ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::READ,
    }];

    let mut sys_w1 = DynamicCSystem::new(
        "sys_w1".to_string(),
        query_write.clone(),
        sys_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_w2 = DynamicCSystem::new(
        "sys_w2".to_string(),
        query_write,
        sys_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let mut sys_r1 = DynamicCSystem::new(
        "sys_r1".to_string(),
        query_read,
        sys_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    let access_w1 = sys_w1.initialize(&mut world);
    let access_w2 = sys_w2.initialize(&mut world);
    let access_r1 = sys_r1.initialize(&mut world);

    // FilteredAccessSet level conflict detection
    if let (SystemAccess::Shared(s_w1), SystemAccess::Shared(s_w2)) = (&access_w1, &access_w2) {
        assert!(
            !s_w1.is_compatible(s_w2),
            "Two systems with WRITE access on Position must NOT be compatible"
        );
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    if let (SystemAccess::Shared(s_w1), SystemAccess::Shared(s_r1)) = (&access_w1, &access_r1) {
        assert!(
            !s_w1.is_compatible(s_r1),
            "WRITE and READ access on Position must NOT be compatible"
        );
    } else {
        panic!("Expected SystemAccess::Shared");
    }

    // Bevy Schedule Ambiguity Detection check
    let mut schedule = Schedule::default();
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((sys_w1, sys_w2));
    let init_res = schedule.initialize(&mut world);
    assert!(
        init_res.is_err(),
        "Bevy scheduler must detect unconstrained WRITE + WRITE borrow conflict as ambiguity error"
    );
}

/// Test 5: Memory allocator boundary isolation.
/// Ensures host-allocated strings in ECS components are managed exclusively by host allocator,
/// and guest strings passed through VTable mutators are safely copied.
#[test]
fn test_memory_allocator_boundary_isolation() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<MetadataTag>();

    const ENTITIES: usize = 1_000;
    let mut entities = Vec::with_capacity(ENTITIES);

    for i in 0..ENTITIES {
        let e = app
            .world_mut()
            .spawn(MetadataTag {
                label: format!("initial_host_string_index_{i}"),
            })
            .id();
        entities.push(e);
    }

    // Inspect VTable setter
    let registry = app.world().resource::<ModRegistry>();
    let tag_meta = registry.get_by_name("MetadataTag").expect("MetadataTag");
    let tag_vt = unsafe { &*(tag_meta.vtable_ptr as *const MetadataTagVTable) };

    // Mutate across boundary using guest-provided buffer
    for (i, &e) in entities.iter().enumerate() {
        let tag_mut = app.world_mut().get_mut::<MetadataTag>(e).unwrap();
        let ptr = tag_mut.as_ref() as *const MetadataTag as *mut core::ffi::c_void;

        let guest_msg = format!("guest_payload_{i}");
        unsafe {
            (tag_vt.set_label)(ptr, guest_msg.as_ptr(), guest_msg.len());
        }
    }

    // Verify all host components hold updated strings
    for (i, &e) in entities.iter().enumerate() {
        let tag = app.world().get::<MetadataTag>(e).unwrap();
        assert_eq!(tag.label, format!("guest_payload_{i}"));
    }

    // Despawn all entities on host -> host drops strings with host allocator
    for e in entities {
        app.world_mut().despawn(e);
    }

    // Clean world tick to ensure zero memory corruption
    app.update();
}

/// Test 6: Cross-Runtime Panic Containment.
/// Confirms that a panicking guest system returns PANIC_CAUGHT, disables the system,
/// and leaves the host Bevy application running without crash or process termination.
#[test]
fn test_cross_runtime_panic_containment() {
    let (rust_dylib, _c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    for i in 0..10 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ));
    }

    let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");

    // Tick healthy (pos.y should increase by 2.0 * 3 = 6.0)
    for _ in 0..3 {
        app.update();
    }

    let mut query = app.world_mut().query::<&Position>();
    for pos in query.iter(app.world()) {
        assert_eq!(pos.y, 6.0);
    }

    // Trigger guest panic in asymmetric_rust_mod
    unsafe {
        let lib = Library::new(&rust_dylib).expect("open rust dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("resolve trigger");
        trigger();
    }

    // Ticking must NOT panic or abort host process. Offending system is isolated and disabled.
    app.update();
    app.update();

    // Verify host application remains active and healthy systems continue
    let mut query2 = app.world_mut().query::<&Position>();
    for pos in query2.iter(app.world()) {
        // Kinematics was not the panicking system (rust_panicking was), so kinematics continues!
        assert_eq!(pos.y, 10.0);
    }

    app.unload_mod(r_id).unwrap();
    reset_rust_mod_panic(&rust_dylib);
}
