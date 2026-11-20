# `bevy_abi_mod_sdk_gen`

Deterministic C99, C++17, Zig, and component header generator (`#![forbid(unsafe_code)]`, zero Bevy dependencies) for [`bevy_mod_abi`](../bevy_mod_abi) (`ABI 0.3.0`, `v0.3.0`).

## Responsibilities

- **Single-Source C99 Header (`bevy_abi_mod.h`):** Generates [`include/bevy_abi_mod.h`](../../include/bevy_abi_mod.h) directly from [`bevy_abi_mod_sys::offsets`](../bevy_abi_mod_sys) metadata, including `BAM_EXPORT`, `BAM_RESTRICT` (`__restrict` on MSVC, `restrict` in C99/C11), all 68 `BAM_HOST_API_OFFSET_*` slot offsets, `BAM_RESULT_CYCLE_DETECTED`, `BAM_RELATION_FLAG_*`, `BAM_SCHEDULE_SET_FLAG_*`, `bam_*` inline helpers, `bam_uuid_from_parts` (`SipHash-2-4-128`), and 64-bit/32-bit `BAM_STATIC_ASSERT` layout assertions.
- **Polyglot SDK Bundle (`sdk::export_sdk`):** Exports `bevy_abi_mod.h`, `bevy_abi_mod.hpp` (C++17 RAII wrappers, `bam::offsets::*`, `BAM_RESTRICT`, `copy_column_restricted`), and `bevy_abi_mod.zig` (Zig `offsets.*`, `BAM_HOST_API_OFFSET_*`, `BAM_RESTRICT_QUALIFIER = "noalias"`, `copyColumnNoAlias`) for [`cargo-bam`](../cargo-bam).
- **Component Header Emission (`component_header`):** Renders C99 component declaration headers with `BAM_STATIC_ASSERT` size/offset checks from `ModComponent::C_DECL` strings.
