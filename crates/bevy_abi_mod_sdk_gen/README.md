# `bevy_abi_mod_sdk_gen`

Deterministic C99, C++17, Zig, and component header generator (`#![forbid(unsafe_code)]`, zero Bevy dependencies) for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`).

## Responsibilities

- **Single-Source C99 Header (`bevy_abi_mod.h`):** Generates [`include/bevy_abi_mod.h`](../../include/bevy_abi_mod.h) directly from [`bevy_abi_mod_sys`](../bevy_abi_mod_sys) metadata, including `BAM_EXPORT`, all 68 `BAM_HOST_API_OFFSET_*` slot offsets, `bam_*` inline helpers, `bam_uuid_from_parts` (`SipHash-2-4-128`), and 64-bit/32-bit `BAM_STATIC_ASSERT` layout assertions.
- **Polyglot SDK Bundle (`sdk::export_sdk`):** Exports `bevy_abi_mod.h`, `bevy_abi_mod.hpp` (C++17 RAII wrappers), and `bevy_abi_mod.zig` (Zig helpers) for [`cargo-bam`](../cargo-bam).
- **Component Header Emission (`component_header`):** Renders C99 component declaration headers with `BAM_STATIC_ASSERT` size/offset checks from `ModComponent::C_DECL` strings.
