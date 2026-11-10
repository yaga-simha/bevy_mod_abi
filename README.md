# `bevy_abi_mod`

**Asymmetric, Zero-Serialization C-ABI Modding Framework for Bevy ECS**

`bevy_abi_mod` is an industrial-grade native plugin and modding architecture designed for Bevy Engine. It enables loading external native shared libraries (`.so`, `.dylib`, `.dll`) compiled with different Rust toolchains or written in C99, C++, or Zig without sharing Rust ABI assumptions, without memory allocator crossover, and without JSON serialization overhead.

## Architecture

See [spec.md](spec.md) for full technical specifications and [gemini.md](gemini.md) for agentic engineering guidelines.

### Workspace Crates
- **`crates/abi_contract`**: Zero-dependency `#[no_std]` C-ABI structs, function pointer types, and status codes.
- **`crates/mod_derive`**: Proc-macro providing `#[derive(ModExposable)]` for Bevy components.
- **`crates/host_runtime`**: Bevy host plugin, dynamic system adapter (`DynamicCSystem`), VTable registry, and `libloading` lifecycle manager.
- **`crates/guest_sdk`**: Lightweight, Bevy-independent safe Rust wrapper for mod developers.

### Test Suites
- **`tests/asymmetric_rust_mod`**: Standalone Cargo project (independent lockfile) compiled to `.cdylib`.
- **`tests/asymmetric_c_mod`**: Pure C99 mod verifying cross-language ABI stability.
