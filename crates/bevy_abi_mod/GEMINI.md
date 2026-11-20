# Agent Guide: `bevy_abi_mod` (Deprecated Compatibility Shim)

Crate guide for **`bevy_abi_mod`** (`v0.3.0`) — minimal `#[deprecated]` compatibility shim forwarding 1:1 to canonical `bevy_mod_abi`.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Status:** **Deprecated compatibility alias** (`#![deprecated(note = "Use `bevy_mod_abi` instead")]`). New development, features, docs, and integration tests belong in **[`bevy_mod_abi`](../bevy_mod_abi)**, NOT here.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root.
- **Single Dependency:** Depends ONLY on `bevy_mod_abi = { version = "0.3.0", ..., default-features = false }` and forwards all 22 feature flags 1:1 (`strict-ffi-checks`, `diagnostics`, `signature-verification`, `hot-reload-watcher`, `bridge-*`, `sub-app-extract`, `preset-*`).

---

## 3. Owned Surface (`src/lib.rs`)
- Must stay a minimal `<80` LoC (`17` LoC) re-export shim exposing:
  - `pub use bevy_mod_abi;`
  - `pub use bevy_mod_abi::*;`
  - `pub mod prelude { pub use bevy_mod_abi::prelude::*; }`

---

## 4. Strict Engineering Rules
1. **No New Logic or Duplicate Tests:** Never add implementation code, extra dependencies, or duplicated `tests/` suites to this crate — keep all facade logic and tests in `bevy_mod_abi`.
2. **1:1 Feature Flag Parity:** If `bevy_mod_abi/Cargo.toml` gains or updates a feature flag, forward it identically in `Cargo.toml` so legacy dependents continue compiling until removed in a future breaking release.
3. **Zero Non-Test Panics & Strict Clippy Compliance:** Keep `src/lib.rs` clean under `-D warnings` and `RUSTDOCFLAGS="-D warnings"`.

---

## 5. Standalone Verification Commands

```bash
wc -l src/lib.rs   # MUST remain < 80 lines
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo check --no-default-features --features preset-2d
cargo check --no-default-features --features preset-full
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Path Dependency:** Update `bevy_mod_abi = { version = "0.3.0", default-features = false }` in `Cargo.toml` (remove `path = "../bevy_mod_abi"` once published or use git/crates.io dependency).
2. **Lifecycle Plan:** Treat this crate as maintenance-only forwarding shim for `v0.3.x`; keep `README.md` migration guidance pointing to `bevy_mod_abi`.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
