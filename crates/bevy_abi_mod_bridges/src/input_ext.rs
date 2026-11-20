//! Extended Bevy `0.20` input bridges (`ModInputExtBridgePlugin`): accumulated mouse motion &
//! scroll resources, plus entity-based `Gamepad` state and haptic `GamepadRumbleRequest` bridges.
//!
//! - Always available when either `bridge-picking` or `bridge-std-types` is enabled:
//!   - [`FfiMouseMotion`] resource synced each frame from [`AccumulatedMouseMotion`].
//!   - [`FfiMouseScroll`] resource synced each frame from [`AccumulatedMouseScroll`].
//! - Available when `bridge-picking` is enabled (activating `bevy_input/gamepad`):
//!   - `FfiGamepadState` entity component synced from Bevy `0.20` `Gamepad` entities (64-bit
//!     digital button masks + left/right stick, triggers, Z axes, D-pad, and USB IDs).
//!   - `FfiGamepadRumble` POD message translating guest haptic requests into Bevy
//!     `GamepadRumbleRequest` messages.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{ComponentCollisionError, ModRegistry};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::assert_abi_pod;
use bevy_app::{App, Plugin, PreUpdate};
use bevy_ecs::error::{ErrorContext, error as log_ecs_error};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::system::{Res, ResMut};
use bevy_ecs::world::World;
use bevy_input::InputSystems;
use bevy_input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit};
use bevy_math::Vec2;

#[cfg(feature = "bridge-picking")]
use {
    bevy_abi_mod_host_sys::ComponentOptions,
    bevy_abi_mod_macros::AbiPod,
    bevy_abi_mod_sys::{FfiBool, FfiEntity},
    bevy_app::PostUpdate,
    bevy_ecs::component::Component,
    bevy_ecs::entity::Entity,
    bevy_ecs::lifecycle::RemovedComponents,
    bevy_ecs::message::{Message, MessageReader, MessageWriter, Messages},
    bevy_ecs::query::{Changed, Or, With, Without},
    bevy_ecs::system::{Commands, Query},
    bevy_input::gamepad::{
        Gamepad, GamepadAxis, GamepadButton, GamepadRumbleIntensity, GamepadRumbleRequest,
    },
    core::time::Duration,
};

/// Canonical ABI name for bridged [`AccumulatedMouseMotion`] (`FfiMouseMotion`).
pub const CANONICAL_MOUSE_MOTION_NAME: &str = "bevy_input::mouse::AccumulatedMouseMotion";
/// Canonical ABI name for bridged [`AccumulatedMouseScroll`] (`FfiMouseScroll`).
pub const CANONICAL_MOUSE_SCROLL_NAME: &str = "bevy_input::mouse::AccumulatedMouseScroll";

/// Canonical ABI name for bridged [`Gamepad`] (`FfiGamepadState`).
#[cfg(feature = "bridge-picking")]
pub const CANONICAL_GAMEPAD_STATE_NAME: &str = "bevy_input::gamepad::Gamepad";
/// Canonical ABI name for bridged [`GamepadRumbleRequest`] (`FfiGamepadRumble`).
#[cfg(feature = "bridge-picking")]
pub const CANONICAL_GAMEPAD_RUMBLE_NAME: &str = "bevy_input::gamepad::GamepadRumbleRequest";

/// Zero-padding `#[repr(C)]` POD resource projecting Bevy's [`AccumulatedMouseMotion`] each frame.
#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_input::mouse::AccumulatedMouseMotion")]
pub struct FfiMouseMotion {
    /// Total mouse motion delta `[dx, dy]` accumulated during the current frame.
    pub delta: [f32; 2],
}

impl FfiMouseMotion {
    /// Projects Bevy's [`AccumulatedMouseMotion`] into [`FfiMouseMotion`].
    #[inline]
    pub const fn from_bevy(motion: &AccumulatedMouseMotion) -> Self {
        Self {
            delta: [motion.delta.x, motion.delta.y],
        }
    }

    /// Returns [`Self::delta`] as a Bevy [`Vec2`].
    #[inline]
    pub const fn delta_vec2(&self) -> Vec2 {
        Vec2::new(self.delta[0], self.delta[1])
    }
}

/// Zero-padding `#[repr(C)]` POD resource projecting Bevy's [`AccumulatedMouseScroll`] each frame.
#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_input::mouse::AccumulatedMouseScroll")]
pub struct FfiMouseScroll {
    /// Total mouse scroll delta `[x, y]` accumulated during the current frame.
    pub delta: [f32; 2],
    /// Scroll unit discriminant ([`Self::UNIT_LINE`] = `0`, [`Self::UNIT_PIXEL`] = `1`).
    pub unit: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

impl FfiMouseScroll {
    /// Scroll delta measured in lines/rows ([`MouseScrollUnit::Line`] = `0`).
    pub const UNIT_LINE: u8 = 0;
    /// Scroll delta measured in pixels ([`MouseScrollUnit::Pixel`] = `1`).
    pub const UNIT_PIXEL: u8 = 1;

    /// Projects Bevy's [`AccumulatedMouseScroll`] into [`FfiMouseScroll`].
    #[inline]
    pub const fn from_bevy(scroll: &AccumulatedMouseScroll) -> Self {
        let unit = match scroll.unit {
            MouseScrollUnit::Line => Self::UNIT_LINE,
            MouseScrollUnit::Pixel => Self::UNIT_PIXEL,
        };
        Self {
            delta: [scroll.delta.x, scroll.delta.y],
            unit,
            reserved0: [0; 3],
        }
    }

    /// Returns [`Self::delta`] as a Bevy [`Vec2`].
    #[inline]
    pub const fn delta_vec2(&self) -> Vec2 {
        Vec2::new(self.delta[0], self.delta[1])
    }

    /// Decodes [`Self::unit`] into a Bevy [`MouseScrollUnit`], or `None` if unknown.
    #[inline]
    pub const fn scroll_unit(&self) -> Option<MouseScrollUnit> {
        match self.unit {
            Self::UNIT_LINE => Some(MouseScrollUnit::Line),
            Self::UNIT_PIXEL => Some(MouseScrollUnit::Pixel),
            _ => None,
        }
    }
}

/// Deterministic bit-index (`0..64`) mirroring Bevy's [`GamepadButton`] variants.
#[cfg(feature = "bridge-picking")]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiGamepadButton(pub u8);

#[cfg(feature = "bridge-picking")]
impl FfiGamepadButton {
    /// Bottom face button (PS Cross / Xbox A = `0`).
    pub const SOUTH: Self = Self(0);
    /// Right face button (PS Circle / Xbox B = `1`).
    pub const EAST: Self = Self(1);
    /// Top face button (PS Triangle / Xbox Y = `2`).
    pub const NORTH: Self = Self(2);
    /// Left face button (PS Square / Xbox X = `3`).
    pub const WEST: Self = Self(3);
    /// C button (`4`).
    pub const C: Self = Self(4);
    /// Z button (`5`).
    pub const Z: Self = Self(5);
    /// First left bumper/trigger (`6`).
    pub const LEFT_TRIGGER: Self = Self(6);
    /// Second left analog trigger (`7`).
    pub const LEFT_TRIGGER2: Self = Self(7);
    /// First right bumper/trigger (`8`).
    pub const RIGHT_TRIGGER: Self = Self(8);
    /// Second right analog trigger (`9`).
    pub const RIGHT_TRIGGER2: Self = Self(9);
    /// Select / Back button (`10`).
    pub const SELECT: Self = Self(10);
    /// Start / Menu button (`11`).
    pub const START: Self = Self(11);
    /// Mode / Guide button (`12`).
    pub const MODE: Self = Self(12);
    /// Left stick click (`13`).
    pub const LEFT_THUMB: Self = Self(13);
    /// Right stick click (`14`).
    pub const RIGHT_THUMB: Self = Self(14);
    /// D-pad up (`15`).
    pub const DPAD_UP: Self = Self(15);
    /// D-pad down (`16`).
    pub const DPAD_DOWN: Self = Self(16);
    /// D-pad left (`17`).
    pub const DPAD_LEFT: Self = Self(17);
    /// D-pad right (`18`).
    pub const DPAD_RIGHT: Self = Self(18);
    /// First bit index allocated to [`GamepadButton::Other`] (`19..64`).
    pub const OTHER_BASE: u8 = 19;

    /// Converts a Bevy [`GamepadButton`] into its deterministic bitset index (`0..64`).
    #[inline]
    pub const fn from_bevy(button: GamepadButton) -> Option<Self> {
        match button {
            GamepadButton::South => Some(Self::SOUTH),
            GamepadButton::East => Some(Self::EAST),
            GamepadButton::North => Some(Self::NORTH),
            GamepadButton::West => Some(Self::WEST),
            GamepadButton::C => Some(Self::C),
            GamepadButton::Z => Some(Self::Z),
            GamepadButton::LeftTrigger => Some(Self::LEFT_TRIGGER),
            GamepadButton::LeftTrigger2 => Some(Self::LEFT_TRIGGER2),
            GamepadButton::RightTrigger => Some(Self::RIGHT_TRIGGER),
            GamepadButton::RightTrigger2 => Some(Self::RIGHT_TRIGGER2),
            GamepadButton::Select => Some(Self::SELECT),
            GamepadButton::Start => Some(Self::START),
            GamepadButton::Mode => Some(Self::MODE),
            GamepadButton::LeftThumb => Some(Self::LEFT_THUMB),
            GamepadButton::RightThumb => Some(Self::RIGHT_THUMB),
            GamepadButton::DPadUp => Some(Self::DPAD_UP),
            GamepadButton::DPadDown => Some(Self::DPAD_DOWN),
            GamepadButton::DPadLeft => Some(Self::DPAD_LEFT),
            GamepadButton::DPadRight => Some(Self::DPAD_RIGHT),
            GamepadButton::Other(n) => {
                let idx = (Self::OTHER_BASE as u32).saturating_add(n);
                if idx < 64 {
                    Some(Self(idx as u8))
                } else {
                    None
                }
            }
        }
    }

    /// Converts this bitset index back into a Bevy [`GamepadButton`].
    #[inline]
    pub const fn to_bevy(self) -> Option<GamepadButton> {
        match self.0 {
            0 => Some(GamepadButton::South),
            1 => Some(GamepadButton::East),
            2 => Some(GamepadButton::North),
            3 => Some(GamepadButton::West),
            4 => Some(GamepadButton::C),
            5 => Some(GamepadButton::Z),
            6 => Some(GamepadButton::LeftTrigger),
            7 => Some(GamepadButton::LeftTrigger2),
            8 => Some(GamepadButton::RightTrigger),
            9 => Some(GamepadButton::RightTrigger2),
            10 => Some(GamepadButton::Select),
            11 => Some(GamepadButton::Start),
            12 => Some(GamepadButton::Mode),
            13 => Some(GamepadButton::LeftThumb),
            14 => Some(GamepadButton::RightThumb),
            15 => Some(GamepadButton::DPadUp),
            16 => Some(GamepadButton::DPadDown),
            17 => Some(GamepadButton::DPadLeft),
            18 => Some(GamepadButton::DPadRight),
            n if n < 64 => Some(GamepadButton::Other((n - Self::OTHER_BASE) as u32)),
            _ => None,
        }
    }

    /// Returns the single-bit mask (`1u64 << self.0`), or `0` if out of range (`>= 64`).
    #[inline]
    pub const fn bit(self) -> u64 {
        if self.0 < 64 { 1u64 << self.0 } else { 0 }
    }
}

/// Zero-padding `#[repr(C)]` entity component mirroring a Bevy `0.20` [`Gamepad`] entity.
///
/// Synchronized onto every entity carrying Bevy's [`Gamepad`] component during [`PreUpdate`]
/// after [`InputSystems`] so guests can iterate connected gamepads as ordinary ECS entities.
#[cfg(feature = "bridge-picking")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_input::gamepad::Gamepad")]
pub struct FfiGamepadState {
    /// 64-bit bitmask of currently pressed [`FfiGamepadButton`]s.
    pub pressed: u64,
    /// 64-bit bitmask of [`FfiGamepadButton`]s pressed during the current frame.
    pub just_pressed: u64,
    /// 64-bit bitmask of [`FfiGamepadButton`]s released during the current frame.
    pub just_released: u64,
    /// Left analog stick `[x, y]` in `[-1.0, 1.0]`.
    pub left_stick: [f32; 2],
    /// Right analog stick `[x, y]` in `[-1.0, 1.0]`.
    pub right_stick: [f32; 2],
    /// Left analog trigger (`GamepadButton::LeftTrigger2`) in `[0.0, 1.0]`.
    pub left_trigger: f32,
    /// Right analog trigger (`GamepadButton::RightTrigger2`) in `[0.0, 1.0]`.
    pub right_trigger: f32,
    /// Left Z axis (`GamepadAxis::LeftZ`) in `[-1.0, 1.0]`.
    pub left_z: f32,
    /// Right Z axis (`GamepadAxis::RightZ`) in `[-1.0, 1.0]`.
    pub right_z: f32,
    /// Directional pad vector `[x, y]` in `[-1.0, 1.0]`.
    pub dpad: [f32; 2],
    /// USB vendor ID when [`Self::has_vendor_id`] is true.
    pub vendor_id: u16,
    /// USB product ID when [`Self::has_product_id`] is true.
    pub product_id: u16,
    /// Whether [`Self::vendor_id`] is populated.
    pub has_vendor_id: FfiBool,
    /// Whether [`Self::product_id`] is populated.
    pub has_product_id: FfiBool,
    /// Whether the underlying [`Gamepad`] component is connected/active.
    pub connected: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

#[cfg(feature = "bridge-picking")]
impl FfiGamepadState {
    /// Projects a Bevy [`Gamepad`] component into [`FfiGamepadState`].
    pub fn from_bevy(gamepad: &Gamepad) -> Self {
        let mut pressed = 0u64;
        for &btn in gamepad.get_pressed() {
            if let Some(code) = FfiGamepadButton::from_bevy(btn) {
                pressed |= code.bit();
            }
        }
        let mut just_pressed = 0u64;
        for &btn in gamepad.get_just_pressed() {
            if let Some(code) = FfiGamepadButton::from_bevy(btn) {
                just_pressed |= code.bit();
            }
        }
        let mut just_released = 0u64;
        for &btn in gamepad.get_just_released() {
            if let Some(code) = FfiGamepadButton::from_bevy(btn) {
                just_released |= code.bit();
            }
        }
        let left_stick = gamepad.left_stick();
        let right_stick = gamepad.right_stick();
        let dpad = gamepad.dpad();
        let (vendor_id, has_vendor_id) = match gamepad.vendor_id() {
            Some(v) => (v, FfiBool::TRUE),
            None => (0, FfiBool::FALSE),
        };
        let (product_id, has_product_id) = match gamepad.product_id() {
            Some(p) => (p, FfiBool::TRUE),
            None => (0, FfiBool::FALSE),
        };
        Self {
            pressed,
            just_pressed,
            just_released,
            left_stick: [left_stick.x, left_stick.y],
            right_stick: [right_stick.x, right_stick.y],
            left_trigger: gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0),
            right_trigger: gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0),
            left_z: gamepad.get(GamepadAxis::LeftZ).unwrap_or(0.0),
            right_z: gamepad.get(GamepadAxis::RightZ).unwrap_or(0.0),
            dpad: [dpad.x, dpad.y],
            vendor_id,
            product_id,
            has_vendor_id,
            has_product_id,
            connected: FfiBool::TRUE,
            reserved0: 0,
        }
    }

    /// Returns `true` if `button` is currently pressed.
    #[inline]
    pub const fn pressed(&self, button: FfiGamepadButton) -> bool {
        (self.pressed & button.bit()) != 0
    }

    /// Returns `true` if `button` was pressed during the current frame.
    #[inline]
    pub const fn just_pressed(&self, button: FfiGamepadButton) -> bool {
        (self.just_pressed & button.bit()) != 0
    }

    /// Returns `true` if `button` was released during the current frame.
    #[inline]
    pub const fn just_released(&self, button: FfiGamepadButton) -> bool {
        (self.just_released & button.bit()) != 0
    }
}

/// Zero-padding `#[repr(C)]` POD message translating guest haptic requests into Bevy
/// [`GamepadRumbleRequest`] messages.
///
/// Guests emit `FfiGamepadRumble` via `msg_write` / `msg_write_batch` (`AccessMode::MESSAGE_WRITE`);
/// [`ModInputExtBridgePlugin`] validates and forwards them to Bevy's `Messages<GamepadRumbleRequest>`
/// in [`PostUpdate`]. Requests referencing non-`Gamepad` entities, non-finite floats, negative
/// durations, or unknown `kind` values are safely ignored.
#[cfg(feature = "bridge-picking")]
#[repr(C)]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_input::gamepad::GamepadRumbleRequest")]
pub struct FfiGamepadRumble {
    /// Target [`Gamepad`] entity (`FfiEntity::NULL` is ignored).
    pub gamepad: FfiEntity,
    /// Rumble duration in seconds for [`Self::KIND_ADD`] (must be finite and `>= 0.0`).
    pub duration_secs: f32,
    /// Low-frequency strong motor intensity in `[0.0, 1.0]` (clamped on translation).
    pub strong_motor: f32,
    /// High-frequency weak motor intensity in `[0.0, 1.0]` (clamped on translation).
    pub weak_motor: f32,
    /// Request kind: [`Self::KIND_ADD`] (`0`) or [`Self::KIND_STOP`] (`1`).
    pub kind: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

#[cfg(feature = "bridge-picking")]
impl FfiGamepadRumble {
    /// Adds a rumble effect ([`GamepadRumbleRequest::Add`] = `0`).
    pub const KIND_ADD: u8 = 0;
    /// Stops all active rumble effects on the gamepad ([`GamepadRumbleRequest::Stop`] = `1`).
    pub const KIND_STOP: u8 = 1;

    /// Constructs a [`Self::KIND_ADD`] rumble request targeting `gamepad`.
    #[inline]
    pub const fn add(
        gamepad: FfiEntity,
        duration_secs: f32,
        strong_motor: f32,
        weak_motor: f32,
    ) -> Self {
        Self {
            gamepad,
            duration_secs,
            strong_motor,
            weak_motor,
            kind: Self::KIND_ADD,
            reserved0: [0; 3],
        }
    }

    /// Constructs a [`Self::KIND_STOP`] rumble request targeting `gamepad`.
    #[inline]
    pub const fn stop(gamepad: FfiEntity) -> Self {
        Self {
            gamepad,
            duration_secs: 0.0,
            strong_motor: 0.0,
            weak_motor: 0.0,
            kind: Self::KIND_STOP,
            reserved0: [0; 3],
        }
    }

    /// Validates and translates this wire request into a Bevy [`GamepadRumbleRequest`].
    ///
    /// Returns `None` if `gamepad` is null/placeholder, `kind` is unknown, or any numeric field
    /// for [`Self::KIND_ADD`] is non-finite or negative.
    pub fn to_bevy(&self) -> Option<GamepadRumbleRequest> {
        if self.gamepad == FfiEntity::NULL {
            return None;
        }
        let entity =
            Entity::try_from_bits(self.gamepad.to_bits()).filter(|&e| e != Entity::PLACEHOLDER)?;
        match self.kind {
            Self::KIND_ADD => {
                if !self.duration_secs.is_finite()
                    || self.duration_secs < 0.0
                    || !self.strong_motor.is_finite()
                    || !self.weak_motor.is_finite()
                {
                    return None;
                }
                let duration = Duration::try_from_secs_f32(self.duration_secs).ok()?;
                let intensity = GamepadRumbleIntensity {
                    strong_motor: self.strong_motor.clamp(0.0, 1.0),
                    weak_motor: self.weak_motor.clamp(0.0, 1.0),
                };
                Some(GamepadRumbleRequest::Add {
                    duration,
                    intensity,
                    gamepad: entity,
                })
            }
            Self::KIND_STOP => Some(GamepadRumbleRequest::Stop { gamepad: entity }),
            _ => None,
        }
    }
}

const _: () = {
    assert_abi_pod::<FfiMouseMotion>();
    assert_abi_pod::<FfiMouseScroll>();
    assert!(<FfiMouseMotion as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiMouseScroll as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(core::mem::size_of::<FfiMouseMotion>() == 8);
    assert!(core::mem::align_of::<FfiMouseMotion>() == 4);
    assert!(core::mem::size_of::<FfiMouseScroll>() == 12);
    assert!(core::mem::align_of::<FfiMouseScroll>() == 4);
};

#[cfg(feature = "bridge-picking")]
const _: () = {
    assert_abi_pod::<FfiGamepadButton>();
    assert_abi_pod::<FfiGamepadState>();
    assert_abi_pod::<FfiGamepadRumble>();
    assert!(<FfiGamepadState as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiGamepadRumble as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(core::mem::size_of::<FfiGamepadState>() == 72);
    assert!(core::mem::align_of::<FfiGamepadState>() == 8);
    assert!(core::mem::size_of::<FfiGamepadRumble>() == 24);
    assert!(core::mem::align_of::<FfiGamepadRumble>() == 8);
};

/// System sets for [`ModInputExtBridgePlugin`] synchronization.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputExtBridgeSet {
    /// Syncs Bevy input resources/components into FFI wire mirrors during [`PreUpdate`] after
    /// [`InputSystems`].
    SyncFromHost,
    /// Translates guest input output requests (such as gamepad rumble) into Bevy messages during
    /// [`bevy_app::PostUpdate`].
    SyncToHost,
}

fn sync_mouse_motion_and_scroll_to_ffi(
    motion: Option<Res<AccumulatedMouseMotion>>,
    scroll: Option<Res<AccumulatedMouseScroll>>,
    mut ffi_motion: ResMut<FfiMouseMotion>,
    mut ffi_scroll: ResMut<FfiMouseScroll>,
) {
    let next_motion = motion
        .as_deref()
        .map(FfiMouseMotion::from_bevy)
        .unwrap_or_default();
    if *ffi_motion != next_motion {
        *ffi_motion = next_motion;
    }

    let next_scroll = scroll
        .as_deref()
        .map(FfiMouseScroll::from_bevy)
        .unwrap_or_default();
    if *ffi_scroll != next_scroll {
        *ffi_scroll = next_scroll;
    }
}

#[cfg(feature = "bridge-picking")]
#[allow(clippy::type_complexity)]
fn sync_gamepads_to_ffi(
    mut commands: Commands,
    mut gamepads: Query<
        (Entity, &Gamepad, Option<&mut FfiGamepadState>),
        Or<(Changed<Gamepad>, Without<FfiGamepadState>)>,
    >,
    mut removed_gamepads: RemovedComponents<Gamepad>,
    ffi_only: Query<(), (With<FfiGamepadState>, Without<Gamepad>)>,
) {
    for (entity, gamepad, ffi_opt) in &mut gamepads {
        let next = FfiGamepadState::from_bevy(gamepad);
        match ffi_opt {
            Some(mut existing) => {
                if *existing != next {
                    *existing = next;
                }
            }
            None => {
                commands.entity(entity).insert(next);
            }
        }
    }

    for entity in removed_gamepads.read() {
        if ffi_only.contains(entity) {
            commands.entity(entity).remove::<FfiGamepadState>();
        }
    }
}

#[cfg(feature = "bridge-picking")]
fn forward_gamepad_rumble_requests(
    mut ffi_rumbles: MessageReader<FfiGamepadRumble>,
    mut host_rumbles: MessageWriter<GamepadRumbleRequest>,
    gamepads: Query<(), With<Gamepad>>,
) {
    for request in ffi_rumbles.read() {
        if let Some(bevy_req) = request.to_bevy()
            && gamepads.contains(bevy_req.gamepad())
        {
            host_rumbles.write(bevy_req);
        }
    }
}

/// Registers [`FfiMouseMotion`], [`FfiMouseScroll`], and (when `bridge-picking` is enabled)
/// `FfiGamepadState` and `FfiGamepadRumble` inside `world`'s [`ModRegistry`].
pub fn register_input_ext_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<FfiMouseMotion>() {
        world.insert_resource(FfiMouseMotion::default());
    }
    if !world.contains_resource::<FfiMouseScroll>() {
        world.insert_resource(FfiMouseScroll::default());
    }
    world.register_mod_resource::<FfiMouseMotion>()?;
    world.register_mod_resource::<FfiMouseScroll>()?;

    #[cfg(feature = "bridge-picking")]
    {
        world.register_mod_component::<FfiGamepadState>(
            ComponentOptions::new().with_default().immutable(),
        )?;
        world.register_mod_message::<FfiGamepadRumble>()?;
        if !world.contains_resource::<Messages<GamepadRumbleRequest>>() {
            world.init_resource::<Messages<GamepadRumbleRequest>>();
        }
    }

    Ok(())
}

/// Bevy [`Plugin`] bridging extended Bevy `0.20` input resources and gamepad entities into
/// `bevy_abi_mod` (`#![forbid(unsafe_code)]`).
///
/// - Registers and syncs [`FfiMouseMotion`] and [`FfiMouseScroll`] in [`PreUpdate`] after
///   [`InputSystems`].
/// - When `feature = "bridge-picking"` is enabled (activating `bevy_input/gamepad`), also
///   registers `FfiGamepadState` (synced per `Gamepad` entity in [`PreUpdate`]) and
///   `FfiGamepadRumble` (forwarded into Bevy `GamepadRumbleRequest` messages in `PostUpdate`).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModInputExtBridgePlugin;

impl Plugin for ModInputExtBridgePlugin {
    fn build(&self, app: &mut App) {
        #[cfg(feature = "bridge-picking")]
        {
            if !app
                .world()
                .contains_resource::<Messages<FfiGamepadRumble>>()
            {
                app.add_message::<FfiGamepadRumble>();
            }
            if !app
                .world()
                .contains_resource::<Messages<GamepadRumbleRequest>>()
            {
                app.add_message::<GamepadRumbleRequest>();
            }
        }

        if let Err(err) = register_input_ext_bridges(app.world_mut()) {
            log_ecs_error(
                err.into(),
                ErrorContext::Command {
                    name: "ModInputExtBridgePlugin::build".into(),
                },
            );
        }

        app.configure_sets(
            PreUpdate,
            InputExtBridgeSet::SyncFromHost.after(InputSystems),
        )
        .add_systems(
            PreUpdate,
            sync_mouse_motion_and_scroll_to_ffi.in_set(InputExtBridgeSet::SyncFromHost),
        );

        #[cfg(feature = "bridge-picking")]
        app.add_systems(
            PreUpdate,
            sync_gamepads_to_ffi.in_set(InputExtBridgeSet::SyncFromHost),
        )
        .add_systems(
            PostUpdate,
            forward_gamepad_rumble_requests.in_set(InputExtBridgeSet::SyncToHost),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_canonical_names_match_bevy_type_names() {
        assert_eq!(
            CANONICAL_MOUSE_MOTION_NAME,
            core::any::type_name::<AccumulatedMouseMotion>()
        );
        assert_eq!(
            CANONICAL_MOUSE_SCROLL_NAME,
            core::any::type_name::<AccumulatedMouseScroll>()
        );
    }

    #[cfg(feature = "bridge-picking")]
    #[test]
    fn gamepad_canonical_names_and_button_round_trip() {
        assert_eq!(
            CANONICAL_GAMEPAD_STATE_NAME,
            core::any::type_name::<Gamepad>()
        );
        assert_eq!(
            CANONICAL_GAMEPAD_RUMBLE_NAME,
            core::any::type_name::<GamepadRumbleRequest>()
        );
        for (idx, button) in GamepadButton::all().into_iter().enumerate() {
            let ffi = FfiGamepadButton::from_bevy(button).expect("standard gamepad button");
            assert_eq!(ffi.0 as usize, idx);
            assert_eq!(ffi.to_bevy(), Some(button));
        }
        let other = FfiGamepadButton::from_bevy(GamepadButton::Other(4)).unwrap();
        assert_eq!(other.0, FfiGamepadButton::OTHER_BASE + 4);
        assert_eq!(other.to_bevy(), Some(GamepadButton::Other(4)));
        assert_eq!(FfiGamepadButton::from_bevy(GamepadButton::Other(100)), None);
        assert_eq!(FfiGamepadButton(64).to_bevy(), None);
        assert_eq!(FfiGamepadButton(64).bit(), 0);
    }

    #[cfg(feature = "bridge-picking")]
    #[test]
    fn gamepad_rumble_rejects_invalid_inputs_safely() {
        let mut world = World::new();
        let gp = world.spawn_empty().id();
        let gp_ffi = FfiEntity::from_bits(gp.to_bits());

        assert!(
            FfiGamepadRumble::add(FfiEntity::NULL, 0.5, 1.0, 1.0)
                .to_bevy()
                .is_none()
        );
        assert!(
            FfiGamepadRumble::add(gp_ffi, -1.0, 0.5, 0.5)
                .to_bevy()
                .is_none()
        );
        assert!(
            FfiGamepadRumble::add(gp_ffi, f32::NAN, 0.5, 0.5)
                .to_bevy()
                .is_none()
        );
        assert!(
            FfiGamepadRumble::add(gp_ffi, 0.25, f32::INFINITY, 0.5)
                .to_bevy()
                .is_none()
        );
        let mut bad_kind = FfiGamepadRumble::add(gp_ffi, 0.25, 0.5, 0.5);
        bad_kind.kind = 99;
        assert!(bad_kind.to_bevy().is_none());

        // Out-of-range intensities clamp to [0.0, 1.0].
        let clamped = FfiGamepadRumble::add(gp_ffi, 0.5, 2.5, -0.5)
            .to_bevy()
            .expect("clamped add rumble");
        match clamped {
            GamepadRumbleRequest::Add {
                duration,
                intensity,
                gamepad,
            } => {
                assert_eq!(gamepad, gp);
                assert!((duration.as_secs_f32() - 0.5).abs() < 1e-6);
                assert_eq!(intensity.strong_motor, 1.0);
                assert_eq!(intensity.weak_motor, 0.0);
            }
            GamepadRumbleRequest::Stop { .. } => panic!("expected Add"),
        }
    }
}
