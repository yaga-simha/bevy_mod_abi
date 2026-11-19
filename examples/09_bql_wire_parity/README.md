# Example 9: `bevy_bql` C99 vs. Safe Rust Differential Wire Parity (`09_bql_wire_parity`)

End-to-end differential wire parity suite proving bit-identical ABI `0.3.0` execution between a
strict ISO C99 guest (`c_mod/bql_parity_mod.c`, compiled with `-std=c99 -Wall -Wextra -Wpedantic
-Werror`) and a `#![forbid(unsafe_code)]`, zero-Bevy Safe Rust guest (`rust_mod/src/lib.rs`).

## Covered `bevy_bql` ABI `0.3.0` Wire Contract

- **Compile-time ABI layout guards:** `BAM_HOST_API_OFFSET_*` static checks and zero-padding
  `AbiPod` structs (`SimClock`, `FrameAccumulator`, `NodeStateV1` `12B`, `NodeStateV2` `16B`,
  `RootAura`, `WorldMetric`, `TxToken`, `EdgeWeight`).
- **Schema Identity & Reflection:** Bit-identical SipHash-2-4 `ComponentUuid`, 64-bit FNV-1a
  `SCHEMA_FINGERPRINT`, and declarative `BamFieldDesc[]` reflection descriptors verified live via
  `ModReflectPlugin` (`ReflectFromPtr` and `DynamicStruct`).
- **`OBJECT` (`bind_named_object`):** Persists `"bql_parity::root_config"` across live hot-reload
  and full mod unload (`ModScoped` stripped automatically).
- **`InternedStr` (`intern_symbol`):** Monotonic UTF-8 symbol table surviving live reload with
  identical `BamSymbol` handles across C99 and Rust worlds.
- **`RELATION` (`register_relation`, `cmd_insert_relation`, `cmd_remove_relation`):** Typed
  `bql_parity::OwnedBy` edge carrying an 8-byte `EdgeWeight` payload with
  `BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE`.
- **`UP ChildOf CASCADE` (`iter_up_cascade`):** Multi-hop ancestor lookup computing deterministic
  parent-before-child hierarchical `WorldMetric` values across a 3-level forest (`596` nodes).
- **`PARALLEL CHUNK` & `PARALLEL READONLY CHUNK`:** Work-stealing `par_for_each_chunk`,
  zero-arena `par_for_each_chunk_readonly`, and non-destructive `iter_count`.
- **`ATOMIC` Transactions:** `cmd_checkpoint` + `cmd_rollback` on failing conditions (zero leaked
  entities/edges) vs. commit on passing conditions.
- **Decoupled `SCHEDULE`:** `register_system_handle`, `configure_schedule_set` with
  `BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK` (`.chain_weak()`), inter-set `after_sets`, and
  `mount_system_in_schedule` into `FixedUpdate`.
- **Live Schema Migration (`migrate_component_column`):** Mid-run frame-250 `reload_mod_from`
  migrating all `596` live `NodeState` rows from `12B` (`v1`) to `16B` (`v2`) in place while
  retaining unchanged POD columns zero-copy.

## Running

```bash
# Run the 500-frame differential wire parity integration test
cargo test --test bql_wire_contract_test -- --nocapture

# Run the standalone parity runner binary
cargo run -p example_09_bql_wire_parity
```
