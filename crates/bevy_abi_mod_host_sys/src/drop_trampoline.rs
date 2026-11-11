//! Lock-free per-World foreign drop trampolines for dynamically registered components.

use crate::sync_ptr::{GuestFnPtr, RetiredSnapshotPtr};
use bevy_abi_mod_sys::{ComponentDropFn, ComponentUuid};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::WorldId;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Maximum number of distinct `(WorldId, ComponentUuid)` foreign drop trampolines supported per process.
pub const MAX_DROP_TRAMPOLINES: usize = 1024;

type DropGuardPair = (GuestFnPtr<ComponentDropFn>, Arc<AtomicBool>);

struct DropSlotSnapshot {
    entries: Box<[DropGuardPair]>,
}

static DROP_SLOT_SNAPSHOTS: [AtomicPtr<DropSlotSnapshot>; MAX_DROP_TRAMPOLINES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_DROP_TRAMPOLINES];

struct DropSlotMeta {
    world_id: WorldId,
    uuid: ComponentUuid,
    entries: Vec<DropGuardPair>,
}

struct DropRegistryState {
    slots: Vec<DropSlotMeta>,
    /// Retains retired `DropSlotSnapshot` allocations across hot-reloads, rollbacks, or test resets
    /// so concurrent lock-free readers on `DROP_SLOT_SNAPSHOTS` never dereference freed memory.
    retired: Vec<RetiredSnapshotPtr<DropSlotSnapshot>>,
}

static DROP_REGISTRY: LazyLock<Mutex<DropRegistryState>> = LazyLock::new(|| {
    Mutex::new(DropRegistryState {
        slots: Vec::new(),
        retired: Vec::new(),
    })
});

/// Invokes the most recent active foreign drop callback bound to `SLOT`, if any.
///
/// # Safety
/// `ptr` must point to a valid component allocation owned by Bevy's storage drop hook.
unsafe fn drop_trampoline<const SLOT: usize>(ptr: OwningPtr<'_>) {
    let snap_ptr = DROP_SLOT_SNAPSHOTS[SLOT].load(Ordering::Acquire);
    if !snap_ptr.is_null() {
        // SAFETY: `snap_ptr` points to a valid `DropSlotSnapshot` retained for the process lifetime
        // (either currently installed in `DROP_SLOT_SNAPSHOTS` or held in `DROP_REGISTRY.retired`).
        let snap = unsafe { &*snap_ptr };
        if let Some(&(GuestFnPtr(drop_fn), _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
        {
            let raw = ptr.as_ptr().cast::<c_void>();
            // SAFETY: Foreign drop function is invoked on a valid component instance while its
            // paired mod instance (`guard == true`) remains mapped in memory.
            unsafe { drop_fn(raw) };
        }
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

/// Binds or updates a static Bevy drop trampoline slot for `(world_id, uuid)` paired with `is_active`.
pub(crate) fn allocate_drop_trampoline(
    world_id: WorldId,
    uuid: ComponentUuid,
    drop_fn: ComponentDropFn,
    is_active: Arc<AtomicBool>,
) -> Option<DropTrampolineFn> {
    let mut reg = DROP_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let wrapped_fn = GuestFnPtr(drop_fn);

    if let Some((slot_idx, meta)) = reg
        .slots
        .iter_mut()
        .enumerate()
        .find(|(_, m)| m.world_id == world_id && m.uuid == uuid)
    {
        meta.entries.retain(|(_, g)| g.load(Ordering::Acquire));
        if let Some(existing) = meta
            .entries
            .iter_mut()
            .find(|(_, g)| Arc::ptr_eq(g, &is_active))
        {
            existing.0 = wrapped_fn;
        } else {
            meta.entries.push((wrapped_fn, is_active));
        }
        let new_snap = Box::into_raw(Box::new(DropSlotSnapshot {
            entries: meta.entries.clone().into_boxed_slice(),
        }));
        let old_snap = DROP_SLOT_SNAPSHOTS[slot_idx].swap(new_snap, Ordering::AcqRel);
        if !old_snap.is_null() {
            reg.retired.push(RetiredSnapshotPtr::new(old_snap));
        }
        return Some(drop_trampoline_fn_for_slot(slot_idx));
    }

    let slot_idx = reg.slots.len();
    if slot_idx >= MAX_DROP_TRAMPOLINES {
        return None;
    }

    let entries = vec![(wrapped_fn, is_active)];
    let new_snap = Box::into_raw(Box::new(DropSlotSnapshot {
        entries: entries.clone().into_boxed_slice(),
    }));
    let old_snap = DROP_SLOT_SNAPSHOTS[slot_idx].swap(new_snap, Ordering::AcqRel);
    if !old_snap.is_null() {
        reg.retired.push(RetiredSnapshotPtr::new(old_snap));
    }
    reg.slots.push(DropSlotMeta {
        world_id,
        uuid,
        entries,
    });
    Some(drop_trampoline_fn_for_slot(slot_idx))
}

/// Disarms `is_active` and prunes all drop trampoline entries paired with `is_active` before `dlclose`.
pub(crate) fn prune_drop_trampolines_for_guard(is_active: &Arc<AtomicBool>) {
    is_active.store(false, Ordering::Release);
    let mut reg = DROP_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut pending_retire = Vec::new();
    for (slot_idx, meta) in reg.slots.iter_mut().enumerate() {
        let before_len = meta.entries.len();
        meta.entries
            .retain(|(_, g)| !Arc::ptr_eq(g, is_active) && g.load(Ordering::Acquire));
        if meta.entries.len() != before_len {
            let new_snap = if meta.entries.is_empty() {
                core::ptr::null_mut()
            } else {
                Box::into_raw(Box::new(DropSlotSnapshot {
                    entries: meta.entries.clone().into_boxed_slice(),
                }))
            };
            let old_snap = DROP_SLOT_SNAPSHOTS[slot_idx].swap(new_snap, Ordering::AcqRel);
            if !old_snap.is_null() {
                pending_retire.push(RetiredSnapshotPtr::new(old_snap));
            }
        }
    }
    reg.retired.extend(pending_retire);
}

/// Resets all drop trampoline slots to unassigned for isolated integration tests.
#[cfg(any(test, feature = "test-utils"))]
pub fn reset_drop_trampolines_for_testing() {
    let mut reg = DROP_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let used = reg.slots.len();
    for slot in DROP_SLOT_SNAPSHOTS.iter().take(used) {
        let old = slot.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !old.is_null() {
            reg.retired.push(RetiredSnapshotPtr::new(old));
        }
    }
    reg.slots.clear();
}
