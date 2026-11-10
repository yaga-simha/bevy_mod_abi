# Gemini Agent Guide: `bevy_abi_mod`

This document defines the agentic workflow, architectural principles, development commands, and coding standards for developing **`bevy_abi_mod`**.

---

## 1. Project Overview & Identity
- **Name:** `bevy_abi_mod`
- **Goal:** Production-grade, zero-serialization, asymmetric C-ABI native modding framework for Bevy ECS (v0.20+).
- **Core Principle:** **Zero Shared Rust ABI Assumptions**. Mod modules can be written in separate Rust compiler versions, C99, C++, or Zig. Communication occurs strictly over an `extern "C"` boundary without dynamic memory allocator crossover or JSON serialization.

---

## 2. Repository Layout & Crate Boundaries

| Crate / Path | Responsibility | Dependencies Allowed |
|---|---|---|
| `crates/abi_contract` | `#[repr(C)]` data types, function pointers, status codes, API handshake. | `#![no_std]`, **NO external dependencies** (no `bevy`, no `alloc`). |
| `crates/mod_derive` | Procedural macro (`#[derive(ModExposable)]`) for host types. | `syn`, `quote`, `proc-macro2`. |
| `crates/host_runtime` | Bevy plugin, `DynamicCSystem`, `UnsafeWorldCell` chunk iterator, `libloading` dynamic manager. | `bevy` (or `bevy_ecs`, `bevy_app`), `libloading`, `abi_contract`. |
| `crates/guest_sdk` | Safe ergonomic wrapper for Rust guest mods. | `abi_contract`. **NO `bevy` dependency.** |
| `tests/asymmetric_rust_mod` | Integration test guest mod in Rust (own Cargo.lock, `.cdylib`). | `guest_sdk`, `abi_contract`. |
| `tests/asymmetric_c_mod` | Pure C99 integration test guest mod. | C standard library (`stdint.h`, `stddef.h`). |

---

## 3. Strict Development Rules

### Rule 1: ABI Purity in `abi_contract`
- Every struct MUST be `#[repr(C)]`.
- Every function pointer MUST be `extern "C" fn(...) -> ...`.
- No Rust fat pointers (`&[T]`, `&str`, `dyn Trait`) in public ABI structs.
- String views must be passed via `StrView { ptr: *const u8, len: usize }`.
- Enums must have an explicit primitive representation (e.g. `#[repr(u32)]` or `#[repr(C)]`).

### Rule 2: Memory & Allocator Isolation
- Memory allocated on the host side must be freed by the host side.
- Memory allocated on the guest side must be freed by the guest side.
- Never pass Rust `Box`, `Vec`, or `String` across the ABI.

### Rule 3: Dynamic System Concurrency Safety
- When registering guest systems, all component read/write accesses must be declared in Bevy's `FilteredAccessSet` via component IDs.
- The host must enforce borrow rules: attempting to register conflicting accesses must be detected by Bevy's scheduler just like native Rust systems.
- Pointer dereferencing inside chunk iterators must respect alignment and size declared in the component VTable.

---

## 4. Key Development & Verification Commands

```bash
# Check entire workspace
cargo check --workspace --all-targets

# Run unit tests in workspace
cargo test --workspace

# Build the asymmetric Rust guest test mod
cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release

# Build the asymmetric C99 guest mod
make -C tests/asymmetric_c_mod

# Run host integration tests loading both compiled mods
cargo test --test integration_tests
```

---

## 5. Agent Workflow & Working with Next-Tier Models
When prompting or continuing work in this codebase:
1. **Always read `spec.md`** first to ensure alignment with architectural contracts and data structures.
2. Maintain strict decoupling: verify that `crates/abi_contract` and `crates/guest_sdk` **never** pull in `bevy`.
3. When adding new features, update `spec.md` and ensure cross-language compatibility (both Rust mod and C mod test cases).
