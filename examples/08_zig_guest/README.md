# Example 08: Zig Guest Mod (`bam_zig_sdk` + `comptime` Component Views)

**Concept taught:** Authoring a Zig guest mod (`zig build`) against `include/bevy_abi_mod.h` (`ABI 0.2.0`) using lightweight `comptime` SDK wrappers (`zig_mod/src/bam_zig_sdk.zig`) and macro-generated component declarations (`zig_mod/src/generated_components.zig` from `ModComponent::ZIG_DECL`).

---

## Build Prerequisites

- A Zig toolchain (`zig 0.14+`, providing `callconv(.c)`)
- Rust toolchain (`cargo`) for the Bevy host application and test harness

*(If `zig` is not installed on `PATH`, the Rust host integration tests verify `ZIG_DECL` synchronization and host component registration and skip only the dynamic library load step.)*

---

## Generated Component Declarations & SDK Wrappers

- `zig_mod/src/generated_components.zig` is emitted deterministically from `ModComponent::ZIG_DECL` in `src/lib.rs` (tested by `generated_zig_components_match_mod_component_zig_decl`). It provides `extern struct` layouts alongside `comptime`-verified typed views:
  - `ZigActorStatsView` + `ZigActorStatsVTable_scores_VecView` (`reserve`, `push`, `pop`, `remove`, `swapRemove`, `truncate`, `clear`, mutable slice iteration via `mutSlice`)
  - `ZigBehaviorModeView` for `#[repr(C, u8)]` tagged enums (`tag()`, `set_patrol()`, `try_get_patrol_speed()`, `set_attack()`)
- `zig_mod/src/bam_zig_sdk.zig` layers `GuestApp`, `SystemContext`, `Chunk`, and `Commands` (`spawnBatch`) cleanly over `BamHostApi`.

---

## What It Shows

1. **Zero-copy SoA chunk & resource iteration:** Mutates `example::Position` columns from `example::Velocity` and `example::ZigSimConfig`.
2. **Typed non-POD & enum VTable views:** Safely mutates host-owned `String`, `Vec<u32>`, `FfiOption<u32>`, `bool`, and `#[repr(C, u8)]` `ZigBehaviorMode` without crossing allocators.
3. **Wave 1–2 Bevy `0.20` patterns:** Demonstrates `BAM_ACCESS_HAS` (`Has<ZigActorStats>`), `BAM_ACCESS_OPTIONAL_READ` (`BamChildOf`), `BAM_EVENT_KIND_ADD` lifecycle observers, and `cmd_spawn_batch` bulk entity spawning.
4. **ABI Safety Guarantees:** Every exported callback uses `callconv(.c)`, Zig error unions (`BamError!void`) convert explicitly to `BamResult` status codes without unwinding across `extern "C"`, and guest allocations (`std.heap.c_allocator`) stay strictly isolated from host allocations.

## Commands

```bash
# Build the Zig shared library standalone
(cd examples/08_zig_guest/zig_mod && zig build)

# Run the Bevy host example
cargo run -p example_08_zig_guest

# Run the end-to-end integration tests (auto-builds zig_mod when `zig` is installed)
cargo test -p example_08_zig_guest
```
