# Example 03: Cross-Language Interoperability

**Concept taught:** Co-scheduling an ISO C99 dynamic library (`c_mod/mod.c` via `include/bevy_abi_mod.h`) and an independent Rust `.cdylib` (`rust_mod`) over the same Bevy entities using `"bam.v0.<canonical_name>.vtable"` VTables and per-`World` instance state.

---

## What It Shows

1. **ISO C99 per-`World` mod (`c_mod/mod.c`):** Exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init(api, load_ctx, instance_out)`, and `bevy_abi_mod_shutdown(instance)` with a heap-allocated `ModState*` (zero process globals), bounded `bam_host_api_copy_bounded`, pre-resolved `BamComponent` handles, and `BamVTableHeader`-validated VTable calls checking `BamResult`.
2. **Rust VTable interop (`rust_mod`):** Resolves `"bam.v0.example::Position.vtable"` and `"bam.v0.example::ColorRgba.vtable"` via `Host::resolve_vtable`, verifying `VTableHeader` (`struct_size`, `abi_version`, and `ComponentUuid`) and checking `FfiResult` on every setter.
3. **Automatic Bevy scheduler coordination:** The C99 mod writes `example::Position` and reads `example::Velocity`; the Rust mod reads `example::Position` and writes `example::ColorRgba`. Bevy's `FilteredAccessSet` orders conflicting accesses automatically while propagating change ticks to host `Changed<T>` queries.

## Commands

```bash
# Build ISO C99 guest mod (-std=c99 -Wall -Wextra -Wpedantic -Werror)
cargo xtask fixtures

# Build standalone Rust guest .cdylib
cargo build --manifest-path examples/03_cross_language_interop/rust_mod/Cargo.toml --release

# Run the host binary
cargo run -p example_03_cross_language_interop
```
