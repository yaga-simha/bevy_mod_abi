# `bevy_bql` Compiler Lowering Contract (`ABI 0.3.0`)

`ABI 0.3.0` (`0x0003_0000`) provides a frozen, self-describing, zero-serialization target for AOT and JIT ECS compilers (such as **`bevy_bql`** / **`bql_jit`**) as well as hand-written ISO C99, C++17, Zig, and Safe Rust guest modules. All **68/68** `BamHostApi` slots are live on the host (`552` B on 64-bit / `280` B on 32-bit).

For exact struct byte layouts and field-level specifications, see [`spec.md` §4.7](../../../spec.md#47-bevy_bql-compiler-lowering-contract) and [`docs/ARCHITECTURE.md` §3.3](../../ARCHITECTURE.md#33-bevy_bql-lowering-contract-internals).

---

## 1. Exact `BamHostApi` Offsets & `BAM_RESTRICT`

Compilers and JIT emitters never hardcode magic table offsets:

- **Rust (`bevy_abi_mod_sys::offsets`):** Exports all 70 `BamHostApi` fields (`STRUCT_SIZE`, `ABI_VERSION`, plus all 68 function-pointer slots) as:
  - Target-native `<FIELD>` (`core::mem::offset_of!`)
  - Architecture-independent 64-bit `OFF64_<FIELD>` (`SIZE_64 = 552`, slot `k` at `8 + 8 * k`)
  - Architecture-independent 32-bit `OFF32_<FIELD>` (`SIZE_32 = 280`, slot `k` at `8 + 4 * k`)
  - `SLOT_COUNT = 68`
- **C99 / C++17 / Zig (`bevy_abi_mod_sdk_gen`):** Mirrors every slot via `BAM_HOST_API_OFFSET_<SLOT_UPPER>` in `include/bevy_abi_mod.h`, `bam::offsets::*` in `bevy_abi_mod.hpp`, and `offsets.*` / `BAM_HOST_API_OFFSET_*` in `bevy_abi_mod.zig`.
- **`BAM_RESTRICT`:** Expands to `__restrict` under MSVC (`_MSC_VER`), `restrict` under ISO C99/C11 (`!defined(__cplusplus) && __STDC_VERSION__ >= 199901L`), and `"noalias"` in Zig (`BAM_RESTRICT_QUALIFIER`), allowing backends to vectorize loops over disjoint SoA columns returned by `iter_column_readonly` and `iter_column_mutable`.

---

## 2. Identity, Zero-Copy Reload, Column Migration & Declarative Reflection

| Compiler Construct | ABI `0.3.0` Wire Primitive | Host Behavior |
|---|---|---|
| **Component Identity** | `ComponentUuid::from_parts(name, size, align, field_schema_hash)` (`bam_uuid_from_parts`) | Keyed SipHash-2-4-128 (`SIPHASH_KEY_0 = 0x4241_4d5f_5555_4944`, `SIPHASH_KEY_1 = 0x5630_5f32_5349_5048`); collisions or layout drift return `FfiResult::TYPE_MISMATCH`. |
| **Unchanged Reload** | `ComponentDesc::schema_fingerprint` (64-bit FNV-1a) | Dynamic `AbiPod` components with an identical non-zero `schema_fingerprint` retain their Bevy `ComponentId` and `BlobVec`/`SparseSet` storage in place with **`0` bytes copied**. |
| **Schema Upgrade (v1→v2)** | `migrate_component_column` (`BamRowMigrateFn`) | Transforms all live rows (active and `Disabled`) through one aligned contiguous buffer per table/sparse-set (**`0` per-entity heap allocations**); aborts atomically before mutating world/registry state if `migrate_fn` fails. |
| **Reflect / BRP / Save** | `ComponentDesc::fields` (`BamFieldDesc[]`) | `ModReflectPlugin` (`bevy_abi_mod_reflect` + `ffi/validation.rs`) synthesizes `TypeInfo::Struct`, `ReflectFromPtr`, `ReflectComponent`, `ReflectResource`, and BRP schemas directly from byte offsets with zero Rust witness VTables. |

---

## 3. Queries, Cascades, Parallelism, Relations, Symbols & Schedules

| DSL / Language Feature | `BamHostApi` Slot(s) | Exact Contract & Flags |
|---|---|---|
| **Random Entity Lookup** | `sys_entity_get`, `sys_entity_get_mut` | $O(1)$ per-entity lookup bounded by declared `FilteredAccessSet` permissions and query filters; `sys_entity_get_mut` marks Bevy change ticks. |
| **`UP Rel CASCADE`** | `iter_up_cascade` | Walks upward along `ChildOf` (`relation = BamComponent::INVALID`), custom 8B/4B parent components, or `ModRelationGraph` edges via lazy per-`(World, RelationComponentId)` `CascadeHierarchyCache` with Brent's zero-allocation cycle guard. |
| **`PARALLEL READONLY CHUNK`** | `par_for_each_chunk_readonly` | Multi-core `ComputeTaskPool` chunk dispatch with **zero per-worker `HostCommands`/`CommandArena` allocations**; rejects `WRITE`/`OPTIONAL_WRITE`/`RESOURCE_WRITE`/`MESSAGE_WRITE` terms with `FfiResult::PERMISSION_DENIED`. |
| **`RELATION`** | `register_relation`, `cmd_insert_relation`, `cmd_remove_relation` | Backed by `ModRelationGraph` + Bevy `0.20` sparse-set lifecycle hooks; supports `BAM_RELATION_FLAG_EXCLUSIVE`, `SYMMETRIC`, `ACYCLIC` (rejecting cycles with `FfiResult::CYCLE_DETECTED`), `CASCADE_DELETE`/`CASCADE_DESPAWN` (iterative cycle-safe BFS), and `CLEANUP_REMOVE`. |
| **`InternedStr`** | `intern_symbol` | Monotonic, lock-free paged, reload-stable UTF-8 interning in `BamSymbolTable` returning `BamSymbol(u32)` (`INVALID_UTF8` on invalid UTF-8). |
| **`OBJECT`** | `bind_named_object` | Persists a canonical name→entity mapping in `ModNamedObjects` and strips `ModScoped` so named objects survive mod unload and reload. |
| **`ATOMIC` Transactions** | `cmd_checkpoint`, `cmd_rollback` | Snapshots and restores `HostCommands` tape length and `CommandArena` watermarks in LIFO order (running uncommitted destructors); wrapped by RAII `GuestTransaction` in Rust guests. |
| **Decoupled `SCHEDULE`** | `register_system_handle`, `configure_schedule_set`, `mount_system_in_schedule` | Backed by `ModScheduleSetRegistry` and `ModScheduleSetRewirePass` supporting `BAM_SCHEDULE_SET_FLAG_SEQUENCE` (`.chain()`), `SEQUENCE_WEAK` (`.chain_weak()`), `PARALLEL`, `BEFORE_WEAK`, `AFTER_WEAK`, cycle detection (`FfiResult::CYCLE_DETECTED`), and schedule-only hot-rewiring without re-initializing `GuestSystem` kernels. |

---

## 4. End-to-End Differential Wire Parity (`examples/09_bql_wire_parity`)

[`examples/09_bql_wire_parity`](../../../examples/09_bql_wire_parity) (`tests/bql_wire_contract_test.rs`) compiles **both** a strict ISO C99 DSO (`-std=c99 -Wall -Wextra -Wpedantic -Werror`) and a Safe Rust `#![forbid(unsafe_code)]` DSO implementing every contract item above, runs two identical Bevy `0.20` worlds for **500 deterministic `FixedUpdate` frames** with a live **frame-250 v1→v2 `migrate_component_column` reload**, and asserts bit-identical sorted archetype column hashes on every single frame:

```bash
cargo test --test bql_wire_contract_test -- --nocapture
```
