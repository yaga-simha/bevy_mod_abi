//! Core Bevy `0.20` standard component and resource bridges (`StdTypesBridgePlugin`).
//!
//! Exposes `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Disabled`, `Time<Virtual>`,
//! `Time<Fixed>`, `Time<Real>`, `ButtonInput<KeyCode>`, and `ButtonInput<MouseButton>` to native
//! C-ABI mods via [`HostWireBridge`] and [`HostStringBridge`] with `#![forbid(unsafe_code)]`.

use bevy_abi_mod_host_sys::{
    ComponentCollisionError, HostStringBridge, HostWireBridge, ModRegistry,
};
use bevy_abi_mod_macros::AbiPod;
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use bevy_app::{App, Plugin};
use bevy_camera::visibility::Visibility;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::error::{ErrorContext, error as log_ecs_error};
use bevy_ecs::name::Name;
use bevy_ecs::world::World;
use bevy_input::ButtonInput;
use bevy_input::keyboard::KeyCode;
use bevy_input::mouse::MouseButton;
use bevy_math::{Quat, Vec3};
use bevy_time::{Fixed, Real, Time, Virtual};
use bevy_transform::components::{GlobalTransform, Transform};

/// Canonical ABI name for bridged `Transform`.
pub const CANONICAL_TRANSFORM_NAME: &str = "bevy_transform::components::transform::Transform";
/// Canonical ABI name for bridged `GlobalTransform`.
pub const CANONICAL_GLOBAL_TRANSFORM_NAME: &str =
    "bevy_transform::components::global_transform::GlobalTransform";
/// Canonical ABI name for bridged `Visibility`.
pub const CANONICAL_VISIBILITY_NAME: &str = "bevy_camera::visibility::Visibility";
/// Canonical ABI name for bridged `Name`.
pub const CANONICAL_NAME_NAME: &str = "bevy_ecs::name::Name";
/// Canonical ABI name for bridged `bevy_ecs::entity_disabling::Disabled`.
pub const CANONICAL_DISABLED_NAME: &str = "bevy_ecs::entity_disabling::Disabled";
/// Canonical ABI name for bridged `Time<Virtual>`.
pub const CANONICAL_TIME_VIRTUAL_NAME: &str = "bevy_time::Time<Virtual>";
/// Canonical ABI name for bridged `Time<Fixed>`.
pub const CANONICAL_TIME_FIXED_NAME: &str = "bevy_time::Time<Fixed>";
/// Canonical ABI name for bridged `Time<Real>`.
pub const CANONICAL_TIME_REAL_NAME: &str = "bevy_time::Time<Real>";
/// Canonical ABI name for bridged `ButtonInput<KeyCode>`.
pub const CANONICAL_KEYBOARD_INPUT_NAME: &str = "bevy_input::ButtonInput<KeyCode>";
/// Canonical ABI name for bridged `ButtonInput<MouseButton>`.
pub const CANONICAL_MOUSE_INPUT_NAME: &str = "bevy_input::ButtonInput<MouseButton>";

/// Fixed-layout `#[repr(C)]` wire projection of [`Transform`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct FfiTransform {
    /// Local translation `[x, y, z]`.
    pub translation: [f32; 3],
    /// Local rotation quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Local non-uniform scale `[x, y, z]`.
    pub scale: [f32; 3],
}

impl FfiTransform {
    /// Identity transform (`translation = [0; 3]`, `rotation = [0, 0, 0, 1]`, `scale = [1; 3]`).
    pub const IDENTITY: Self = Self {
        translation: [0.0, 0.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0, 1.0, 1.0],
    };

    /// Constructs a transform at `(x, y, z)` with identity rotation and unit scale.
    #[inline]
    pub const fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self {
            translation: [x, y, z],
            ..Self::IDENTITY
        }
    }
}

impl Default for FfiTransform {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl From<&Transform> for FfiTransform {
    #[inline]
    fn from(t: &Transform) -> Self {
        Self {
            translation: t.translation.to_array(),
            rotation: t.rotation.to_array(),
            scale: t.scale.to_array(),
        }
    }
}

/// Fixed-layout `#[repr(C)]` wire projection of [`GlobalTransform`] (`Affine3A` 3x4 columns).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct FfiGlobalTransform {
    /// Column-major 3x4 affine basis (`x_axis`, `y_axis`, `z_axis`, `translation`).
    pub matrix_cols: [[f32; 3]; 4],
}

impl FfiGlobalTransform {
    /// Identity affine transform.
    pub const IDENTITY: Self = Self {
        matrix_cols: [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
        ],
    };

    /// Returns the world-space translation column `[x, y, z]`.
    #[inline]
    pub const fn translation(&self) -> [f32; 3] {
        self.matrix_cols[3]
    }

    /// Expands this 3x4 affine matrix to a column-major 4x4 matrix.
    #[inline]
    pub const fn to_mat4(&self) -> [[f32; 4]; 4] {
        let c0 = self.matrix_cols[0];
        let c1 = self.matrix_cols[1];
        let c2 = self.matrix_cols[2];
        let c3 = self.matrix_cols[3];
        [
            [c0[0], c0[1], c0[2], 0.0],
            [c1[0], c1[1], c1[2], 0.0],
            [c2[0], c2[1], c2[2], 0.0],
            [c3[0], c3[1], c3[2], 1.0],
        ]
    }
}

impl Default for FfiGlobalTransform {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Fixed-layout `#[repr(transparent)]` wire projection of [`Visibility`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiVisibility(pub u8);

impl FfiVisibility {
    /// Inherit visibility from parent hierarchy (`0`).
    pub const INHERITED: Self = Self(0);
    /// Unconditionally hidden (`1`).
    pub const HIDDEN: Self = Self(1);
    /// Unconditionally visible (`2`).
    pub const VISIBLE: Self = Self(2);
}

/// Fixed-layout `#[repr(C)]` wire projection of [`Time<Virtual>`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
pub struct FfiTimeVirtual {
    /// Delta time since last advance in `f64` seconds.
    pub delta_secs_f64: f64,
    /// Total elapsed virtual time in `f64` seconds.
    pub elapsed_secs_f64: f64,
    /// Delta time since last advance in `f32` seconds.
    pub delta_secs: f32,
    /// Total elapsed virtual time in `f32` seconds.
    pub elapsed_secs: f32,
    /// Current virtual clock speed multiplier (`1.0` = real-time).
    pub relative_speed: f32,
    /// Whether the virtual clock is currently paused.
    pub paused: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

/// Fixed-layout `#[repr(C)]` wire projection of [`Time<Fixed>`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
pub struct FfiTimeFixed {
    /// Delta time of the current fixed tick in `f64` seconds.
    pub delta_secs_f64: f64,
    /// Total elapsed fixed time in `f64` seconds.
    pub elapsed_secs_f64: f64,
    /// Configured fixed timestep interval in `f64` seconds.
    pub timestep_secs: f64,
    /// Delta time of the current fixed tick in `f32` seconds.
    pub delta_secs: f32,
    /// Total elapsed fixed time in `f32` seconds.
    pub elapsed_secs: f32,
    /// Fractional overstep between `[0.0, 1.0)` toward the next fixed step.
    pub overstep_fraction: f32,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 4],
}

/// Fixed-layout `#[repr(C)]` wire projection of [`Time<Real>`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
pub struct FfiTimeReal {
    /// Real wall-clock delta in `f64` seconds.
    pub delta_secs_f64: f64,
    /// Total elapsed real time in `f64` seconds.
    pub elapsed_secs_f64: f64,
    /// Real wall-clock delta in `f32` seconds.
    pub delta_secs: f32,
    /// Total elapsed real time in `f32` seconds.
    pub elapsed_secs: f32,
}

/// Compact deterministic bit-index key code (`0..194`) mirroring Bevy's named [`KeyCode`] variants.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiKeyCode(pub u16);

impl FfiKeyCode {
    /// Converts a Bevy [`KeyCode`] into its deterministic bitset index (`None` for `Unidentified`).
    pub const fn from_bevy(code: KeyCode) -> Option<Self> {
        let idx = match code {
            KeyCode::Backquote => 0,
            KeyCode::Backslash => 1,
            KeyCode::BracketLeft => 2,
            KeyCode::BracketRight => 3,
            KeyCode::Comma => 4,
            KeyCode::Digit0 => 5,
            KeyCode::Digit1 => 6,
            KeyCode::Digit2 => 7,
            KeyCode::Digit3 => 8,
            KeyCode::Digit4 => 9,
            KeyCode::Digit5 => 10,
            KeyCode::Digit6 => 11,
            KeyCode::Digit7 => 12,
            KeyCode::Digit8 => 13,
            KeyCode::Digit9 => 14,
            KeyCode::Equal => 15,
            KeyCode::IntlBackslash => 16,
            KeyCode::IntlRo => 17,
            KeyCode::IntlYen => 18,
            KeyCode::KeyA => 19,
            KeyCode::KeyB => 20,
            KeyCode::KeyC => 21,
            KeyCode::KeyD => 22,
            KeyCode::KeyE => 23,
            KeyCode::KeyF => 24,
            KeyCode::KeyG => 25,
            KeyCode::KeyH => 26,
            KeyCode::KeyI => 27,
            KeyCode::KeyJ => 28,
            KeyCode::KeyK => 29,
            KeyCode::KeyL => 30,
            KeyCode::KeyM => 31,
            KeyCode::KeyN => 32,
            KeyCode::KeyO => 33,
            KeyCode::KeyP => 34,
            KeyCode::KeyQ => 35,
            KeyCode::KeyR => 36,
            KeyCode::KeyS => 37,
            KeyCode::KeyT => 38,
            KeyCode::KeyU => 39,
            KeyCode::KeyV => 40,
            KeyCode::KeyW => 41,
            KeyCode::KeyX => 42,
            KeyCode::KeyY => 43,
            KeyCode::KeyZ => 44,
            KeyCode::Minus => 45,
            KeyCode::Period => 46,
            KeyCode::Quote => 47,
            KeyCode::Semicolon => 48,
            KeyCode::Slash => 49,
            KeyCode::AltLeft => 50,
            KeyCode::AltRight => 51,
            KeyCode::Backspace => 52,
            KeyCode::CapsLock => 53,
            KeyCode::ContextMenu => 54,
            KeyCode::ControlLeft => 55,
            KeyCode::ControlRight => 56,
            KeyCode::Enter => 57,
            KeyCode::SuperLeft => 58,
            KeyCode::SuperRight => 59,
            KeyCode::ShiftLeft => 60,
            KeyCode::ShiftRight => 61,
            KeyCode::Space => 62,
            KeyCode::Tab => 63,
            KeyCode::Convert => 64,
            KeyCode::KanaMode => 65,
            KeyCode::Lang1 => 66,
            KeyCode::Lang2 => 67,
            KeyCode::Lang3 => 68,
            KeyCode::Lang4 => 69,
            KeyCode::Lang5 => 70,
            KeyCode::NonConvert => 71,
            KeyCode::Delete => 72,
            KeyCode::End => 73,
            KeyCode::Help => 74,
            KeyCode::Home => 75,
            KeyCode::Insert => 76,
            KeyCode::PageDown => 77,
            KeyCode::PageUp => 78,
            KeyCode::ArrowDown => 79,
            KeyCode::ArrowLeft => 80,
            KeyCode::ArrowRight => 81,
            KeyCode::ArrowUp => 82,
            KeyCode::NumLock => 83,
            KeyCode::Numpad0 => 84,
            KeyCode::Numpad1 => 85,
            KeyCode::Numpad2 => 86,
            KeyCode::Numpad3 => 87,
            KeyCode::Numpad4 => 88,
            KeyCode::Numpad5 => 89,
            KeyCode::Numpad6 => 90,
            KeyCode::Numpad7 => 91,
            KeyCode::Numpad8 => 92,
            KeyCode::Numpad9 => 93,
            KeyCode::NumpadAdd => 94,
            KeyCode::NumpadBackspace => 95,
            KeyCode::NumpadClear => 96,
            KeyCode::NumpadClearEntry => 97,
            KeyCode::NumpadComma => 98,
            KeyCode::NumpadDecimal => 99,
            KeyCode::NumpadDivide => 100,
            KeyCode::NumpadEnter => 101,
            KeyCode::NumpadEqual => 102,
            KeyCode::NumpadHash => 103,
            KeyCode::NumpadMemoryAdd => 104,
            KeyCode::NumpadMemoryClear => 105,
            KeyCode::NumpadMemoryRecall => 106,
            KeyCode::NumpadMemoryStore => 107,
            KeyCode::NumpadMemorySubtract => 108,
            KeyCode::NumpadMultiply => 109,
            KeyCode::NumpadParenLeft => 110,
            KeyCode::NumpadParenRight => 111,
            KeyCode::NumpadStar => 112,
            KeyCode::NumpadSubtract => 113,
            KeyCode::Escape => 114,
            KeyCode::Fn => 115,
            KeyCode::FnLock => 116,
            KeyCode::PrintScreen => 117,
            KeyCode::ScrollLock => 118,
            KeyCode::Pause => 119,
            KeyCode::BrowserBack => 120,
            KeyCode::BrowserFavorites => 121,
            KeyCode::BrowserForward => 122,
            KeyCode::BrowserHome => 123,
            KeyCode::BrowserRefresh => 124,
            KeyCode::BrowserSearch => 125,
            KeyCode::BrowserStop => 126,
            KeyCode::Eject => 127,
            KeyCode::LaunchApp1 => 128,
            KeyCode::LaunchApp2 => 129,
            KeyCode::LaunchMail => 130,
            KeyCode::MediaPlayPause => 131,
            KeyCode::MediaSelect => 132,
            KeyCode::MediaStop => 133,
            KeyCode::MediaTrackNext => 134,
            KeyCode::MediaTrackPrevious => 135,
            KeyCode::Power => 136,
            KeyCode::Sleep => 137,
            KeyCode::AudioVolumeDown => 138,
            KeyCode::AudioVolumeMute => 139,
            KeyCode::AudioVolumeUp => 140,
            KeyCode::WakeUp => 141,
            KeyCode::Meta => 142,
            KeyCode::Hyper => 143,
            KeyCode::Turbo => 144,
            KeyCode::Abort => 145,
            KeyCode::Resume => 146,
            KeyCode::Suspend => 147,
            KeyCode::Again => 148,
            KeyCode::Copy => 149,
            KeyCode::Cut => 150,
            KeyCode::Find => 151,
            KeyCode::Open => 152,
            KeyCode::Paste => 153,
            KeyCode::Props => 154,
            KeyCode::Select => 155,
            KeyCode::Undo => 156,
            KeyCode::Hiragana => 157,
            KeyCode::Katakana => 158,
            KeyCode::F1 => 159,
            KeyCode::F2 => 160,
            KeyCode::F3 => 161,
            KeyCode::F4 => 162,
            KeyCode::F5 => 163,
            KeyCode::F6 => 164,
            KeyCode::F7 => 165,
            KeyCode::F8 => 166,
            KeyCode::F9 => 167,
            KeyCode::F10 => 168,
            KeyCode::F11 => 169,
            KeyCode::F12 => 170,
            KeyCode::F13 => 171,
            KeyCode::F14 => 172,
            KeyCode::F15 => 173,
            KeyCode::F16 => 174,
            KeyCode::F17 => 175,
            KeyCode::F18 => 176,
            KeyCode::F19 => 177,
            KeyCode::F20 => 178,
            KeyCode::F21 => 179,
            KeyCode::F22 => 180,
            KeyCode::F23 => 181,
            KeyCode::F24 => 182,
            KeyCode::F25 => 183,
            KeyCode::F26 => 184,
            KeyCode::F27 => 185,
            KeyCode::F28 => 186,
            KeyCode::F29 => 187,
            KeyCode::F30 => 188,
            KeyCode::F31 => 189,
            KeyCode::F32 => 190,
            KeyCode::F33 => 191,
            KeyCode::F34 => 192,
            KeyCode::F35 => 193,
            KeyCode::Unidentified(_) => return None,
        };
        Some(Self(idx))
    }
}

/// 256-bit keyboard state snapshot (`pressed`, `just_pressed`, `just_released`) projecting
/// [`ButtonInput<KeyCode>`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct FfiKeyboardInput {
    /// Bitset of currently pressed keys.
    pub pressed: [u64; 4],
    /// Bitset of keys pressed during the current frame.
    pub just_pressed: [u64; 4],
    /// Bitset of keys released during the current frame.
    pub just_released: [u64; 4],
}

impl FfiKeyboardInput {
    #[inline]
    fn set_bit(words: &mut [u64; 4], code: FfiKeyCode) {
        let idx = code.0 as usize;
        if idx < 256 {
            words[idx / 64] |= 1u64 << (idx % 64);
        }
    }

    #[inline]
    const fn get_bit(words: &[u64; 4], code: FfiKeyCode) -> bool {
        let idx = code.0 as usize;
        if idx >= 256 {
            false
        } else {
            (words[idx / 64] & (1u64 << (idx % 64))) != 0
        }
    }

    /// Returns `true` if `code` is currently pressed.
    #[inline]
    pub const fn pressed(&self, code: FfiKeyCode) -> bool {
        Self::get_bit(&self.pressed, code)
    }

    /// Returns `true` if `code` was pressed this tick.
    #[inline]
    pub const fn just_pressed(&self, code: FfiKeyCode) -> bool {
        Self::get_bit(&self.just_pressed, code)
    }

    /// Returns `true` if `code` was released this tick.
    #[inline]
    pub const fn just_released(&self, code: FfiKeyCode) -> bool {
        Self::get_bit(&self.just_released, code)
    }
}

/// Compact deterministic bit-index mouse button (`0..64`) mirroring [`MouseButton`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiMouseButton(pub u8);

impl FfiMouseButton {
    /// Left mouse button (`0`).
    pub const LEFT: Self = Self(0);
    /// Right mouse button (`1`).
    pub const RIGHT: Self = Self(1);
    /// Middle mouse button (`2`).
    pub const MIDDLE: Self = Self(2);
    /// Back mouse button (`3`).
    pub const BACK: Self = Self(3);
    /// Forward mouse button (`4`).
    pub const FORWARD: Self = Self(4);

    /// Converts a Bevy [`MouseButton`] into a bitset index (`0..64`).
    #[inline]
    pub const fn from_bevy(button: MouseButton) -> Option<Self> {
        match button {
            MouseButton::Left => Some(Self::LEFT),
            MouseButton::Right => Some(Self::RIGHT),
            MouseButton::Middle => Some(Self::MIDDLE),
            MouseButton::Back => Some(Self::BACK),
            MouseButton::Forward => Some(Self::FORWARD),
            MouseButton::Other(n) => {
                let idx = 5u16.saturating_add(n);
                if idx < 64 {
                    Some(Self(idx as u8))
                } else {
                    None
                }
            }
        }
    }
}

/// 64-bit mouse button state snapshot (`pressed`, `just_pressed`, `just_released`) projecting
/// [`ButtonInput<MouseButton>`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct FfiMouseInput {
    /// Bitset of currently pressed mouse buttons.
    pub pressed: u64,
    /// Bitset of mouse buttons pressed during the current frame.
    pub just_pressed: u64,
    /// Bitset of mouse buttons released during the current frame.
    pub just_released: u64,
}

impl FfiMouseInput {
    #[inline]
    fn set_bit(mask: &mut u64, button: FfiMouseButton) {
        if button.0 < 64 {
            *mask |= 1u64 << button.0;
        }
    }

    /// Returns `true` if `button` is currently pressed.
    #[inline]
    pub const fn pressed(&self, button: FfiMouseButton) -> bool {
        button.0 < 64 && (self.pressed & (1u64 << button.0)) != 0
    }

    /// Returns `true` if `button` was pressed this tick.
    #[inline]
    pub const fn just_pressed(&self, button: FfiMouseButton) -> bool {
        button.0 < 64 && (self.just_pressed & (1u64 << button.0)) != 0
    }

    /// Returns `true` if `button` was released this tick.
    #[inline]
    pub const fn just_released(&self, button: FfiMouseButton) -> bool {
        button.0 < 64 && (self.just_released & (1u64 << button.0)) != 0
    }
}

const _: () = {
    assert!(core::mem::size_of::<FfiTransform>() == 40);
    assert!(core::mem::align_of::<FfiTransform>() == 4);
    assert!(core::mem::size_of::<FfiGlobalTransform>() == 48);
    assert!(core::mem::align_of::<FfiGlobalTransform>() == 4);
    assert!(core::mem::size_of::<FfiVisibility>() == 1);
    assert!(core::mem::align_of::<FfiVisibility>() == 1);
    assert!(core::mem::size_of::<FfiTimeVirtual>() == 32);
    assert!(core::mem::align_of::<FfiTimeVirtual>() == 8);
    assert!(core::mem::size_of::<FfiTimeFixed>() == 40);
    assert!(core::mem::align_of::<FfiTimeFixed>() == 8);
    assert!(core::mem::size_of::<FfiTimeReal>() == 24);
    assert!(core::mem::align_of::<FfiTimeReal>() == 8);
    assert!(core::mem::size_of::<FfiKeyboardInput>() == 96);
    assert!(core::mem::align_of::<FfiKeyboardInput>() == 8);
    assert!(core::mem::size_of::<FfiMouseInput>() == 24);
    assert!(core::mem::align_of::<FfiMouseInput>() == 8);
};

struct TransformBridge;
impl HostWireBridge<Transform> for TransformBridge {
    type Wire = FfiTransform;
    const CANONICAL_NAME: &'static str = CANONICAL_TRANSFORM_NAME;
    const ALIAS: Option<&'static str> = Some("Transform");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &Transform) -> FfiTransform {
        FfiTransform::from(value)
    }

    #[inline]
    fn apply_wire(value: &mut Transform, wire: FfiTransform) -> FfiResult {
        if wire.translation.iter().any(|v| !v.is_finite())
            || wire.rotation.iter().any(|v| !v.is_finite())
            || wire.scale.iter().any(|v| !v.is_finite())
        {
            return FfiResult::INVALID_ARGUMENT;
        }
        value.translation = Vec3::from_array(wire.translation);
        value.rotation = Quat::from_array(wire.rotation);
        value.scale = Vec3::from_array(wire.scale);
        FfiResult::OK
    }
}

struct GlobalTransformBridge;
impl HostWireBridge<GlobalTransform> for GlobalTransformBridge {
    type Wire = FfiGlobalTransform;
    const CANONICAL_NAME: &'static str = CANONICAL_GLOBAL_TRANSFORM_NAME;
    const ALIAS: Option<&'static str> = Some("GlobalTransform");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(value: &GlobalTransform) -> FfiGlobalTransform {
        let aff = value.affine();
        FfiGlobalTransform {
            matrix_cols: [
                aff.x_axis.to_array(),
                aff.y_axis.to_array(),
                aff.z_axis.to_array(),
                aff.translation.to_array(),
            ],
        }
    }
}

struct VisibilityBridge;
impl HostWireBridge<Visibility> for VisibilityBridge {
    type Wire = FfiVisibility;
    const CANONICAL_NAME: &'static str = CANONICAL_VISIBILITY_NAME;
    const ALIAS: Option<&'static str> = Some("Visibility");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &Visibility) -> FfiVisibility {
        match value {
            Visibility::Inherited => FfiVisibility::INHERITED,
            Visibility::Hidden => FfiVisibility::HIDDEN,
            Visibility::Visible => FfiVisibility::VISIBLE,
        }
    }

    #[inline]
    fn apply_wire(value: &mut Visibility, wire: FfiVisibility) -> FfiResult {
        match wire {
            FfiVisibility::INHERITED => *value = Visibility::Inherited,
            FfiVisibility::HIDDEN => *value = Visibility::Hidden,
            FfiVisibility::VISIBLE => *value = Visibility::Visible,
            _ => return FfiResult::INVALID_ARGUMENT,
        }
        FfiResult::OK
    }
}

struct NameBridge;
impl HostStringBridge<Name> for NameBridge {
    const CANONICAL_NAME: &'static str = CANONICAL_NAME_NAME;
    const ALIAS: Option<&'static str> = Some("Name");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn as_str(value: &Name) -> &str {
        value.as_str()
    }

    #[inline]
    fn set_str(value: &mut Name, text: &str) -> FfiResult {
        *value = Name::new(String::from(text));
        FfiResult::OK
    }
}

struct TimeVirtualBridge;
impl HostWireBridge<Time<Virtual>> for TimeVirtualBridge {
    type Wire = FfiTimeVirtual;
    const CANONICAL_NAME: &'static str = CANONICAL_TIME_VIRTUAL_NAME;
    const ALIAS: Option<&'static str> = Some("Time<Virtual>");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(value: &Time<Virtual>) -> FfiTimeVirtual {
        FfiTimeVirtual {
            delta_secs_f64: value.delta_secs_f64(),
            elapsed_secs_f64: value.elapsed_secs_f64(),
            delta_secs: value.delta_secs(),
            elapsed_secs: value.elapsed_secs(),
            relative_speed: value.relative_speed(),
            paused: FfiBool::from_bool(value.is_paused()),
            reserved0: [0; 3],
        }
    }
}

struct TimeFixedBridge;
impl HostWireBridge<Time<Fixed>> for TimeFixedBridge {
    type Wire = FfiTimeFixed;
    const CANONICAL_NAME: &'static str = CANONICAL_TIME_FIXED_NAME;
    const ALIAS: Option<&'static str> = Some("Time<Fixed>");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(value: &Time<Fixed>) -> FfiTimeFixed {
        FfiTimeFixed {
            delta_secs_f64: value.delta_secs_f64(),
            elapsed_secs_f64: value.elapsed_secs_f64(),
            timestep_secs: value.timestep().as_secs_f64(),
            delta_secs: value.delta_secs(),
            elapsed_secs: value.elapsed_secs(),
            overstep_fraction: value.overstep_fraction(),
            reserved0: [0; 4],
        }
    }
}

struct TimeRealBridge;
impl HostWireBridge<Time<Real>> for TimeRealBridge {
    type Wire = FfiTimeReal;
    const CANONICAL_NAME: &'static str = CANONICAL_TIME_REAL_NAME;
    const ALIAS: Option<&'static str> = Some("Time<Real>");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(value: &Time<Real>) -> FfiTimeReal {
        FfiTimeReal {
            delta_secs_f64: value.delta_secs_f64(),
            elapsed_secs_f64: value.elapsed_secs_f64(),
            delta_secs: value.delta_secs(),
            elapsed_secs: value.elapsed_secs(),
        }
    }
}

struct KeyboardInputBridge;
impl HostWireBridge<ButtonInput<KeyCode>> for KeyboardInputBridge {
    type Wire = FfiKeyboardInput;
    const CANONICAL_NAME: &'static str = CANONICAL_KEYBOARD_INPUT_NAME;
    const ALIAS: Option<&'static str> = Some("ButtonInput<KeyCode>");
    const SUPPORTS_SET: bool = false;

    fn to_wire(value: &ButtonInput<KeyCode>) -> FfiKeyboardInput {
        let mut out = FfiKeyboardInput::default();
        for &k in value.get_pressed() {
            if let Some(code) = FfiKeyCode::from_bevy(k) {
                FfiKeyboardInput::set_bit(&mut out.pressed, code);
            }
        }
        for &k in value.get_just_pressed() {
            if let Some(code) = FfiKeyCode::from_bevy(k) {
                FfiKeyboardInput::set_bit(&mut out.just_pressed, code);
            }
        }
        for &k in value.get_just_released() {
            if let Some(code) = FfiKeyCode::from_bevy(k) {
                FfiKeyboardInput::set_bit(&mut out.just_released, code);
            }
        }
        out
    }
}

struct MouseInputBridge;
impl HostWireBridge<ButtonInput<MouseButton>> for MouseInputBridge {
    type Wire = FfiMouseInput;
    const CANONICAL_NAME: &'static str = CANONICAL_MOUSE_INPUT_NAME;
    const ALIAS: Option<&'static str> = Some("ButtonInput<MouseButton>");
    const SUPPORTS_SET: bool = false;

    fn to_wire(value: &ButtonInput<MouseButton>) -> FfiMouseInput {
        let mut out = FfiMouseInput::default();
        for &b in value.get_pressed() {
            if let Some(code) = FfiMouseButton::from_bevy(b) {
                FfiMouseInput::set_bit(&mut out.pressed, code);
            }
        }
        for &b in value.get_just_pressed() {
            if let Some(code) = FfiMouseButton::from_bevy(b) {
                FfiMouseInput::set_bit(&mut out.just_pressed, code);
            }
        }
        for &b in value.get_just_released() {
            if let Some(code) = FfiMouseButton::from_bevy(b) {
                FfiMouseInput::set_bit(&mut out.just_released, code);
            }
        }
        out
    }
}

struct DisabledBridge;
impl HostWireBridge<Disabled> for DisabledBridge {
    type Wire = FfiBool;
    const CANONICAL_NAME: &'static str = CANONICAL_DISABLED_NAME;
    const ALIAS: Option<&'static str> = Some("Disabled");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(_value: &Disabled) -> FfiBool {
        FfiBool::TRUE
    }
}

/// Registers all core Bevy `0.20` standard component and resource bridges on `world`'s
/// [`ModRegistry`] (`Transform`, `GlobalTransform`, `Visibility`, `Name`, `Disabled`,
/// `Time<Virtual>`, `Time<Fixed>`, `Time<Real>`, `ButtonInput<KeyCode>`,
/// `ButtonInput<MouseButton>`).
pub fn register_std_types_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }

    let transform_id = world.register_component::<Transform>();
    let global_transform_id = world.register_component::<GlobalTransform>();
    let visibility_id = world.register_component::<Visibility>();
    let name_id = world.register_component::<Name>();
    let disabled_id = world.register_component::<Disabled>();
    let time_virtual_id = world.register_component::<Time<Virtual>>();
    let time_fixed_id = world.register_component::<Time<Fixed>>();
    let time_real_id = world.register_component::<Time<Real>>();
    let kb_input_id = world.register_component::<ButtonInput<KeyCode>>();
    let mouse_input_id = world.register_component::<ButtonInput<MouseButton>>();

    let mut reg = world.resource_mut::<ModRegistry>();
    reg.register_bridged_component::<Transform, TransformBridge>(
        transform_id,
        0,
        Some(ModRegistry::default_inserter_for::<Transform>()),
    )?;
    reg.register_bridged_component::<GlobalTransform, GlobalTransformBridge>(
        global_transform_id,
        0,
        None,
    )?;
    reg.register_bridged_component::<Visibility, VisibilityBridge>(
        visibility_id,
        0,
        Some(ModRegistry::default_inserter_for::<Visibility>()),
    )?;
    reg.register_bridged_string_component::<Name, NameBridge>(
        name_id,
        0,
        Some(ModRegistry::default_inserter_for::<Name>()),
    )?;
    reg.register_bridged_component::<Disabled, DisabledBridge>(
        disabled_id,
        0,
        Some(ModRegistry::default_inserter_for::<Disabled>()),
    )?;
    reg.register_bridged_resource::<Time<Virtual>, TimeVirtualBridge>(time_virtual_id)?;
    reg.register_bridged_resource::<Time<Fixed>, TimeFixedBridge>(time_fixed_id)?;
    reg.register_bridged_resource::<Time<Real>, TimeRealBridge>(time_real_id)?;
    reg.register_bridged_resource::<ButtonInput<KeyCode>, KeyboardInputBridge>(kb_input_id)?;
    reg.register_bridged_resource::<ButtonInput<MouseButton>, MouseInputBridge>(mouse_input_id)?;
    Ok(())
}

/// Bevy [`Plugin`] exposing core Bevy `0.20` components (`Transform`, `GlobalTransform`,
/// `Visibility`, `Name`, `Disabled`) and resources (`Time<Virtual|Fixed|Real>`,
/// `ButtonInput<KeyCode|MouseButton>`) to native mods via [`ModRegistry`].
#[derive(Default, Debug, Clone, Copy)]
pub struct StdTypesBridgePlugin;

impl Plugin for StdTypesBridgePlugin {
    fn build(&self, app: &mut App) {
        if let Err(err) = register_std_types_bridges(app.world_mut()) {
            log_ecs_error(
                err.into(),
                ErrorContext::Command {
                    name: "StdTypesBridgePlugin::build".into(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_host::AbiModPlugin;
    use bevy_abi_mod_sys::{ComponentUuid, MappedStringVTable, MappedWireVTable};
    use bevy_app::App;
    use core::time::Duration;

    #[test]
    fn std_types_plugin_bridges_components_resources_and_default_inserters() {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StdTypesBridgePlugin));

        let mut time_v = Time::<Virtual>::default();
        time_v.advance_by(Duration::from_millis(16));
        time_v.set_relative_speed(0.5);
        time_v.pause();
        app.insert_resource(time_v);

        let mut time_f = Time::<Fixed>::from_hz(60.0);
        time_f.accumulate_overstep(Duration::from_millis(8));
        time_f.advance_by(Duration::from_millis(10));
        app.insert_resource(time_f);

        let mut time_r = Time::<Real>::default();
        time_r.advance_by(Duration::from_millis(25));
        app.insert_resource(time_r);

        let mut kb = ButtonInput::<KeyCode>::default();
        kb.press(KeyCode::Space);
        kb.press(KeyCode::KeyW);
        kb.release(KeyCode::KeyW);
        app.insert_resource(kb);

        let mut mouse = ButtonInput::<MouseButton>::default();
        mouse.press(MouseButton::Left);
        mouse.press(MouseButton::Other(2));
        app.insert_resource(mouse);

        let bare = app.world_mut().spawn_empty().id();

        // Verify ModRegistry entries, UUIDs, aliases, and `"bam.v0.*.vtable"` symbol pointers.
        {
            let reg = app.world().resource::<ModRegistry>();
            let tf_entry = reg.get_by_name("Transform").unwrap();
            assert_eq!(
                tf_entry.uuid,
                ComponentUuid::from_canonical_name(CANONICAL_TRANSFORM_NAME)
            );
            assert!(!tf_entry.is_immutable());
            assert!(tf_entry.insert_default_fn.is_some());
            assert!(reg.resolve_symbol("bam.v0.Transform.vtable").is_some());
            assert!(
                reg.resolve_symbol(
                    "bam.v0.bevy_transform::components::transform::Transform.vtable"
                )
                .is_some()
            );

            let gtf_entry = reg.get_by_name("GlobalTransform").unwrap();
            assert!(gtf_entry.is_immutable());
            assert!(gtf_entry.insert_default_fn.is_none());
            assert!(
                reg.resolve_symbol("bam.v0.GlobalTransform.vtable")
                    .is_some()
            );

            let vis_entry = reg.get_by_name("Visibility").unwrap();
            assert!(!vis_entry.is_immutable());
            assert!(vis_entry.insert_default_fn.is_some());
            assert!(reg.resolve_symbol("bam.v0.Visibility.vtable").is_some());

            let name_entry = reg.get_by_name("Name").unwrap();
            assert!(!name_entry.is_immutable());
            assert!(name_entry.insert_default_fn.is_some());
            assert!(reg.resolve_symbol("bam.v0.Name.vtable").is_some());
            assert!(
                reg.resolve_symbol("bam.v0.bevy_ecs::name::Name.vtable")
                    .is_some()
            );

            let disabled_entry = reg.get_by_name("Disabled").unwrap();
            assert_eq!(
                disabled_entry.uuid,
                ComponentUuid::from_canonical_name(CANONICAL_DISABLED_NAME)
            );
            assert_eq!(disabled_entry.size, 0);
            assert_eq!(disabled_entry.align, 1);
            assert!(disabled_entry.is_immutable());
            assert!(disabled_entry.insert_default_fn.is_some());
            assert!(reg.resolve_symbol("bam.v0.Disabled.vtable").is_some());
            assert!(
                reg.resolve_symbol("bam.v0.bevy_ecs::entity_disabling::Disabled.vtable")
                    .is_some()
            );

            for res_name in [
                "Time<Virtual>",
                "Time<Fixed>",
                "Time<Real>",
                "ButtonInput<KeyCode>",
                "ButtonInput<MouseButton>",
            ] {
                let entry = reg.get_by_name(res_name).unwrap();
                assert!(entry.is_resource());
                assert!(entry.is_immutable());
                let sym = format!("bam.v0.{res_name}.vtable");
                assert!(reg.resolve_symbol(&sym).is_some());
            }
        }

        // Verify HostWireBridge & HostStringBridge conversions directly in safe Rust.
        let mut host_tf = Transform::from_xyz(1.0, 2.0, 3.0);
        let mut wire_tf = TransformBridge::to_wire(&host_tf);
        assert_eq!(wire_tf.translation, [1.0, 2.0, 3.0]);
        wire_tf.translation = [4.0, 5.0, 6.0];
        assert_eq!(
            TransformBridge::apply_wire(&mut host_tf, wire_tf),
            FfiResult::OK
        );
        assert_eq!(host_tf.translation, Vec3::new(4.0, 5.0, 6.0));
        assert_eq!(
            TransformBridge::apply_wire(&mut host_tf, FfiTransform::from_xyz(f32::NAN, 0.0, 0.0)),
            FfiResult::INVALID_ARGUMENT
        );

        let gtf = GlobalTransform::from(Transform::from_xyz(10.0, 20.0, 30.0));
        let wire_gtf = GlobalTransformBridge::to_wire(&gtf);
        assert_eq!(wire_gtf.translation(), [10.0, 20.0, 30.0]);
        assert_eq!(wire_gtf.to_mat4()[3], [10.0, 20.0, 30.0, 1.0]);

        let mut host_vis = Visibility::Hidden;
        assert_eq!(VisibilityBridge::to_wire(&host_vis), FfiVisibility::HIDDEN);
        assert_eq!(
            VisibilityBridge::apply_wire(&mut host_vis, FfiVisibility::VISIBLE),
            FfiResult::OK
        );
        assert_eq!(host_vis, Visibility::Visible);
        assert_eq!(
            VisibilityBridge::apply_wire(&mut host_vis, FfiVisibility(99)),
            FfiResult::INVALID_ARGUMENT
        );

        let mut host_name = Name::new("Hero");
        assert_eq!(NameBridge::as_str(&host_name), "Hero");
        assert_eq!(NameBridge::set_str(&mut host_name, "Boss"), FfiResult::OK);
        assert_eq!(host_name.as_str(), "Boss");

        let w_tv = TimeVirtualBridge::to_wire(app.world().resource::<Time<Virtual>>());
        assert!(w_tv.paused.get());
        assert!((w_tv.relative_speed - 0.5).abs() < 1e-6);

        let w_tf = TimeFixedBridge::to_wire(app.world().resource::<Time<Fixed>>());
        assert!(w_tf.timestep_secs > 0.01 && w_tf.overstep_fraction > 0.0);

        let w_tr = TimeRealBridge::to_wire(app.world().resource::<Time<Real>>());
        assert!((w_tr.delta_secs - 0.025).abs() < 1e-5);

        let w_kb = KeyboardInputBridge::to_wire(app.world().resource::<ButtonInput<KeyCode>>());
        assert!(w_kb.pressed(FfiKeyCode::from_bevy(KeyCode::Space).unwrap()));
        assert!(w_kb.just_pressed(FfiKeyCode::from_bevy(KeyCode::Space).unwrap()));
        assert!(w_kb.just_released(FfiKeyCode::from_bevy(KeyCode::KeyW).unwrap()));
        assert!(!w_kb.pressed(FfiKeyCode::from_bevy(KeyCode::KeyW).unwrap()));

        let w_ms = MouseInputBridge::to_wire(app.world().resource::<ButtonInput<MouseButton>>());
        assert!(w_ms.pressed(FfiMouseButton::LEFT));
        assert!(w_ms.pressed(FfiMouseButton::from_bevy(MouseButton::Other(2)).unwrap()));
        assert!(!w_ms.pressed(FfiMouseButton::RIGHT));

        // Verify `insert_default_fn` inserts host `Transform`, `Visibility`, `Name`, and `Disabled`.
        assert_eq!(DisabledBridge::to_wire(&Disabled), FfiBool::TRUE);
        let (tf_ins, vis_ins, name_ins, disabled_ins) = {
            let reg = app.world().resource::<ModRegistry>();
            (
                reg.get_by_name("Transform")
                    .unwrap()
                    .insert_default_fn
                    .unwrap(),
                reg.get_by_name("Visibility")
                    .unwrap()
                    .insert_default_fn
                    .unwrap(),
                reg.get_by_name("Name").unwrap().insert_default_fn.unwrap(),
                reg.get_by_name("Disabled")
                    .unwrap()
                    .insert_default_fn
                    .unwrap(),
            )
        };
        tf_ins(app.world_mut(), bare);
        vis_ins(app.world_mut(), bare);
        name_ins(app.world_mut(), bare);
        disabled_ins(app.world_mut(), bare);

        assert_eq!(
            *app.world().get::<Transform>(bare).unwrap(),
            Transform::IDENTITY
        );
        assert_eq!(
            *app.world().get::<Visibility>(bare).unwrap(),
            Visibility::Inherited
        );
        assert!(app.world().get::<Name>(bare).is_some());
        assert!(app.world().get::<Disabled>(bare).is_some());

        let _ = (
            core::mem::size_of::<MappedWireVTable<FfiTransform>>(),
            core::mem::size_of::<MappedStringVTable>(),
        );
    }
}
