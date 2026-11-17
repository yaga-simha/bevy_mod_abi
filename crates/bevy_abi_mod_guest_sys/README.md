# `bevy_abi_mod_guest_sys`

Low-level, **zero-Bevy-dependency** guest FFI runtime for Rust `.cdylib` mods targeting [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.2.0`).

Isolates all guest-side `unsafe` code (`#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`) so [`bevy_abi_mod_guest`](../bevy_abi_mod_guest) enforces `#![forbid(unsafe_code)]`:

- **`Host` & `ValidatedLoadCtx`**: Bounded `BamHostApi` copy, `ABI 0.2.0` SemVer verification, `"bam.v0.*"` custom symbol lookup, and `VTableHeader` checks.
- **`GuestInstance`**: Per-World heap state owner with deterministic LIFO closure teardown and hot-reload `save_state` / `restore_state` trampolines.
- **`panic` & `ReentrantCell`**: `extern "C"` (`nounwind`) panic guards (`catch_guest_result`, `catch_guest_bool`, `catch_guest_unit`) paired with lock-free `UnsafeCell` + `AtomicBool` re-entrancy guards so unwind payloads are dropped strictly on the guest heap.
- **Validated Iterators & Commands**: Layout-, alignment-, UUID-, and length-checked wrappers for dense columns, sparse rows, `GuestRes` change ticks, deferred commands, dynamic component drop/hook trampolines, and observers.

Mod authors should depend on `bevy_abi_mod_guest` rather than calling this `-sys` crate directly.
