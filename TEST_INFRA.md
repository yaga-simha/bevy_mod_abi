# Test Infrastructure Specification: `bevy_abi_mod`

**Asymmetric, Zero-Serialization C-ABI Modding Framework for Bevy ECS**  
**Integrity Mode:** Development | **Standard:** Dual-Track Project Pattern

---

## 1. Test Philosophy & Requirements Grounding

### 1.1 Opaque-Box & Requirement-Driven Architecture
The test infrastructure for `bevy_abi_mod` enforces **opaque-box testing**. Tests exercise the framework strictly across its defined external interfaces:
1. **The C-ABI Contract Boundary** (`abi_contract`): `extern "C"` functions, primitive types, `#[repr(C)]` structs, and transparent newtypes.
2. **The Bevy Host ECS Runtime** (`host_runtime`): Scheduling, archetype chunk traversal, change detection, and dynamic systems.
3. **The Guest Mod Lifecycle** (`guest_sdk` / native shared libraries): Dynamic symbol resolution, memory isolation, panic containment, and hot-reloading.

Tests do **not** depend on private implementation internals, compiler-private symbols, or fragile memory layout assumptions. Every test case verifies an observable contract guarantee derived from `ORIGINAL_REQUEST.md` and `PROJECT.md`:
- **R1. Bevy Runtime Testbed Environment:** Execute native mods (both C99 and Rust) interacting with ECS entities, components, and systems in representative game scenarios rather than sterile unit tests.
- **R2. Performance Benchmarking Suite:** Quantify chunk iteration throughput, C-ABI call overhead, and scaling across 1k, 10k, and 100k entity distributions.
- **R3. Safety Test Suite:** Verify pointer alignment (1, 4, 8, 16, 64 bytes), chunk iterator bounds safety, FFI type safety, self-aliasing memory protection, Bevy borrow conflict detection, and panic isolation.
- **R4. Idempotency & Lifecycle Test Suite:** Verify repeated registration/unloading cycles (50+ iterations), mod hot-reloading without host restart, zero state leakage, and world consistency.

### 1.2 Expected Output Derivation & Verification Oracles
For every test case, the expected output is derived from explicit authoritative sources:
1. **Mathematical & Layout Invariants:** Component size and alignment matching `core::mem::size_of` and `core::mem::align_of`; stride calculations strictly matching `index * size`.
2. **FFI Soundness Invariants:** Transparent integer representations (`FfiResult`, `ComponentAccessMode`) that never invoke undefined behavior when encountering foreign bit patterns.
3. **ECS State Oracles:** Comparing results against pure native Bevy baseline systems or direct mathematical projections.
4. **Lifecycle Oracles:** Entity IDs, system counts, and memory allocations pre- and post-mod load/unload cycles.

---

## 2. Complete Feature Inventory & Tier Mapping

The 26 features defined in `PROJECT.md § Feature Inventory` are systematically verified across four progressive test tiers:

| # | Feature | Milestone | Tier 1 (Isolation) | Tier 2 (Boundary & Corner) | Tier 3 (Pairwise) | Tier 4 (Scenario) |
|---|---------|-----------|--------------------|----------------------------|-------------------|-------------------|
| 1 | Host Mod Registry (`ModRegistry`) | M1 | T1-F01 (5 tests) | T2-F01 (5 tests) | Pairwise with F2, F18 | Scenario D, E |
| 2 | Dynamic C System (`DynamicCSystem`) | M1 | T1-F02 (5 tests) | T2-F02 (5 tests) | Pairwise with F3, F18, F20 | Scenario A, B, D |
| 3 | Raw Chunk Iterator (`RawChunkIter`) | M1 | T1-F03 (5 tests) | T2-F03 (5 tests) | Pairwise with F14, F15, F19 | Scenario A, B, C |
| 4 | Deferred Command Bridge (`CommandQueueBridge`) | M1 | T1-F04 (5 tests) | T2-F04 (5 tests) | Pairwise with F3, F26 | Scenario B, C |
| 5 | 3D Kinematics Scenario | M2, M3 | T1-F05 (5 tests) | T2-F05 (5 tests) | Pairwise with F9, F11 | Scenario A |
| 6 | Combat & Health Damage Scenario | M2, M3 | T1-F06 (5 tests) | T2-F06 (5 tests) | Pairwise with F4, F18 | Scenario B |
| 7 | Dynamic Metadata Scenario | M2, M3 | T1-F07 (5 tests) | T2-F07 (5 tests) | Pairwise with F17, F21 | Scenario C |
| 8 | Dynamic Buffer Scenario | M2, M3 | T1-F08 (5 tests) | T2-F08 (5 tests) | Pairwise with F17, F21 | Scenario C |
| 9 | Chunk Iteration Throughput Benchmark | M4 | T1-F09 (5 tests) | T2-F09 (5 tests) | Pairwise with F11, F12 | Scenario A |
| 10 | C-ABI Boundary Overhead Benchmark | M4 | T1-F10 (5 tests) | T2-F10 (5 tests) | Pairwise with F2, F9 | Scenario A |
| 11 | Multi-Entity Scale Scaling | M4 | T1-F11 (5 tests) | T2-F11 (5 tests) | Pairwise with F3, F12 | Scenario A, D |
| 12 | Archetype Fragmentation Benchmark | M4 | T1-F12 (5 tests) | T2-F12 (5 tests) | Pairwise with F3, F9 | Scenario A, D |
| 13 | Baseline Recording & Regression Tool | M4 | T1-F13 (5 tests) | T2-F13 (5 tests) | Pairwise with F9, F10 | Scenario A |
| 14 | Component Alignment Verification | M5 | T1-F14 (5 tests) | T2-F14 (5 tests) | Pairwise with F3, F15 | Scenario A, C |
| 15 | Chunk Iterator Bounds & Empty Table Guard | M5 | T1-F15 (5 tests) | T2-F15 (5 tests) | Pairwise with F3, F19 | Scenario A, B |
| 16 | FFI Transparent Type Safety | M5 | T1-F16 (5 tests) | T2-F16 (5 tests) | Pairwise with F1, F2 | Scenario D |
| 17 | Self-Aliasing String/Vec Overlap Guard | M5 | T1-F17 (5 tests) | T2-F17 (5 tests) | Pairwise with F7, F8, F21 | Scenario C |
| 18 | Bevy ECS Borrow Rule Enforcement | M5 | T1-F18 (5 tests) | T2-F18 (5 tests) | Pairwise with F1, F2 | Scenario D |
| 19 | Column Permission Enforcement | M5 | T1-F19 (5 tests) | T2-F19 (5 tests) | Pairwise with F3, F18 | Scenario B, D |
| 20 | Cross-Runtime Panic Containment | M5 | T1-F20 (5 tests) | T2-F20 (5 tests) | Pairwise with F2, F26 | Scenario D, E |
| 21 | Memory Allocator Boundary Isolation | M5 | T1-F21 (5 tests) | T2-F21 (5 tests) | Pairwise with F7, F8, F17 | Scenario C, E |
| 22 | Mod Unloading & Teardown Hook | M5 | T1-F22 (5 tests) | T2-F22 (5 tests) | Pairwise with F23, F24 | Scenario E |
| 23 | Mod Hot-Reloading | M5 | T1-F23 (5 tests) | T2-F23 (5 tests) | Pairwise with F22, F24, F26 | Scenario E |
| 24 | Repeated Registration Stress Cycle | M5 | T1-F24 (5 tests) | T2-F24 (5 tests) | Pairwise with F22, F23, F25 | Scenario E |
| 25 | Hot-Path Zero-Allocation Verification | M5 | T1-F25 (5 tests) | T2-F25 (5 tests) | Pairwise with F2, F3, F9 | Scenario A |
| 26 | ECS World Consistency After Failure | M5 | T1-F26 (5 tests) | T2-F26 (5 tests) | Pairwise with F20, F23 | Scenario D, E |

---

## 3. Tier Coverage & Test Case Specifications

### 3.1 Tier 1: Feature Isolation (Happy-Path Isolation, ≥5 Tests Per Feature)
Tier 1 verifies that each feature operates correctly in total isolation under normal conditions. 
**Threshold:** 26 features × 5 tests = **130 tests**.

- **Feature 1: ModRegistry**
  - `T1-F01-01`: Register component by UUID; verify lookup returns matching ComponentId.
  - `T1-F01-02`: Register component by C-string name; verify resolution returns static VTable address.
  - `T1-F01-03`: Register accessor symbols; verify `get_proc_address` retrieves function pointer.
  - `T1-F01-04`: Query unregistered component; verify returns null pointer safely.
  - `T1-F01-05`: Multiple component registration; verify entries do not alias or overwrite.
- **Feature 2: DynamicCSystem**
  - `T1-F02-01`: Instantiate DynamicCSystem with 1 READ query; verify `System::name` returns descriptor name.
  - `T1-F02-02`: Initialize DynamicCSystem on World; verify component access set contains READ lock.
  - `T1-F02-03`: Initialize DynamicCSystem with WRITE query; verify access set contains WRITE lock.
  - `T1-F02-04`: Execute system callback with valid user_data pointer; verify callback receives pointer.
  - `T1-F02-05`: Execute system returning `FfiResult::OK`; verify system status remains active.
- **Feature 3: RawChunkIter**
  - `T1-F03-01`: Advance iterator on populated table; verify `iter_next_chunk` returns entity count.
  - `T1-F03-02`: Retrieve read-only column; verify non-null pointer matching entity data.
  - `T1-F03-03`: Retrieve mutable column; verify non-null pointer matching entity data.
  - `T1-F03-04`: Retrieve entity column; verify `iter_entities` returns slice of `FfiEntity`.
  - `T1-F03-05`: Query column stride; verify returned stride equals `size_of::<T>()`.
- **Feature 4: CommandQueueBridge**
  - `T1-F04-01`: Enqueue entity spawn command; verify entity is created upon queue flush.
  - `T1-F04-02`: Enqueue entity despawn command; verify entity is removed upon queue flush.
  - `T1-F04-03`: Enqueue component insertion; verify component is attached to entity upon flush.
  - `T1-F04-04`: Enqueue component removal; verify component is detached upon flush.
  - `T1-F04-05`: Enqueue sequential spawn and insert; verify entity has inserted component.
- **Feature 5: 3D Kinematics Scenario**
  - `T1-F05-01`: Evaluate Position + Velocity integration step in Rust mod; verify Position updated.
  - `T1-F05-02`: Evaluate Position + Velocity integration step in C mod; verify Position updated.
  - `T1-F05-03`: Evaluate kinematics on 100 entities; verify all entities updated according to formula.
  - `T1-F05-04`: Stride traversal across Position array; verify non-aliasing element updates.
  - `T1-F05-05`: Stride traversal across Velocity array; verify correct vector addition.
- **Feature 6: Combat & Health Damage Scenario**
  - `T1-F06-01`: Mod applies damage to Health; verify Health decreases by exact damage amount.
  - `T1-F06-02`: Mod applies damage; verify Bevy change detection marks `Changed<Health>`.
  - `T1-F06-03`: Unmodified entity Health; verify `Changed<Health>` is false.
  - `T1-F06-04`: Health reduction to 0; verify entity flagged for death.
  - `T1-F06-05`: Health clamps or negative value handling per game rules.
- **Feature 7: Dynamic Metadata Scenario**
  - `T1-F07-01`: Read NameTag string via `FfiSlice<u8>`; verify string contents match.
  - `T1-F07-02`: Write new NameTag string via setter; verify string updated on host.
  - `T1-F07-03`: Update NameTag with shorter string; verify host string truncated.
  - `T1-F07-04`: Update NameTag with longer string; verify host string grows and reallocates.
  - `T1-F07-05`: Read NameTag after multiple writes; verify persistence across frames.
- **Feature 8: Dynamic Buffer Scenario**
  - `T1-F08-01`: Read `Vec<u32>` buffer slice; verify elements match host buffer.
  - `T1-F08-02`: Append element via setter; verify host buffer length increments.
  - `T1-F08-03`: Replace buffer contents via setter; verify host buffer updated.
  - `T1-F08-04`: Clear buffer via setter; verify buffer length becomes 0.
  - `T1-F08-05`: Mutate buffer across multiple iterations; verify consistency.
- **Feature 9: Chunk Iteration Throughput Benchmark**
  - `T1-F09-01`: Benchmark 1,000 entities iteration; verify throughput reported in ns/entity.
  - `T1-F09-02`: Benchmark 10,000 entities iteration; verify linear time scaling.
  - `T1-F09-03`: Compare read-only iteration vs mutable iteration throughput.
  - `T1-F09-04`: Verify throughput exceeds 10,000,000 entities/sec threshold on modern CPU.
  - `T1-F09-05`: Verify timer fidelity and statistical run consistency across 10 sample runs.
- **Feature 10: C-ABI Boundary Overhead Benchmark**
  - `T1-F10-01`: Measure invocation overhead of empty C-ABI callback; verify < 10 ns per call.
  - `T1-F10-02`: Compare C-ABI DynamicCSystem against native Rust system; verify overhead < 15%.
  - `T1-F10-03`: Measure symbol lookup latency via `get_proc_address`; verify < 5 ns cached.
  - `T1-F10-04`: Measure VTable function pointer direct call overhead; verify negligible vs direct call.
  - `T1-F10-05`: Measure command queue bridge enqueue overhead.
- **Feature 11: Multi-Entity Scale Scaling**
  - `T1-F11-01`: Scale test at 1,000 entities; verify memory and execution baseline.
  - `T1-F11-02`: Scale test at 10,000 entities; verify memory proportionality.
  - `T1-F11-03`: Scale test at 100,000 entities; verify zero OOM and throughput stability.
  - `T1-F11-04`: Verify chunk chunking does not degrade at large table counts.
  - `T1-F11-05`: Entity index bounds check across 100k entities.
- **Feature 12: Archetype Fragmentation Benchmark**
  - `T1-F12-01`: Execute query against single monolithic archetype table; record throughput.
  - `T1-F12-02`: Execute query against 4 fragmented archetype tables; verify correct total entity sum.
  - `T1-F12-03`: Execute query against 16 fragmented archetype tables; verify traversal completeness.
  - `T1-F12-04`: Measure chunk switch overhead between fragmented tables.
  - `T1-F12-05`: Verify empty tables interspersed among fragmented archetypes are skipped efficiently.
- **Feature 13: Baseline Recording & Regression Tool**
  - `T1-F13-01`: Execute programmatic benchmark; verify JSON metrics file is created.
  - `T1-F13-02`: Parse emitted JSON output; verify required metric schema fields exist.
  - `T1-F13-03`: Compare run against baseline JSON; verify regression check executes without error.
  - `T1-F13-04`: Verify throughput difference calculation is mathematically accurate.
  - `T1-F13-05`: Verify CLI exit code is 0 when performance meets regression threshold.
- **Feature 14: Component Alignment Verification**
  - `T1-F14-01`: Test 1-byte aligned component (`u8`); verify pointer address `ptr % 1 == 0`.
  - `T1-F14-02`: Test 4-byte aligned component (`u32`, `f32`); verify pointer address `ptr % 4 == 0`.
  - `T1-F14-03`: Test 8-byte aligned component (`u64`, `f64`); verify pointer address `ptr % 8 == 0`.
  - `T1-F14-04`: Test 16-byte aligned component (`Vec4`, `Mat2`); verify pointer address `ptr % 16 == 0`.
  - `T1-F14-05`: Test 64-byte aligned component (`CacheAligned`); verify pointer address `ptr % 64 == 0`.
- **Feature 15: Chunk Iterator Bounds & Empty Table Guard**
  - `T1-F15-01`: Query table with 0 entities; verify `iter_next_chunk` returns 0 immediately.
  - `T1-F15-02`: Request column on empty table; verify does not dereference invalid memory.
  - `T1-F15-03`: Iterate until exhaustion; verify subsequent `iter_next_chunk` calls safely return 0.
  - `T1-F15-04`: Query single-entity chunk; verify loop executes exactly once.
  - `T1-F15-05`: Query partial chunk; verify entity count matches table length.
- **Feature 16: FFI Transparent Type Safety**
  - `T1-F16-01`: Verify `FfiResult::OK` value is 0 and `is_ok()` returns true.
  - `T1-F16-02`: Verify `ComponentAccessMode::READ` value is 0 and `is_valid()` returns true.
  - `T1-F16-03`: Verify `ComponentAccessMode::WRITE` value is 1 and `is_mutable()` returns true.
  - `T1-F16-04`: Verify `ComponentAccessMode::FILTER_ONLY` value is 2 and `is_filter_only()` returns true.
  - `T1-F16-05`: Verify `FfiEntity` preserves 64-bit entity bits across `from_bits` / `to_bits`.
- **Feature 17: Self-Aliasing String/Vec Overlap Guard**
  - `T1-F17-01`: Set NameTag string from distinct foreign buffer; verify update succeeds.
  - `T1-F17-02`: Set NameTag string passing its own existing slice (`get_slice`); verify no UAF.
  - `T1-F17-03`: Set NameTag string passing a sub-slice of itself; verify correct truncation without UB.
  - `T1-F17-04`: Set `Vec<u32>` buffer passing its own slice; verify contents preserved without corruption.
  - `T1-F17-05`: Set `Vec<u32>` buffer passing overlapping memory region; verify copy occurs safely.
- **Feature 18: Bevy ECS Borrow Rule Enforcement**
  - `T1-F18-01`: Two systems with READ access on same component; verify co-scheduled safely.
  - `T1-F18-02`: Two systems with WRITE access on disjoint components; verify co-scheduled safely.
  - `T1-F18-03`: System with WRITE access and system with READ access on same component; verify scheduler detects conflict.
  - `T1-F18-04`: Two systems with WRITE access on same component; verify scheduler detects conflict.
  - `T1-F18-05`: System with FILTER_ONLY access alongside WRITE system; verify scheduler permits execution.
- **Feature 19: Column Permission Enforcement**
  - `T1-F19-01`: Call `iter_column_readonly` on READ column; verify returns valid pointer.
  - `T1-F19-02`: Call `iter_column_mutable` on WRITE column; verify returns valid mutable pointer.
  - `T1-F19-03`: Call `iter_column_mutable` on READ column; verify returns `null_mut()`.
  - `T1-F19-04`: Call `iter_column_mutable` on FILTER_ONLY column; verify returns `null_mut()`.
  - `T1-F19-05`: Call `iter_column_readonly` on FILTER_ONLY column; verify returns `null()`.
- **Feature 20: Cross-Runtime Panic Containment**
  - `T1-F20-01`: Guest system panics inside `guest_sdk`; verify `catch_guest_panic` returns `FfiResult::PANIC_CAUGHT`.
  - `T1-F20-02`: Host receives `PANIC_CAUGHT`; verify host process does not abort.
  - `T1-F20-03`: Offending panicking system is disabled; verify subsequent schedule ticks skip system.
  - `T1-F20-04`: Healthy systems continue executing after peer system panics.
  - `T1-F20-05`: Host catches unwinding panic from in-process mock callback and returns error status.
- **Feature 21: Memory Allocator Boundary Isolation**
  - `T1-F21-01`: String allocated by host is freed by host when component is dropped.
  - `T1-F21-02`: String buffer created in guest mod is copied into host-allocated String via setter.
  - `T1-F21-03`: Dropping guest mod does not deallocate host ECS components.
  - `T1-F21-04`: Host ECS teardown does not deallocate guest static memory.
  - `T1-F21-05`: Vec buffer allocated by host never exposes direct owned `Vec` pointer to guest.
- **Feature 22: Mod Unloading & Teardown Hook**
  - `T1-F22-01`: Load mod shared library; verify `bevy_abi_mod_init` called.
  - `T1-F22-02`: Unload mod shared library; verify `bevy_abi_mod_shutdown` called.
  - `T1-F22-03`: Drop `ModHost`; verify all loaded mods receive shutdown hook.
  - `T1-F22-04`: Mod shutdown cleans up guest-allocated resources without error.
  - `T1-F22-05`: Shared library handle is cleanly closed via `dlclose` / `FreeLibrary`.
- **Feature 23: Mod Hot-Reloading**
  - `T1-F23-01`: Load mod version 1; execute 1 tick; unload mod; load mod version 2; execute 1 tick.
  - `T1-F23-02`: Hot-reload mod; verify new system callback logic replaces old logic.
  - `T1-F23-03`: Hot-reload mod; verify ECS entity state created by version 1 persists in world.
  - `T1-F23-04`: Hot-reload mod with altered component queries; verify schedule adapts access set.
  - `T1-F23-05`: Verify no memory leak across hot-reload cycle.
- **Feature 24: Repeated Registration Stress Cycle**
  - `T1-F24-01`: Register and unregister mod systems 10 consecutive cycles; verify clean state.
  - `T1-F24-02`: Register, execute, and unregister mod systems 50 consecutive cycles; verify stability.
  - `T1-F24-03`: Verify system count in schedule returns to baseline after each cycle.
  - `T1-F24-04`: Verify entity component values remain consistent across 50 cycles.
  - `T1-F24-05`: Verify process resident set size (RSS) does not continuously grow across cycles.
- **Feature 25: Hot-Path Zero-Allocation Verification**
  - `T1-F25-01`: Measure heap allocations during `DynamicCSystem::run_unsafe`; verify 0 bytes allocated.
  - `T1-F25-02`: Measure heap allocations during `iter_next_chunk` loop; verify 0 bytes allocated.
  - `T1-F25-03`: Measure heap allocations during column pointer resolution; verify 0 bytes allocated.
  - `T1-F25-04`: Verify table ID cache avoids per-tick allocation in steady-state.
  - `T1-F25-05`: Execute 100 consecutive frames; verify allocation counter delta is 0.
- **Feature 26: ECS World Consistency After Failure**
  - `T1-F26-01`: Mod system panics mid-iteration; verify entities in world retain valid state.
  - `T1-F26-02`: Mod unloads unexpectedly; verify remaining systems can query world without crash.
  - `T1-F26-03`: Verify no dangling pointers to unloaded mod code exist in Bevy schedule.
  - `T1-F26-04`: World can spawn and despawn entities normally after mod failure.
  - `T1-F26-05`: World tick counter continues advancing monotonically after mod error.

---

### 3.2 Tier 2: Boundary & Corner Cases (≥5 Tests Per Feature)
Tier 2 exercises extreme bounds, invalid inputs, edge conditions, resource exhaustion, and adversarial inputs.  
**Threshold:** 26 features × 5 tests = **130 tests**.

- **Feature 1: ModRegistry**
  - `T2-F01-01`: Pass null C-string pointer to `get_proc_address`; verify returns null safely without panic.
  - `T2-F01-02`: Query symbol with empty string `""`; verify returns null safely.
  - `T2-F01-03`: Query symbol with extremely long string (16KB); verify handled safely without buffer overflow.
  - `T2-F01-04`: Query symbol with invalid UTF-8 byte sequences; verify returns null without panic.
  - `T2-F01-05`: Attempt duplicate registration of same UUID with conflicting VTable sizes; verify error rejected.
- **Feature 2: DynamicCSystem**
  - `T2-F02-01`: Register system with `query_count == 0` (no component accesses); verify valid empty system.
  - `T2-F02-02`: Register system with null queries pointer and non-zero query_count; verify returns `NULL_POINTER`.
  - `T2-F02-03`: Register system with `struct_size < sizeof(SystemDescriptor)`; verify returns `INVALID_VERSION`.
  - `T2-F02-04`: System callback receives null chunk iterator or command queue; verify handled safely.
  - `T2-F02-05`: Register system with 64 component queries (near max limit); verify scheduler handles gracefully.
- **Feature 3: RawChunkIter**
  - `T2-F03-01`: Pass invalid/out-of-bounds column index `usize::MAX`; verify returns `null_mut()` safely.
  - `T2-F03-02`: Advance chunk iterator after it already returned 0; verify consistently returns 0 (idempotent).
  - `T2-F03-03`: Table with exactly 1 entity (boundary between empty and multi-entity); verify correct traversal.
  - `T2-F03-04`: Table with maximum archetype table capacity; verify pointer calculations do not overflow.
  - `T2-F03-05`: Call `iter_mark_changed` with row index >= entity count; verify returns error status.
- **Feature 4: CommandQueueBridge**
  - `T2-F04-01`: Enqueue command with null component data pointer; verify handled safely or rejected with error.
  - `T2-F04-02`: Enqueue despawn on invalid/stale entity ID (`FfiEntity::PLACEHOLDER`); verify ignored safely.
  - `T2-F04-03`: Flush command queue when empty (0 commands); verify no-op without error.
  - `T2-F04-04`: Enqueue 10,000 commands in single frame; verify queue expands without memory corruption.
  - `T2-F04-05`: Command queue flushed during world teardown; verify no memory leak.
- **Feature 5: 3D Kinematics Scenario**
  - `T2-F05-01`: Kinematics step with `NaN` or `Infinity` coordinates; verify does not cause memory UB.
  - `T2-F05-02`: Kinematics step with maximum float values (`f32::MAX`); verify numeric overflow safety.
  - `T2-F05-03`: Kinematics on zero entities; verify cleanly finishes with 0 updates.
  - `T2-F05-04`: Position and Velocity located in non-adjacent columns with dissimilar strides; verify correct offset.
  - `T2-F05-05`: Kinematics step with subnormal float values; verify no denormal flush crash.
- **Feature 6: Combat & Health Damage Scenario**
  - `T2-F06-01`: Apply damage value `i32::MAX`; verify integer underflow handled deterministically.
  - `T2-F06-02`: Apply negative damage (healing) exceeding `i32::MAX`; verify overflow clamped safely.
  - `T2-F06-03`: Entity with 0 max health; verify immediate death trigger without divide-by-zero.
  - `T2-F06-04`: Multiple damage events in single tick for same entity; verify cumulative damage calculation.
  - `T2-F06-05`: Apply damage to already despawned entity; verify no panic or memory violation.
- **Feature 7: Dynamic Metadata Scenario**
  - `T2-F07-01`: Set NameTag with empty string `""` (`len == 0`); verify host string becomes empty.
  - `T2-F07-02`: Set NameTag with null byte pointer and non-zero length; verify returns `NULL_POINTER`.
  - `T2-F07-03`: Set NameTag with invalid UTF-8 bytes (e.g. `[0xFF, 0xFE]`); verify returns `INVALID_UTF8`.
  - `T2-F07-04`: Set NameTag with 1MB string; verify host reallocates without crash.
  - `T2-F07-05`: Set NameTag with multi-byte Unicode strings (emojis, CJK); verify UTF-8 boundaries preserved.
- **Feature 8: Dynamic Buffer Scenario**
  - `T2-F08-01`: Set `Vec<u32>` buffer with `len == 0`; verify buffer cleared safely.
  - `T2-F08-02`: Set `Vec<u32>` buffer with null pointer and non-zero length; verify returns `NULL_POINTER`.
  - `T2-F08-03`: Set buffer with 100,000 elements; verify host buffer expands and preserves elements.
  - `T2-F08-04`: Repeatedly toggle buffer between 0 and 1,000 elements; verify capacity reuse without leak.
  - `T2-F08-05`: Misaligned buffer pointer passed across FFI; verify detected and rejected safely.
- **Feature 9: Chunk Iteration Throughput Benchmark**
  - `T2-F09-01`: Benchmark with 0 entities; verify duration does not report division by zero.
  - `T2-F09-02`: Benchmark under extreme CPU frequency governor throttling; verify metrics output format valid.
  - `T2-F09-03`: Benchmark single archetype with 1,000,000 entities; verify memory consumption bounded.
  - `T2-F09-04`: Benchmark concurrent execution across 16 worker threads; verify thread-safety.
  - `T2-F09-05`: Benchmark with cold cache vs warm cache; verify variance captured.
- **Feature 10: C-ABI Boundary Overhead Benchmark**
  - `T2-F10-01`: Measure overhead with 0-parameter callback vs multi-parameter callback.
  - `T2-F10-02`: Rapidly invoke callback 1,000,000 times in loop; verify stack stability.
  - `T2-F10-03`: Measure overhead during active garbage collection or OS context switching.
  - `T2-F10-04`: Overhead comparison across release and debug build profiles.
  - `T2-F10-05`: Overhead measurement with simulated cache-miss memory barriers.
- **Feature 11: Multi-Entity Scale Scaling**
  - `T2-F11-01`: Scale test at 500,000 entities; verify memory scaling linearity.
  - `T2-F11-02`: Rapidly spawn and despawn 50,000 entities; verify table compaction.
  - `T2-F11-03`: Scale test under restricted memory limit; verify clean failure if allocation fails.
  - `T2-F11-04`: Entity generation overflow (entity index recycled); verify generation handling.
  - `T2-F11-05`: Query scaling when 99% of entities are filtered out by archetype query.
- **Feature 12: Archetype Fragmentation Benchmark**
  - `T2-F12-01`: 100 distinct archetype tables with 1 entity each (maximum fragmentation); verify iteration safety.
  - `T2-F12-02`: Rapid archetype transitions (adding/removing components during benchmark).
  - `T2-F12-03`: Highly skewed archetype distribution (1 table with 99,000 entities, 100 tables with 10 entities).
  - `T2-F12-04`: Query matching zero archetypes in fragmented world; verify 0 ticks executed cleanly.
  - `T2-F12-05`: Empty tables interspersed between heavily populated tables; verify stride skips.
- **Feature 13: Baseline Recording & Regression Tool**
  - `T2-F13-01`: Output directory does not exist; verify tool creates directory structure.
  - `T2-F13-02`: Read corrupted/invalid JSON baseline file; verify tool reports helpful syntax error.
  - `T2-F13-03`: Baseline contains different hardware/CPU architecture; verify tool warns on architecture mismatch.
  - `T2-F13-04`: Extreme performance regression (>50% drop); verify CLI exits with non-zero error code.
  - `T2-F13-05`: Writing baseline to read-only filesystem; verify error surfaced gracefully.
- **Feature 14: Component Alignment Verification**
  - `T2-F14-01`: Component with odd struct size (3 bytes, 7 bytes); verify alignment padding respected.
  - `T2-F14-02`: Over-aligned component (`align(128)`); verify memory address satisfies 128-byte boundary.
  - `T2-F14-03`: Array iteration with 64-byte aligned component; verify `(ptr + i * stride) % 64 == 0` for all `i`.
  - `T2-F14-04`: Zero-sized component (`()`, marker component); verify size is 0 and stride is handled.
  - `T2-F14-05`: Dynamically sized heap buffer alignment inside component; verify internal pointer alignment.
- **Feature 15: Chunk Iterator Bounds & Empty Table Guard**
  - `T2-F15-01`: Request column index equal to `usize::MAX`; verify null pointer returned.
  - `T2-F15-02`: Call `iter_mark_changed` on empty table; verify error returned without panic.
  - `T2-F15-03`: Chunk iterator created on world with 0 total archetypes; verify `iter_next_chunk` returns 0.
  - `T2-F15-04`: Concurrent chunk iteration on separate threads; verify no shared mutable iterator state.
  - `T2-F15-05`: Request mutable column twice on same chunk; verify returns same pointer without double-borrow crash.
- **Feature 16: FFI Transparent Type Safety**
  - `T2-F16-01`: Pass foreign `FfiResult` value `99999` (unrecognized code); verify `is_err()` is true without UB.
  - `T2-F16-02`: Pass foreign `ComponentAccessMode` value `100`; verify `is_valid()` is false and access is rejected.
  - `T2-F16-03`: Construct `FfiEntity` with `u64::MAX`; verify represents placeholder entity.
  - `T2-F16-04`: Pass foreign negative integer cast to `u32` status code; verify safe handling.
  - `T2-F16-05`: Verify bitwise layout of `FfiSlice<u8>` matches C struct `{ const uint8_t* ptr; size_t len; }`.
- **Feature 17: Self-Aliasing String/Vec Overlap Guard**
  - `T2-F17-01`: Pass string slice pointing to middle of existing string; verify host copies correctly.
  - `T2-F17-02`: Pass string slice spanning exact start to middle; verify no buffer overlap truncation.
  - `T2-F17-03`: Repeatedly overwrite string with itself in a loop 1,000 times; verify string remains identical.
  - `T2-F17-04`: Pass vector slice spanning exactly `vec[1..3]` back into setter; verify safe shrink.
  - `T2-F17-05`: Overlapping pointer from adjacent struct field passed into setter; verify host isolation.
- **Feature 18: Bevy ECS Borrow Rule Enforcement**
  - `T2-F18-01`: System declares WRITE and FILTER_ONLY on same component; verify conflict detected.
  - `T2-F18-02`: System declares READ and WRITE on same component; verify promoted to WRITE or flagged.
  - `T2-F18-03`: Mod system conflicts with native Bevy schedule system; verify scheduler prevents simultaneous execution.
  - `T2-F18-04`: Cyclic dependency between systems; verify scheduler detects cycle or resolves stages.
  - `T2-F18-05`: 10 systems all requesting WRITE on same component; verify executed serially without race.
- **Feature 19: Column Permission Enforcement**
  - `T2-F19-01`: Mod system attempts to call `iter_column_mutable` on undeclared component; verify null returned.
  - `T2-F19-02`: Mod system attempts to call `iter_column_readonly` on undeclared component; verify null returned.
  - `T2-F19-03`: Mod system calls `iter_mark_changed` on READ-only column; verify returns `PERMISSION_DENIED`.
  - `T2-F19-04`: Mod system calls `iter_mark_changed` on FILTER_ONLY column; verify returns `PERMISSION_DENIED`.
  - `T2-F19-05`: Verify writing through pointer obtained when mutable returned null triggers address check.
- **Feature 20: Cross-Runtime Panic Containment**
  - `T2-F20-01`: Guest panics with non-string payload (e.g. integer panic `panic!(42)`); verify caught safely.
  - `T2-F20-02`: Guest panics during `bevy_abi_mod_init`; verify mod loading fails gracefully with error.
  - `T2-F20-03`: Guest panics during `bevy_abi_mod_shutdown`; verify teardown continues without host abort.
  - `T2-F20-04`: Guest thread panics in background thread; verify host main thread remains uncorrupted.
  - `T2-F20-05`: Repeated panics across consecutive ticks; verify system remains permanently disabled.
- **Feature 21: Memory Allocator Boundary Isolation**
  - `T2-F21-01`: Host deallocates entity while guest holds borrowed `StrView`; verify access safety rules documented.
  - `T2-F21-02`: Guest attempts to pass stack-allocated buffer into setter; verify host copies onto heap.
  - `T2-F21-03`: Guest attempts to pass static string literal (`'static`); verify host copies onto heap.
  - `T2-F21-04`: Guest allocator running out of memory; verify host allocator unaffected.
  - `T2-F21-05`: Custom allocator configured on host; verify guest requires no knowledge of host allocator.
- **Feature 22: Mod Unloading & Teardown Hook**
  - `T2-F22-01`: Mod does not export optional `bevy_abi_mod_shutdown`; verify unloading succeeds without error.
  - `T2-F22-02`: Mod shutdown hook returns `FfiResult::INTERNAL_ERROR`; verify error logged and unload completes.
  - `T2-F22-03`: Unload mod while its systems are registered in active schedule; verify systems purged first.
  - `T2-F22-04`: Double unload of same mod ID; verify idempotent no-op or error.
  - `T2-F22-05`: Teardown hook invoked with null host context; verify handled safely.
- **Feature 23: Mod Hot-Reloading**
  - `T2-F23-01`: Reload mod when new dynamic library file is locked/busy; verify old mod retained without crash.
  - `T2-F23-02`: Reload mod when new dynamic library has invalid C-ABI version; verify rejected and old mod retained.
  - `T2-F23-03`: Reload mod when new dynamic library is missing `bevy_abi_mod_init`; verify rejected safely.
  - `T2-F23-04`: Hot-reload triggered concurrently with frame execution; verify reload deferred to stage boundary.
  - `T2-F23-05`: Reload mod 100 times in rapid succession without ticking; verify no file descriptor leak.
- **Feature 24: Repeated Registration Stress Cycle**
  - `T2-F24-01`: Register and unregister mod with 0 systems; verify no resource accumulation.
  - `T2-F24-02`: Register and unregister mod with 50 systems; verify all 50 registered and unregistered cleanly.
  - `T2-F24-03`: Rapid registration cycle with alternating component schemas; verify registry cleanup.
  - `T2-F24-04`: System registration failure halfway through descriptor array; verify rollback of prior systems.
  - `T2-F24-05`: Repeated registration under memory pressure; verify stability.
- **Feature 25: Hot-Path Zero-Allocation Verification**
  - `T2-F25-01`: World with 1,000 archetypes; verify chunk traversal performs zero allocations in steady state.
  - `T2-F25-02`: DynamicCSystem execution with 10 query terms; verify zero allocations per frame.
  - `T2-F25-03`: Command queue buffer reuse; verify queue flushes without reallocating each frame.
  - `T2-F25-04`: Multiple concurrent systems executing; verify global allocator lock is not contended.
  - `T2-F25-05`: Change detection tick updates; verify performed in-place without heap activity.
- **Feature 26: ECS World Consistency After Failure**
  - `T2-F26-01`: Mod system panics while holding partial chunk iteration; verify subsequent queries unaffected.
  - `T2-F26-02`: Mod system aborts foreign thread; verify Bevy main thread execution continues.
  - `T2-F26-03`: Verify entity count invariant (`world.entities().len()`) holds after mod crash.
  - `T2-F26-04`: Verify archetype table counts remain consistent after mod failure.
  - `T2-F26-05`: Reloading recovered mod restores normal gameplay logic without orphaned entities.

---

### 3.3 Tier 3: Cross-Feature Pairwise Combinations
Tier 3 tests the interactions between pairs of major features to expose coupling defects, concurrency hazards, and cross-boundary side effects:

1. **RawChunkIter (F3) + Alignment Heterogeneity (F14):** Traversal of contiguous tables containing interleaved components with 1B, 4B, 8B, 16B, and 64B alignments.
2. **DynamicCSystem (F2) + Bevy Borrow Conflict Detection (F18):** Parallel execution of multiple guest systems verifying that conflicting read/write component queries are correctly scheduled sequentially while non-conflicting systems run in parallel.
3. **Dynamic Metadata String/Vec (F7, F8) + Memory Allocator Boundary Isolation (F21):** Continuous string mutations and vector resizing across FFI verifying that host heap allocations never cross the boundary or leak.
4. **CommandQueueBridge (F4) + Chunk Iteration Traversal (F3):** Mod system enqueues entity spawns and component insertions while iterating active chunks; verify deferred commands flush safely at stage boundary without invalidating active iterators.
5. **Mod Hot-Reloading (F23) + Teardown Hook (F22) + Panic Containment (F20):** Mod hot-reloading triggered immediately after a system panic; verify old faulty library is shut down and new healthy library initializes seamlessly.
6. **Multi-Language Shared Schedule (C99 Mod + Rust Mod) (F5, F6):** A C99 mod system and a Rust mod system executing in the same Bevy schedule, reading and writing to each other's components across the shared C-ABI.
7. **Archetype Fragmentation (F12) + Scale Scaling (F11):** 100,000 entities distributed across 16 fragmented archetypes traversed by native mod systems.
8. **Column Permission Enforcement (F19) + Change Detection (F6):** Verify that attempting to mark change on a READ-only column returns `PERMISSION_DENIED` and does not dirty component ticks.
9. **Zero-Allocation Hot Path (F25) + Benchmark Suite (F9, F10):** Verify during benchmark execution that chunk iteration and FFI invocation record zero heap allocations.
10. **Repeated Registration Cycle (F24) + ECS World Consistency (F26):** Execute 50 cycles of mod registration, execution, and teardown while continuously spawning and despawning background entities in Bevy World.

---

### 3.4 Tier 4: Real-World Application Scenarios
Tier 4 provides end-to-end game scenarios simulating production modding environments with concurrent systems, multi-archetype distributions, dynamic state mutations, and live mod management:

#### Scenario A: 3D Kinematics & Physics Integration
- **Components:** `Position { x: f32, y: f32, z: f32 }`, `Velocity { x: f32, y: f32, z: f32 }`, `Mass { value: f32 }`.
- **Setup:** 10,000 entities spawned across 4 archetype tables (stationary objects, projectiles, lightweight actors, heavy vehicles).
- **Execution:** Rust guest mod updates `Velocity` with gravity and drag; C99 guest mod updates `Position` from `Velocity` using SoA chunk iterators.
- **Verification:** Mathematical verification of final positions after 60 ticks; zero memory allocations during ticks; change detection ticks marked accurately.

#### Scenario B: Combat, Bleed Damage Over Time, & Change Detection
- **Components:** `Health { current: i32, max: i32 }`, `BleedStatus { ticks_remaining: u32, damage_per_tick: i32 }`.
- **Setup:** 2,500 combat entities with varying health and bleed counters.
- **Execution:** Guest damage system decrements bleed ticks and reduces `Health`; Bevy host UI/audio system observes `Changed<Health>` and triggers deferred entity despawns for entities reaching 0 health via `CommandQueueBridge`.
- **Verification:** Exactly the damaged entities have `Changed<Health> == true`; despawned entities are removed at frame boundary without dangling pointers; health calculations are exact.

#### Scenario C: Dynamic RPG Inventory & Metadata
- **Components:** `NameTag { label: String }`, `Inventory { item_ids: Vec<u32> }`, `PlayerStats { level: u32 }`.
- **Setup:** 1,000 player and NPC entities with dynamic string labels and variable-length inventory buffers.
- **Execution:** Guest loot mod equips items, appends to inventory buffers, updates player labels, and performs self-aliasing updates (passing current name slice back to setter).
- **Verification:** Host heap manages all strings and vectors; no memory leaks; self-aliasing overlap guard prevents use-after-free; UTF-8 validation protects invalid byte inputs.

#### Scenario D: Multi-Mod Schedule Contention & Parallel Execution
- **Setup:** 4 distinct mod systems registered in Bevy schedule:
  - System 1 (Rust Mod A): `READ Position`, `WRITE Velocity`
  - System 2 (C Mod B): `READ Velocity`, `WRITE Position` (Conflicting with Mod A)
  - System 3 (Rust Mod C): `READ Health`, `WRITE Health` (Independent)
  - System 4 (C Mod D): `FILTER_ONLY Mass`, `READ Position` (Non-conflicting)
- **Execution:** Bevy scheduler schedules Mod C and Mod D in parallel, while ordering Mod A and Mod B sequentially. Mod C simulates an intentional panic on tick 10.
- **Verification:** Scheduler prevents race conditions between Mod A and Mod B; Mod C panic is contained with `PANIC_CAUGHT` and Mod C is disabled; remaining mods continue ticking for 50 ticks without crash or world corruption.

#### Scenario E: 50-Cycle Hot-Reload & Dynamic Lifecycle Stress
- **Setup:** Active Bevy application running 5,000 entities with ongoing physics and combat.
- **Execution:** A background controller triggers 50 sequential mod hot-reload cycles:
  1. Executes 5 ticks with active mod.
  2. Disables mod systems in schedule.
  3. Invokes `bevy_abi_mod_shutdown`.
  4. Drops shared library handle (`dlclose`).
  5. Dynamically compiles/reloads modified mod library with updated gameplay logic.
  6. Re-executes `bevy_abi_mod_init` and registers updated systems.
  7. Executes 5 ticks with updated logic.
- **Verification:** World entity count and existing component data remain 100% intact across all 50 reload cycles; memory usage does not leak; no dangling function pointers; final world state matches mathematical projection.

---

## 4. Test Architecture & Runner Invocation

### 4.1 Directory Layout
```text
bevy_abi_mod/
├── TEST_INFRA.md                   # This master test specification
├── TEST_READY.md                   # Readiness summary, runner commands & results
├── scripts/
│   └── run_e2e_tests.sh            # Automated end-to-end test execution script
├── crates/
│   ├── host_runtime/
│   │   └── tests/
│   │       ├── e2e_suite.rs        # Master E2E integration test target
│   │       ├── e2e_suite/
│   │       │   ├── harness.rs      # Fixtures, testbed world, mock VTables, probes
│   │       │   ├── tier1_features.rs # Tier 1: 130 feature isolation tests
│   │       │   ├── tier2_boundaries.rs # Tier 2: 130 boundary & corner tests
│   │       │   ├── tier3_pairwise.rs # Tier 3: Cross-feature interaction tests
│   │       │   └── tier4_scenarios.rs # Tier 4: Real-world application scenarios
│   │       ├── c_vtable_interop.rs # C99 dynamic library VTable integration test
│   │       ├── benchmarks.rs       # Programmatic benchmark runner (M4)
│   │       ├── safety_tests.rs     # Specialized safety assertions (M5)
│   │       └── lifecycle_tests.rs  # Specialized lifecycle stress assertions (M5)
│   ├── mod_derive/
│   │   └── tests/
│   │       └── validation_gate_1.rs # Macro generation and VTable gate tests
│   └── guest_sdk/
│       └── src/lib.rs              # Panic containment unit tests
└── tests/
    ├── asymmetric_c_mod/           # C99 guest mod source and Makefile
    └── asymmetric_rust_mod/        # Rust guest mod source and Cargo.toml
```

### 4.2 Runner Commands

#### 1. Full Workspace Test Suite
```bash
cargo test --workspace
```

#### 2. Master E2E Test Suite
```bash
# Run all E2E test tiers (Tiers 1-4)
cargo test --test e2e_suite

# Run specific tier
cargo test --test e2e_suite tier1
cargo test --test e2e_suite tier2
cargo test --test e2e_suite tier3
cargo test --test e2e_suite tier4
```

#### 3. Automated Shell Runner Script
```bash
./scripts/run_e2e_tests.sh
```

#### 4. C99 Mod Compilation & Interop
```bash
make -C tests/asymmetric_c_mod
cargo test --test c_vtable_interop
```

#### 5. Rust Guest Mod Compilation
```bash
cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release
```

#### 6. Programmatic Benchmarks & Baseline Recording
```bash
cargo test --test benchmarks -- --nocapture
```

---

## 5. Summary Coverage Thresholds

| Test Tier | Scope | Minimum Required Tests | Status |
|-----------|-------|------------------------|--------|
| **Tier 1: Feature Isolation** | 26 features in complete isolation | ≥ 5 per feature (**130 tests**) | Fully Specified & Defined |
| **Tier 2: Boundary & Corner Cases** | Edge values, nulls, bounds, overflows | ≥ 5 per feature (**130 tests**) | Fully Specified & Defined |
| **Tier 3: Pairwise Combinations** | Cross-feature interactions & concurrency | Key pairwise matrix (**10 combinations**) | Fully Specified & Defined |
| **Tier 4: Application Scenarios** | Full game scenarios (kinematics, combat, RPG, reload) | ≥ 5 realistic scenarios (**5 scenarios**) | Fully Specified & Defined |
| **Total Test Cases** | Comprehensive E2E Verification Suite | **≥ 275 distinct test verifications** | **READY** |
