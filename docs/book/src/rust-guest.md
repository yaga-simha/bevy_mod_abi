# Rust Guest SDK Guide (`bevy_abi_mod_guest`)

`bevy_abi_mod_guest` (`v0.3.0`) provides an ergonomic, `#![forbid(unsafe_code)]` Rust SDK covering all **68/68** `ABI 0.3.0` host slots for building `.cdylib` mods with **zero `bevy` dependencies**.

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
    const VERSION: &'static CStr = c"0.3.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        // Register runtime components, relations, schedule sets, and systems here
        FfiResult::OK
    }
}

export_mod!(plugin: PhysicsMod);
```

---

## 2. `AbiPod` vs. VTable-Accessed Opaque Components

- **`#[repr(C)]` `AbiPod` Types (`#[derive(AbiPod, GuestComponent)]` or `#[derive(ModComponent)]`):**
  Eligible for zero-copy `&[T]` / `&mut [T]` slice iteration (typed `query_typed` / `ctx.single_query::<&mut T>().for_each(...)` or `Chunk::component`, `Chunk::component_mut`, `Chunk::read_write`) and raw byte commands (`GuestCommands::insert`, batched `GuestCommands::spawn`). Pair `#[derive(AbiPod, GuestComponent)]` with `#[guest_component(name = "...", sparse_set, immutable, summary_tick)]` for zero-boilerplate guest components (or use `#[mod_component(name = "...", guest)]` when sharing a `ModComponent` definition). Both derives verify at compile time that structs have zero implicit compiler padding (`size_of::<Self>() == sum(size_of::<Fields>())`), and `#[derive(ModComponent)]` automatically stamps `SCHEMA_FINGERPRINT` and `BAM_FIELDS` for zero-copy reload retention, `migrate_component_column`, and declarative `ReflectFromPtr`/BRP reflection.
  - Allowed primitives: `u8`/`i8`, `u16`/`i16`, `u32`/`i32`, `u64`/`i64`, `f32`, `f64`, `FfiBool`, `FfiOption<T>`, `BamComponent`, `BamSystem`, `BamSystemSet`, `BamSymbol`, `FfiEntity`, `FfiChildOf`, `BamChangeTicks`, `FfiAssetHandle`, `FfiAssetLoadState`, `BamEventKind`, `BamFieldKind`, `ComponentUuid`, `[T; N]`.
  - Disallowed in `AbiPod`: `bool`, `char`, non-`#[repr(C)]`/untagged Rust `enum`s, references (invalid bit patterns = UB), padded structs without explicit padding fields, and `usize`/`isize`/`u128`/`i128` (architecture/compiler layout divergence).
- **Opaque Host Components (`#[repr(Rust)]`, `String`, `Vec<T>`, `bool`):**
  Inserted via `cmds.insert_default(entity, handle)` and mutated safely through the host's generated `#[repr(C)]` `<Struct>VTable` (`unsafe { app.host().resolve_vtable::<MyVTable>(c"bam.v0.game::UnitTag.vtable", expected_uuid) }`, where `Host::resolve_vtable` requires `unsafe` because it verifies an external `VTableHeader` against `T`), where every setter returns `FfiResult` (`INVALID_UTF8`, `NULL_POINTER`, `OK`).

---

## 3. Unified System Builder, 16-Term Queries & Decoupled Schedule Sets

`GuestApp::system` configures single-query and multi-query (`0..=8`) systems, typed tuple queries up to **16 terms** (`QueryTuple`), tuple-generic **`Or<(T0..T7)>`** (2..=8 filter branches, preserving `Or2`/`Or3` aliases), schedules, ordering, `run_if` predicates, and main-thread pinning:

```rust
let res = app.system(c"integrate_velocity")
    .query(|q| {
        q.write::<Position>()      // term 0: &mut Position
         .read::<Velocity>()       // term 1: &Velocity
         .with::<ActiveTag>()      // term 2: With<ActiveTag>
         .without::<FrozenTag>()   // term 3: Without<FrozenTag>
    })
    .resource_read::<SimClock>()   // declaration-indexed singleton resource
    .schedule(ScheduleId::FIXED_UPDATE)
    .in_set(c"host::physics_step")
    .after(c"apply_forces")
    .before(c"other_mod::collision_broadphase")
    .run_if(|_ctx| true)
    .build(|ctx| {
        let Ok(clock) = ctx.res_with_ticks::<SimClock>(0) else {
            return FfiResult::NULL_POINTER;
        };
        let dt = if clock.is_changed() { clock.dt } else { 1.0 / 60.0 };

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
    });
if !res.is_ok() {
    return res;
}
```

### Decoupled Schedule Sets (`.chain_weak()`, `PARALLEL`, Hot-Rewire)
Instead of binding a system to a schedule immediately, call `.register_handle()` on `SystemBuilder` to receive a `BamSystem` handle, configure a `BamSystemSet` with `ScheduleSetBuilder` (`sequence()`, `sequence_weak()` -> Bevy `.chain_weak()`, `parallel()`, `before_weak()`, `after_weak()`), and mount via `app.mount_system(sys_handle, ScheduleId::FIXED_UPDATE, set_handle)`.

### Parallel Chunks, Ancestor Cascades, Sparse Columns & Change Tracking
- **Parallel Chunks (`par_chunks` & `par_chunks_readonly`):** Call `ctx.par_chunks(|mut chunk| { ... })` for mutable chunk parallelism or `ctx.par_chunks_readonly(|chunk| { ... })` / `TypedChunkIter::par_for_each_chunk_readonly` (statically guarded by `Q::IS_READ_ONLY`, zero per-worker command-arena allocations) across Bevy's `ComputeTaskPool`.
- **Non-Destructive Count & Ancestor Cascades:** Call `iter.count()` to count matched entities without advancing chunks, and `iter.up_cascade::<Rel, T>(entity, term_idx)` (or `up_cascade_pod`) to walk upward along `ChildOf` or custom relations via the host's lazy `CascadeHierarchyCache`.
- **Sparse-Set Columns:** When a component uses `StorageType::SparseSet`, call `chunk.rows::<T>(term_idx)` or `chunk.rows_mut::<T>(term_idx)` to iterate batched indirect row references without falling back to 1-entity FFI calls.
- **Untracked Mutable Slices:** When inspecting a mutable column and modifying only occasional elements, use `chunk.column_mut_untracked::<T>(term_idx)` and call `chunk.mark_changed_range(term_idx, row, 1)` only on mutated rows so downstream `Changed<T>` systems skip untouched rows.

---

## 4. Commands, RAII Transactions, Relations, Symbols, Migration & Observers

- **Batched Spawns & RAII `GuestTransaction`:** Resolve `BamComponent` handles once in `GuestPlugin::build` (`app.resolve_component::<Position>()?` or `app.resolve_component_by_uuid(&uuid)?`), or register private mod-owned components via `app.register_component::<MyState>()` / `app.register_component_with_layout(...)`. Wrap multi-step command mutations in `cmds.transaction(|tx| { ... })` (or `let tx = cmds.begin_transaction()?; ... tx.commit()?;`) — committing on `Ok`, or automatically rolling back via `cmd_rollback` on `Err`, uncommitted `Drop`, or unwind!
- **Typed Entity Relationships (`RelationDescBuilder`):** Register edges with `app.register_relation(&RelationDescBuilder::new(c"my_mod::Targets", uuid).exclusive().acyclic().cascade_delete())` and queue `cmds.insert_relation(src, rel, dst)` / `insert_relation_with_payload(...)` / `remove_relation(src, rel, dst)`.
- **Interned Symbols, Persistent Named Objects, Diagnostics & Migration:**
  - `app.intern_symbol("faction.red")` returns an $O(1)$-comparable, reload-stable `BamSymbol`.
  - `app.bind_named_object("world.director", entity)` binds a canonical name and strips `ModScoped` so the entity survives reload/unload.
  - `guest_log!(ctx, info, "tick {}", n)` / `app.log_diagnostic(...)` emits structured host `tracing` diagnostics without clobbering `last_error`; `app.report_fault(...)` reports structured faults into `ModSystemFaulted` and Bevy's `FallbackErrorHandler`.
  - `app.migrate_pod_component_column::<Old, New>(handle, old_fp, &new_desc, |old| New::from(old))` upgrades live rows across schema versions with zero per-entity heap allocations.
- **Buffered `Messages<M>` & Dynamic Observers:** Declare `.message_read::<M>()` / `.message_write::<M>()` on `SystemBuilder` and drain/emit batches via `ctx.read_messages` / `ctx.send_messages`. Emit immediate triggers via `cmds.trigger(&ev)` / `cmds.trigger_entity(entity, &ev)` and subscribe via `app.observe` / `app.observe_entity` / `ObserverBuilder` (which also supports composable `.run_if(...)` / `.run_if_fn(...)` predicates).

```rust
let cmds = ctx.commands();
let Ok(entity) = cmds.transaction(|tx| {
    let e = tx.spawn(&[
        SpawnEntry::new(pos_h, &Position { x: 0.0, y: 5.0, z: 0.0 }),
        SpawnEntry::new(vel_h, &Velocity { x: 1.0, y: 0.0, z: 0.0 }),
    ])?;
    Ok(e)
}) else {
    return FfiResult::INTERNAL_ERROR;
};
```

---

## 5. Panic Policy: `panic = "unwind"` vs. `panic = "abort"`

- **`panic = "unwind"` (Default & Recommended):** `bevy_abi_mod_guest_sys` catches panics inside the `.cdylib` before reaching `extern "C"`, drops the panic payload on the guest allocator, discards uncommitted commands from that tick, and returns `FfiResult::PANIC_CAUGHT` so the host can quarantine the system cleanly.
- **`panic = "abort"`:** `catch_unwind` is a no-op; any guest panic immediately terminates the OS process via `abort()` with zero UB.
