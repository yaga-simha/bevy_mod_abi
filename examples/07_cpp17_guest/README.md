# Example 07: ISO C++17 Guest Mod (`bam::GuestApp` + Typed Component Views)

**Concept taught:** Authoring a strict ISO C++17 guest mod (`-std=c++17 -Wall -Wextra -Wpedantic -Werror`) against `include/bevy_abi_mod.h` (`ABI 0.3.0`) using idiomatic C++17 `noexcept` RAII wrappers (`bam_cpp17_sdk.hpp`) and macro-generated typed views (`example_components.h` from `ModComponent::C_DECL`).

---

## Build Prerequisites

- A C++17 compiler (`c++`, `clang++`, or `g++`; overridden via `CXX`)
- Rust toolchain (`cargo`) for the Bevy host application and test harness

---

## Generated Component Headers & SDK Wrappers

- `cpp_mod/example_components.h` is emitted deterministically from `ModComponent::C_DECL` in `src/lib.rs` (tested by `example_components_header_matches_mod_component_c_decl`). It provides C99 struct/VTable declarations alongside `#ifdef __cplusplus` `noexcept` view classes:
  - `ActorStatsView` + `bam::HostVecView` (`reserve`, `push`, `pop`, `remove`, `swap_remove`, `truncate`, `clear`, mutable range-based `for` via `as_mut_slice`)
  - `BehaviorModeView` for `#[repr(C, u8)]` tagged enums (`tag()`, `set_patrol()`, `try_get_patrol_speed()`, `set_attack()`)
  - `bam::VTableHandle<VT>` and move-only `bam::UniqueGuest<CppModInstance, ...>`
- `cpp_mod/bam_cpp17_sdk.hpp` layers `bam::GuestApp`, `bam::SystemContext`, `bam::Chunk`, and `bam::Commands` (`spawn_batch`) cleanly over `BamHostApi`.

---

## What It Shows

1. **Zero-copy SoA chunk & resource iteration:** Mutates `example::Position` and `example::Velocity` columns directly with `example::SimConfig`.
2. **Typed non-POD & enum VTable views:** Safely mutates host-owned `String`, `Vec<u32>`, `FfiOption<u32>`, `bool`, and `#[repr(C, u8)]` `BehaviorMode` without crossing allocators.
3. **Bevy `0.20` ECS patterns:** Demonstrates `BAM_ACCESS_HAS` (`Has<ActorStats>`), `BAM_ACCESS_OPTIONAL_READ` (`BamChildOf`), `BAM_EVENT_KIND_ADD` lifecycle observers, and `cmd_spawn_batch` bulk entity spawning.
4. **ABI Safety Guarantees:** All `extern "C"` boundaries are strict `nounwind` (`try / catch (...) -> BAM_RESULT_PANIC_CAUGHT`), per-`World` instance state uses zero globals, and host vs. guest allocations stay strictly isolated.

## Commands

```bash
# Build the C++17 shared library standalone via Makefile
make -C examples/07_cpp17_guest/cpp_mod

# Run the Bevy host example
cargo run -p example_07_cpp17_guest

# Run the end-to-end integration tests (RAII, typed views, Has<T>, observers, batch spawn, fault containment)
cargo test -p example_07_cpp17_guest
```
