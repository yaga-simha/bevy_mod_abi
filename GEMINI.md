# Agent Guide: `bevy_abi_mod`

Repository guide defining architectural invariants, crate boundaries, verification commands, and Bevy ECS (`v0.20+`) standards for developing **`bevy_abi_mod`**.

---

## 1. Project Identity
- **Name:** `bevy_abi_mod`
- **Goal:** Production-grade, zero-serialization, asymmetric C-ABI native modding framework for Bevy ECS (`v0.20+`).
- **Core Principle:** **Zero Shared Rust ABI Assumptions**. Guest modules may use mismatched `rustc` versions, debug/release profiles, C99, C++17, or Zig across an `extern "C"` boundary without allocator crossover or serialization.

---

## 2. Workspace Crate Boundaries (`-sys` vs. `#![forbid(unsafe_code)]`)

| Crate / Path | Responsibility | Safety & Allowed Dependencies |
|---|---|---|
| `crates/bevy_abi_mod_sys` | `#[repr(C)]` descriptors, primitives, `extern "C"` table (`BamHostApi`), `ModComponent`, `AbiPod`, `RawInsertable`. | `#![no_std]`, **ZERO external dependencies** (no `bevy`, no `alloc`). |
| `crates/bevy_abi_mod_macros` | `#[derive(ModComponent, AbiPod)]` hygienic VTables & C/Zig codegen. | **`#![forbid(unsafe_code)]`**, `syn`, `quote`, `proc-macro2`, `proc-macro-crate`. **NO `bevy` dependency.** |
| `crates/bevy_abi_mod_host_sys` | Host FFI (`HOST_API`, `HostIter`, `HostCommands` / `CommandArena`, `GuestSystem`, `ModRegistry`, `ModHost`, `DROP_SLOTS`). | `bevy_ecs`, `bevy_app`, `libloading`, `arc-swap`, `tracing`, `thiserror`, `bevy_abi_mod_sys`. Isolates all host `unsafe`. |
| `crates/bevy_abi_mod_host` | Safe Bevy plugin (`AbiModPlugin`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModVisibleSets`, `prelude`). | **`#![forbid(unsafe_code)]`**. `bevy_app`, `bevy_ecs`, `bevy_abi_mod_host_sys`, `bevy_abi_mod_sys`, `bevy_abi_mod_macros`. |
| `crates/bevy_abi_mod` | Top-level Bevy host facade re-exporting `bevy_abi_mod_host`, derives, `bevy_abi_mod_sys`, and `prelude`. | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_host`, `bevy_abi_mod_macros`, `bevy_abi_mod_sys`. |
| `crates/bevy_abi_mod_guest_sys` | Guest FFI (`GuestInstance`, panic guards, `extern "C"` trampolines, validated slice/row/command/resource wrappers). | `bevy_abi_mod_sys` only. **NO `bevy` dependency.** Isolates all guest `unsafe`. |
| `crates/bevy_abi_mod_guest` | Ergonomic safe Rust guest SDK (`GuestPlugin`, `GuestApp`, `SystemBuilder`, `ObserverBuilder`, `SystemContext`, `ChunkIter`, `Chunk`, `GuestScene`, `export_mod!`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_guest_sys`, `bevy_abi_mod_sys`, optional `bevy_abi_mod_macros` (`derive`). **NO `bevy` dependency.** |
| `crates/cargo-bam` | Standalone developer CLI (`new`, `build --sign-key`, `export-sdk`/`header`, `check-mods`, `doctor`, `keygen`, `sign`, `verify`). | **`#![forbid(unsafe_code)]`**. `bevy_abi_mod_sys` plus pure-Rust CLI/signing crates. **NO `bevy` dependency.** |

### Optional Host Feature Flags (`bevy_abi_mod` / `bevy_abi_mod_host`)
- `strict-ffi-checks`, `diagnostics`, `signature-verification`, `hot-reload-watcher`
- `bridge-std-types` (`Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<*>`)
- `bridge-asset` (`ModAssetPlugin` isolated `mod://<namespace>/...` `AssetSource` + refcounted `FfiAssetHandle`)
- `bridge-state` (`ModStateExt` Bevy `States` exposure, transition schedules, `in_state` run conditions)
- `bridge-reflect` (`ModReflectPlugin` witness `DynamicStruct` reflection, `bevy_remote` BRP methods, `bevy_settings` TOML sync)
- `bridge-bsn` (`ModBsnPlugin` Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, `bevy_scene::Ready` observers)
- `sub-app-extract` (`ModSubAppPlugin<L>` cross-world `SubApp` extraction & `ModWeslMaterialPlugin` `.wesl` / `AlignedVec` `ShaderBuffer` / 2D & 3D materials)

---

## 3. Strict Engineering Rules

1. **ABI Purity (`bevy_abi_mod_sys`):** Every wire struct must be `#[repr(C)]` or `#[repr(transparent)]` with zero implicit padding and frozen `MIN_V0_2_SIZE`; all callbacks use `Option<unsafe extern "C" fn(...)>`; strings use `StrView`.
2. **Memory & Allocator Isolation:** Host allocations are freed only by the host; guest allocations are freed only by the guest. Never pass Rust `Box`, `Vec`, `String`, or fat pointers across FFI.
3. **Concurrency & `-sys` Isolation:** Safe crates (`bevy_abi_mod`, `bevy_abi_mod_host`, `bevy_abi_mod_guest`, `bevy_abi_mod_macros`, `cargo-bam`, `xtask`) enforce `#![forbid(unsafe_code)]`. All `unsafe` belongs in `-sys` crates with `// SAFETY:` comments and `# Safety` docs, declaring exact Bevy `FilteredAccessSet` permissions.
4. **Documentation Alignment:** Keep [`spec.md`](./spec.md) and [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) synchronized with any ABI or architectural changes, and verify Rust, ISO C99, C++17, and Zig integration fixtures.

---

## 4. Key Verification Commands

```bash
# Verify zero Bevy dependencies in ABI, macro, guest, & CLI crates (must print NOTHING)
cargo tree -p bevy_abi_mod_sys -p bevy_abi_mod_macros -p bevy_abi_mod_guest_sys -p bevy_abi_mod_guest -p cargo-bam | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy

# Run full lint, header, fixture build, & workspace test suite
cargo xtask ci

# Fast formatting, clippy (default + --all-features), zero-Bevy, header, & C99 conformance checks
cargo xtask lint
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo xtask header --check --test-c99

# Rebuild all fixtures (including examples & benchmarks) and run E2E + Miri + Loom suites
cargo xtask fixtures --all
BAM_FIXTURE_DIR="$(pwd)/target/fixtures" cargo test -p bevy_abi_mod_host --test e2e --all-features -- --test-threads=1
MIRIFLAGS="-Zmiri-tree-borrows" cargo xtask miri --all
cargo test -p bevy_abi_mod_host_sys --test loom_drop_trampoline --release
```

---

## 5. Idiomatic Bevy ECS Standards (`v0.20+`)

- **Borrow Precision:** Prefer `Res<T>` and `&T` over `ResMut<T>` and `&mut T` to maximize scheduler parallelism; narrow queries with `With<T>`/`Without<T>` and `Ref<T>`/`Added<T>`/`Changed<T>`.
- **Single-Responsibility Systems & Ordering:** Use `SystemSet`s ordered via `app.configure_sets(...)`, declarative `run_if(...)`, and `ParamSet` when disjoint mutable queries are required.
- **Modern Bevy Patterns:** Use `#[require(...)]`, `On<E>` observers (`app.add_observer(...)`), and component lifecycle hooks where relational invariants belong, keeping safe host code `#![forbid(unsafe_code)]`.
