# Gemini Agent Guide: `bevy_abi_mod`

This document defines the agentic workflow, architectural principles, development commands, and coding standards for developing **`bevy_abi_mod`**.

---

## 1. Project Overview & Identity
- **Name:** `bevy_abi_mod`
- **Goal:** Production-grade, zero-serialization, asymmetric C-ABI native modding framework for Bevy ECS (v0.20+).
- **Core Principle:** **Zero Shared Rust ABI Assumptions**. Mod modules can be written in separate Rust compiler versions, C99, C++, or Zig. Communication occurs strictly over an `extern "C"` boundary without dynamic memory allocator crossover or JSON serialization.

---

## 2. Repository Layout & Crate Boundaries (`-sys` vs. Safe Rust Split)

| Crate / Path | Responsibility | Safety & Dependencies Allowed |
|---|---|---|
| `crates/bevy_abi_mod_sys` | `#[repr(C)]` descriptors, `extern "C"` function pointers, status codes, API handshake, `AbiPod`, `FfiLayout`. | `#![no_std]`, **NO external dependencies** (no `bevy`, no `alloc`). |
| `crates/bevy_abi_mod_macros` | Procedural macros (`#[derive(ModExposable, AbiPod)]`). | `syn`, `quote`, `proc-macro2`. **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_host_sys` | Low-level host FFI internals (`UnsafeWorldCell` chunk iterator, raw deferred commands, `DynamicCSystem`, `ModRegistry`, `libloading` loader, `HOST_API`). | `bevy_ecs`, `bevy_app`, `libloading`, `nonmax`, `bevy_abi_mod_sys`. Isolates all host `unsafe`. |
| `crates/bevy_abi_mod_host` | Idiomatic safe Bevy host plugin (`AbiModPlugin`, `ModAppExt`, lifecycle events, system sets, `prelude`). | **`#![forbid(unsafe_code)]`**. `bevy_app`, `bevy_ecs`, `bevy_abi_mod_host_sys`, `bevy_abi_mod_sys`, `bevy_abi_mod_macros`. |
| `crates/bevy_abi_mod` | Top-level host facade crate re-exporting `bevy_abi_mod_host`, `ModExposable`, `AbiPod`, `bevy_abi_mod_sys`, and `prelude`. | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `bevy_abi_mod_macros`, `bevy_abi_mod_sys`. |
| `crates/bevy_abi_mod_guest_sys` | Low-level guest FFI internals (`extern "C"` trampolines, lifecycle state cells, VTable resolution, raw pointer/slice/command/resource bridges). | `bevy_abi_mod_sys` only. **NO `bevy` dependency.** Isolates all guest `unsafe`. |
| `crates/bevy_abi_mod_guest` | Idiomatic ergonomic Rust guest SDK (`GuestPlugin`, `GuestApp`, `SystemContext`, `MultiQueryContext`, `Chunk`, `export_mod!`, `prelude`). | `bevy_abi_mod_guest_sys`, `bevy_abi_mod_sys`, optional `bevy_abi_mod_macros` (`derive` feature). **NO `bevy` dependency.** |
| `tests/asymmetric_rust_mod` | Integration test guest mod in Rust (independent `Cargo.lock`, `.cdylib`). | `bevy_abi_mod_guest`, `bevy_abi_mod_sys`. **NO `bevy` dependency.** |
| `tests/asymmetric_c_mod` | Pure C99 integration test guest mod. | C standard library (`stdint.h`, `stddef.h`). |

---

## 3. Strict Development Rules

### Rule 1: ABI Purity in `bevy_abi_mod_sys`
- Every struct MUST be `#[repr(C)]` (or `#[repr(transparent)]` over a C primitive).
- Every function pointer MUST be `extern "C" fn(...) -> ...`.
- No Rust fat pointers (`&[T]`, `&str`, `dyn Trait`) in public ABI structs.
- String views must be passed via `StrView { ptr: *const u8, len: usize }`.
- Status codes and modes must use explicit primitive representations (e.g. `#[repr(transparent)]` over `u32`).

### Rule 2: Memory & Allocator Isolation
- Memory allocated on the host side must be freed by the host side.
- Memory allocated on the guest side must be freed by the guest side.
- Never pass Rust `Box`, `Vec`, or `String` across the ABI.

### Rule 3: Dynamic System Concurrency Safety & `-sys` Isolation
- `bevy_abi_mod_host` and `bevy_abi_mod` enforce `#![forbid(unsafe_code)]`; all host `unsafe` belongs in `bevy_abi_mod_host_sys` with `// SAFETY:` comments and `# Safety` docs.
- When registering guest systems, all component read/write accesses must be declared in Bevy's `FilteredAccessSet` via component IDs.
- The host must enforce borrow rules: attempting to register conflicting accesses must be detected by Bevy's scheduler just like native Rust systems.
- Pointer dereferencing inside chunk iterators must respect alignment and size declared in the component VTable.

---

## 4. Key Development & Verification Commands

```bash
# Verify zero Bevy dependencies in ABI & guest crates (must print NOTHING)
cargo tree -p bevy_abi_mod_sys -p bevy_abi_mod_guest_sys -p bevy_abi_mod_guest | grep -v bevy_abi_mod | grep -i bevy

# Check formatting & lint entire workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Build the asymmetric Rust & C99 guest test mods
cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release
make -C tests/asymmetric_c_mod

# Run default workspace tests (excludes heavy GUI/3D examples)
cargo test -- --test-threads=1

# Run host integration tests loading both compiled mods
cargo test -p bevy_abi_mod_host --test integration_tests -- --test-threads=1
```

---

## 5. Agent Workflow & Working with Next-Tier Models
When prompting or continuing work in this codebase:
1. **Always read `spec.md` and `docs/IMPROVEMENT_PLAN.md`** first to ensure alignment with architectural contracts and data structures.
2. Maintain strict decoupling: verify that `bevy_abi_mod_sys`, `bevy_abi_mod_macros`, `bevy_abi_mod_guest_sys`, and `bevy_abi_mod_guest` **never** pull in `bevy`.
3. When adding new features, update `spec.md` and ensure cross-language compatibility (both Rust mod and C mod test cases).
