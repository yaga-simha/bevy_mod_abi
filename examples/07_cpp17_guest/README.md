# Example 07: Minimal C++17 Guest Mod

**Concept taught:** Authoring a strict ISO C++17 guest mod (`-std=c++17 -Wall -Wextra -Wpedantic -Werror`) against `include/bevy_abi_mod.h` (`ABI 0.2.0`) with RAII per-`World` `BamInstance` ownership and C++ exception containment (`BAM_RESULT_PANIC_CAUGHT`).

---

## What It Shows

1. **`extern "C"` ABI `0.2.0` entry points (`BAM_EXPORT`):** Exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` without name mangling.
2. **RAII per-`World` `CppModInstance`:** Allocates per-instance state via `std::make_unique<CppModInstance>`, releasing ownership to `*instance_out` only after `register_system` succeeds and deleting it inside `bevy_abi_mod_shutdown`.
3. **Exception containment at `nounwind` boundary:** Wraps `init` and system callbacks in `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }` so C++ exceptions never unwind across `extern "C"` or cross allocators, allowing the Bevy host to observe `ModSystemFaulted` and apply `ModErrorPolicy`.
4. **Zero-copy chunk & resource access:** Resolves `example::Position`, `example::Velocity`, and `example::SimConfig` once during `init` and mutates contiguous SoA `Table` columns directly in C++17.

## Commands

```bash
# Compile the C++17 shared library
make -C examples/07_cpp17_guest/cpp_mod

# Run the Bevy host example
cargo run -p example_07_cpp17_guest

# Run the end-to-end integration test (including C++ exception fault containment)
cargo test -p example_07_cpp17_guest
```
