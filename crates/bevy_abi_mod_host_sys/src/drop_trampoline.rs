//! Lock-free `ArcSwapOption` per-`World` drop trampolines with slot reclamation (ADR 0001).
//!
//! Implements the exact ADR 0001 slot lifecycle:
//! - Hot path (`drop_trampoline::<SLOT>`): wait-free `ArcSwapOption::load()` with zero `Mutex`
//!   acquisition and zero leaked `RetiredSnapshotPtr` allocations.
//! - Per-`World` RAII ownership (`WorldDropGuard` resource): tracks every slot allocated for a
//!   `World` and transitions each slot `SLOT_IN_USE -> SLOT_CLOSING` when the `World` drops.
//! - Deferred reclamation (`live_values: AtomicUsize`): because Bevy stores `Resource`s inside
//!   component `Table`s whose drop order relative to dynamic component `Table`s is arbitrary during
//!   `World::drop`, a slot is returned to the global free list only once *both* `WorldDropGuard`
//!   has closed the slot (`SLOT_CLOSING`) *and* `live_values` reaches `0`.

use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use arc_swap::ArcSwapOption;
use bevy_abi_mod_sys::{ComponentDropFn, ComponentHookFn, ComponentUuid, FfiEntity};
use bevy_ecs::lifecycle::{ComponentHook, HookContext};
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::DeferredWorld;
use bevy_platform::collections::HashMap;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Maximum number of distinct `(World, ComponentUuid)` foreign drop trampolines active simultaneously.
pub const MAX_DROP_TRAMPOLINES: usize = 1024;

/// Logical slot state: unassigned and present in `FREE_LIST`.
pub const SLOT_FREE: u8 = 0;
/// Logical slot state: owned by a live `WorldDropGuard`.
pub const SLOT_IN_USE: u8 = 1;
/// Logical slot state: `WorldDropGuard` has dropped; awaiting `live_values == 0` before returning to `FREE_LIST`.
pub const SLOT_CLOSING: u8 = 2;

/// High bit set on a slot's packed lifecycle word while in [`SLOT_CLOSING`].
pub const SLOT_CLOSING_BIT: usize = 1usize << (usize::BITS - 1);
/// Second-highest bit set on a slot's packed lifecycle word while in [`SLOT_IN_USE`].
pub const SLOT_IN_USE_BIT: usize = 1usize << (usize::BITS - 2);
/// Mask extracting `live_values` from a slot's packed lifecycle word.
pub const SLOT_LIVE_COUNT_MASK: usize = !(SLOT_CLOSING_BIT | SLOT_IN_USE_BIT);

/// Decodes a packed slot lifecycle word into `(SLOT_*, live_values)`.
#[inline]
pub const fn decode_slot_lifecycle(raw: usize) -> (u8, usize) {
    let state = if (raw & SLOT_CLOSING_BIT) != 0 {
        SLOT_CLOSING
    } else if (raw & SLOT_IN_USE_BIT) != 0 {
        SLOT_IN_USE
    } else {
        SLOT_FREE
    };
    (state, raw & SLOT_LIVE_COUNT_MASK)
}

type DropGuardPair = (GuestFnPtr<ComponentDropFn>, Arc<AtomicBool>);

struct DropSlotSnapshot {
    entries: Box<[DropGuardPair]>,
}

static DROP_SLOTS: [ArcSwapOption<DropSlotSnapshot>; MAX_DROP_TRAMPOLINES] =
    [const { ArcSwapOption::const_empty() }; MAX_DROP_TRAMPOLINES];

/// Single-word packed `(state_bits | live_values)` atomic per slot, placing `WorldDropGuard::drop`
/// (`fetch_xor`) and `drop_trampoline` (`fetch_sub`) in one C11 modification order without `SeqCst`
/// Store-Load reordering hazards.
static SLOT_LIFECYCLE: [AtomicUsize; MAX_DROP_TRAMPOLINES] =
    [const { AtomicUsize::new(0) }; MAX_DROP_TRAMPOLINES];

struct DropControlPlane {
    free_list: Vec<usize>,
    entries: Vec<Vec<DropGuardPair>>,
}

static DROP_CONTROL: LazyLock<Mutex<DropControlPlane>> = LazyLock::new(|| {
    Mutex::new(DropControlPlane {
        free_list: (0..MAX_DROP_TRAMPOLINES).rev().collect(),
        entries: (0..MAX_DROP_TRAMPOLINES).map(|_| Vec::new()).collect(),
    })
});

#[inline]
fn reclaim_closed_slot(slot: usize) {
    // Only the single winner transitioning `(SLOT_CLOSING_BIT | 0) -> 0` pushes `slot` onto
    // `free_list`, preventing double-reclamation under all C11 thread interleavings.
    if SLOT_LIFECYCLE[slot]
        .compare_exchange(SLOT_CLOSING_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        DROP_SLOTS[slot].store(None);
        let mut ctrl = DROP_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ctrl.entries[slot].clear();
        ctrl.free_list.push(slot);
    }
}

/// Staged drop-trampoline mutation recorded during `bevy_abi_mod_init` so [`crate::loader::transaction::LoadTransaction`]
/// or [`crate::rollback_unscheduled_mod_in_world`] can atomically undo uncommitted bindings (`ADR 0001` §2.4).
#[derive(Clone)]
pub(crate) struct StagedDropBinding {
    uuid: ComponentUuid,
    slot: usize,
    fresh_slot_allocated: bool,
    previous_snapshot: Option<Arc<DropSlotSnapshot>>,
    is_active: Arc<AtomicBool>,
}

/// Invokes the most recent active foreign destructor bound to `slot` on an uncommitted arena
/// payload (`CommandArena`) **without** decrementing `SLOT_LIFECYCLE` (`on_insert` has not fired).
///
/// # Safety
/// `raw_ptr` must point to a valid initialized guest component payload matching `slot`'s layout.
#[inline]
pub(crate) unsafe fn run_uncommitted_slot_drop(slot: usize, raw_ptr: *mut u8) {
    if slot >= MAX_DROP_TRAMPOLINES || raw_ptr.is_null() {
        return;
    }
    if let Some(snap) = DROP_SLOTS[slot].load().as_ref()
        && let Some(&(GuestFnPtr(drop_fn), _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
    {
        // SAFETY: `raw_ptr` holds an uncommitted guest payload whose owning mod (`guard == true`)
        // remains mapped in memory.
        unsafe { drop_fn(raw_ptr.cast::<c_void>()) };
    }
}

/// Increments the live-instance count for `slot` when a value of a drop-bearing dynamic component
/// is inserted into Bevy ECS storage.
#[inline]
pub(crate) fn increment_drop_slot_live_values(slot: usize) {
    SLOT_LIFECYCLE[slot].fetch_add(1, Ordering::AcqRel);
}

/// Invokes the most recent active foreign drop callback bound to `SLOT`, if any, and participates
/// in ADR 0001 deferred slot reclamation when `WorldDropGuard` has already closed `SLOT`.
///
/// # Safety
/// `ptr` must point to a valid component allocation owned by Bevy's storage drop hook.
unsafe fn drop_trampoline<const SLOT: usize>(ptr: OwningPtr<'_>) {
    if let Some(snap) = DROP_SLOTS[SLOT].load().as_ref()
        && let Some(&(GuestFnPtr(drop_fn), _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
    {
        let raw = ptr.as_ptr().cast::<c_void>();
        // SAFETY: Foreign drop function is invoked on a valid component instance while its
        // paired mod load session (`guard == true`) remains mapped in memory.
        unsafe { drop_fn(raw) };
    }

    let prev = SLOT_LIFECYCLE[SLOT].fetch_sub(1, Ordering::AcqRel);
    if prev == (SLOT_CLOSING_BIT | 1) {
        reclaim_closed_slot(SLOT);
    }
}

macro_rules! tramp_16 {
    ($b:expr) => {
        [
            drop_trampoline::<{ $b }>,
            drop_trampoline::<{ $b + 1 }>,
            drop_trampoline::<{ $b + 2 }>,
            drop_trampoline::<{ $b + 3 }>,
            drop_trampoline::<{ $b + 4 }>,
            drop_trampoline::<{ $b + 5 }>,
            drop_trampoline::<{ $b + 6 }>,
            drop_trampoline::<{ $b + 7 }>,
            drop_trampoline::<{ $b + 8 }>,
            drop_trampoline::<{ $b + 9 }>,
            drop_trampoline::<{ $b + 10 }>,
            drop_trampoline::<{ $b + 11 }>,
            drop_trampoline::<{ $b + 12 }>,
            drop_trampoline::<{ $b + 13 }>,
            drop_trampoline::<{ $b + 14 }>,
            drop_trampoline::<{ $b + 15 }>,
        ]
    };
}

macro_rules! tramp_256 {
    ($b:expr) => {
        [
            tramp_16!($b),
            tramp_16!($b + 16),
            tramp_16!($b + 32),
            tramp_16!($b + 48),
            tramp_16!($b + 64),
            tramp_16!($b + 80),
            tramp_16!($b + 96),
            tramp_16!($b + 112),
            tramp_16!($b + 128),
            tramp_16!($b + 144),
            tramp_16!($b + 160),
            tramp_16!($b + 176),
            tramp_16!($b + 192),
            tramp_16!($b + 208),
            tramp_16!($b + 224),
            tramp_16!($b + 240),
        ]
    };
}

pub(crate) type DropTrampolineFn = for<'a> unsafe fn(OwningPtr<'a>);
type DropTrampolineBlock = [[DropTrampolineFn; 16]; 16];

static DROP_TRAMPOLINE_BLOCKS: [DropTrampolineBlock; 4] = [
    tramp_256!(0),
    tramp_256!(256),
    tramp_256!(512),
    tramp_256!(768),
];

#[inline]
fn drop_trampoline_fn_for_slot(slot: usize) -> DropTrampolineFn {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    DROP_TRAMPOLINE_BLOCKS[b256][b16][idx]
}

fn on_insert_trampoline<const SLOT: usize>(_world: DeferredWorld<'_>, _ctx: HookContext) {
    increment_drop_slot_live_values(SLOT);
}

macro_rules! insert_hook_16 {
    ($b:expr) => {
        [
            on_insert_trampoline::<{ $b }>,
            on_insert_trampoline::<{ $b + 1 }>,
            on_insert_trampoline::<{ $b + 2 }>,
            on_insert_trampoline::<{ $b + 3 }>,
            on_insert_trampoline::<{ $b + 4 }>,
            on_insert_trampoline::<{ $b + 5 }>,
            on_insert_trampoline::<{ $b + 6 }>,
            on_insert_trampoline::<{ $b + 7 }>,
            on_insert_trampoline::<{ $b + 8 }>,
            on_insert_trampoline::<{ $b + 9 }>,
            on_insert_trampoline::<{ $b + 10 }>,
            on_insert_trampoline::<{ $b + 11 }>,
            on_insert_trampoline::<{ $b + 12 }>,
            on_insert_trampoline::<{ $b + 13 }>,
            on_insert_trampoline::<{ $b + 14 }>,
            on_insert_trampoline::<{ $b + 15 }>,
        ]
    };
}

macro_rules! insert_hook_256 {
    ($b:expr) => {
        [
            insert_hook_16!($b),
            insert_hook_16!($b + 16),
            insert_hook_16!($b + 32),
            insert_hook_16!($b + 48),
            insert_hook_16!($b + 64),
            insert_hook_16!($b + 80),
            insert_hook_16!($b + 96),
            insert_hook_16!($b + 112),
            insert_hook_16!($b + 128),
            insert_hook_16!($b + 144),
            insert_hook_16!($b + 160),
            insert_hook_16!($b + 176),
            insert_hook_16!($b + 192),
            insert_hook_16!($b + 208),
            insert_hook_16!($b + 224),
            insert_hook_16!($b + 240),
        ]
    };
}

type InsertHookBlock = [[ComponentHook; 16]; 16];

static INSERT_HOOK_BLOCKS: [InsertHookBlock; 4] = [
    insert_hook_256!(0),
    insert_hook_256!(256),
    insert_hook_256!(512),
    insert_hook_256!(768),
];

/// Returns the monomorphized `ComponentHook` `fn` pointer incrementing `live_values` for `slot`.
#[inline]
pub(crate) fn on_insert_hook_for_slot(slot: usize) -> ComponentHook {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    INSERT_HOOK_BLOCKS[b256][b16][idx]
}

// ============================================================================
// Guest Dynamic Component Lifecycle Hooks (`on_add` / `on_remove`)
// ============================================================================

#[derive(Clone)]
struct HookEntry {
    on_add: Option<GuestFnPtr<ComponentHookFn>>,
    on_remove: Option<GuestFnPtr<ComponentHookFn>>,
    user_data: GuestUserData,
    is_active: Arc<AtomicBool>,
}

pub(crate) struct HookSlotSnapshot {
    entries: Box<[HookEntry]>,
}

static HOOK_SLOTS: [ArcSwapOption<HookSlotSnapshot>; MAX_DROP_TRAMPOLINES] =
    [const { ArcSwapOption::const_empty() }; MAX_DROP_TRAMPOLINES];

struct HookControlPlane {
    free_list: Vec<usize>,
    entries: Vec<Vec<HookEntry>>,
}

static HOOK_CONTROL: LazyLock<Mutex<HookControlPlane>> = LazyLock::new(|| {
    Mutex::new(HookControlPlane {
        free_list: (0..MAX_DROP_TRAMPOLINES).rev().collect(),
        entries: (0..MAX_DROP_TRAMPOLINES).map(|_| Vec::new()).collect(),
    })
});

/// Staged dynamic component hook mutation recorded during `bevy_abi_mod_init` so `LoadTransaction`
/// or `rollback_unscheduled_mod_in_world` can atomically undo uncommitted hook bindings.
#[derive(Clone)]
pub(crate) struct StagedHookBinding {
    uuid: ComponentUuid,
    slot: usize,
    fresh_slot_allocated: bool,
    previous_snapshot: Option<Arc<HookSlotSnapshot>>,
    is_active: Arc<AtomicBool>,
}

fn on_add_trampoline<const SLOT: usize>(world: DeferredWorld<'_>, ctx: HookContext) {
    let Some(snap) = HOOK_SLOTS[SLOT].load_full() else {
        return;
    };
    let Some(entry) = snap
        .entries
        .iter()
        .rev()
        .find(|e| e.is_active.load(Ordering::Acquire))
    else {
        return;
    };
    let Some(GuestFnPtr(cb)) = entry.on_add else {
        return;
    };
    let Ok(comp_ptr) = world
        .entity(ctx.entity)
        .get_by_id(ctx.component_id)
        .map(|p| p.as_ptr().cast::<c_void>())
    else {
        return;
    };
    let ffi_entity = FfiEntity::from_bits(ctx.entity.to_bits());
    // SAFETY: `entry.is_active` is `true` and exclusive `&mut World` is required before `dlclose`,
    // so `cb` remains mapped in memory for the entire duration of this hook invocation; `comp_ptr`
    // points to the newly inserted component storage on `ctx.entity`.
    unsafe {
        cb(ffi_entity, comp_ptr, entry.user_data.as_ptr());
    }
}

fn on_remove_trampoline<const SLOT: usize>(world: DeferredWorld<'_>, ctx: HookContext) {
    let Some(snap) = HOOK_SLOTS[SLOT].load_full() else {
        return;
    };
    let Some(entry) = snap
        .entries
        .iter()
        .rev()
        .find(|e| e.is_active.load(Ordering::Acquire))
    else {
        return;
    };
    let Some(GuestFnPtr(cb)) = entry.on_remove else {
        return;
    };
    // Bevy executes `on_remove` strictly before removing component bytes from `Table`/`SparseSet`.
    let Ok(comp_ptr) = world
        .entity(ctx.entity)
        .get_by_id(ctx.component_id)
        .map(|p| p.as_ptr().cast::<c_void>())
    else {
        return;
    };
    let ffi_entity = FfiEntity::from_bits(ctx.entity.to_bits());
    // SAFETY: Same liveness and read-only component pointer guarantees as `on_add_trampoline`.
    unsafe {
        cb(ffi_entity, comp_ptr, entry.user_data.as_ptr());
    }
}

macro_rules! hook_fn_16 {
    ($fn_ident:ident, $b:expr) => {
        [
            $fn_ident::<{ $b }>,
            $fn_ident::<{ $b + 1 }>,
            $fn_ident::<{ $b + 2 }>,
            $fn_ident::<{ $b + 3 }>,
            $fn_ident::<{ $b + 4 }>,
            $fn_ident::<{ $b + 5 }>,
            $fn_ident::<{ $b + 6 }>,
            $fn_ident::<{ $b + 7 }>,
            $fn_ident::<{ $b + 8 }>,
            $fn_ident::<{ $b + 9 }>,
            $fn_ident::<{ $b + 10 }>,
            $fn_ident::<{ $b + 11 }>,
            $fn_ident::<{ $b + 12 }>,
            $fn_ident::<{ $b + 13 }>,
            $fn_ident::<{ $b + 14 }>,
            $fn_ident::<{ $b + 15 }>,
        ]
    };
}

macro_rules! hook_fn_256 {
    ($fn_ident:ident, $b:expr) => {
        [
            hook_fn_16!($fn_ident, $b),
            hook_fn_16!($fn_ident, $b + 16),
            hook_fn_16!($fn_ident, $b + 32),
            hook_fn_16!($fn_ident, $b + 48),
            hook_fn_16!($fn_ident, $b + 64),
            hook_fn_16!($fn_ident, $b + 80),
            hook_fn_16!($fn_ident, $b + 96),
            hook_fn_16!($fn_ident, $b + 112),
            hook_fn_16!($fn_ident, $b + 128),
            hook_fn_16!($fn_ident, $b + 144),
            hook_fn_16!($fn_ident, $b + 160),
            hook_fn_16!($fn_ident, $b + 176),
            hook_fn_16!($fn_ident, $b + 192),
            hook_fn_16!($fn_ident, $b + 208),
            hook_fn_16!($fn_ident, $b + 224),
            hook_fn_16!($fn_ident, $b + 240),
        ]
    };
}

static ON_ADD_HOOK_BLOCKS: [InsertHookBlock; 4] = [
    hook_fn_256!(on_add_trampoline, 0),
    hook_fn_256!(on_add_trampoline, 256),
    hook_fn_256!(on_add_trampoline, 512),
    hook_fn_256!(on_add_trampoline, 768),
];

static ON_REMOVE_HOOK_BLOCKS: [InsertHookBlock; 4] = [
    hook_fn_256!(on_remove_trampoline, 0),
    hook_fn_256!(on_remove_trampoline, 256),
    hook_fn_256!(on_remove_trampoline, 512),
    hook_fn_256!(on_remove_trampoline, 768),
];

#[inline]
pub(crate) fn on_add_hook_for_slot(slot: usize) -> ComponentHook {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    ON_ADD_HOOK_BLOCKS[b256][b16][idx]
}

#[inline]
pub(crate) fn on_remove_hook_for_slot(slot: usize) -> ComponentHook {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    ON_REMOVE_HOOK_BLOCKS[b256][b16][idx]
}

/// Per-`World` RAII resource owning all drop trampoline slots bound to `ComponentDescriptor`s in
/// that `World` (ADR 0001).
///
/// When the `World` is dropped, `WorldDropGuard::drop` disarms every owned slot (`DROP_SLOTS[slot].store(None)`),
/// transitions its state to `SLOT_CLOSING`, and immediately returns any slot with `live_values == 0`
/// to the global `1024`-slot free list (while slots with remaining live component instances are
/// reclaimed automatically by the final `drop_trampoline::<SLOT>` invocation).
#[derive(Resource, Default)]
pub struct WorldDropGuard {
    slots_by_uuid: HashMap<ComponentUuid, usize>,
    hook_slots_by_uuid: HashMap<ComponentUuid, usize>,
    guards: Vec<Arc<AtomicBool>>,
}

impl WorldDropGuard {
    /// Creates an empty [`WorldDropGuard`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of trampoline slots bound to this `World`.
    #[inline]
    pub fn len(&self) -> usize {
        self.slots_by_uuid.len()
    }

    /// Returns `true` if this `World` currently owns zero drop trampoline slots.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots_by_uuid.is_empty()
    }

    /// Allocates a new drop trampoline slot for `uuid` or updates the existing slot bound to `uuid`
    /// in this `World`, returning `(slot_index, trampoline_fn, staged_binding)` or `None` when all
    /// [`MAX_DROP_TRAMPOLINES`] slots are currently in use (`FfiResult::CAPACITY_EXCEEDED`).
    pub(crate) fn bind_or_update(
        &mut self,
        uuid: ComponentUuid,
        drop_fn: ComponentDropFn,
        is_active: Arc<AtomicBool>,
    ) -> Option<(usize, DropTrampolineFn, StagedDropBinding)> {
        let mut ctrl = DROP_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let wrapped = GuestFnPtr(drop_fn);

        if let Some(&slot) = self.slots_by_uuid.get(&uuid) {
            let previous_snapshot = DROP_SLOTS[slot].load_full();
            let entries = &mut ctrl.entries[slot];
            entries.retain(|(_, g)| g.load(Ordering::Acquire));
            if let Some(existing) = entries.iter_mut().find(|(_, g)| Arc::ptr_eq(g, &is_active)) {
                existing.0 = wrapped;
            } else {
                entries.push((wrapped, Arc::clone(&is_active)));
            }
            let snap = Arc::new(DropSlotSnapshot {
                entries: entries.clone().into_boxed_slice(),
            });
            DROP_SLOTS[slot].store(Some(snap));
            drop(ctrl);

            if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
                self.guards.push(Arc::clone(&is_active));
            }
            let staged = StagedDropBinding {
                uuid,
                slot,
                fresh_slot_allocated: false,
                previous_snapshot,
                is_active,
            };
            return Some((slot, drop_trampoline_fn_for_slot(slot), staged));
        }

        let slot = ctrl.free_list.pop()?;
        SLOT_LIFECYCLE[slot].store(SLOT_IN_USE_BIT, Ordering::Release);

        let entries = &mut ctrl.entries[slot];
        entries.clear();
        entries.push((wrapped, Arc::clone(&is_active)));
        let snap = Arc::new(DropSlotSnapshot {
            entries: entries.clone().into_boxed_slice(),
        });
        DROP_SLOTS[slot].store(Some(snap));
        drop(ctrl);

        self.slots_by_uuid.insert(uuid, slot);
        if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
            self.guards.push(Arc::clone(&is_active));
        }
        let staged = StagedDropBinding {
            uuid,
            slot,
            fresh_slot_allocated: true,
            previous_snapshot: None,
            is_active,
        };
        Some((slot, drop_trampoline_fn_for_slot(slot), staged))
    }

    /// Rolls back uncommitted [`StagedDropBinding`]s in reverse registration order (`ADR 0001` §2.4):
    /// disarms `is_active`, restores `previous_snapshot` on hot-updated slots, and immediately
    /// returns freshly allocated slots (`live_values == 0`) to `FREE_LIST`.
    pub(crate) fn rollback_staged_bindings(&mut self, bindings: &[StagedDropBinding]) {
        if bindings.is_empty() {
            return;
        }
        for binding in bindings.iter().rev() {
            binding.is_active.store(false, Ordering::Release);
        }

        let mut ctrl = DROP_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for binding in bindings.iter().rev() {
            let slot = binding.slot;
            if binding.fresh_slot_allocated {
                self.slots_by_uuid.remove(&binding.uuid);
                DROP_SLOTS[slot].store(None);
                SLOT_LIFECYCLE[slot].store(0, Ordering::Release);
                ctrl.entries[slot].clear();
                if !ctrl.free_list.contains(&slot) {
                    ctrl.free_list.push(slot);
                }
            } else {
                ctrl.entries[slot].retain(|(_, g)| {
                    !Arc::ptr_eq(g, &binding.is_active) && g.load(Ordering::Acquire)
                });
                DROP_SLOTS[slot].store(binding.previous_snapshot.clone());
            }
        }
        drop(ctrl);

        self.guards.retain(|g| g.load(Ordering::Acquire));
    }

    /// Allocates or updates a dynamic component `on_add` / `on_remove` hook slot for `uuid`.
    pub(crate) fn bind_or_update_hooks(
        &mut self,
        uuid: ComponentUuid,
        on_add: Option<ComponentHookFn>,
        on_remove: Option<ComponentHookFn>,
        user_data: *mut c_void,
        is_active: Arc<AtomicBool>,
    ) -> Option<(usize, StagedHookBinding)> {
        let mut ctrl = HOOK_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let new_entry = HookEntry {
            on_add: on_add.map(GuestFnPtr),
            on_remove: on_remove.map(GuestFnPtr),
            user_data: GuestUserData::new(user_data),
            is_active: Arc::clone(&is_active),
        };

        if let Some(&slot) = self.hook_slots_by_uuid.get(&uuid) {
            let previous_snapshot = HOOK_SLOTS[slot].load_full();
            let entries = &mut ctrl.entries[slot];
            entries.retain(|e| e.is_active.load(Ordering::Acquire));
            if let Some(existing) = entries
                .iter_mut()
                .find(|e| Arc::ptr_eq(&e.is_active, &is_active))
            {
                *existing = new_entry;
            } else {
                entries.push(new_entry);
            }
            let snap = Arc::new(HookSlotSnapshot {
                entries: entries.clone().into_boxed_slice(),
            });
            HOOK_SLOTS[slot].store(Some(snap));
            drop(ctrl);

            if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
                self.guards.push(Arc::clone(&is_active));
            }
            return Some((
                slot,
                StagedHookBinding {
                    uuid,
                    slot,
                    fresh_slot_allocated: false,
                    previous_snapshot,
                    is_active,
                },
            ));
        }

        let slot = ctrl.free_list.pop()?;
        let entries = &mut ctrl.entries[slot];
        entries.clear();
        entries.push(new_entry);
        let snap = Arc::new(HookSlotSnapshot {
            entries: entries.clone().into_boxed_slice(),
        });
        HOOK_SLOTS[slot].store(Some(snap));
        drop(ctrl);

        self.hook_slots_by_uuid.insert(uuid, slot);
        if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
            self.guards.push(Arc::clone(&is_active));
        }
        Some((
            slot,
            StagedHookBinding {
                uuid,
                slot,
                fresh_slot_allocated: true,
                previous_snapshot: None,
                is_active,
            },
        ))
    }

    /// Rolls back uncommitted [`StagedHookBinding`]s in reverse registration order.
    pub(crate) fn rollback_staged_hook_bindings(&mut self, bindings: &[StagedHookBinding]) {
        if bindings.is_empty() {
            return;
        }
        for binding in bindings.iter().rev() {
            binding.is_active.store(false, Ordering::Release);
        }

        let mut ctrl = HOOK_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for binding in bindings.iter().rev() {
            let slot = binding.slot;
            if binding.fresh_slot_allocated {
                self.hook_slots_by_uuid.remove(&binding.uuid);
                HOOK_SLOTS[slot].store(None);
                ctrl.entries[slot].clear();
                if !ctrl.free_list.contains(&slot) {
                    ctrl.free_list.push(slot);
                }
            } else {
                ctrl.entries[slot].retain(|e| {
                    !Arc::ptr_eq(&e.is_active, &binding.is_active)
                        && e.is_active.load(Ordering::Acquire)
                });
                HOOK_SLOTS[slot].store(binding.previous_snapshot.clone());
            }
        }
        drop(ctrl);
        self.guards.retain(|g| g.load(Ordering::Acquire));
    }
}

impl Drop for WorldDropGuard {
    fn drop(&mut self) {
        for guard in &self.guards {
            guard.store(false, Ordering::Release);
        }
        for &slot in self.slots_by_uuid.values() {
            DROP_SLOTS[slot].store(None);
            let prev = SLOT_LIFECYCLE[slot]
                .fetch_xor(SLOT_IN_USE_BIT | SLOT_CLOSING_BIT, Ordering::AcqRel);
            if (prev & SLOT_LIVE_COUNT_MASK) == 0 {
                reclaim_closed_slot(slot);
            }
        }
        if !self.hook_slots_by_uuid.is_empty() {
            let mut hctrl = HOOK_CONTROL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for &slot in self.hook_slots_by_uuid.values() {
                HOOK_SLOTS[slot].store(None);
                hctrl.entries[slot].clear();
                if !hctrl.free_list.contains(&slot) {
                    hctrl.free_list.push(slot);
                }
            }
        }
    }
}

/// Disarms `is_active` and prunes all drop and hook trampoline entries paired with `is_active` before `dlclose`.
pub(crate) fn prune_drop_trampolines_for_guard(is_active: &Arc<AtomicBool>) {
    is_active.store(false, Ordering::Release);
    {
        let mut ctrl = DROP_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for slot in 0..MAX_DROP_TRAMPOLINES {
            let (state, _) = decode_slot_lifecycle(SLOT_LIFECYCLE[slot].load(Ordering::Acquire));
            if state != SLOT_IN_USE {
                continue;
            }
            let entries = &mut ctrl.entries[slot];
            let before = entries.len();
            entries.retain(|(_, g)| !Arc::ptr_eq(g, is_active) && g.load(Ordering::Acquire));
            if entries.len() != before {
                if entries.is_empty() {
                    DROP_SLOTS[slot].store(None);
                } else {
                    let snap = Arc::new(DropSlotSnapshot {
                        entries: entries.clone().into_boxed_slice(),
                    });
                    DROP_SLOTS[slot].store(Some(snap));
                }
            }
        }
    }

    let mut hctrl = HOOK_CONTROL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (entries, slot_cell) in hctrl.entries.iter_mut().zip(HOOK_SLOTS.iter()) {
        if entries.is_empty() {
            continue;
        }
        let before = entries.len();
        entries.retain(|e| {
            !Arc::ptr_eq(&e.is_active, is_active) && e.is_active.load(Ordering::Acquire)
        });
        if entries.len() != before {
            if entries.is_empty() {
                slot_cell.store(None);
            } else {
                let snap = Arc::new(HookSlotSnapshot {
                    entries: entries.clone().into_boxed_slice(),
                });
                slot_cell.store(Some(snap));
            }
        }
    }
}

/// Returns the current number of free drop trampoline slots in the global pool (for diagnostics).
pub fn available_drop_trampoline_slots() -> usize {
    DROP_CONTROL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .free_list
        .len()
}
