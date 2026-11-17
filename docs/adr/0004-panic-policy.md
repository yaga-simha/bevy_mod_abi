# ADR 0004: Cross-ABI Panic & Unwind Policy (`extern "C"` Nounwind, Host `catch_unwind` Removal, and Guest `panic = "abort"`)

- **Status:** Accepted
- **Date:** 2026-11-11
- **Target Crates:** `bevy_abi_mod_host_sys`, `bevy_abi_mod_guest_sys`, `bevy_abi_mod_guest`

---

## 1. Context & Problem Statement

`bevy_abi_mod` enforces two foundational invariants (`GEMINI.md` §1 & §3):
1. **Zero Shared Rust ABI Assumptions:** Host and guest binaries may be compiled with different `rustc` versions, different standard libraries, different panic strategies (`panic = "unwind"` vs `panic = "abort"`), or non-Rust compilers (C99, C++, Zig).
2. **Strict Memory & Allocator Isolation (Rule 2):** Memory allocated on the guest side (`Box<dyn Any + Send>` panic payloads, `String` messages) must be freed exclusively on the guest side.

In v0.1, panic handling suffered from two architectural contradictions:

### 1.1 Dead & Misleading Host `catch_unwind` Around `extern "C"` Calls
`host_runtime` wrapped four foreign `extern "C"` function pointer invocations in `std::panic::catch_unwind(AssertUnwindSafe(...))`:
- `init_fn` and `shutdown_fn` in `host.rs`
- `run_if` and `callback` in `dynamic_system.rs`

Under **Rust 1.81+ (RFC 2945 / `c_unwind` stabilization)**:
1. Every `extern "C" fn` pointer carries the LLVM **`nounwind`** attribute at the call site.
2. Any Rust `extern "C" fn` body automatically contains a compiler-generated nounwind abort guard (`core::panicking::panic_cannot_unwind`): if a Rust panic escapes to the `extern "C"` boundary without being caught *inside* the callee, the guest runtime immediately aborts the process before unwinding a single stack frame.
3. Conversely, if a C++ or assembly guest forcibly unwound a foreign exception (Itanium `_Unwind_RaiseException` or Windows SEH) across an `extern "C"` `nounwind` call site into the host, unwinding across an LLVM `nounwind` call instruction is **immediate Undefined Behavior** — LLVM elides landing pads around `nounwind` calls!
4. Therefore, `std::panic::catch_unwind` around an `extern "C"` call in `bevy_abi_mod_host_sys` can **never** catch a foreign panic; it is 100% dead code that falsely implies sandboxed fault isolation against C/aborting guests.

### 1.2 Undocumented & Untested `panic = "abort"` Behavior in `guest_sdk`
Many production game mods and release builds compile `.cdylib` targets with `panic = "abort"` for binary size and codegen speed. In `guest_sdk`, `catch_guest_panic` called `std::panic::catch_unwind`, whose behavior under `panic = "abort"` was neither documented in the API contract nor verified in CI.

---

## 2. Decision

### 2.1 Strict `extern "C"` (Never `extern "C-unwind"`) Across the ABI

Every function pointer in `bevy_abi_mod_sys` (`BamManifestFn`, `BamInitFn`, `BamShutdownFn`, `BamSystemFn`, `BamRunIfFn`, `ComponentDropFn`, every slot in `BamHostApi`, and every generated VTable function) MUST use `unsafe extern "C" fn(...)`, **never** `extern "C-unwind"`.

**Why `extern "C-unwind"` is forbidden:**
Even though `extern "C-unwind"` permits foreign frames to unwind in LLVM, catching a Rust panic from an asymmetric `.cdylib` inside the host's `std::panic::catch_unwind` is **unsound**:
- Rust's panic exception class (`MOZ\0RUST` / SEH) is not guaranteed to have a stable payload layout across different `rustc` versions.
- Even when compiled with the exact same `rustc`, `catch_unwind` in the host takes ownership of `Box<dyn Any + Send>` allocated on the **guest's** heap (e.g., Windows DLL private `HeapAlloc`, `mimalloc`, or `jemalloc`) and frees it on the **host's** global allocator when the `Result::Err(payload)` drops — violating **Rule 2** and corrupting the heap!

### 2.2 Host Policy: Delete All `catch_unwind` Around Foreign FFI Calls

1. **Remove all `std::panic::catch_unwind` wrappers around foreign `extern "C"` calls** in `bevy_abi_mod_host_sys` (`ModHost::load_internal`, `LoadedMod::shutdown`, `GuestSystem::run_unsafe`, and `drop_trampoline`).
2. **Value-Based Fault Signaling:** The host detects guest faults exclusively via explicit `#[repr(transparent)]` return codes (`FfiResult` — including `FfiResult::PANIC_CAUGHT` (`3`)).
3. **Host `ModErrorPolicy` Execution:** When `BamSystemFn` returns any `res != FfiResult::OK` (such as `FfiResult::PANIC_CAUGHT`, `INVALID_COMPONENT`, or `OUT_OF_RANGE`):
   - Uncommitted commands in the faulting system's `HostCommands` arena are discarded cleanly (`ADR 0003`).
   - `ModSystemFaulted { mod_id, mod_name, system_name, result: res }` is emitted.
   - `ModErrorPolicy` (`Continue`, `DisableSystem`, `UnloadMod` / `DisableMod`, or `PanicHost`) acts deterministically in safe Rust inside the host.
4. **Host-Provided `BamHostApi` Callbacks:** Every `extern "C"` function implemented by `bevy_abi_mod_host_sys` in `HOST_API` MUST validate all pointer, alignment, handle, and state preconditions up front and return an `FfiResult` (`NULL_POINTER`, `INVALID_ARGUMENT`, `INVALID_STATE`, etc.) without panicking. If an internal host bug ever triggers a panic inside a `HOST_API` function, Rust 1.81+'s `extern "C"` nounwind guard aborts the process rather than unwinding into C99/Zig/asymmetric-Rust guest frames.

### 2.3 Guest Policy: `catch_guest_panic` Inside `bevy_abi_mod_guest_sys`

All `extern "C"` entry points and trampolines in `bevy_abi_mod_guest_sys` isolate Rust panics **inside the guest `.cdylib`'s own runtime and allocator** before returning across `extern "C"`:

```rust
// crates/bevy_abi_mod_guest_sys/src/panic.rs
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Executes `f` inside the guest `.cdylib` runtime.
///
/// - Under `panic = "unwind"`: catches any Rust panic, drops the `Box<dyn Any + Send>`
///   payload on the **guest** allocator, invokes optional guest panic reporting, and returns
///   `FfiResult::PANIC_CAUGHT` (`3`) across the `extern "C"` boundary without unwinding.
/// - Under `panic = "abort"`: `catch_unwind` compiles to a direct call; any panic aborts
///   the OS process immediately inside the guest runtime before reaching the `extern "C"` frame.
#[inline]
pub fn catch_guest_result<F: FnOnce() -> FfiResult>(f: F) -> FfiResult {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            report_guest_panic_payload(&payload);
            drop(payload); // Explicitly freed on GUEST allocator before returning!
            FfiResult::PANIC_CAUGHT
        }
    }
}

#[inline]
pub fn catch_guest_bool<F: FnOnce() -> bool>(f: F) -> FfiBool {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(flag) => FfiBool::from(flag),
        Err(payload) => {
            report_guest_panic_payload(&payload);
            drop(payload);
            FfiBool::FALSE
        }
    }
}

#[inline]
pub fn catch_guest_unit<F: FnOnce()>(f: F) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        report_guest_panic_payload(&payload);
        drop(payload);
    }
}
```

### 2.4 Explicit Behavior Matrix Across Guest Languages & Profiles

| Guest Language / Build Profile | Fault Event Inside Guest Callback | Runtime Behavior | Host Observation |
|---|---|---|---|
| **Rust (`panic = "unwind"`, default)** | `panic!("...")` / `.unwrap()` / bounds check | Caught by `catch_guest_result` inside guest `.cdylib`; payload freed by guest allocator; zero cross-FFI unwinding | Receives `FfiResult::PANIC_CAUGHT` (`3`); applies `ModErrorPolicy` & emits `ModSystemFaulted` |
| **Rust (`panic = "abort"`)** | `panic!("...")` / `.unwrap()` / bounds check | Guest `std` invokes `libc::abort()` immediately at panic site | Deterministic OS process abort (`SIGABRT` / exit code) — zero UB |
| **C99** | Explicit error check (`return BAM_RESULT_...`) | Returns `BamResult` by value | Applies `ModErrorPolicy` & emits `ModSystemFaulted` |
| **C99** | `abort()` / null dereference | OS signal (`SIGABRT` / `SIGSEGV`) | Process termination (native DLL trust model) |
| **C++** | `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }` | Caught inside C++ guest runtime; exception freed by C++ runtime | Receives `BAM_RESULT_PANIC_CAUGHT` (`3`); applies `ModErrorPolicy` |
| **C++** | Uncaught `throw` escaping `extern "C"` (`noexcept` in C++) | C++ ABI calls `std::terminate()` before leaving `extern "C"` | Deterministic OS process abort |
| **Zig** | Zig `error` union mapped to `BamResult` | Returns `BamResult` by value | Applies `ModErrorPolicy` |
| **Zig** | `@panic(...)` / safety-checked overflow | Zig default panic handler aborts process (unless custom Zig panic handler longjmps locally, which is discouraged) | Deterministic OS process abort |

> **Documentation Contract:** Both `bevy_abi_mod_guest` crate-level rustdoc and `include/bevy_abi_mod.h` MUST prominently state:
> 1. Native mods execute in-process at full CPU speed without OS process/Wasm sandboxing.
> 2. Rust mods that want recoverable system-level fault isolation (`ModErrorPolicy::DisableSystem` / `ModSystemFaulted`) MUST compile with `panic = "unwind"` (Cargo's default). Compiling a guest `.cdylib` with `panic = "abort"` is fully supported and ABI-sound, and causes any guest `panic!` to terminate the host process immediately.
> 3. C++ guest callbacks MUST mark their `extern "C"` entry points `noexcept` or wrap bodies in `try { ... } catch (...)` returning `BAM_RESULT_PANIC_CAUGHT`.

---

## 3. Alternatives Considered

| Alternative | Why Rejected |
|---|---|
| **`extern "C-unwind"` + Host `catch_unwind`** | Violates Rule 2 (host drops guest-allocated `Box<dyn Any>`) and assumes identical Rust unwind payload ABI across asymmetric `rustc` compilers. |
| **`setjmp` / `longjmp` Guard in C Shim** | Skipping C++/Rust stack frames with `longjmp` leaks destructors and causes Undefined Behavior in Rust frames with live drops (`c_unwind` RFC). |
| **Out-of-Process / Wasm Sandboxing** | Violates `bevy_abi_mod`'s core identity: zero-serialization direct pointer access to Bevy archetype `Table` columns in shared address space (`spec.md` §1). |

---

## 4. Consequences & Verification Criteria

1. **Codebase audit:** `grep -rn "catch_unwind" crates/bevy_abi_mod_host_sys/src crates/bevy_abi_mod_host/src` MUST return zero matches.
2. **`panic = "unwind"` integration test:** Loading the Rust fixture mod compiled with default `panic = "unwind"` and triggering a panic system verifies `FfiResult::PANIC_CAUGHT`, `ModSystemFaulted` event delivery, arena command rollback, and zero memory leaks.
3. **`panic = "abort"` subprocess integration test:** Compiling a dedicated Rust fixture `.cdylib` with `RUSTFLAGS="-C panic=abort"` and executing a child test runner process that triggers a guest panic verifies clean, deterministic process termination (`!status.success()`) without segfaulting inside unwind machinery.
