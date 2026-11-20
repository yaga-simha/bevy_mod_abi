# Agent Guide: `bevy_abi_mod_host_sys`

Crate guide for **`bevy_abi_mod_host_sys`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — the low-level host FFI execution engine, CoW registry, chunk/sparse iterators, command arena, and dynamic loader.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_host_sys` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Isolates ALL host-side `unsafe` FFI and Bevy ECS raw storage access behind documented safety contracts so `bevy_abi_mod_host`, companion crates, and `bevy_mod_abi` enforce `#![forbid(unsafe_code)]`.

---

## 2. Safety & Dependency Boundaries
- **`unsafe` Policy:** Enforces `unsafe_op_in_unsafe_fn = "deny"`, `improper_ctypes_definitions = "deny"`, `#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`. Every `unsafe` block MUST have a `// SAFETY:` comment; every `unsafe fn` MUST have `# Safety` docs. Keep inline `tests/unit/*_tests.rs` cleanly separated via `#[path = ...]`.
- **Allowed Dependencies:** `bevy_abi_mod_sys`, `bevy_ecs`, `bevy_app`, `bevy_platform`, `bevy_tasks`, `libloading`, `nonmax`, `arc-swap`, `thiserror`, `tracing`, plus strictly optional `bevy_abi_mod_trust` (`signature-verification`) and `bevy_reflect` + `bevy_ecs/bevy_reflect` (`bridge-reflect`). **NEVER** add `bevy_render`, `bevy_scene`, `bevy_asset`, or `notify`.

---

## 3. Owned Surface & Feature Flags
- **Features:** `strict-ffi-checks`, `test-utils`, `diagnostics`, `signature-verification`, `bridge-reflect`.
- **68/68 `HOST_API` (`ffi/`):** Sealed `pub(crate)` FFI submodules (`host_api`, `iter`, `commands`, `register`, `relations`, `observers_ext`, `v03_ext`, `validation`), consolidated `handle_header.rs` (`AlignedBuf`), `ffi_try!`, and `HostIter` + `CascadeHierarchyCache` + zero-arena `par_for_each_chunk_readonly`.
- **Registry & Drop Safety (`registry/`, `drop_trampoline.rs`, `sync_ptr.rs`):** Generation-tracked `Arc<RegistrySnapshot>` `ModRegistry`, `ComponentOptions::with_summary_tick()` (auto-inheriting `T::HAS_SUMMARY_TICK` in `ComponentOptions::<T>::new()`), `ModRelationGraph` (`allows_self_referential()`, opt-in `__bam_rel_bind` self-referential hook), `BamSymbolTable`, `ModNamedObjects`, `DROP_PAGES` (`ArcSwapOption` wait-free drop trampolines) & `WorldDropGuard`.
- **Scheduling, Commands & Loader (`system/`, `schedule.rs`, `loader/`):** `GuestSystem` with exact `FilteredAccessSet<ComponentId>`, `ModScheduleSetRegistry` + `ModScheduleSetRewirePass` (`ScheduleSetConfig`, Bevy `0.20` `.chain_weak()`), observer & system fault routing through Bevy `0.20.1` `FallbackErrorHandler` (`ErrorContext::Observer` / `ErrorContext::System`), `CommandArena` (`cmd_checkpoint`/`cmd_rollback`), `ModHost` shadow-copy loading, transactional rollback, `despawn_all_where`, and `migrate_component_column`.

---

## 4. Strict Engineering Rules
1. **ABI & Allocator Isolation:** Host allocations are freed only by the host; guest allocations only by the guest. Never change `BamHostApi` (`68` slots) or `ABI 0.3.0` wire layouts.
2. **Zero Non-Test Panics:** Keep non-test `.unwrap()` / `unreachable!()` / `#[allow(...)]` at zero; only explicit trait-boundary `.expect()`s on Bevy invariants are acceptable.
3. **Miri & Loom Parity:** Any change to `drop_trampoline`, `sync_ptr`, `handle_header`, `ffi/iter`, `ffi/commands`, or `ffi/validation` MUST pass Miri (`-Zmiri-tree-borrows`) and `loom_drop_trampoline`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
BAM_FIXTURE_DIR="/path/to/fixtures" cargo test --all-features
cargo test --test loom_drop_trampoline --release
MIRIFLAGS="-Zmiri-tree-borrows" rustup run nightly cargo miri test --lib
MIRIFLAGS="-Zmiri-tree-borrows" rustup run nightly cargo miri test --lib --features bridge-reflect
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_sys`, `bevy_abi_mod_trust`, `bevy_abi_mod_macros`, `bevy_*` (`0.20.1`), `libloading`, `nonmax`, `arc-swap`, `thiserror`, `tracing`, `loom`, `proptest`, and `ssh-key` in `Cargo.toml`.
2. **Fixture Path Dependency:** Integration tests like `host_sys_integration_tests.rs` load compiled guest `.dylib`/`.so` fixtures via `BAM_FIXTURE_DIR`; build/provide compiled guest fixture binaries in CI or skip E2E fixture tests cleanly when absent.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
