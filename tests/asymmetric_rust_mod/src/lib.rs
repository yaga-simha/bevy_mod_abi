#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use guest_sdk::{
    export_mod, register_runtime_component, wrap_system_callback, CommandBridgeVTable,
    ComponentAccessDesc, ComponentAccessMode, FfiEntity, FfiResult, GetProcAddressFn,
    GuestChunkIter, GuestCommandQueue, HostApi, RawChunkIter, RawCommandQueue, StrView,
    SystemDescriptor,
};

// ============================================================================
// 1. Component VTable Definitions Matching Host Layouts
// ============================================================================

#[repr(C)]
pub struct PositionVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct VelocityVTable {
    pub size: usize,
    pub align: usize,
    pub get_x: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_x: unsafe extern "C" fn(*mut c_void, f32),
    pub get_y: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_y: unsafe extern "C" fn(*mut c_void, f32),
    pub get_z: unsafe extern "C" fn(*const c_void) -> f32,
    pub set_z: unsafe extern "C" fn(*mut c_void, f32),
}

#[repr(C)]
pub struct CombatHealthVTable {
    pub size: usize,
    pub align: usize,
    pub get_current: unsafe extern "C" fn(*const c_void) -> i32,
    pub set_current: unsafe extern "C" fn(*mut c_void, i32),
    pub get_max: unsafe extern "C" fn(*const c_void) -> i32,
    pub set_max: unsafe extern "C" fn(*mut c_void, i32),
}

#[repr(C)]
pub struct MetadataTagVTable {
    pub size: usize,
    pub align: usize,
    pub get_label: unsafe extern "C" fn(*const c_void) -> StrView,
    pub set_label: unsafe extern "C" fn(*mut c_void, *const u8, usize),
}

// ============================================================================
// 1b. Dynamic Runtime Component Definitions (Milestone 3)
// ============================================================================

/// Milestone 3: Dynamic runtime component defined purely by the guest mod.
///
/// This component is not known to the host application at compile time.
/// It is registered dynamically with Bevy ECS during `mod_init` via `register_runtime_component`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

// ============================================================================
// 2. Mod Static State
// ============================================================================

static HOST_API_PTR: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static POS_VTABLE: AtomicPtr<PositionVTable> = AtomicPtr::new(core::ptr::null_mut());
static VEL_VTABLE: AtomicPtr<VelocityVTable> = AtomicPtr::new(core::ptr::null_mut());
static HEALTH_VTABLE: AtomicPtr<CombatHealthVTable> = AtomicPtr::new(core::ptr::null_mut());
static TAG_VTABLE: AtomicPtr<MetadataTagVTable> = AtomicPtr::new(core::ptr::null_mut());

static COMMAND_VTABLE: AtomicPtr<CommandBridgeVTable> = AtomicPtr::new(core::ptr::null_mut());
static mut STATIC_CMD_VTABLE: CommandBridgeVTable = CommandBridgeVTable {
    spawn_empty: None,
    despawn: None,
    insert_component: None,
};

static PANIC_TRIGGER: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_CALLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_INSTANCES: AtomicUsize = AtomicUsize::new(0);

// Test harness controls

/// Triggers a simulated panic during subsequent system executions.
///
/// # Safety
/// This function must be called from a test harness thread when it is safe to toggle
/// the test panic trigger flag. It operates atomically with `SeqCst` ordering.
#[no_mangle]
pub unsafe extern "C" fn asymmetric_rust_mod_trigger_panic() {
    PANIC_TRIGGER.store(true, Ordering::SeqCst);
}

/// Resets the simulated panic trigger so subsequent system executions run normally.
///
/// # Safety
/// This function must be called from a test harness thread when it is safe to reset
/// the test panic trigger flag. It operates atomically with `SeqCst` ordering.
#[no_mangle]
pub unsafe extern "C" fn asymmetric_rust_mod_reset_panic() {
    PANIC_TRIGGER.store(false, Ordering::SeqCst);
}

/// Queries whether the mod has received a shutdown signal via `bevy_abi_mod_shutdown`.
///
/// # Safety
/// Callers must ensure the dynamic library remains loaded in memory during the call.
/// It operates atomically with `SeqCst` ordering.
#[no_mangle]
pub unsafe extern "C" fn asymmetric_rust_mod_was_shutdown() -> bool {
    SHUTDOWN_CALLED.load(Ordering::SeqCst)
}

// ============================================================================
// 3. Scenario Systems
// ============================================================================

// --- Scenario 1: 3D Kinematics System ---
unsafe fn rust_kinematics_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let host_api_ptr = HOST_API_PTR.load(Ordering::Acquire);
    let pos_vt_ptr = POS_VTABLE.load(Ordering::Acquire);
    let vel_vt_ptr = VEL_VTABLE.load(Ordering::Acquire);

    if host_api_ptr.is_null() || pos_vt_ptr.is_null() || vel_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }

    // SAFETY: Valid static pointers resolved during mod_init.
    let api = unsafe { &*host_api_ptr };
    let pos_vt = unsafe { &*pos_vt_ptr };
    let vel_vt = unsafe { &*vel_vt_ptr };

    loop {
        let count = unsafe { (api.iter_next_chunk)(iter) };
        if count == 0 {
            break;
        }

        let pos_base = unsafe { (api.iter_column_mutable)(iter, 0) };
        let vel_base = unsafe { (api.iter_column_readonly)(iter, 1) };
        let pos_stride = unsafe { (api.iter_column_stride)(iter, 0) };
        let vel_stride = unsafe { (api.iter_column_stride)(iter, 1) };

        if pos_base.is_null() || vel_base.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for i in 0..count {
            let p_ptr = unsafe { pos_base.add(i * pos_stride) } as *mut c_void;
            let v_ptr = unsafe { vel_base.add(i * vel_stride) } as *const c_void;

            let px = unsafe { (pos_vt.get_x)(p_ptr) };
            let vx = unsafe { (vel_vt.get_x)(v_ptr) };
            unsafe { (pos_vt.set_x)(p_ptr, px + vx) };

            let py = unsafe { (pos_vt.get_y)(p_ptr) };
            let vy = unsafe { (vel_vt.get_y)(v_ptr) };
            unsafe { (pos_vt.set_y)(p_ptr, py + vy) };

            let pz = unsafe { (pos_vt.get_z)(p_ptr) };
            let vz = unsafe { (vel_vt.get_z)(v_ptr) };
            unsafe { (pos_vt.set_z)(p_ptr, pz + vz) };

            // Call iter_mark_changed explicitly
            let mark_res = unsafe { (api.iter_mark_changed)(iter, 0, i) };
            if mark_res.is_err() {
                return mark_res;
            }
        }
    }

    FfiResult::OK
}

// --- Scenario 2: Combat Damage System ---
unsafe fn rust_combat_system(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let host_api_ptr = HOST_API_PTR.load(Ordering::Acquire);
    let health_vt_ptr = HEALTH_VTABLE.load(Ordering::Acquire);
    let cmd_vt_ptr = COMMAND_VTABLE.load(Ordering::Acquire);

    if host_api_ptr.is_null() || health_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*host_api_ptr };
    let health_vt = unsafe { &*health_vt_ptr };
    let cmd_vt = if !cmd_vt_ptr.is_null() {
        unsafe { *cmd_vt_ptr }
    } else {
        CommandBridgeVTable::default()
    };
    let mut cmd_queue = GuestCommandQueue::new(commands, cmd_vt);

    loop {
        let count = unsafe { (api.iter_next_chunk)(iter) };
        if count == 0 {
            break;
        }

        let health_base = unsafe { (api.iter_column_mutable)(iter, 0) };
        let health_stride = unsafe { (api.iter_column_stride)(iter, 0) };
        let ents_ptr = unsafe { (api.iter_entities)(iter) };

        if health_base.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for i in 0..count {
            let h_ptr = unsafe { health_base.add(i * health_stride) } as *mut c_void;
            let current = unsafe { (health_vt.get_current)(h_ptr) };
            let new_hp = (current - 15).max(0);
            unsafe { (health_vt.set_current)(h_ptr, new_hp) };

            // Trigger change detection for CombatHealth
            unsafe { (api.iter_mark_changed)(iter, 0, i) };

            // If health reaches 0, trigger deferred despawn command
            if new_hp == 0 && !ents_ptr.is_null() && !commands.is_null() {
                let entity = unsafe { *ents_ptr.add(i) };
                cmd_queue.despawn(entity);
            }
        }
    }

    FfiResult::OK
}

// --- Scenario 3: Dynamic Metadata System ---
unsafe fn rust_metadata_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let host_api_ptr = HOST_API_PTR.load(Ordering::Acquire);
    let tag_vt_ptr = TAG_VTABLE.load(Ordering::Acquire);

    if host_api_ptr.is_null() || tag_vt_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*host_api_ptr };
    let tag_vt = unsafe { &*tag_vt_ptr };

    loop {
        let count = unsafe { (api.iter_next_chunk)(iter) };
        if count == 0 {
            break;
        }

        let tag_base = unsafe { (api.iter_column_mutable)(iter, 0) };
        let tag_stride = unsafe { (api.iter_column_stride)(iter, 0) };

        if tag_base.is_null() {
            return FfiResult::NULL_POINTER;
        }

        for i in 0..count {
            let t_ptr = unsafe { tag_base.add(i * tag_stride) } as *mut c_void;
            let view = unsafe { (tag_vt.get_label)(t_ptr) };

            // Safe UTF-8 inspection across C-ABI
            if let Ok(label) = unsafe { view.as_str() } {
                if !label.starts_with("Modded_") {
                    let mut updated = String::with_capacity(label.len() + 8);
                    updated.push_str("Modded_");
                    updated.push_str(label);

                    // Safe host-allocated mutation
                    unsafe {
                        (tag_vt.set_label)(t_ptr, updated.as_ptr(), updated.len());
                    }
                    unsafe { (api.iter_mark_changed)(iter, 0, i) };
                }
            }
        }
    }

    FfiResult::OK
}

// --- Scenario 4: Deferred Commands System ---
unsafe fn rust_command_system(
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if commands.is_null() {
        return FfiResult::OK;
    }
    let cmd_vt_ptr = COMMAND_VTABLE.load(Ordering::Acquire);
    if cmd_vt_ptr.is_null() {
        return FfiResult::OK;
    }

    let cmd_vt = unsafe { *cmd_vt_ptr };
    let mut cmd_queue = GuestCommandQueue::new(commands, cmd_vt);

    // 1. Spawn a new entity
    let new_entity = cmd_queue.spawn_empty();
    if new_entity.0 == 0 || new_entity == FfiEntity::PLACEHOLDER {
        return FfiResult::INTERNAL_ERROR;
    }

    // 2. Insert Position component bytes
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawPos {
        x: f32,
        y: f32,
        z: f32,
    }
    let p = RawPos {
        x: 100.0,
        y: 200.0,
        z: 300.0,
    };
    let insert_res = unsafe {
        cmd_queue.insert_bytes(
            new_entity,
            c"Position".as_ptr(),
            &p as *const RawPos as *const u8,
            core::mem::size_of::<RawPos>(),
        )
    };
    if insert_res.is_err() {
        return insert_res;
    }

    // Insert dynamic runtime component via typed helper (Milestone 3)
    let custom_data = RustModCustomData {
        power: 42.0,
        shield: 100,
    };
    let insert_custom_res = cmd_queue.insert_runtime_component(
        new_entity,
        c"RustModCustomData",
        &custom_data,
    );
    if insert_custom_res.is_err() {
        return insert_custom_res;
    }

    let despawn_res = cmd_queue.despawn(new_entity);
    if despawn_res.is_err() {
        return despawn_res;
    }

    FfiResult::OK
}

// --- Scenario 5: Fault Isolation Panicking System ---
unsafe fn rust_panicking_system(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if PANIC_TRIGGER.load(Ordering::SeqCst) {
        panic!("Intentional cross-runtime panic in asymmetric_rust_mod");
    }
    FfiResult::OK
}

// --- Scenario 7: Dynamic Runtime Component Mutation System (Milestone 3) ---
unsafe fn rust_custom_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let host_api_ptr = HOST_API_PTR.load(Ordering::Acquire);
    if host_api_ptr.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }

    let api = unsafe { &*host_api_ptr };
    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        {
            let mut slice = chunk_iter.mutable_slice(0, count);
            let Some(items) = slice.as_typed_slice::<RustModCustomData>() else {
                return FfiResult::INTERNAL_ERROR;
            };

            for item in items.iter_mut() {
                item.power += 10.0;
                item.shield += 1;
            }
        }

        for i in 0..count {
            let mark_res = chunk_iter.mark_changed(0, i);
            if mark_res.is_err() {
                return mark_res;
            }
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_rust_kinematics calls rust_kinematics_system);
wrap_system_callback!(fn c_rust_combat calls rust_combat_system);
wrap_system_callback!(fn c_rust_metadata calls rust_metadata_system);
wrap_system_callback!(fn c_rust_command calls rust_command_system);
wrap_system_callback!(fn c_rust_panicking calls rust_panicking_system);
wrap_system_callback!(fn c_rust_custom_system calls rust_custom_system);

// ============================================================================
// 4. Mod Lifecycle (Scenario 6)
// ============================================================================

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    PANIC_TRIGGER.store(false, Ordering::SeqCst);
    ACTIVE_INSTANCES.fetch_add(1, Ordering::SeqCst);

    if host_api.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API_PTR.store(host_api as *mut HostApi, Ordering::Release);

    // Register Dynamic Component (Milestone 3)
    let reg_res = unsafe {
        register_runtime_component::<RustModCustomData>(
            host_api,
            host_context,
            c"RustModCustomData",
        )
    };
    if reg_res.is_err() {
        return reg_res;
    }

    // Resolve VTables with fallback naming support
    let pos_sym = unsafe { (get_proc_address)(c"Position_vtable".as_ptr()) };
    let vel_sym = unsafe { (get_proc_address)(c"Velocity_vtable".as_ptr()) };
    if pos_sym.is_null() || vel_sym.is_null() {
        return FfiResult::INVALID_COMPONENT;
    }
    POS_VTABLE.store(pos_sym as *mut PositionVTable, Ordering::Release);
    VEL_VTABLE.store(vel_sym as *mut VelocityVTable, Ordering::Release);

    let mut health_sym =
        unsafe { (get_proc_address)(c"CombatHealth_vtable".as_ptr()) };
    if health_sym.is_null() {
        health_sym = unsafe { (get_proc_address)(c"Health_vtable".as_ptr()) };
    }
    if !health_sym.is_null() {
        HEALTH_VTABLE.store(health_sym as *mut CombatHealthVTable, Ordering::Release);
    }

    let mut tag_sym =
        unsafe { (get_proc_address)(c"MetadataTag_vtable".as_ptr()) };
    if tag_sym.is_null() {
        tag_sym = unsafe { (get_proc_address)(c"NameTag_vtable".as_ptr()) };
    }
    if !tag_sym.is_null() {
        TAG_VTABLE.store(tag_sym as *mut MetadataTagVTable, Ordering::Release);
    }

    // Resolve deferred command functions
    let resolved = CommandBridgeVTable::resolve(get_proc_address);
    unsafe {
        STATIC_CMD_VTABLE = resolved;
        COMMAND_VTABLE.store(core::ptr::addr_of_mut!(STATIC_CMD_VTABLE), Ordering::Release);
    }

    let api = unsafe { &*host_api };

    // Register System 1: Kinematics
    let kin_queries = [
        ComponentAccessDesc {
            name: c"Position".as_ptr(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDesc {
            name: c"Velocity".as_ptr(),
            mode: ComponentAccessMode::READ,
        },
    ];
    let kin_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"rust_kinematics".as_ptr(),
        queries: kin_queries.as_ptr(),
        query_count: kin_queries.len(),
        callback: c_rust_kinematics,
        user_data: core::ptr::null_mut(),
    };
    let status = unsafe { (api.register_system)(host_context, &kin_desc) };
    if status.is_err() {
        return status;
    }

    // Register System 2: Combat Damage (if health component resolved)
    if !health_sym.is_null() {
        let health_name = if unsafe {
            !(get_proc_address)(c"CombatHealth_vtable".as_ptr()).is_null()
        } {
            c"CombatHealth".as_ptr()
        } else {
            c"Health".as_ptr()
        };

        let combat_queries = [ComponentAccessDesc {
            name: health_name,
            mode: ComponentAccessMode::WRITE,
        }];
        let combat_desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: c"rust_combat".as_ptr(),
            queries: combat_queries.as_ptr(),
            query_count: combat_queries.len(),
            callback: c_rust_combat,
            user_data: core::ptr::null_mut(),
        };
        let _ = unsafe { (api.register_system)(host_context, &combat_desc) };
    }

    // Register System 3: Metadata (if tag component resolved)
    if !tag_sym.is_null() {
        let tag_name = if unsafe {
            !(get_proc_address)(c"MetadataTag_vtable".as_ptr()).is_null()
        } {
            c"MetadataTag".as_ptr()
        } else {
            c"NameTag".as_ptr()
        };

        let tag_queries = [ComponentAccessDesc {
            name: tag_name,
            mode: ComponentAccessMode::WRITE,
        }];
        let tag_desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: c"rust_metadata".as_ptr(),
            queries: tag_queries.as_ptr(),
            query_count: tag_queries.len(),
            callback: c_rust_metadata,
            user_data: core::ptr::null_mut(),
        };
        let _ = unsafe { (api.register_system)(host_context, &tag_desc) };
    }

    // Register System 4: Command Spawner (if command symbols resolved)
    let spawner_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"rust_command_spawner".as_ptr(),
        queries: core::ptr::null(),
        query_count: 0,
        callback: c_rust_command,
        user_data: core::ptr::null_mut(),
    };
    let _ = unsafe { (api.register_system)(host_context, &spawner_desc) };

    // Register System 6: Dynamic Custom Data Mutation (Milestone 3)
    let custom_queries = [ComponentAccessDesc {
        name: c"RustModCustomData".as_ptr(),
        mode: ComponentAccessMode::WRITE,
    }];
    let custom_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"rust_custom_system".as_ptr(),
        queries: custom_queries.as_ptr(),
        query_count: custom_queries.len(),
        callback: c_rust_custom_system,
        user_data: core::ptr::null_mut(),
    };
    let custom_status = unsafe { (api.register_system)(host_context, &custom_desc) };
    if custom_status.is_err() {
        return custom_status;
    }

    // Register System 5: Panicking
    let panic_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"rust_panicking".as_ptr(),
        queries: core::ptr::null(),
        query_count: 0,
        callback: c_rust_panicking,
        user_data: core::ptr::null_mut(),
    };
    unsafe { (api.register_system)(host_context, &panic_desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    SHUTDOWN_CALLED.store(true, Ordering::SeqCst);
    PANIC_TRIGGER.store(false, Ordering::SeqCst);

    let prev = ACTIVE_INSTANCES
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
            if val > 0 {
                Some(val - 1)
            } else {
                Some(0)
            }
        })
        .unwrap_or(0);

    if prev == 1 {
        COMMAND_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
        POS_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
        VEL_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
        HEALTH_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
        TAG_VTABLE.store(core::ptr::null_mut(), Ordering::Release);
        HOST_API_PTR.store(core::ptr::null_mut(), Ordering::Release);
    }

    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
