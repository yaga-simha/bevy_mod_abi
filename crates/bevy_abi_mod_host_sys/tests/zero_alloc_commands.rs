//! Counting-allocator proof that `GuestSystem` + `CommandArena` + batched `cmd_spawn` perform
//! **zero** heap allocations in steady-state frames.

use bevy_abi_mod_host_sys::{ComponentOptions, GuestSystem, HOST_API, ModRegistry};
use bevy_abi_mod_sys::{
    AbiPod, BamCommands, BamComponent, BamSystemCtx, ComponentUuid, FfiEntity, FfiResult,
    ModComponent, RawInsertable, SymbolEntry, VTableHeader,
};
use bevy_ecs::prelude::{Component, World};
use bevy_ecs::system::System;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::System as StdSystemAlloc;

struct CountingAllocator;

static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Delegates directly to `std::alloc::System` while atomically counting allocations when enabled.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING_ENABLED.load(Ordering::Relaxed) {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: Forwarding `layout` directly to OS system allocator.
        unsafe { StdSystemAlloc.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwarding `(ptr, layout)` directly to OS system allocator.
        unsafe { StdSystemAlloc.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING_ENABLED.load(Ordering::Relaxed) {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        // SAFETY: Forwarding `(ptr, layout, new_size)` directly to OS system allocator.
        unsafe { StdSystemAlloc.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Pos {
    x: f32,
    y: f32,
    z: f32,
}

// SAFETY: `#[repr(C)]` struct of `f32` primitives with universal bit validity.
unsafe impl RawInsertable for Pos {}
// SAFETY: Cross-language POD layout (`3 * f32`, `align = 4`).
unsafe impl AbiPod for Pos {}

impl ModComponent for Pos {
    const CANONICAL_NAME: &'static str = "zero_alloc::Pos";
    const COMPONENT_CSTR: &'static CStr = c"zero_alloc::Pos";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("zero_alloc::Pos");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;

    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader = VTableHeader::new::<VTableHeader, Pos>(Pos::UUID);
        &VT
    }

    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct PaddedPayload {
    tag: u8,
    // 7 bytes of natural C inter-field alignment padding (``MaybeUninit<u8>` copy test).
    value: u64,
}

// SAFETY: `#[repr(C)]` struct of `u8` + `u64` primitives.
unsafe impl RawInsertable for PaddedPayload {}
// SAFETY: Fixed-width `u8` + `u64` C-ABI POD layout.
unsafe impl AbiPod for PaddedPayload {}

impl ModComponent for PaddedPayload {
    const CANONICAL_NAME: &'static str = "zero_alloc::PaddedPayload";
    const COMPONENT_CSTR: &'static CStr = c"zero_alloc::PaddedPayload";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("zero_alloc::PaddedPayload");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;

    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader =
            VTableHeader::new::<VTableHeader, PaddedPayload>(PaddedPayload::UUID);
        &VT
    }

    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

#[repr(C)]
struct SpawnState {
    pos_comp: BamComponent,
    pad_comp: BamComponent,
    last_spawned: FfiEntity,
    tick_counter: u64,
}

unsafe extern "C" fn steady_state_commands_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `SpawnState` owned by the test stack.
    let state = unsafe { &mut *user_data.cast::<SpawnState>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: Calling `HOST_API.sys_commands` on active `sys_ctx`.
    let st = unsafe { (HOST_API.sys_commands.unwrap())(sys_ctx, &mut cmds) };
    if st.is_err() {
        return st;
    }

    // Despawn previous tick's entity so Bevy's archetype table recycles its row without growing!
    if !state.last_spawned.is_null() {
        // SAFETY: Despawning previously spawned entity via active `cmds`.
        let dsp = unsafe { (HOST_API.cmd_despawn.unwrap())(cmds, state.last_spawned) };
        if dsp.is_err() {
            return dsp;
        }
    }

    state.tick_counter += 1;
    let pos = Pos {
        x: state.tick_counter as f32,
        y: 2.0,
        z: 3.0,
    };
    let pad = PaddedPayload {
        tag: (state.tick_counter & 0xFF) as u8,
        value: state.tick_counter * 100,
    };

    let comps = [state.pos_comp, state.pad_comp];
    let ptrs = [
        (&pos as *const Pos).cast::<u8>(),
        (&pad as *const PaddedPayload).cast::<u8>(),
    ];
    let mut spawned = FfiEntity::NULL;
    // SAFETY: Batched `cmd_spawn` of 2 components into `CommandArena`.
    let sp_st = unsafe {
        (HOST_API.cmd_spawn.unwrap())(
            cmds,
            comps.as_ptr(),
            ptrs.as_ptr(),
            comps.len(),
            &mut spawned,
        )
    };
    if sp_st.is_err() {
        return sp_st;
    }

    // Also overwrite `Pos` via `cmd_insert` in the same frame to exercise `RecordedCommand::Insert`.
    let updated_pos = Pos {
        x: pos.x + 0.5,
        y: pos.y,
        z: pos.z,
    };
    // SAFETY: Valid `cmds`, `spawned`, and `Pos` payload.
    let ins_st = unsafe {
        (HOST_API.cmd_insert.unwrap())(
            cmds,
            spawned,
            state.pos_comp,
            (&updated_pos as *const Pos).cast::<u8>(),
            core::mem::size_of::<Pos>(),
        )
    };
    if ins_st.is_err() {
        return ins_st;
    }

    state.last_spawned = spawned;
    FfiResult::OK
}

#[test]
fn command_arena_and_batched_spawn_have_zero_steady_state_allocations() {
    let mut world = World::new();
    let pos_id = world.register_component::<Pos>();
    let pad_id = world.register_component::<PaddedPayload>();

    let mut registry = ModRegistry::new();
    let pos_comp = registry
        .register_component::<Pos>(pos_id, ComponentOptions::new())
        .unwrap();
    let pad_comp = registry
        .register_component::<PaddedPayload>(pad_id, ComponentOptions::new())
        .unwrap();
    world.insert_resource(registry.clone());

    let mut state = SpawnState {
        pos_comp,
        pad_comp,
        last_spawned: FfiEntity::NULL,
        tick_counter: 0,
    };

    let mut sys = GuestSystem::new(
        "steady_state_commands",
        Vec::new(),
        steady_state_commands_cb,
        (&mut state as *mut SpawnState).cast::<c_void>(),
        registry,
    );
    sys.initialize(&mut world);

    // Pre-populate Bevy's two-tier entity free list (`128`-entry `local_free` + `shared.free`)
    // so `world.entity_allocator().alloc()` recycles existing `Entity` slots without growing
    // Bevy's internal `Entities::meta` table during our steady-state `CommandArena` measurement.
    let prewarmed: Vec<_> = (0..256).map(|_| world.spawn_empty().id()).collect();
    for e in prewarmed {
        world.despawn(e);
    }

    // Warmup 4 frames to initialize our 64 KiB `CommandArena` slab, command `Vec` capacities,
    // and Bevy's double-buffered `RemovedComponentMessages` ring (cleared each frame via `clear_trackers`).
    for _ in 0..4 {
        sys.run((), &mut world).unwrap();
        world.clear_trackers();
    }

    ALLOC_CALLS.store(0, Ordering::SeqCst);
    ALLOC_BYTES.store(0, Ordering::SeqCst);
    TRACKING_ENABLED.store(true, Ordering::SeqCst);

    // Run 100 full frames of `cmd_despawn` + batched `cmd_spawn` + `cmd_insert`: 0 allocations!
    for _ in 0..100 {
        let res = sys.run((), &mut world);
        debug_assert!(res.is_ok());
        world.clear_trackers();
    }

    TRACKING_ENABLED.store(false, Ordering::SeqCst);

    let calls = ALLOC_CALLS.load(Ordering::SeqCst);
    let bytes = ALLOC_BYTES.load(Ordering::SeqCst);
    assert_eq!(
        calls, 0,
        "expected 0 steady-state heap allocations across 100 frames, got {calls} calls ({bytes} bytes)"
    );
    assert_eq!(bytes, 0);

    // Verify final entity (`4 + 100 = 104` ticks) holds the exact batched + updated component payloads.
    let entity = bevy_ecs::entity::Entity::from_bits(state.last_spawned.to_bits());
    assert_eq!(
        world.get::<Pos>(entity),
        Some(&Pos {
            x: 104.5,
            y: 2.0,
            z: 3.0
        })
    );
    assert_eq!(
        world.get::<PaddedPayload>(entity),
        Some(&PaddedPayload {
            tag: 104,
            value: 10400
        })
    );
}
