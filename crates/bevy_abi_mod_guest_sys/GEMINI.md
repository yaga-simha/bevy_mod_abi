# Agent Guide: `bevy_abi_mod_guest_sys`

Crate guide for **`bevy_abi_mod_guest_sys`** (`v0.3.0`, ABI `0x0003_0000`) — low-level, zero-Bevy guest FFI runtime, panic guards, and validated wrappers.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_guest_sys` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.87`)
- **Role:** Isolates ALL guest-side `unsafe` FFI and pointer handling behind documented contracts so `bevy_abi_mod_guest` enforces `#![forbid(unsafe_code)]`.

---

## 2. Safety & Dependency Boundaries
- **`unsafe` Policy:** Enforces `unsafe_op_in_unsafe_fn = "deny"`, `improper_ctypes_definitions = "deny"`, `#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`. Every `unsafe` block MUST have a `// SAFETY:` comment; every `unsafe fn` MUST have `# Safety` docs. Keep integration/soundness tests in `tests/`.
- **Zero Bevy Dependencies:** Depends strictly on `bevy_abi_mod_sys` ONLY (`std` only, zero external crates). **NEVER** add any `bevy*` dependency.

---

## 3. Owned Surface
- **`Host` & `ValidatedLoadCtx` (`host.rs`, `lib.rs`):** Bounded `BamHostApi` copy, `ABI 0.3.0` SemVer verification, `covers_slot` + `Option` guards on all **68/68** host slots (`host_slot!` macro), `"bam.v0.*"` lookup, and `VTableHeader` checks.
- **`GuestInstance` (`instance.rs`):** Per-World heap state owner with raw-owned runtime allocations (`into_raw`/`from_raw` — move-safe under Stacked & Tree Borrows), deterministic LIFO closure teardown, and zero process-global mutable state.
- **Panic & Re-entrancy Guards (`panic.rs`):** `catch_guest_result`, `catch_guest_bool`, `catch_guest_unit` across all `extern "C"` entry points (including `forward_field` and `append_field` in `iter.rs`) + lock-free `ReentrantCell`.
- **Validated Wrappers (`trampoline.rs`, `iter.rs`, `commands.rs`, `component.rs`):** Fail-closed `component_layout_info` validation on handle-based resource/wire/moved-component APIs (`insert_moved`, `spawn_moved`, `SpawnEntry`, `SpawnBatchColumn`), non-`Copy` `ValidatedSystemRef`, exact `M::UUID` checks on non-POD messages, `stride != 0` / `isize::MAX` / alignment / byte-overlap guards across dense slices & sparse rows.

---

## 4. Strict Engineering Rules
1. **Zero Soundness Regressions (S1–S8):** Never allow unchecked handle sizes/UUIDs/alignments, `Copy` mutable system handles, unguarded `extern "C"` callbacks that can unwind, or `Box`-move pointer retagging UB.
2. **Allocator Isolation:** Unwind payloads, closures, and guest buffers MUST be allocated and dropped strictly on the guest heap.
3. **Miri Dual-Model Gate:** Changes to `instance.rs`, `trampoline.rs`, `iter.rs`, or `commands.rs` MUST pass Miri under both Tree Borrows and Stacked Borrows (`tests/soundness_regressions.rs`).

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" rustup run nightly cargo miri test
MIRIFLAGS="-Zmiri-strict-provenance" rustup run nightly cargo miri test --test soundness_regressions
cargo tree | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `bevy_abi_mod_sys = { workspace = true }` in `Cargo.toml` with explicit crates.io version (`"0.3.0"`) or git/path coordinates.
2. **Preserve MSRV `1.87`:** Keep `rust-version = "1.87"` so guests targeting older Rust compilers than the host continue building cleanly across mismatched toolchains.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
