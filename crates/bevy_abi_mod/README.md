# `bevy_abi_mod`

Top-level Bevy ECS (`v0.20+`) host facade for the zero-serialization, asymmetric C-ABI (`ABI 0.2.0`) native modding framework.

This crate re-exports [`bevy_abi_mod_host`](../bevy_abi_mod_host), procedural derive macros (`#[derive(ModComponent, AbiPod)]`), [`bevy_abi_mod_sys`](../bevy_abi_mod_sys), and [`prelude`], while enforcing `#![forbid(unsafe_code)]` across all public host APIs.

## Quickstart

```rust,no_run
use bevy_abi_mod::prelude::*;
use bevy_app::App;
use bevy_ecs::component::Component;

#[repr(C)]
#[derive(Component, ModComponent, Clone, Copy, Default)]
#[mod_component(name = "my_game::Health")]
struct Health {
    current: f32,
    max: f32,
}

fn main() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<Health>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Health");
    app.load_mod_with("mods/libcombat_mod.dylib", LoadOptions::default())
        .expect("load mod");
    app.run();
}
```

## Features

- **`strict-ffi-checks`**: Enables extra runtime FFI pointer, alignment, and handle-generation validation in `bevy_abi_mod_host`.
- **`diagnostics`**: Publishes Bevy `Diagnostics` counters for loaded mod counts, active systems, and faulted invocations.
- **`signature-verification`**: Enables optional pure-Rust OpenSSH `SSHSIG` (`.sig` sidecars), Ed25519 CA certificate verification (`ModTrustPolicy`, `VerifiedModSignature`), OpenSSH `SSHKRL` / signed KRL revocation lists (`KrlBuilder`, `with_min_krl_version`) with runtime `evict_revoked_mods()`, and strict Ed25519 curve validation prior to `dlopen` / `LoadLibraryW`. Manage keys, certs, KRLs, and signatures via standard `ssh-keygen -Y`/`-k`/`-Q` or `cargo xtask sign <keygen|cert|file|krl|verify>`.

## Documentation

See the repository [`README.md`](../../README.md) and [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the complete architecture, lifecycle invariants, and polyglot C99/C++17/Zig guest integration guides.
