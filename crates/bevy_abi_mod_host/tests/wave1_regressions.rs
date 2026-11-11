//! Wave 1 regression tests for `bevy_abi_mod_host` and `bevy_abi_mod_host_sys`
//! (`S-04`, `S-05`, `S-06`, `S-07`, `S-10`, `L-01`, `L-05a/b/c`, and `B-04`).

#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment
)]

use bevy_abi_mod_host::{
    AbiModPlugin, ModAppExt, ModErrorPolicy, ModHost, ModLoadError, ModNamedSystemSet, ModRegistry,
    ModSystemFaulted, StartupCompleted, SystemInitError,
};
use bevy_abi_mod_host_sys::{
    CommandQueueBridge, ComponentAccessDescOwned, CurrentRegistryGuard, DynamicCSystem, HOST_API,
    MAX_QUERY_COMPONENTS, MAX_SYSTEM_QUERIES, MOD_INIT_CONTEXT_MAGIC, ModInitContext,
    ModInitContextHeader, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_register_component, host_register_conditional_multi_query_system,
    host_register_conditional_system, host_register_multi_query_system,
    host_register_scheduled_system, host_register_system, host_register_system_in_schedule,
};
use bevy_abi_mod_macros::ModExposable;
use bevy_abi_mod_sys::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, ConditionalMultiQuerySystemDescriptor,
    ConditionalSystemDescriptor, FFI_BOOL_TRUE, FfiBool, FfiEntity, FfiResult,
    ModComponentDescriptor, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    ScheduleLabelId, ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor,
    SystemDescriptor,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{ScheduleLabel, Schedules};
use bevy_ecs::system::{System, SystemAccess};
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, ModExposable, Copy, Clone, Debug, PartialEq, Eq)]
struct PaddedPair {
    tag: u8,
    // 7 padding bytes at offsets 1..8
    value: u64,
}

#[repr(C)]
#[derive(Resource, ModExposable, Copy, Clone, Debug, Default, PartialEq, Eq)]
struct CounterRes {
    startup_hits: u32,
    update_hits: u32,
}

#[repr(C)]
#[derive(Resource, ModExposable, Copy, Clone, Debug, Default, PartialEq, Eq)]
struct OrderTraceRes {
    steps: [u32; 4],
    len: u32,
}

fn c_dylib_filename(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

fn compile_temp_c_mod(stem: &str, c_source: &str) -> (PathBuf, PathBuf) {
    static UNIQUE_COUNTER: AtomicU32 = AtomicU32::new(1);
    let n = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bevy_abi_mod_wave1_{}_{}_{}",
        stem,
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir for C fixture");

    let c_path = dir.join(format!("{stem}.c"));
    std::fs::write(&c_path, c_source).expect("write C fixture source");

    let out_path = dir.join(c_dylib_filename(stem));
    let repo_include = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("include");

    let status = Command::new("cc")
        .arg("-std=c99")
        .arg("-O2")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-I")
        .arg(&repo_include)
        .arg(&c_path)
        .arg("-o")
        .arg(&out_path)
        .status()
        .expect("invoke `cc` to build temporary C mod");
    assert!(status.success(), "`cc` failed for temporary C mod `{stem}`");

    (dir, out_path)
}

unsafe extern "C" fn noop_single_cb(
    _iter: *mut RawChunkIter,
    _cmds: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_multi_cb(
    _iters: *const *mut RawChunkIter,
    _count: usize,
    _cmds: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn always_true_cond(_user_data: *mut c_void) -> FfiBool {
    FFI_BOOL_TRUE
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
// S-05: Clear guest VTables & symbols on unload without global symbol leakage
// ============================================================================

#[test]
fn regression_s05_stale_vtable_after_unload() {
    let c_src = r#"
#include "bevy_abi_mod.h"

static uint64_t g_dummy_vtable = 0x1122334455667788ULL;
static void guest_drop_hook(void* ptr) { (void)ptr; }

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)get_proc_address;
    ModComponentDescriptor desc;
    desc.struct_size = sizeof(ModComponentDescriptor);
    desc.name = "S05GuestComp";
    desc.uuid = bevy_abi_uuid_from_name("S05GuestComp");
    desc.size = 8;
    desc.align = 8;
    desc.drop_fn = guest_drop_hook;
    desc.vtable_ptr = (const void*)&g_dummy_vtable;
    return api->register_component(ctx, &desc);
}
"#;

    let (temp_dir, dylib_path) = compile_temp_c_mod("s05_vtable_mod", c_src);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app
        .try_load_mod(&dylib_path)
        .expect("load s05_vtable_mod succeeds");

    // Register an extra mod-owned symbol to verify symbol eviction as well.
    let sentinel_word: u64 = 0xABCD;
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_mod_symbol(
                "s05_custom_fn",
                (&sentinel_word as *const u64).cast::<c_void>(),
                mod_id,
            );
    }

    let comp_id_before = {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .get_by_name("S05GuestComp")
            .expect("S05GuestComp registered");
        assert!(!comp.vtable_ptr.is_null());
        assert!(comp.drop_fn.is_some());
        assert_eq!(comp.owner, Some(mod_id));
        assert!(reg.resolve_symbol("S05GuestComp_vtable").is_some());
        assert!(reg.resolve_symbol("s05_custom_fn").is_some());

        // Without an active `CURRENT_REGISTRY` guard, `host_get_proc_address` queries `GLOBAL_SYMBOLS`,
        // which MUST NOT contain guest-owned VTable pointers!
        let leaked_global = unsafe { host_get_proc_address(c"S05GuestComp_vtable".as_ptr()) };
        assert!(
            leaked_global.is_null(),
            "Guest VTable pointer must never be inserted into process-global GLOBAL_SYMBOLS"
        );

        // With `CURRENT_REGISTRY` active, resolution succeeds while loaded.
        let reg_arc = Arc::new(reg.clone());
        let _guard: CurrentRegistryGuard = reg_arc.make_current();
        let active_sym = unsafe { host_get_proc_address(c"S05GuestComp_vtable".as_ptr()) };
        assert!(!active_sym.is_null());

        comp.component_id
    };

    // Unload the mod (`dlclose`).
    app.try_unload_mod(mod_id)
        .expect("unload s05_vtable_mod succeeds");

    {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .get_by_name("S05GuestComp")
            .expect("ComponentId binding preserved across unload for hot-reload");
        assert_eq!(comp.component_id, comp_id_before);
        assert!(
            comp.vtable_ptr.is_null(),
            "vtable_ptr must be nulled on unload"
        );
        assert!(comp.drop_fn.is_none(), "drop_fn must be cleared on unload");
        assert!(comp.owner.is_none(), "owner must be cleared on unload");
        assert!(
            reg.resolve_symbol("S05GuestComp_vtable").is_none(),
            "guest VTable symbol must be evicted on unload"
        );
        assert!(
            reg.resolve_symbol("s05_custom_fn").is_none(),
            "mod-owned custom symbol must be evicted on unload"
        );
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

// ============================================================================
// S-06: Failed init / strict validation rolls back before dlclose
// ============================================================================

#[test]
fn regression_s06_init_failure_rolls_back() {
    let c_fail_init = r#"
#include "bevy_abi_mod.h"

static uint64_t g_bad_vtable = 0x99887766ULL;
static void bad_drop_hook(void* ptr) { (void)ptr; }

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)get_proc_address;
    ModComponentDescriptor desc;
    desc.struct_size = sizeof(ModComponentDescriptor);
    desc.name = "S06RollbackComp";
    desc.uuid = bevy_abi_uuid_from_name("S06RollbackComp");
    desc.size = 8;
    desc.align = 8;
    desc.drop_fn = bad_drop_hook;
    desc.vtable_ptr = (const void*)&g_bad_vtable;
    FfiResult r = api->register_component(ctx, &desc);
    if (r != FFI_RESULT_OK) { return r; }
    return FFI_RESULT_INVALID_ARGUMENT;
}
"#;

    let (dir1, dylib_fail) = compile_temp_c_mod("s06_fail_init", c_fail_init);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let err = app
        .try_load_mod(&dylib_fail)
        .expect_err("init returning INVALID_ARGUMENT must fail");
    assert!(matches!(
        err,
        ModLoadError::InitFailed {
            status: FfiResult::INVALID_ARGUMENT,
            ..
        }
    ));

    let reg = app.world().resource::<ModRegistry>();
    assert!(
        reg.get_by_name("S06RollbackComp").is_none(),
        "LoadTransaction must roll back newly registered dynamic component before dlclose"
    );
    assert!(reg.resolve_symbol("S06RollbackComp_vtable").is_none());
    assert!(app.world().resource::<ModHost>().is_empty());

    // Also verify `try_load_mod_strict` rolls back registered dynamic components and drop hooks
    // when a system references an unregistered component.
    let c_strict_fail = r#"
#include "bevy_abi_mod.h"

static uint64_t g_strict_vt = 0x55443322ULL;
static void strict_drop(void* ptr) { (void)ptr; }
static FfiResult noop_sys(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)it; (void)q; (void)ud;
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)get_proc_address;
    ModComponentDescriptor cdesc;
    cdesc.struct_size = sizeof(ModComponentDescriptor);
    cdesc.name = "S06StrictTempComp";
    cdesc.uuid = bevy_abi_uuid_from_name("S06StrictTempComp");
    cdesc.size = 8;
    cdesc.align = 8;
    cdesc.drop_fn = strict_drop;
    cdesc.vtable_ptr = (const void*)&g_strict_vt;
    FfiResult r = api->register_component(ctx, &cdesc);
    if (r != FFI_RESULT_OK) { return r; }

    ComponentAccessDesc term = bam_access_desc("TotallyMissingHostComp", COMPONENT_ACCESS_READ);
    SystemDescriptor sdesc;
    sdesc.struct_size = sizeof(SystemDescriptor);
    sdesc.name = "strict_bad_sys";
    sdesc.queries = &term;
    sdesc.query_count = 1;
    sdesc.callback = noop_sys;
    sdesc.user_data = NULL;
    return api->register_system(ctx, &sdesc);
}
"#;

    let (dir2, dylib_strict) = compile_temp_c_mod("s06_strict_fail", c_strict_fail);
    let strict_err = app
        .try_load_mod_strict(&dylib_strict)
        .expect_err("strict load must reject missing component");
    assert!(matches!(strict_err, ModLoadError::MissingComponents { .. }));

    let reg_after = app.world().resource::<ModRegistry>();
    assert!(reg_after.get_by_name("S06StrictTempComp").is_none());
    assert!(
        reg_after
            .resolve_symbol("S06StrictTempComp_vtable")
            .is_none()
    );
    assert!(app.world().resource::<ModHost>().is_empty());

    let _ = std::fs::remove_dir_all(dir1);
    let _ = std::fs::remove_dir_all(dir2);
}

// ============================================================================
// S-07: ModInitContext magic + generation + active-pointer validation
// ============================================================================

#[test]
fn regression_s07_register_outside_init_rejected() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));

    let comp_desc =
        ModComponentDescriptor::new(c"S07Dyn".as_ptr(), ComponentUuid::from_name("S07Dyn"), 4, 4);
    let sys_desc = SystemDescriptor::new(
        c"s07_sys".as_ptr(),
        core::ptr::null(),
        0,
        noop_single_cb,
        core::ptr::null_mut(),
    );
    let sched_desc = ScheduledSystemDescriptor::from_base(sys_desc, ScheduleLabelId::UPDATE);
    let cond_desc = ConditionalSystemDescriptor::from_scheduled(
        sched_desc,
        Some(always_true_cond),
        core::ptr::null_mut(),
    );
    let specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"s07_mq".as_ptr(),
        specs.as_ptr(),
        1,
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let cond_mq_desc = ConditionalMultiQuerySystemDescriptor::from_scheduled(
        ScheduledMultiQuerySystemDescriptor::from_base(mq_desc, ScheduleLabelId::UPDATE),
        Some(always_true_cond),
        core::ptr::null_mut(),
    );

    let assert_all_entry_points_return = |ctx_ptr: *mut c_void, expected: FfiResult| unsafe {
        assert_eq!((HOST_API.register_component)(ctx_ptr, &comp_desc), expected);
        assert_eq!((HOST_API.register_system)(ctx_ptr, &sys_desc), expected);
        assert_eq!(host_register_component(ctx_ptr, &comp_desc), expected);
        assert_eq!(host_register_system(ctx_ptr, &sys_desc), expected);
        assert_eq!(
            host_register_system_in_schedule(ctx_ptr, &sys_desc, ScheduleLabelId::UPDATE.0),
            expected
        );
        assert_eq!(
            host_register_scheduled_system(ctx_ptr, &sched_desc),
            expected
        );
        assert_eq!(
            host_register_conditional_system(ctx_ptr, &cond_desc),
            expected
        );
        assert_eq!(
            host_register_multi_query_system(ctx_ptr, &mq_desc),
            expected
        );
        assert_eq!(
            host_register_conditional_multi_query_system(ctx_ptr, &cond_mq_desc),
            expected
        );
    };

    // 1. Null pointer -> NULL_POINTER
    assert_all_entry_points_return(core::ptr::null_mut(), FfiResult::NULL_POINTER);

    // 2. Unactivated ModInitContext -> INVALID_STATE
    let mut unactivated =
        ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
    let unactivated_ptr = (&mut unactivated as *mut ModInitContext).cast::<c_void>();
    assert_all_entry_points_return(unactivated_ptr, FfiResult::INVALID_STATE);

    // 3. Stale pointer stashed across init and used after guard drops -> INVALID_STATE
    {
        let mut ctx = ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
        let ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
        {
            let _guard = ctx.make_active();
            assert_eq!(
                unsafe { host_register_system(ptr, &sys_desc) },
                FfiResult::OK
            );
        }
        assert_all_entry_points_return(ptr, FfiResult::INVALID_STATE);
    }

    // 4. Forged header / different context pointer while another init is active -> INVALID_STATE
    let mut real_ctx =
        ModInitContext::with_world(Arc::clone(&reg), Arc::clone(&active), &mut world);
    let _active_guard = real_ctx.make_active();

    #[repr(C)]
    struct ForgedHeader {
        magic: u64,
        generation: u32,
        _pad: u32,
    }
    assert_eq!(
        core::mem::size_of::<ForgedHeader>(),
        core::mem::size_of::<ModInitContextHeader>()
    );
    let mut forged = ForgedHeader {
        magic: MOD_INIT_CONTEXT_MAGIC,
        generation: 999_999,
        _pad: 0,
    };
    let forged_ptr = (&mut forged as *mut ForgedHeader).cast::<c_void>();
    assert_all_entry_points_return(forged_ptr, FfiResult::INVALID_STATE);
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

// ============================================================================
// L-01: Runtime-loaded Startup systems execute immediately once
// ============================================================================

#[test]
fn regression_l01_runtime_loaded_startup_runs_once() {
    let c_src = r#"
#include "bevy_abi_mod.h"

typedef struct CounterRes {
    uint32_t startup_hits;
    uint32_t update_hits;
} CounterRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult on_startup(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    CounterRes* c = (CounterRes*)g_res_mut(it, "CounterRes");
    if (c != NULL) { c->startup_hits += 1; }
    return FFI_RESULT_OK;
}

static FfiResult on_update(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    CounterRes* c = (CounterRes*)g_res_mut(it, "CounterRes");
    if (c != NULL) { c->update_hits += 1; }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)api;
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    RegisterScheduledSystemFn reg_sched =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");
    if (g_res_mut == NULL || reg_sched == NULL) { return FFI_RESULT_NOT_FOUND; }

    ComponentAccessDesc term = bam_access_desc("CounterRes", COMPONENT_ACCESS_RESOURCE_WRITE);

    ScheduledSystemDescriptor d1 = {0};
    d1.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d1.base.name = "late_startup_sys";
    d1.base.queries = &term;
    d1.base.query_count = 1;
    d1.base.callback = on_startup;
    d1.schedule = SCHEDULE_LABEL_STARTUP;
    FfiResult r1 = reg_sched(ctx, &d1);
    if (r1 != FFI_RESULT_OK) { return r1; }

    ScheduledSystemDescriptor d2 = {0};
    d2.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d2.base.name = "regular_update_sys";
    d2.base.queries = &term;
    d2.base.query_count = 1;
    d2.base.callback = on_update;
    d2.schedule = SCHEDULE_LABEL_UPDATE;
    return reg_sched(ctx, &d2);
}
"#;

    let (temp_dir, dylib_path) = compile_temp_c_mod("l01_late_startup", c_src);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<CounterRes>();
    app.insert_resource(CounterRes::default());

    // Advance the app once so `Startup` and `PostStartup` have already executed.
    app.update();
    assert_eq!(
        app.world().resource::<StartupCompleted>(),
        &StartupCompleted(true)
    );
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 0,
            update_hits: 0,
        }
    );

    // Load the mod at runtime after `Startup` has completed.
    let mod_id = app
        .try_load_mod(&dylib_path)
        .expect("runtime load after Startup succeeds");

    // `late_startup_sys` ran immediately once during `try_load_mod`.
    assert_eq!(
        *app.world().resource::<CounterRes>(),
        CounterRes {
            startup_hits: 1,
            update_hits: 0,
        }
    );

    // Subsequent updates run `regular_update_sys` each tick, never re-running `late_startup_sys`.
    for expected_updates in 1..=3 {
        app.update();
        assert_eq!(
            *app.world().resource::<CounterRes>(),
            CounterRes {
                startup_hits: 1,
                update_hits: expected_updates,
            }
        );
    }

    app.try_unload_mod(mod_id).expect("unload succeeds");
    let _ = std::fs::remove_dir_all(temp_dir);
}

// ============================================================================
// L-05a: Cross-mod system name isolation
// ============================================================================

#[test]
fn regression_l05a_cross_mod_name_isolation() {
    fn mod_with_step_and_finish(tag_step: u32, tag_finish: u32) -> String {
        format!(
            r#"
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {{
    uint32_t steps[4];
    uint32_t len;
}} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult sys_step(RawChunkIter* it, RawCommandQueue* q, void* ud) {{
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) {{ r->steps[r->len++] = {tag_step}u; }}
    return FFI_RESULT_OK;
}}

static FfiResult sys_finish(RawChunkIter* it, RawCommandQueue* q, void* ud) {{
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) {{ r->steps[r->len++] = {tag_finish}u; }}
    return FFI_RESULT_OK;
}}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {{
    return BEVY_ABI_VERSION_PACKED;
}}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {{
    (void)api;
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    RegisterScheduledSystemFn reg =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");

    ComponentAccessDesc term = bam_access_desc("OrderTraceRes", COMPONENT_ACCESS_RESOURCE_WRITE);

    ScheduledSystemDescriptor d_step = {{0}};
    d_step.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d_step.base.name = "step";
    d_step.base.queries = &term;
    d_step.base.query_count = 1;
    d_step.base.callback = sys_step;
    d_step.schedule = SCHEDULE_LABEL_UPDATE;
    if (reg(ctx, &d_step) != FFI_RESULT_OK) {{ return FFI_RESULT_INTERNAL_ERROR; }}

    const char* after_dep = "step";
    ScheduledSystemDescriptor d_fin = {{0}};
    d_fin.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d_fin.base.name = "finish";
    d_fin.base.queries = &term;
    d_fin.base.query_count = 1;
    d_fin.base.callback = sys_finish;
    d_fin.schedule = SCHEDULE_LABEL_UPDATE;
    d_fin.after = &after_dep;
    d_fin.after_count = 1;
    return reg(ctx, &d_fin);
}}
"#
        )
    }

    let (dir_a, path_a) = compile_temp_c_mod("mod_alpha", &mod_with_step_and_finish(10, 11));
    let (dir_b, path_b) = compile_temp_c_mod("mod_beta", &mod_with_step_and_finish(20, 21));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<OrderTraceRes>();
    app.insert_resource(OrderTraceRes::default());

    let id_a = app.try_load_mod(&path_a).expect("load mod_alpha");
    let id_b = app.try_load_mod(&path_b).expect("load mod_beta");

    let set_a_step = ModNamedSystemSet::new("mod_alpha", "step");
    let set_b_step = ModNamedSystemSet::new("mod_beta", "step");
    {
        let schedules = app.world().resource::<Schedules>();
        let update_sched = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set_a_step.is_present_in(update_sched));
        assert!(set_b_step.is_present_in(update_sched));
    }

    app.update();
    let trace = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace.len, 4);
    let pos_10 = trace.steps.iter().position(|&x| x == 10).unwrap();
    let pos_11 = trace.steps.iter().position(|&x| x == 11).unwrap();
    let pos_20 = trace.steps.iter().position(|&x| x == 20).unwrap();
    let pos_21 = trace.steps.iter().position(|&x| x == 21).unwrap();
    assert!(
        pos_10 < pos_11,
        "mod_alpha step must precede mod_alpha finish"
    );
    assert!(
        pos_20 < pos_21,
        "mod_beta step must precede mod_beta finish"
    );

    // Unloading `mod_beta` must NOT remove `mod_alpha`'s `"step"` set!
    app.try_unload_mod(id_b).expect("unload mod_beta");
    {
        let schedules = app.world().resource::<Schedules>();
        let update_sched = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set_a_step.is_present_in(update_sched));
        assert!(!set_b_step.is_present_in(update_sched));
    }

    app.insert_resource(OrderTraceRes::default());
    app.update();
    let trace_after = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace_after.len, 2);
    assert_eq!(&trace_after.steps[..2], &[10, 11]);

    app.try_unload_mod(id_a).expect("unload mod_alpha");
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}

// ============================================================================
// L-05b: Later mod ordering `after("earlier_mod::produce")` works when earlier
//        mod registered zero ordering constraints of its own
// ============================================================================

#[test]
fn regression_l05b_later_mod_after_earlier_system() {
    // `mod_producer` uses plain `api->register_system` with NO `after`/`before` constraints!
    let producer_src = r#"
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult produce_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 1u; }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    ComponentAccessDesc term = bam_access_desc("OrderTraceRes", COMPONENT_ACCESS_RESOURCE_WRITE);

    SystemDescriptor desc;
    desc.struct_size = sizeof(SystemDescriptor);
    desc.name = "produce";
    desc.queries = &term;
    desc.query_count = 1;
    desc.callback = produce_cb;
    desc.user_data = NULL;
    return api->register_system(ctx, &desc);
}
"#;

    // `mod_consumer` declares `after: ["mod_producer::produce"]`.
    let consumer_src = r#"
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

static IterResourceMutableFn g_res_mut = NULL;

static FfiResult consume_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)q; (void)ud;
    OrderTraceRes* r = (OrderTraceRes*)g_res_mut(it, "OrderTraceRes");
    if (r != NULL && r->len < 4) { r->steps[r->len++] = 2u; }
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)api;
    g_res_mut = (IterResourceMutableFn)get_proc_address("iter_resource_mutable");
    RegisterScheduledSystemFn reg =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");

    ComponentAccessDesc term = bam_access_desc("OrderTraceRes", COMPONENT_ACCESS_RESOURCE_WRITE);
    const char* after_dep = "mod_producer::produce";

    ScheduledSystemDescriptor desc = {0};
    desc.base.struct_size = sizeof(ScheduledSystemDescriptor);
    desc.base.name = "consume";
    desc.base.queries = &term;
    desc.base.query_count = 1;
    desc.base.callback = consume_cb;
    desc.schedule = SCHEDULE_LABEL_UPDATE;
    desc.after = &after_dep;
    desc.after_count = 1;
    return reg(ctx, &desc);
}
"#;

    let (dir_p, path_p) = compile_temp_c_mod("mod_producer", producer_src);
    let (dir_c, path_c) = compile_temp_c_mod("mod_consumer", consumer_src);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_resource::<OrderTraceRes>();
    app.insert_resource(OrderTraceRes::default());

    let id_p = app.try_load_mod(&path_p).expect("load mod_producer");
    let id_c = app.try_load_mod(&path_c).expect("load mod_consumer");

    for _ in 0..5 {
        app.insert_resource(OrderTraceRes::default());
        app.update();
        let trace = *app.world().resource::<OrderTraceRes>();
        assert_eq!(trace.len, 2);
        assert_eq!(
            &trace.steps[..2],
            &[1, 2],
            "mod_producer::produce must always run before mod_consumer::consume"
        );
    }

    app.try_unload_mod(id_c).expect("unload mod_consumer");
    app.try_unload_mod(id_p).expect("unload mod_producer");
    let _ = std::fs::remove_dir_all(dir_p);
    let _ = std::fs::remove_dir_all(dir_c);
}

// ============================================================================
// L-05c: Named sets are removed from Bevy's ScheduleGraph on unload
// ============================================================================

#[test]
fn regression_l05c_named_sets_removed_on_unload() {
    let c_src = r#"
#include "bevy_abi_mod.h"

static FfiResult noop_cb(RawChunkIter* it, RawCommandQueue* q, void* ud) {
    (void)it; (void)q; (void)ud;
    return FFI_RESULT_OK;
}

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* api,
    void* ctx
) {
    (void)api;
    RegisterScheduledSystemFn reg =
        (RegisterScheduledSystemFn)get_proc_address("register_scheduled_system");

    ScheduledSystemDescriptor d1 = {0};
    d1.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d1.base.name = "stage_one";
    d1.base.callback = noop_cb;
    d1.schedule = SCHEDULE_LABEL_UPDATE;
    if (reg(ctx, &d1) != FFI_RESULT_OK) { return FFI_RESULT_INTERNAL_ERROR; }

    const char* dep = "stage_one";
    ScheduledSystemDescriptor d2 = {0};
    d2.base.struct_size = sizeof(ScheduledSystemDescriptor);
    d2.base.name = "stage_two";
    d2.base.callback = noop_cb;
    d2.schedule = SCHEDULE_LABEL_UPDATE;
    d2.after = &dep;
    d2.after_count = 1;
    return reg(ctx, &d2);
}
"#;

    let (temp_dir, dylib_path) = compile_temp_c_mod("l05c_clean_mod", c_src);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app.try_load_mod(&dylib_path).expect("load l05c_clean_mod");
    app.update();

    let set1 = ModNamedSystemSet::new("l05c_clean_mod", "stage_one");
    let set2 = ModNamedSystemSet::new("l05c_clean_mod", "stage_two");
    {
        let schedules = app.world().resource::<Schedules>();
        let update = schedules.get(Update.intern()).expect("Update schedule");
        assert!(set1.is_present_in(update));
        assert!(set2.is_present_in(update));
    }

    app.try_unload_mod(mod_id).expect("unload l05c_clean_mod");

    {
        let schedules = app.world().resource::<Schedules>();
        let update = schedules.get(Update.intern()).expect("Update schedule");
        assert!(
            !set1.is_present_in(update),
            "stage_one ModNamedSystemSet must be removed on unload"
        );
        assert!(
            !set2.is_present_in(update),
            "stage_two ModNamedSystemSet must be removed on unload"
        );
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

// ============================================================================
// B-04: Explicit SystemInitError variants (synchronous + ModSystemFaulted)
// ============================================================================

#[test]
fn regression_b04_too_many_terms() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    let c_names: Vec<CString> = (0..=MAX_QUERY_COMPONENTS)
        .map(|i| CString::new(format!("C{i}")).unwrap())
        .collect();
    let raw_terms: Vec<ComponentAccessDesc> = c_names
        .iter()
        .map(|s| ComponentAccessDesc::read(s.as_ptr()))
        .collect();
    let desc = SystemDescriptor::new(
        c"too_many_sys".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );

    // 1. Synchronous rejection at registration time -> OUT_OF_RANGE
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::OUT_OF_RANGE);

    // 2. Also verify `MAX_SYSTEM_QUERIES + 1` in multi-query -> OUT_OF_RANGE
    let empty_specs = [QuerySpec {
        queries: core::ptr::null(),
        query_count: 0,
    }; MAX_SYSTEM_QUERIES + 1];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"too_many_mq".as_ptr(),
        empty_specs.as_ptr(),
        empty_specs.len(),
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe {
        host_register_multi_query_system((&mut ctx as *mut ModInitContext).cast(), &mq_desc)
    };
    assert_eq!(rc_mq, FfiResult::OUT_OF_RANGE);

    // 3. Direct DynamicCSystem construction records `SystemInitError::TooManyTerms`
    let owned_terms: Vec<ComponentAccessDescOwned> = (0..=MAX_QUERY_COMPONENTS)
        .map(|i| ComponentAccessDescOwned {
            name: format!("C{i}"),
            mode: ComponentAccessMode::READ,
        })
        .collect();
    let mut sys = DynamicCSystem::new(
        "too_many_direct".into(),
        owned_terms,
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(sys.init_error(), Some(&SystemInitError::TooManyTerms));
}

#[test]
fn regression_b04_invalid_mode() {
    let mut world = World::new();
    let reg = Arc::new(ModRegistry::new());
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    let raw_terms = [
        ComponentAccessDesc::read(c"PaddedPair".as_ptr()),
        ComponentAccessDesc {
            name: c"CounterRes".as_ptr(),
            mode: ComponentAccessMode(99),
        },
    ];
    let desc = SystemDescriptor::new(
        c"bad_mode_sys".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );

    // Synchronous rejection at registration time -> INVALID_ARGUMENT
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::INVALID_ARGUMENT);

    // Direct DynamicCSystem construction records `InvalidMode { index: 1 }`
    let mut sys = DynamicCSystem::new(
        "bad_mode_direct".into(),
        vec![
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "CounterRes".into(),
                mode: ComponentAccessMode(99),
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(
        sys.init_error(),
        Some(&SystemInitError::InvalidMode { index: 1 })
    );
}

#[test]
fn regression_b04_unknown_component() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let sys = DynamicCSystem::new(
        "unknown_comp_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "GhostComponent".into(),
            mode: ComponentAccessMode::READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    let mut cursor = msgs.get_cursor();
    let events: Vec<&ModSystemFaulted> = cursor.read(msgs).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].system_name, "unknown_comp_sys");
    assert_eq!(events[0].status, FfiResult::INVALID_COMPONENT);
    assert_eq!(
        events[0].init_error,
        Some(SystemInitError::UnknownComponent("GhostComponent".into()))
    );
}

#[test]
fn regression_b04_kind_mismatch() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<PaddedPair>();
    app.register_mod_resource::<CounterRes>();

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());

    // 1. Entity Component queried with `RESOURCE_READ` -> KindMismatch("PaddedPair")
    let sys_comp_as_res = DynamicCSystem::new(
        "comp_as_res_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "PaddedPair".into(),
            mode: ComponentAccessMode::RESOURCE_READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg),
    );

    // 2. Singleton Resource queried with entity `READ` -> KindMismatch("CounterRes")
    let sys_res_as_comp = DynamicCSystem::new(
        "res_as_comp_sys".into(),
        vec![ComponentAccessDescOwned {
            name: "CounterRes".into(),
            mode: ComponentAccessMode::READ,
        }],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );

    app.add_systems(Update, (sys_comp_as_res, sys_res_as_comp));
    app.update();

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    let mut cursor = msgs.get_cursor();
    let errors: Vec<Option<SystemInitError>> =
        cursor.read(msgs).map(|e| e.init_error.clone()).collect();
    assert!(errors.contains(&Some(SystemInitError::KindMismatch("PaddedPair".into()))));
    assert!(errors.contains(&Some(SystemInitError::KindMismatch("CounterRes".into()))));
}

#[test]
fn regression_b04_self_conflict() {
    let mut world = World::new();
    let comp_id = world.register_component::<PaddedPair>();
    let mut registry = ModRegistry::new();
    registry.register_component::<PaddedPair>(comp_id);
    let reg = Arc::new(registry);
    let active = Arc::new(AtomicBool::new(true));
    let mut ctx = ModInitContext::with_world(Arc::clone(&reg), active, &mut world);
    let _guard = ctx.make_active();

    // 1. Intra-query WRITE + READ on same component -> synchronous ACCESS_CONFLICT
    let raw_terms = [
        ComponentAccessDesc::write(c"PaddedPair".as_ptr()),
        ComponentAccessDesc::read(c"PaddedPair".as_ptr()),
    ];
    let desc = SystemDescriptor::new(
        c"self_conflict_single".as_ptr(),
        raw_terms.as_ptr(),
        raw_terms.len(),
        noop_single_cb,
        core::ptr::null_mut(),
    );
    let rc = unsafe { host_register_system((&mut ctx as *mut ModInitContext).cast(), &desc) };
    assert_eq!(rc, FfiResult::ACCESS_CONFLICT);

    // 2. Cross-group RESOURCE_WRITE + RESOURCE_READ in multi-query -> synchronous ACCESS_CONFLICT
    let q0 = [ComponentAccessDesc::resource_write(c"CounterRes".as_ptr())];
    let q1 = [ComponentAccessDesc::resource_read(c"CounterRes".as_ptr())];
    let specs = [
        QuerySpec {
            queries: q0.as_ptr(),
            query_count: 1,
        },
        QuerySpec {
            queries: q1.as_ptr(),
            query_count: 1,
        },
    ];
    let mq_desc = MultiQuerySystemDescriptor::new(
        c"self_conflict_mq".as_ptr(),
        specs.as_ptr(),
        specs.len(),
        noop_multi_cb,
        core::ptr::null_mut(),
    );
    let rc_mq = unsafe {
        host_register_multi_query_system((&mut ctx as *mut ModInitContext).cast(), &mq_desc)
    };
    assert_eq!(rc_mq, FfiResult::ACCESS_CONFLICT);

    // 3. Direct DynamicCSystem construction records `SystemInitError::SelfConflict`
    let mut sys = DynamicCSystem::new(
        "self_conflict_direct".into(),
        vec![
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "PaddedPair".into(),
                mode: ComponentAccessMode::READ,
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        reg,
    );
    assert!(matches!(sys.initialize(&mut world), SystemAccess::None));
    assert_eq!(
        sys.init_error(),
        Some(&SystemInitError::SelfConflict {
            a: "PaddedPair".into(),
            b: "PaddedPair".into(),
        })
    );
}
