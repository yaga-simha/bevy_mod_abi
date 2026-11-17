//! `loom` exhaustive concurrency model tests for ADR 0001 drop trampoline publication,
//! lock-free reader execution, and single-word (`SLOT_CLOSING_BIT | live_values`) slot reclamation.

use bevy_abi_mod_host_sys::drop_trampoline::{
    SLOT_CLOSING_BIT, SLOT_FREE, SLOT_IN_USE, SLOT_IN_USE_BIT, SLOT_LIVE_COUNT_MASK,
    decode_slot_lifecycle,
};
use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use loom::thread;

struct ModelSlot {
    /// Models `DROP_SLOTS[slot]: ArcSwapOption<DropSlotSnapshot>` pointer publication.
    snapshot_ptr: AtomicPtr<Arc<AtomicBool>>,
    /// Models `SLOT_LIFECYCLE[slot]: AtomicUsize` (`state_bits | live_values`).
    lifecycle: AtomicUsize,
    /// Counts how many times `reclaim_closed_slot` transitioned `SLOT_CLOSING -> SLOT_FREE`.
    reclaim_count: AtomicUsize,
    /// Counts foreign destructor invocations while `is_active == true`.
    foreign_drop_calls: AtomicUsize,
}

impl ModelSlot {
    fn new_in_use(initial_live_values: usize, guard: Arc<AtomicBool>) -> Self {
        let boxed = Box::into_raw(Box::new(guard));
        Self {
            snapshot_ptr: AtomicPtr::new(boxed),
            lifecycle: AtomicUsize::new(SLOT_IN_USE_BIT | initial_live_values),
            reclaim_count: AtomicUsize::new(0),
            foreign_drop_calls: AtomicUsize::new(0),
        }
    }

    /// Mirrors `reclaim_closed_slot` in `crates/bevy_abi_mod_host_sys/src/drop_trampoline.rs`.
    fn reclaim_closed_slot(&self) {
        if self
            .lifecycle
            .compare_exchange(SLOT_CLOSING_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.reclaim_count.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Mirrors `drop_trampoline::<SLOT>` in `crates/bevy_abi_mod_host_sys/src/drop_trampoline.rs`.
    fn run_drop_trampoline(&self) {
        let snap = self.snapshot_ptr.load(Ordering::Acquire);
        if !snap.is_null() {
            // SAFETY: Under the model, `snap` remains allocated until `ModelSlot` teardown so we
            // can inspect concurrent `publish` / `close` interleavings without allocator noise.
            let guard = unsafe { &*snap };
            if guard.load(Ordering::Acquire) {
                self.foreign_drop_calls.fetch_add(1, Ordering::Relaxed);
            }
        }

        let prev = self.lifecycle.fetch_sub(1, Ordering::AcqRel);
        if prev == (SLOT_CLOSING_BIT | 1) {
            self.reclaim_closed_slot();
        }
    }

    /// Mirrors `WorldDropGuard::drop` in `crates/bevy_abi_mod_host_sys/src/drop_trampoline.rs`.
    fn close_world(&self, guard: &Arc<AtomicBool>) -> *mut Arc<AtomicBool> {
        guard.store(false, Ordering::Release);
        let old = self
            .snapshot_ptr
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        let prev = self
            .lifecycle
            .fetch_xor(SLOT_IN_USE_BIT | SLOT_CLOSING_BIT, Ordering::AcqRel);
        if (prev & SLOT_LIVE_COUNT_MASK) == 0 {
            self.reclaim_closed_slot();
        }
        old
    }
}

#[test]
fn loom_concurrent_publish_drop_and_world_close_reclaims_exactly_once() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);
    builder.check(|| {
        let guard_v1 = Arc::new(AtomicBool::new(true));
        let guard_v2 = Arc::new(AtomicBool::new(true));
        let slot = Arc::new(ModelSlot::new_in_use(1, Arc::clone(&guard_v1)));

        // Thread 1: Hot-reload publishes a new snapshot (`guard_v2`) into `DROP_SLOTS[slot]`.
        let slot_pub = Arc::clone(&slot);
        let g2_pub = Arc::clone(&guard_v2);
        let pub_handle = thread::spawn(move || {
            let new_ptr = Box::into_raw(Box::new(g2_pub));
            let (state, _) = decode_slot_lifecycle(slot_pub.lifecycle.load(Ordering::Acquire));
            if state == SLOT_IN_USE {
                let prev_ptr = slot_pub.snapshot_ptr.swap(new_ptr, Ordering::AcqRel);
                let (after_state, _) =
                    decode_slot_lifecycle(slot_pub.lifecycle.load(Ordering::Acquire));
                if after_state != SLOT_IN_USE {
                    let raced = slot_pub
                        .snapshot_ptr
                        .swap(core::ptr::null_mut(), Ordering::AcqRel);
                    return (prev_ptr, raced);
                }
                (prev_ptr, core::ptr::null_mut())
            } else {
                (new_ptr, core::ptr::null_mut())
            }
        });

        // Thread 2: Component `Table::drop` runs `drop_trampoline::<SLOT>` for the live value.
        let slot_drop = Arc::clone(&slot);
        let drop_handle = thread::spawn(move || {
            slot_drop.run_drop_trampoline();
        });

        // Main thread: `WorldDropGuard::drop` closes the slot.
        guard_v1.store(false, Ordering::Release);
        let closed_ptr = slot.close_world(&guard_v2);

        let (ptr_a, ptr_b) = pub_handle.join().unwrap();
        drop_handle.join().unwrap();

        // Exactly one winner (`WorldDropGuard::drop` OR the final `drop_trampoline`) must reclaim
        // the slot across every C11 weak-memory interleaving!
        let (final_state, final_live) =
            decode_slot_lifecycle(slot.lifecycle.load(Ordering::Acquire));
        assert_eq!(final_live, 0);
        assert_eq!(final_state, SLOT_FREE);
        assert_eq!(slot.reclaim_count.load(Ordering::Acquire), 1);

        let residual = slot
            .snapshot_ptr
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        for raw in [closed_ptr, ptr_a, ptr_b, residual] {
            if !raw.is_null() {
                // SAFETY: Every `Box::into_raw` pointer was swapped out to a unique owner above.
                unsafe {
                    drop(Box::from_raw(raw));
                }
            }
        }
    });
}

#[test]
fn loom_two_concurrent_value_drops_vs_world_close_never_double_reclaims() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);
    builder.check(|| {
        let guard = Arc::new(AtomicBool::new(true));
        let slot = Arc::new(ModelSlot::new_in_use(2, Arc::clone(&guard)));

        let s1 = Arc::clone(&slot);
        let t1 = thread::spawn(move || {
            s1.run_drop_trampoline();
        });

        let s2 = Arc::clone(&slot);
        let t2 = thread::spawn(move || {
            s2.run_drop_trampoline();
        });

        let closed_ptr = slot.close_world(&guard);

        t1.join().unwrap();
        t2.join().unwrap();

        let (final_state, final_live) =
            decode_slot_lifecycle(slot.lifecycle.load(Ordering::Acquire));
        assert_eq!(final_live, 0);
        assert_eq!(final_state, SLOT_FREE);
        assert_eq!(slot.reclaim_count.load(Ordering::Acquire), 1);

        if !closed_ptr.is_null() {
            // SAFETY: `closed_ptr` is the sole `Box::into_raw` allocation created at setup.
            unsafe {
                drop(Box::from_raw(closed_ptr));
            }
        }
    });
}
