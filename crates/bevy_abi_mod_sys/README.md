# `bevy_abi_mod_sys`

Strictly `#![no_std]`, **zero-dependency** C-ABI `0.2.0` wire contract for [`bevy_abi_mod`](../bevy_abi_mod).

This crate defines every cross-boundary type, descriptor, vtable header, error code, and trait shared by the Bevy host, procedural macros, Rust guest SDK, and non-Rust guests via the auto-generated [`include/bevy_abi_mod.h`](../../include/bevy_abi_mod.h) header:

- **Wire Primitives**: `FfiResult`, `FfiBool`, `StrView`, `FfiSlice<T>`, `FfiEntity`, `FfiChildOf`, `BamComponent`, `ComponentUuid` (`const` SipHash-2-4-128), `ScheduleId`, `AccessMode`, `BamChangeTicks`, `BamColumnInfo`.
- **Extensible Descriptors**: `BamManifest`, `BamHostApi` (`#[repr(C)]` function pointer table with frozen `MIN_V0_2_SIZE`), `BamSystemDesc`, `BamQueryDesc`, `BamTermDesc`, `ComponentDesc`, `BamObserverDesc`, `VTableHeader`, and `ExtensibleStruct` prefix reading.
- **POD & Safety Traits**: `ModComponent`, `RawInsertable`, and `AbiPod` (implemented only for fixed-width, bit-valid, zero-drop C-compatible types).

`bevy_abi_mod_sys` has zero dependencies (not even `alloc` or `std`) so host, proc-macro, and guest crates share one identical binary contract without pulling in Bevy internals.
