# `cargo-bam`

Standalone, `#![forbid(unsafe_code)]`, zero-Bevy CLI toolchain (`cargo bam` / `cargo-bam`) for scaffolding, building, validating, signing, and diagnosing `bevy_abi_mod` guest mods targeting host ABI `0.2.0`.

## Installation

```bash
cargo install --path crates/cargo-bam
```

Once installed, run either `cargo bam <subcommand>` or `cargo-bam <subcommand>`.

## Commands

### `new`
Scaffold a complete, buildable guest mod project in Rust, C99, C++17, or Zig:

```bash
cargo bam new my_mod --lang rust --dir ./mods
cargo bam new my_c_mod --lang c99 --dir ./mods
cargo bam new my_cpp_mod --lang cpp17 --dir ./mods
cargo bam new my_zig_mod --lang zig --dir ./mods
```

- `--lang <rust|c99|cpp17|zig>`: Target guest language (default: `rust`).
- `--dir <path>`: Parent directory in which `<name>` is created (default: `.`).
- `--guest-sdk-path <path>`: Optional local path to `crates/bevy_abi_mod_guest` for Rust mods.

### `export-sdk` (alias: `header`)
Export canonical polyglot SDK headers (`bevy_abi_mod.h`, `bevy_abi_mod.hpp`, `bevy_abi_mod.zig`):

```bash
cargo bam export-sdk --out ./include
# or:
cargo bam header --out ./include
```

### `build`
Compile a Rust `.cdylib` or C99 / C++17 / Zig shared library according to `mod.toml` / `Cargo.toml`, optionally staging `<out-dir>/<mod_id>/` and creating an OpenSSH `SSHSIG` `.sig` sidecar:

```bash
cargo bam build --dir ./mods/my_mod --release --out-dir ./dist
cargo bam build --manifest-path ./mods/my_mod/mod.toml --sign-key ./keys/mod_dev --out-dir ./dist
```

- `--manifest-path <path>`: Explicit path to `mod.toml` or `Cargo.toml`.
- `--dir <path>`: Project directory containing `mod.toml` / `Cargo.toml`.
- `--release`: Build with release optimizations.
- `--out-dir <path>`: Stage `<out-dir>/<mod_id>/` with the compiled shared library, `mod.toml`, and optional `.sig`.
- `--sign-key <path>`: OpenSSH Ed25519 private key to detach-sign the built shared library.
- `--sign-namespace <ns>`: `SSHSIG` namespace used when `--sign-key` is passed (default: `bevy-abi-mod`).

### `check-mods`
Discover `mod.toml` manifests inside `<dir>`, validate host `abi_version` (`0.2.0`), verify required/optional SemVer dependencies, reject dependency cycles, and print the deterministic topological load order:

```bash
cargo bam check-mods ./dist
```

### `doctor`
Inspect host ABI (`0.2.0`), Rust (`cargo` / `rustc`), C99 (`CC` / `cc` / `gcc` / `clang` / `cl`), C++17 (`CXX` / `c++` / `g++` / `clang++` / `cl`), optional `zig`, embedded SDK headers, and optionally validate a mods directory:

```bash
cargo bam doctor
cargo bam doctor --require-c --dir ./dist
```

### `keygen`, `sign`, and `verify`
Generate OpenSSH Ed25519 signing keys, create detached `SSHSIG` `.sig` sidecars, and verify guest mod binaries:

```bash
cargo bam keygen --out ./keys/mod_dev --comment "mod-dev@example"
cargo bam sign --key ./keys/mod_dev ./dist/my_mod/libmy_mod.so
cargo bam verify --pubkey ./keys/mod_dev.pub ./dist/my_mod/libmy_mod.so
```

## `mod.toml` Manifest Schema

```toml
[mod]
id = "my_mod"              # Canonical mod ID (falls back to `name` if `id` is omitted)
version = "0.1.0"          # Mod SemVer
abi_version = "^0.2.0"     # Optional host ABI SemVer requirement (checked against 0.2.0)
language = "rust"          # Optional: rust | c99 | cpp17 | zig
entry = "libmy_mod.so"     # Optional explicit library filename (alias: `library`)

[dependencies]
base_mod = "^1.0"

[optional_dependencies]
hud_mod = ">=0.1.0"
```
