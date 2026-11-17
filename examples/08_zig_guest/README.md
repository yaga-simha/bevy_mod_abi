# Example 08: Minimal Zig Guest Mod

**Concept taught:** Authoring a Zig guest mod (`zig build`) against `include/bevy_abi_mod.h` (`ABI 0.2.0`) via `@cImport(@cInclude("bevy_abi_mod.h"))`, mapping Zig error unions (`BamError!void`) to `BamResult`, and isolating per-`World` state with `errdefer`.

---

## What It Shows

1. **Zero-glue header import (`@cImport`):** Imports `include/bevy_abi_mod.h` directly in `zig_mod/src/root.zig` with `comptime` layout checks matching the host's `#[repr(C)]` `Position`, `Velocity`, and `ZigSimConfig`.
2. **`callconv(.c)` entry points & `errdefer` rollback:** Exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` using `std.heap.c_allocator` so partial allocations are deterministically freed on init error and destroyed on unload.
3. **Value-based Zig error mapping:** Translates Zig errors (`BamError`) into `BamResult` status codes (`BAM_RESULT_INVALID_ARGUMENT`, `BAM_RESULT_TYPE_MISMATCH`, etc.) at the ABI boundary without unwinding.
4. **Graceful toolchain detection:** If `zig` is not installed on the local machine, the host binary and integration test print a clear notice and exit `0` cleanly, while CI builds and executes the full Zig guest via `goto-bus-stop/setup-zig`.

## Commands

```bash
# Build the Zig guest shared library (requires Zig 0.14+)
(cd examples/08_zig_guest/zig_mod && zig build -Doptimize=ReleaseFast)

# Run the Bevy host example (gracefully skips runtime load if `zig` is absent)
cargo run -p example_08_zig_guest

# Run the integration test
cargo test -p example_08_zig_guest
```
