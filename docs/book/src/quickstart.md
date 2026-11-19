# 5-Minute Quickstart

This walkthrough builds a minimal Bevy `0.20+` host and a decoupled Rust `.cdylib` guest mod communicating over `ABI 0.3.0` (`0x0003_0000`) with zero `bevy` dependencies in the guest.

> **Tip:** To scaffold, watch, and package a mod project automatically in Rust, C99, C++17, or Zig, install `cargo-bam` (`cargo install --path crates/cargo-bam`) and run `cargo bam new my_guest_mod --lang rust` — see [Bridges, Render, Reflect, Trust & `cargo-bam`](./bridges-and-tooling.md).

---

## 1. Host Application (`bevy_mod_abi` / `bevy_abi_mod`)

`Cargo.toml`:
```toml
[dependencies]
bevy_app = "0.20"
bevy_ecs = "0.20"
bevy_mod_abi = "0.3.0"
```

`src/main.rs`:
```rust
use bevy_mod_abi::prelude::*;
use bevy_app::App;
use bevy_ecs::component::Component;

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

fn main() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));

    // Expose `my_game::Position` for zero-copy slices + raw/default deferred insertion
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Position");

    let entity = app
        .world_mut()
        .spawn(Position { x: 1.0, y: 2.0, z: 3.0 })
        .id();

    // Shadow-copy load avoids Windows DLL locks and macOS Apple Silicon Mach-O overwrite SIGKILLs
    let mod_id = app
        .load_mod_with(
            "path/to/libmy_guest_mod.dylib",
            LoadOptions {
                shadow_copy: true,
                strict: true,
                ..Default::default()
            },
        )
        .expect("load guest mod");

    app.update();
    println!("Updated position: {:?}", app.world().get::<Position>(entity));

    app.unload_mod(mod_id).expect("unload guest mod");
}
```

---

## 2. Standalone Rust Guest Mod (`bevy_abi_mod_guest`)

Create a separate crate (`[workspace]` standalone `Cargo.lock`, zero `bevy` dependencies):

`guest_mod/Cargo.toml`:
```toml
[package]
name = "my_guest_mod"
version = "0.3.0"
edition = "2024"

[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = { version = "0.3.0", features = ["derive"] }
```

`guest_mod/src/lib.rs`:
```rust
use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "my_game::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static CStr = c"my_game::Position";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[derive(Default)]
pub struct MyGuestMod;

impl GuestPlugin for MyGuestMod {
    const NAME: &'static CStr = c"my_guest_mod";
    const VERSION: &'static CStr = c"0.3.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"nudge_positions")
            .query(|q| q.write::<Position>())
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(positions) = chunk.component_mut::<Position>(0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for p in positions {
                        p.x += 1.0;
                        p.y += 2.0;
                        p.z += 3.0;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: MyGuestMod);
```

Build the guest with `cargo build --manifest-path guest_mod/Cargo.toml --release` (or `cargo bam build --dir guest_mod --release`) and run your host! See [`examples/01_minimal_handshake`](../../../examples/01_minimal_handshake) for a complete runnable project.
