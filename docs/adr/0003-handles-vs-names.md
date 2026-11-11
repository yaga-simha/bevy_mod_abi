# ADR 0003: Pre-Resolved `BamComponent` Handles, Declaration-Indexed Resources, and Arena-Backed Batched Commands

- **Status:** Accepted
- **Date:** 2026-11-11
- **Tracking Items:** `C-05`, `P-05`, `L-07` (cross-references: `S-04`, `C-10`, `C-13`)
- **Target Crates:** `bevy_abi_mod_sys`, `bevy_abi_mod_host_sys` (`src/registry/`, `src/commands.rs`, `src/ffi/`), `bevy_abi_mod_guest_sys`, `bevy_abi_mod_guest`

---

## 1. Context & Problem Statement

`spec.md` §5 specifies a **zero-allocation hot path** across the host/guest boundary. While dense chunk column iteration (`iter_next_chunk` + `iter_column_*`) met that goal in v0.1, three other per-frame paths violated it severely:

### 1.1 Per-Call String Validation & SipHash Lookups (`C-05`)
In v0.1, both deferred commands and singleton resource accessors took `StrView` component/resource names on **every invocation**:
- `host_cmd_insert_component(queue, entity, component_name: StrView, data_ptr, data_len)`
- `host_cmd_insert_default(queue, entity, component_name: StrView)`
- `host_cmd_remove_component(queue, entity, component_name: StrView)`
- `host_iter_resource_readonly(iter, resource_name: StrView)`
- `host_iter_resource_mutable(iter, resource_name: StrView)`

Every call ran `core::str::from_utf8` over the entire name string followed by a `HashMap<String, RegisteredComponent>` SipHash-1-3 lookup (and linear verification against the system's declared resource list). Inserting 3 components across 10,000 spawned projectiles performed **30,000 UTF-8 scans + 30,000 `HashMap` lookups per frame**.

### 1.2 Per-Command Heap Allocations & `K` Archetype Moves (`P-05`, `S-04`)
In v0.1 `commands.rs`:
1. Every `cmd_insert_component` allocated a fresh heap buffer (`AlignedBuffer` via `std::alloc::alloc` or `Vec<u8>`) for that single component value, **and** pushed a boxed trait-object command (`CommandQueue::push`) onto Bevy's `CommandQueue`.
2. Worse, copying the payload first called `core::slice::from_raw_parts(data_ptr, data_len)` (`&[u8]`), which is **immediate Undefined Behavior in Rust (`S-04`)** if a C struct contains any uninitialized padding byte.
3. Because v0.1 provided only `cmd_spawn_empty` + single-component `cmd_insert_component`, spawning an entity with `K` components triggered **`K` sequential archetype graph transitions and table-row moves** during `apply_deferred` (`() -> (A,) -> (A, B) -> ... -> (A, B, ..., K)`).

### 1.3 Duplicated Registry Clones Across Systems (`L-07`)
In v0.1, `ModInitContext`, `ModHost`, and every `DynamicCSystem::initialize` performed deep clones of `ModRegistry` (`HashMap<String, RegisteredComponent>` + `HashMap<String, usize>`), copying thousands of heap-allocated `String` keys per system.

---

## 2. Decision

### 2.1 1-Based `BamComponent(u32)` Handles & CoW `RegistrySnapshot` (`C-05`, `L-07`)

#### `BamComponent` Wire Handle
Components and resources share Bevy's unified `ComponentId` space and are represented on the ABI wire by a 1-based 32-bit integer handle:

```rust
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct BamComponent(pub u32);

impl BamComponent {
    pub const INVALID: Self = Self(0); // C: BAM_COMPONENT_INVALID
}
```

- `0` (`BAM_COMPONENT_INVALID`) is reserved as the null/uninitialized sentinel so zero-initialized C globals (`static BamComponent g_pos;`) fail fast with `FfiResult::INVALID_COMPONENT` (`1`) if used before `init`.
- Valid handles are monotonically assigned per `World` starting at `1`, mapping directly to `0`-based index `(handle.0 as usize) - 1` inside `RegistrySnapshot::entries`.

#### Single Source of Truth: Copy-on-Write `ModRegistry` (`L-07`)

```rust
use std::collections::HashMap;
use std::sync::Arc;
use bevy_ecs::component::ComponentId;
use bevy_ecs::resource::Resource;
use bevy_abi_mod_sys::{BamComponent, ComponentUuid};

pub struct RegisteredComponent {
    pub handle: BamComponent,              // 1-based index (`entries[handle.0 as usize - 1]`)
    pub canonical_name: Arc<str>,          // Zero-copy shared UTF-8 name ("namespace::Name")
    pub uuid: ComponentUuid,
    pub component_id: ComponentId,
    pub size: usize,
    pub align: usize,
    pub storage_flags: u32,                // BAM_STORAGE_FLAG_{SPARSE_SET, IMMUTABLE, RESOURCE}
    pub is_abi_pod: bool,
    pub raw_ops: ComponentRawOps,          // insert_raw, insert_default, remove_fn, drop_slot
}

pub struct RegistrySnapshot {
    /// Dense 0-indexed table: `entries[(handle.0 - 1) as usize]`.
    pub entries: Vec<RegisteredComponent>,
    /// Init-time lookup indices (never touched on per-frame hot paths).
    pub by_name: HashMap<Arc<str>, BamComponent>,
    pub by_uuid: HashMap<ComponentUuid, BamComponent>,
    /// Custom game symbols + component VTables (`"bam.v0.<name>"`).
    /// VTable entries carry `mod_id: Option<u64>` so unload (`S-05`) removes/nulls them cleanly.
    pub symbols: HashMap<Arc<str>, SymbolEntry>,
}

#[derive(Resource)]
pub struct ModRegistry {
    snapshot: Arc<RegistrySnapshot>,
    generation: u64,
}
```

1. **CoW Mutation Protocol (`L-07`):**
   - Registering a host component (`app.register_mod_component::<T>()`), committing a `LoadTransaction` (`ADR 0001`), or unloading a mod (`S-05`) calls `Arc::make_mut(&mut self.snapshot)` (cloning the `RegistrySnapshot` metadata once per load/unload event, sharing all `Arc<str>` names) and increments `self.generation += 1`.
   - Each `GuestSystem` caches `(cached_generation: u64, cached_snapshot: Arc<RegistrySnapshot>)`. At the start of `GuestSystem::run_unsafe` / `initialize`, it compares `world_registry.generation != self.cached_generation` (`1` integer load) and clones only the `Arc` pointer when a load/unload actually occurred.
2. **Init-Time Resolution (`resolve_component`) vs. Hot-Path Lookup (`O(1)` Slice Index):**
   - During `bevy_abi_mod_init`, guests call `api.resolve_component(host_ctx, name_cstr, optional_uuid_ptr, &mut handle)` (or `api.register_component(load_ctx, &desc, &mut handle)`).
   - When both `name != NULL` and `uuid != NULL` are passed (the default for Rust `GuestApp::resolve_component::<T: GuestComponent>()`), the host looks up `name` in `by_name` and **verifies `entry.uuid == *uuid`** at load time, returning `FfiResult::TYPE_MISMATCH` (`15`) immediately if the guest's `T` layout disagrees with the host's `T` (`C-10`).
   - On every hot-path call (`cmd_insert`, `cmd_spawn`, `cmd_insert_default`, `cmd_remove`), handle lookup is a single bounds-checked slice index:

```rust
#[inline(always)]
pub(crate) fn get_by_handle(&self, handle: BamComponent) -> Result<&RegisteredComponent, FfiResult> {
    let idx = (handle.0 as usize).checked_sub(1).ok_or(FfiResult::INVALID_COMPONENT)?;
    self.entries.get(idx).ok_or(FfiResult::INVALID_COMPONENT)
}
```

- **Guest SDK Ergonomics:** Name-based convenience helpers (`app.component("my_game::Health")` or cached `Local` handle map) exist **solely** in `bevy_abi_mod_guest`, caching the `BamComponent` at system build/init time so zero string lookups ever cross the FFI on `Update`.

---

### 2.2 Query-Declaration-Indexed Resource Access (`C-05`)

Resources are declared inside `BamQueryDesc::terms` using `AccessMode::RESOURCE_READ` (`8`) or `AccessMode::RESOURCE_WRITE` (`9`), alongside or independent of entity terms:

```c
// Init-time query declaration (C99):
BamTermDesc terms[3] = {
    { .component = pos_comp,    .mode = BAM_ACCESS_WRITE,         .flags = 0 }, // term_index = 0
    { .component = vel_comp,    .mode = BAM_ACCESS_READ,          .flags = 0 }, // term_index = 1
    { .component = time_res,    .mode = BAM_ACCESS_RESOURCE_READ, .flags = 0 }, // term_index = 2
};
```

At runtime, the guest accesses the singleton resource in **`O(1)`** via its declaration index (`term_index = 2`):

```c
const GameTime* time = (const GameTime*)api->res_ptr(iter, 2);
GameScore* score     = (GameScore*)api->res_mut_ptr(iter, 3);
```

#### Host Validation & Concurrency Invariants (`HostIter`)
1. **Scheduler Registration (`Rule 3`):** During `GuestSystem::initialize`, every term with `RESOURCE_READ` calls `filtered_access.add_resource_read(component_id)` (and `component_access_set.add_unfiltered_resource_read`), and every `RESOURCE_WRITE` calls `add_resource_write` (rejecting intra-query / cross-query write aliases with `FfiResult::ACCESS_CONFLICT`, `S-07`).
2. **Pre-Resolved Pointers per Invocation:** When `GuestSystem::run_unsafe` prepares `HostIter` for a query, each resource term's pointer (`UnsafeWorldCell::get_resource_by_id` / `get_resource_mut_by_id`) is resolved by `ComponentId` (`usize` array index inside Bevy's `Storages::resources`) — **zero strings, zero hashing**.
3. **Strict Mode Checking:**
   - `res_ptr(iter, term_index)` returns `*const u8` if `term_index < term_count` and `mode` is `RESOURCE_READ` or `RESOURCE_WRITE` (and the resource exists in the `World`); otherwise returns `core::ptr::null()`.
   - `res_mut_ptr(iter, term_index)` returns `*mut u8` **only** if `mode == RESOURCE_WRITE`, marking the resource's Bevy change tick (`Tick`) on first mutable fetch in that run; calling `res_mut_ptr` on a `RESOURCE_READ` or entity column term returns `core::ptr::null()`.
4. **Resource-Only Queries:** If **every** term in `BamQueryDesc` has `mode` in `{RESOURCE_READ, RESOURCE_WRITE}`, `HostIter` represents a singleton resource bundle: `res_ptr` / `res_mut_ptr` are immediately valid upon `sys_query(sys_ctx, query_idx, &mut iter)` without calling `iter_next_chunk` (`iter_next_chunk` returns `FfiBool::FALSE`).
5. **Type-Confusion Introspection (`C-13`, `S-03`):** `iter_column_info(iter, term_index, &mut info)` works uniformly for both entity component terms and resource terms (setting `BAM_STORAGE_FLAG_RESOURCE` in `info.storage_flags`), allowing `bevy_abi_mod_guest` to assert `info.uuid == R::UUID` (`R: AbiPod + GuestComponent`) before casting `*const u8` to `&R`.

---

### 2.3 Per-System Bump Arena & Batched `cmd_spawn` (`P-05`, `S-04`)

#### 1. Per-System Reusable `CommandArena` & Flat Command Tape

Each `GuestSystem` owns a persistent `HostCommands` instance across frames (no global allocator contention between parallel systems!):

```rust
use core::mem::MaybeUninit;

/// Contiguous bump-allocated byte chunks reused across frames (`cursor` resets to 0 after `apply_deferred`).
pub(crate) struct CommandArena {
    chunks: Vec<ArenaChunk>, // 64 KiB default initial slab; grows geometrically if needed, retains max high-water slab
    current_chunk: usize,
    offset: usize,
}

impl CommandArena {
    /// Allocates `size` bytes aligned to `align` (`align` power of two) and copies `size` raw bytes
    /// directly from `src` into `MaybeUninit<u8>` storage WITHOUT forming `&[u8]` (`S-04`).
    ///
    /// # Safety
    /// If `size > 0`, `src` must be valid for reading `size` bytes (which MAY include uninitialized
    /// C/Rust struct padding bytes) and must not overlap the arena slab.
    pub unsafe fn alloc_copy_raw(&mut self, src: *const u8, size: usize, align: usize) -> ArenaSlice {
        let dst: *mut MaybeUninit<u8> = self.alloc_aligned(size, align);
        if size > 0 {
            // SAFETY: `MaybeUninit<u8>` permits copying arbitrary initialized + padding bytes
            // without violating Rust slice validity invariants (`S-04`).
            unsafe {
                core::ptr::copy_nonoverlapping(src.cast::<MaybeUninit<u8>>(), dst, size);
            }
        }
        ArenaSlice { ptr: dst.cast::<u8>(), size, align }
    }
}

pub(crate) enum RecordedCommand {
    SpawnBatch {
        entity: bevy_ecs::entity::Entity,
        /// Slice into `HostCommands::batch_items: Vec<(BamComponent, ArenaSlice)>`
        items_start: u32,
        items_len: u16,
    },
    Despawn {
        entity: bevy_ecs::entity::Entity,
    },
    Insert {
        entity: bevy_ecs::entity::Entity,
        component: BamComponent,
        payload: ArenaSlice,
    },
    InsertDefault {
        entity: bevy_ecs::entity::Entity,
        component: BamComponent,
    },
    Remove {
        entity: bevy_ecs::entity::Entity,
        component: BamComponent,
    },
}
```

- **Zero steady-state heap allocations:** After frame 1 warms `CommandArena`, `commands: Vec<RecordedCommand>`, `batch_items: Vec<...>`, and scratch `Vec<ComponentId>` / `Vec<OwningPtr<'_>>`, subsequent frames record and apply 10,000+ spawns/inserts with **zero `malloc`/`free` calls** and **zero `Box<dyn Command>` closures**.
- **Rollback / Panic Cleanup:** If a guest system faults (`FfiResult != OK` or `PANIC_CAUGHT`) and `ModErrorPolicy` discards uncommitted commands (or if `World` drops before `apply_deferred`), `HostCommands::discard()` drops any guest-constructed payloads that had a `drop_fn` before resetting `arena.reset()`.

#### 2. Batched `cmd_spawn` (`1` Archetype Placement Instead of `K` Moves)

```c
BamResult cmd_spawn(
    BamCommands*        commands,
    const BamComponent* components, /* length `count` (NULL allowed iff count == 0) */
    const uint8_t* const* data_ptrs,/* length `count` (each element points to component bytes; ZST may be non-null/dangling) */
    size_t              count,
    BamEntity*          out_entity
);
```

##### Execution Steps during `GuestSystem::run_unsafe`:
1. Validate `commands != NULL` and `out_entity != NULL` (`NULL_POINTER`). If `count > 0`, validate `components != NULL && data_ptrs != NULL` (`NULL_POINTER`) and `count <= u16::MAX as usize` (`CAPACITY_EXCEEDED`).
2. For each `i in 0..count`:
   - Resolve `reg = registry.get_by_handle(components[i])?` (`INVALID_COMPONENT`).
   - Verify `reg.is_abi_pod` (or dynamic guest component with raw layout) and `(reg.storage_flags & BAM_STORAGE_FLAG_RESOURCE) == 0` (`PERMISSION_DENIED` / `INVALID_COMPONENT`).
   - Verify no duplicate `ComponentId` appears in `components[0..count]` (`INVALID_ARGUMENT`, via stack bitmask / small sorted check).
   - If `reg.size > 0`, verify `!data_ptrs[i].is_null()` (`NULL_POINTER`). *(Note: `data_ptrs[i]` from C stack buffers need not be over-aligned if packed, because `CommandArena::alloc_copy_raw` copies bytes via `u8`-aligned `copy_nonoverlapping` into an `reg.align`-aligned arena destination before constructing `OwningPtr`!)*
3. Reserve a fresh `Entity` atomically via `unsafe_world_cell.entities().reserve_entity()` (thread-safe lock-free atomic bump in `bevy_ecs::entity::Entities` — no `&mut World` required during parallel system execution!).
4. Copy all `count` payloads into `CommandArena`, push `RecordedCommand::SpawnBatch`, write `FfiEntity::from_bits(entity.to_bits())` to `*out_entity`, and return `FfiResult::OK`.

##### Execution Steps during `GuestSystem::apply_deferred(world: &mut World)`:
1. Flush `world.flush()` so reserved entities are materialized.
2. For each `RecordedCommand::SpawnBatch`:
   - Materialize `&[ComponentId]` and `[OwningPtr<'_>]` into pre-allocated scratch `Vec`s on `HostCommands`.
   - Increment `DROP_META[slot].live_values` for any dynamic drop-bearing component (`ADR 0001`).
   - Call `world.entity_mut(entity).insert_by_ids(&scratch_ids, scratch_ptrs.drain(..))` — Bevy computes the final target `Bundle` / `Archetype` once and writes all `K` columns directly into the destination `Table` row with **zero intermediate archetype moves**!
3. Reset `arena.reset()`, `commands.clear()`, and `batch_items.clear()` (preserving capacity).

---

## 3. Performance Impact Comparison

| Operation (10,000 entities × 3 components) | v0.1 (`StrView` + per-call alloc) | v0.2 (`BamComponent` + Arena + `cmd_spawn`) |
|---|---|---|
| UTF-8 validations per frame | 30,000 | **0** |
| `HashMap` SipHash lookups per frame | 30,000 | **0** (`3` array bounds checks per spawn) |
| Heap allocations (`malloc`/`free`) per frame | 60,000 (`30k` buffers + `30k` commands) | **0** (steady-state `CommandArena` reuse) |
| Bevy archetype table moves per frame | 30,000 (`() -> A -> AB -> ABC`) | **10,000** (`1` direct `insert_by_ids` placement per entity) |
| Uninitialized struct padding safety (`S-04`) | UB (`slice::from_raw_parts::<u8>`) | **Sound** (`MaybeUninit<u8>` raw `copy_nonoverlapping`) |

---

## 4. Alternatives Considered

| Alternative | Why Rejected |
|---|---|
| **Pass `ComponentUuid` (16 bytes) Directly on Every Hot-Path Command** | Still requires a 128-bit `HashMap` probe on every `cmd_insert` instead of a 1-instruction `(handle.0 - 1)` dense slice index, and takes 4× the cache footprint in `BamTermDesc` and `cmd_spawn` arrays. |
| **Expose Bevy's Internal `ComponentId` Directly as `BamComponent`** | Coupling wire values directly to `ComponentId::index()` exposes Bevy-internal component IDs that were never opted into `ModRegistry` (security/encapsulation violation: a guest forging `BamComponent(3)` could overwrite internal Bevy `Resource`s!). Mapping `BamComponent` strictly through `RegistrySnapshot::entries` ensures guests can **only** touch host-exposed or mod-registered components. |
| **Global Shared Command Arena** | Would require mutex synchronization across parallel guest systems. A per-`GuestSystem` `CommandArena` requires zero synchronization because Bevy's scheduler never runs the same system instance concurrently with itself. |

---

## 5. Consequences & Verification Criteria

1. **Encapsulation invariant:** Forging an out-of-bounds or zero `BamComponent` returns `FfiResult::INVALID_COMPONENT` and can never address unexposed Bevy components/resources.
2. **Benchmark gates (`C-05`, `P-05`, `L-07`):**
   - Criterion benchmark `commands/10k_inserts_per_frame` achieves **>= 2×** throughput over v0.1 (`C-05`) and **0 steady-state heap allocations** verified by a counting global allocator (`P-05`).
   - Criterion benchmark `system_init/1k_registered_components` shows `O(1)` `Arc` clone cost on `GuestSystem::initialize` (`L-07`).
3. **Miri verification (`S-04`, `T-02`):** Passing a `#[repr(C)]` struct `(u8, u64)` with 7 uninitialized padding bytes through `cmd_insert` and `cmd_spawn` passes `cargo +nightly miri test` cleanly.
