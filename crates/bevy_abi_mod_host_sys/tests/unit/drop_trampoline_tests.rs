use super::*;
use bevy_ecs::world::World;

unsafe extern "C" fn noop_drop(_ptr: *mut c_void) {}

#[test]
fn paged_slab_recycles_5000_distinct_uuids_in_one_world_with_generation_bump() {
    let _serial = TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _ctrl_sanity = available_drop_trampoline_slots();
    let mut guard = WorldDropGuard::new();
    let mut expected_slot: Option<usize> = None;
    let mut prev_gen: Option<u64> = None;

    let cycles = if cfg!(miri) { 16u64 } else { 5_000u64 };
    for i in 0..cycles {
        let uuid = ComponentUuid::from_parts(&format!("unit_slab::{i}"), 8, 8, i);
        let is_active = Arc::new(AtomicBool::new(true));
        let (slot, _tramp, _staged) = guard
            .bind_or_update(uuid, noop_drop, Arc::clone(&is_active))
            .expect("slot allocation must succeed across 5,000 recycled cycles");

        let (_, slot_gen) = guard.binding_for_uuid(&uuid).expect("binding recorded");
        if let Some(s) = expected_slot {
            if slot == s {
                assert_eq!(slot_gen, prev_gen.unwrap() + 1);
                prev_gen = Some(slot_gen);
            }
        } else {
            expected_slot = Some(slot);
            prev_gen = Some(slot_gen);
        }

        // Simulate spawning & despawning a `ModScoped` entity (`live_values: 0 -> 1 -> 0`).
        increment_drop_slot_live_values(slot);
        assert_eq!(drop_slot_live_values(slot), Some(1));
        let mut dummy_val: u64 = i;
        // SAFETY: `OwningPtr::make` provides valid aligned pointer for `drop_trampoline_inner`.
        unsafe {
            OwningPtr::make(dummy_val, |ptr| {
                drop_trampoline_inner(slot, ptr);
            });
        }
        let _ = &mut dummy_val;
        assert_eq!(drop_slot_live_values(slot), Some(0));

        is_active.store(false, Ordering::Release);
        let recycled = guard.recycle_slots_for_guard(&is_active);
        assert_eq!(recycled, 1);
        assert_eq!(guard.len(), 0);
    }

    drop(guard);
    let _ = World::new();
}

#[test]
fn recycle_skips_slots_with_live_persistent_values_until_world_drop() {
    let _serial = TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut guard = WorldDropGuard::new();
    let uuid = ComponentUuid::from_parts("unit_slab::PersistentComp", 8, 8, 999);
    let is_active = Arc::new(AtomicBool::new(true));
    let (slot, _tramp, _staged) = guard
        .bind_or_update(uuid, noop_drop, Arc::clone(&is_active))
        .expect("allocate slot");

    increment_drop_slot_live_values(slot);
    assert_eq!(drop_slot_live_values(slot), Some(1));

    // Mod unloads while a persistent entity still holds a live component (`live_values == 1`):
    // must NOT recycle prematurely!
    is_active.store(false, Ordering::Release);
    let recycled = guard.recycle_slots_for_guard(&is_active);
    assert_eq!(recycled, 0);
    assert_eq!(guard.len(), 1);

    // Now persistent entity despawns (`live_values -> 0`), and `WorldDropGuard::drop` reclaims it.
    // SAFETY: Valid `u64` stack pointer via `OwningPtr::make`.
    unsafe {
        OwningPtr::make(7u64, |ptr| {
            drop_trampoline_inner(slot, ptr);
        });
    }
    assert_eq!(drop_slot_live_values(slot), Some(0));
    let gen_before_drop = drop_slot_generation(slot).unwrap();
    drop(guard);
    assert_eq!(drop_slot_generation(slot), Some(gen_before_drop + 1));
}
