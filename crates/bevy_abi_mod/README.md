# `bevy_abi_mod` (Deprecated Compatibility Shim)

> **Deprecated:** **`bevy_abi_mod`** is now a minimal `#![forbid(unsafe_code)]` compatibility shim re-exporting the canonical host facade crate **[`bevy_mod_abi`](../bevy_mod_abi)** (`#[deprecated(note = "Use `bevy_mod_abi` instead")]`).

All APIs, `prelude`, curated bridge presets (`preset-2d`, `preset-3d`, `preset-ui`, `preset-full`), and granular feature flags are forwarded 1:1 from [`bevy_mod_abi`](../bevy_mod_abi).

## Migration

Update your `Cargo.toml`:

```toml
[dependencies]
# Replace:
# bevy_abi_mod = { version = "0.3.0", features = ["bridge-std-types"] }
# With:
bevy_mod_abi = { version = "0.3.0", features = ["preset-2d"] } # or granular features
```

And replace `use bevy_abi_mod::prelude::*;` with `use bevy_mod_abi::prelude::*;`.

See [`crates/bevy_mod_abi/README.md`](../bevy_mod_abi/README.md) and the repository [`README.md`](../../README.md) for current quickstarts, presets, and documentation.
