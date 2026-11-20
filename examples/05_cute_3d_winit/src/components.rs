//! Host ECS components and UI mod-slot state for Example 05: Cute 3D Slime Meadow (`winit` + `wgpu`).

use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use bevy_mod_abi::{ModComponent, ModId};

/// 3D world-space position of a slime spirit (`cute3d::Position`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "cute3d::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

impl Position {
    /// Creates a new [`Position`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// 3D linear velocity of a slime spirit (`cute3d::Velocity`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "cute3d::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

impl Velocity {
    /// Creates a new [`Velocity`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Non-uniform 3D squash-and-stretch scale (`cute3d::Scale3D`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "cute3d::Scale3D")]
pub struct Scale3D {
    /// X scale factor.
    pub x: f32,
    /// Y scale factor.
    pub y: f32,
    /// Z scale factor.
    pub z: f32,
}

impl Scale3D {
    /// Creates a uniform [`Scale3D`].
    pub const fn uniform(s: f32) -> Self {
        Self { x: s, y: s, z: s }
    }
}

/// Pastel RGBA color of a slime spirit's body (`cute3d::ColorRgba`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "cute3d::ColorRgba")]
pub struct ColorRgba {
    /// Red channel in `[0.0, 1.0]`.
    pub r: f32,
    /// Green channel in `[0.0, 1.0]`.
    pub g: f32,
    /// Blue channel in `[0.0, 1.0]`.
    pub b: f32,
    /// Alpha channel in `[0.0, 1.0]`.
    pub a: f32,
}

impl ColorRgba {
    /// Creates a new [`ColorRgba`].
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

/// Expressive animation parameters mutated by dynamic guest mods (`cute3d::CuteSpirit`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "cute3d::CuteSpirit")]
pub struct CuteSpirit {
    /// Facing angle in radians around the Y axis.
    pub yaw: f32,
    /// Bunny-ear tilt angle in radians.
    pub ear_wiggle: f32,
    /// Cheek blush intensity in `[0.0, 1.0]`.
    pub blush: f32,
}

impl CuteSpirit {
    /// Creates a new [`CuteSpirit`].
    pub const fn new(yaw: f32, ear_wiggle: f32, blush: f32) -> Self {
        Self {
            yaw,
            ear_wiggle,
            blush,
        }
    }
}

/// Host-private baseline color and phase so unloading a mod restores each slime's original look.
#[derive(Component, Debug, Clone, Copy)]
pub struct HomeStyle {
    /// Original body coat tint.
    pub base_color: ColorRgba,
    /// Per-slime idle breathing phase offset.
    pub phase_offset: f32,
}

/// Active 3D mod slot selection in the Cute Slime Meadow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ActiveMod {
    /// Baseline meadow stroll with no active guest mod.
    #[default]
    None,
    /// Bouncy 3D hops and squash-stretch (`mod_boing`, Rust `.cdylib`).
    Boing,
    /// Counter-rotating 3D carousel waltz (`mod_carousel`, Rust `.cdylib`).
    Carousel,
    /// Sunflower cuddle huddle (`mod_cuddle_c99`, ISO C99).
    CuddleC99,
}

impl ActiveMod {
    /// Human-readable display label.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "Meadow Stroll (Baseline)",
            Self::Boing => "Jelly Boing Hop (Rust cdylib)",
            Self::Carousel => "Pastel Carousel Waltz (Rust cdylib)",
            Self::CuddleC99 => "Cozy Cuddle Huddle (Pure C99)",
        }
    }

    /// Short uppercase badge label for the pixel HUD.
    pub const fn short_badge(self) -> &'static str {
        match self {
            Self::None => "IDLE STROLL",
            Self::Boing => "MOD 1: JELLY BOING (RUST)",
            Self::Carousel => "MOD 2: PASTEL CAROUSEL (RUST)",
            Self::CuddleC99 => "MOD 3: COZY CUDDLE (C99)",
        }
    }

    /// Accent RGB tint for the pixel HUD badge.
    pub const fn accent_rgb(self) -> [f32; 3] {
        match self {
            Self::None => [0.58, 0.86, 0.72],
            Self::Boing => [1.00, 0.62, 0.76],
            Self::Carousel => [0.76, 0.68, 1.00],
            Self::CuddleC99 => [1.00, 0.78, 0.52],
        }
    }

    /// Shared-library file stem on disk (`None` when unloaded).
    pub const fn binary_stem(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Boing => Some("mod_boing"),
            Self::Carousel => Some("mod_carousel"),
            Self::CuddleC99 => Some("mod_cuddle"),
        }
    }
}

/// ECS [`Resource`] mirroring current mod slot state for 3D HUD and window title rendering.
#[derive(Resource, Debug, Clone)]
pub struct ModStatusResource {
    /// Currently active mod slot.
    pub active_mod: ActiveMod,
    /// Loaded [`ModId`] assigned by the host loader, if any.
    pub mod_id: Option<ModId>,
    /// Number of hot-reloads performed on the current mod slot.
    pub reload_count: u32,
    /// Cumulative system faults reported via `ModSystemFaulted`.
    pub fault_count: usize,
    /// Latest status message.
    pub status_message: String,
}

impl Default for ModStatusResource {
    fn default() -> Self {
        Self {
            active_mod: ActiveMod::None,
            mod_id: None,
            reload_count: 0,
            fault_count: 0,
            status_message: "Slimes are strolling peacefully. Press [1] [2] [3] to load 3D mods."
                .to_string(),
        }
    }
}
