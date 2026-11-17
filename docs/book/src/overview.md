# Overview & Threat Model (`ABI 0.2.0`)

`bevy_abi_mod` is an **asymmetric, zero-serialization `extern "C"` modding framework** for **Bevy ECS (`v0.20+`)**. It lets a Bevy application load, execute, quarantine, and hot-unload native dynamic libraries (`.so`, `.dylib`, `.dll`) compiled with **different Rust compiler versions**, **different optimization flags (`-O0` vs. `--release`)**, or **non-Rust languages (ISO C99, C++17/20, Zig)** without sharing Rust ABI assumptions or serializing ECS tables.

---

## Why Asymmetric C-ABI Instead of Rust `dylib`s?

Standard Rust `crate-type = ["dylib"]` plugins share internal `#[repr(Rust)]` layouts, `TypeId` hashes, thread-local destructors, and heap allocators:
- **Compiler Lock-In:** Any difference in `rustc` commit, `Cargo.lock`, or `-C opt-level` scrambles `StableCrateId` and struct field order (`ABI_LOCKED`).
- **`dlclose` `SIGSEGV` Hazards:** Unmapping a coupled Rust `dylib` leaves `TypeId` drop vtables and TLS destructors pointing into unmapped pages.
- **Allocator Crossover:** Freeing a guest `String` or panic payload on the host's allocator (for example Windows per-DLL `HeapAlloc` or `mimalloc`) corrupts process heaps.

`bevy_abi_mod` solves all three by placing a strict **`#[repr(C)]` wire boundary (`ABI 0.2.0`, `0x0002_0000`)** between host and guest:
1. **Versioned 38-Slot `BamHostApi` Handshake:** Every core and extended iterator, resource, message, command, observer, and registration entry point lives in a `struct_size`-extensible `Option<extern "C" fn(...)>` table.
2. **1-Based `BamComponent(u32)` Handles:** Components and resources resolve by canonical `"<namespace>::<Name>"` + 128-bit `ComponentUuid` (`SipHash-2-4-128`) once during `bevy_abi_mod_init`. Per-frame hot paths perform zero string hashing.
3. **Direct SoA & Batched Sparse-Set Views:** Dense `Table` columns are borrowed directly as `&[T]` / `&mut [T]` slices (`T: AbiPod`), and `SparseSet` columns yield batched indirect row-pointer slices.
4. **First-Class Bevy Parallelism:** Each guest system registers a native Bevy `GuestSystem` with exact `FilteredAccessSet` read/write locks, allowing non-conflicting host and mod systems to run concurrently on `ComputeTaskPool`.

---

## Seven-Crate `-sys` vs. `#![forbid(unsafe_code)]` Split

| Crate | `#![forbid(unsafe_code)]` | `bevy*` Allowed | Role |
|---|---|---|---|
| `bevy_abi_mod_sys` | No (`#![no_std]`) | **Never** | Zero-dependency `ABI 0.2.0` `#[repr(C)]` wire contract (`include/bevy_abi_mod.h` source of truth). |
| `bevy_abi_mod_macros` | Yes | **Never** | `#[derive(ModComponent, AbiPod)]` hygienic VTable and layout codegen. |
| `bevy_abi_mod_host_sys` | No (isolates host `unsafe`) | Yes | `HOST_API`, `HostIter`, `CommandArena`, `GuestSystem`, CoW `ModRegistry`, `DROP_SLOTS`, shadow-copy loader. |
| `bevy_abi_mod_host` | **Yes** | Yes | Safe Bevy `AbiModPlugin`, `ModWorldExt`, `ModAppExt`, `ModCommandsExt`, `LoadOptions`, `ComponentOptions`. |
| `bevy_abi_mod` | **Yes** | Yes | Top-level host facade crate re-exporting `bevy_abi_mod_host`, macros, and `prelude`. |
| `bevy_abi_mod_guest_sys` | No (isolates guest `unsafe`) | **Never** | Per-World `GuestInstance`, `catch_guest_*` panic guards, lock-free closure trampolines, slice validators. |
| `bevy_abi_mod_guest` | **Yes** | **Never** | Safe Rust guest SDK (`GuestPlugin`, `GuestApp`, `app.system(...)`, `Chunk`, `GuestCommands`, `export_mod!`). |

---

## Threat Model & Native Trust Boundary

> **Important:** Native dynamic libraries loaded via `dlopen` / `LoadLibraryW` execute **in-process inside the host's virtual address space with full OS privileges of the host process**.

- **Trusted Native Code Only:** `bevy_abi_mod` is **not** a security sandbox against adversarial or untrusted third-party binaries. Malicious machine code in a `.dll`/`.so`/`.dylib` can read arbitrary memory or invoke arbitrary OS syscalls.
- **What `bevy_abi_mod` Guarantees:** For trusted or cooperative mods (studio gameplay modules, DLCs, decoupled debug/release iteration, locally opted-in native mods), `bevy_abi_mod` enforces **compiler ABI independence**, **allocator isolation**, **compile-time `AbiPod` bit-validity**, **Bevy scheduler aliasing safety**, **guest-side panic containment (`panic = "unwind"`)**, and **deterministic `dlclose` teardown**.
- **Untrusted User Mods:** Games accepting untrusted mods from arbitrary internet sources should execute untrusted scripts in a **WebAssembly (Wasm)** sandbox and reserve `bevy_abi_mod` for trusted native extensions.
