# Rust Guest SDK Guide (`bevy_abi_mod_guest`)

`bevy_abi_mod_guest` provides an ergonomic, `#![forbid(unsafe_code)]` Rust SDK for building `.cdylib` mods with **zero `bevy` dependencies**.

---

## 1. Per-`World` Plugin Lifecycle (`GuestPlugin` & `export_mod!`)

Unlike legacy C plugins that stash state in process-wide `static mut` globals, `export_mod!(plugin: MyPlugin)` allocates a fresh `MyPlugin` + `GuestInstance` on the **guest heap** for every Bevy `World` that loads the `.cdylib`, and drops it deterministically inside `bevy_abi_mod_shutdown(instance)`:

```rust
#[derive(Default)]
pub struct PhysicsMod {
    tick_counter: u64,
}

impl GuestPlugin for PhysicsMod {
    const NAME: &'static CStr = c"physics_mod";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        // Register runtime components and systems here
        FfiResult::OK
    }
}

export_mod!(plugin: PhysicsMod);
```

---

## 2. `AbiPod` vs. VTable-Accessed Opaque Components

- **`#[repr(C)]` `AbiPod` Types (`#[derive(ModComponent)]` or `#[derive(AbiPod)]`):**
  Eligible for zero-copy `&[T]` / `&mut [T]` slice iteration (`Chunk::component`, `Chunk::component_mut`, `Chunk::read_write`) and raw byte commands (`GuestCommands::insert`, batched `GuestCommands::spawn`).
  - Allowed primitives: `u8`/`i8`, `u16`/`i16`, `u32`/`i32`, `u64`/`i64`, `f32`, `f64`, `FfiBool`, `BamComponent`, `FfiEntity`, `ComponentUuid`, `[T; N]`.
  - Disallowed in `AbiPod`: `bool`, `char`, `enum`s, references (invalid bit patterns = UB), and `usize`/`isize`/`u128`/`i128` (architecture/compiler layout divergence).
- **Opaque Host Components (`#[repr(Rust)]`, `String`, `Vec<T>`, `bool`):**
  Inserted via `cmds.insert_default(entity, handle)` and mutated safely through the host's generated `#[repr(C)]` `<Struct>VTable` (`app.resolve_vtable::<MyVTable>(c"bam.v0.game::UnitTag.vtable", expected_uuid)`), where every setter returns `FfiResult` (`INVALID_UTF8`, `NULL_POINTER`, `OK`).

---

## 3. Unified System Builder (`app.system(...)`)

`GuestApp::system` configures single-query and multi-query (`0..=8`) systems, schedules, ordering, `run_if` predicates, and main-thread pinning through one builder:

```rust
app.system(c"integrate_velocity")
    .query(|q| {
        q.write::<Position>()      // term 0: &mut Position
         .read::<Velocity>()       // term 1: &Velocity
         .with::<ActiveTag>()      // term 2: With<ActiveTag>
         .without::<FrozenTag>()   // term 3: Without<FrozenTag>
         .res_read::<SimClock>()   // term 4: Res<SimClock>
    })
    .schedule(ScheduleId::FIXED_UPDATE)
    .after(c"apply_forces")
    .before(c"other_mod::collision_broadphase")
    .run_if(|| true)
    .build(|ctx| {
        let Some(clock) = ctx.iter().resource::<SimClock>(4) else {
            return FfiResult::NULL_POINTER;
        };
        let dt = clock.dt;

        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            // Disjoint split borrow of (&[Velocity], &mut [Position])
            let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                return FfiResult::TYPE_MISMATCH;
            };
            for (p, v) in pos.iter_mut().zip(vel.iter()) {
                p.x += v.x * dt;
                p.y += v.y * dt;
                p.z += v.z * dt;
            }
        }
        FfiResult::OK
    })?;
```

### Sparse-Set Columns & Fine-Grained Change Tracking
- **Sparse-Set Components:** When a component uses `StorageType::SparseSet`, call `chunk.rows::<T>(term_idx)` or `chunk.rows_mut::<T>(term_idx)` to iterate batched indirect row references without falling back to 1-entity FFI calls.
- **Untracked Mutable Slices:** When inspecting a mutable column and modifying only occasional elements, use `chunk.column_mut_untracked::<T>(term_idx)` and call `chunk.mark_changed_range(term_idx, row, 1)` only on mutated rows so downstream `Changed<T>` systems skip untouched rows.

---

## 4. Handle-Based & Batched Deferred Commands (`GuestCommands`)

Resolve `BamComponent` handles once in `GuestPlugin::build` (`let pos_h = app.resolve_component::<Position>()?;`) and enqueue zero-steady-state-allocation commands inside systems:

```rust
let mut cmds = ctx.commands()?;
let entity = cmds.spawn(&[
    SpawnEntry::new(pos_h, &Position { x: 0.0, y: 5.0, z: 0.0 }),
    SpawnEntry::new(vel_h, &Velocity { x: 1.0, y: 0.0, z: 0.0 }),
])?;
```

`cmds.spawn(&entries)` reserves an `FfiEntity` atomically and materializes all components directly into the final Bevy archetype in a single table transition during `apply_deferred`.

---

## 5. Panic Policy: `panic = "unwind"` vs. `panic = "abort"`

- **`panic = "unwind"` (Default & Recommended):** `bevy_abi_mod_guest_sys` catches panics inside the `.cdylib` before reaching `extern "C"`, drops the panic payload on the guest allocator, discards uncommitted commands from that tick, and returns `FfiResult::PANIC_CAUGHT` so the host can quarantine the system cleanly.
- **`panic = "abort"`:** `catch_unwind` is a no-op; any guest panic immediately terminates the OS process via `abort()` with zero UB.
