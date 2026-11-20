# Agent Guide: `bevy_abi_mod_sdk_gen`

Crate guide for **`bevy_abi_mod_sdk_gen`** (`v0.3.0`, ABI `0x0003_0000`) — deterministic C99, C++17, Zig, and component header generator.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_sdk_gen` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Generates the canonical ISO C99 ABI header (`include/bevy_abi_mod.h`), C++17 RAII wrapper (`bevy_abi_mod.hpp`), Zig module bindings (`bevy_abi_mod.zig`), and `ModComponent::C_DECL` component headers directly from `bevy_abi_mod_sys::offsets` and ABI constants.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root.
- **Zero Bevy Dependencies:** Depends strictly on `bevy_abi_mod_sys` ONLY. **NEVER** add any `bevy*` dependency or build-time toolchain subprocess invocations inside library code.

---

## 3. Owned Surface
- **C99 Header Generator (`header_gen.rs`):** `render_c99_header` and `write_or_check_header` emitting deterministic `include/bevy_abi_mod.h` (`BAM_EXPORT`, `BAM_RESTRICT`, all 68 `BAM_HOST_API_OFFSET_*` constants, `BAM_RESULT_CYCLE_DETECTED`, `BAM_RELATION_FLAG_*`, `BAM_SCHEDULE_SET_FLAG_*`, `bam_uuid_from_parts`, inline helpers, and 64-bit/32-bit `BAM_STATIC_ASSERT` layout guards).
- **Polyglot SDK Bundle (`sdk.rs`):** `export_sdk` writing `bevy_abi_mod.h`, `bevy_abi_mod.hpp` (C++17 RAII + `bam::offsets::*` + `copy_column_restricted`), and `bevy_abi_mod.zig` (`offsets.*`, `BAM_RESTRICT_QUALIFIER = "noalias"`, `copyColumnNoAlias`) used by `cargo bam export-sdk`.
- **Component Headers (`component_header.rs`):** `render_component_header` and `section_for` rendering C99 component layout assertions from `ModComponent::C_DECL`.

---

## 4. Strict Engineering Rules
1. **Single Source of Truth:** All generated offsets, struct sizes, flags, and error codes MUST come directly from `bevy_abi_mod_sys` (`offsets::OFF64_*`, `offsets::OFF32_*`, `SLOT_COUNT = 68`) — never hardcode duplicate wire numbers.
2. **Deterministic Byte-Identical Output:** Generated headers must contain zero timestamps, machine paths, or randomized ordering so `--check` modes in CI and `cargo bam` are deterministic.
3. **Strict Standard Conformance:** C99 output must compile cleanly under strict `-std=c99 -Wall -Wextra -Werror -pedantic`; C++17 output must compile under `-std=c++17`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo tree | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** In `Cargo.toml`, replace `bevy_abi_mod_sys = { workspace = true }` with explicit crates.io version (`"0.3.0"`) or git/path coordinates.
2. **Header Synchronization Note:** In the monorepo, `cargo xtask header --check --test-c99` verifies `include/bevy_abi_mod.h` against `render_c99_header()`; after splitting, keep a C99 header conformance test in CI (e.g., compiling `render_c99_header()` output with `cc`/`clang -std=c99 -Wall -Wextra -Werror`).
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`; update `repository` in `Cargo.toml` if moved to its own Git repo.
