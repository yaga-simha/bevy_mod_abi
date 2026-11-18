//! Guest-side `#[repr(C)]` wire types and safe VTable wrappers for core Bevy `0.20` components
//! and resources (`Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time`, `ButtonInput`).
//!
//! Enforces `#![forbid(unsafe_code)]` with zero Bevy dependencies.

use bevy_abi_mod_guest_sys::{Host, ValidatedIter, ValidatedStringVTable};
#[cfg(feature = "derive")]
use bevy_abi_mod_guest_sys::{ValidatedSystemRef, ValidatedWireVTable};
#[cfg(feature = "derive")]
use bevy_abi_mod_macros::AbiPod;
#[cfg(feature = "derive")]
use bevy_abi_mod_sys::{AbiPod, BamChangeTicks, BamComponent};
use bevy_abi_mod_sys::{ComponentUuid, FfiBool, FfiResult};
use core::ffi::CStr;
#[cfg(feature = "derive")]
use core::marker::PhantomData;

#[cfg(feature = "derive")]
use crate::app::SystemContext;
use crate::iter::{Chunk, ChunkIter};

/// Canonical ABI name for bridged host `Transform`.
pub const CANONICAL_TRANSFORM_NAME: &str = "bevy_transform::components::transform::Transform";
/// Canonical ABI name for bridged host `GlobalTransform`.
pub const CANONICAL_GLOBAL_TRANSFORM_NAME: &str =
    "bevy_transform::components::global_transform::GlobalTransform";
/// Canonical ABI name for bridged host `Visibility`.
pub const CANONICAL_VISIBILITY_NAME: &str = "bevy_camera::visibility::Visibility";
/// Canonical ABI name for bridged host `Name`.
pub const CANONICAL_NAME_NAME: &str = "bevy_ecs::name::Name";
/// Canonical ABI name for bridged host `Time<Virtual>`.
pub const CANONICAL_TIME_VIRTUAL_NAME: &str = "bevy_time::Time<Virtual>";
/// Canonical ABI name for bridged host `Time<Fixed>`.
pub const CANONICAL_TIME_FIXED_NAME: &str = "bevy_time::Time<Fixed>";
/// Canonical ABI name for bridged host `Time<Real>`.
pub const CANONICAL_TIME_REAL_NAME: &str = "bevy_time::Time<Real>";
/// Canonical ABI name for bridged host `ButtonInput<KeyCode>`.
pub const CANONICAL_KEYBOARD_INPUT_NAME: &str = "bevy_input::ButtonInput<KeyCode>";
/// Canonical ABI name for bridged host `ButtonInput<MouseButton>`.
pub const CANONICAL_MOUSE_INPUT_NAME: &str = "bevy_input::ButtonInput<MouseButton>";

/// Fixed-layout `#[repr(C)]` wire projection of Bevy `Transform`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

    /// Constructs a transform from a `[x, y, z]` translation vector.
    #[inline]
    pub const fn from_translation(translation: [f32; 3]) -> Self {
        Self {
            translation,
            ..Self::IDENTITY
        }
    }

    /// Constructs a transform from an `[x, y, z, w]` quaternion.
    #[inline]
    pub const fn from_rotation(rotation: [f32; 4]) -> Self {
        Self {
            rotation,
            ..Self::IDENTITY
        }
    }

    /// Constructs a transform from an `[x, y, z]` scale vector.
    #[inline]
    pub const fn from_scale(scale: [f32; 3]) -> Self {
        Self {
            scale,
            ..Self::IDENTITY
        }
    }

    /// Returns a copy of this transform with `translation` replaced.
    #[inline]
    #[must_use]
    pub const fn with_translation(mut self, translation: [f32; 3]) -> Self {
        self.translation = translation;
        self
    }

    /// Returns a copy of this transform with `rotation` replaced.
    #[inline]
    #[must_use]
    pub const fn with_rotation(mut self, rotation: [f32; 4]) -> Self {
        self.rotation = rotation;
        self
    }

    /// Returns a copy of this transform with `scale` replaced.
    #[inline]
    #[must_use]
    pub const fn with_scale(mut self, scale: [f32; 3]) -> Self {
        self.scale = scale;
        self
    }
}

impl Default for FfiTransform {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Fixed-layout `#[repr(C)]` wire projection of Bevy `GlobalTransform` (`Affine3A` 3x4 columns).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

/// Fixed-layout `#[repr(transparent)]` wire projection of Bevy `Visibility`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
pub struct FfiVisibility(pub u8);

impl FfiVisibility {
    /// Inherit visibility from parent hierarchy (`0`).
    pub const INHERITED: Self = Self(0);
    /// Unconditionally hidden (`1`).
    pub const HIDDEN: Self = Self(1);
    /// Unconditionally visible (`2`).
    pub const VISIBLE: Self = Self(2);

    /// Returns `true` if `self == Self::INHERITED`.
    #[inline]
    pub const fn is_inherited(self) -> bool {
        self.0 == Self::INHERITED.0
    }

    /// Returns `true` if `self == Self::HIDDEN`.
    #[inline]
    pub const fn is_hidden(self) -> bool {
        self.0 == Self::HIDDEN.0
    }

    /// Returns `true` if `self == Self::VISIBLE`.
    #[inline]
    pub const fn is_visible(self) -> bool {
        self.0 == Self::VISIBLE.0
    }
}

/// Fixed-layout `#[repr(C)]` wire projection of Bevy `Time<Virtual>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

/// Fixed-layout `#[repr(C)]` wire projection of Bevy `Time<Fixed>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

/// Fixed-layout `#[repr(C)]` wire projection of Bevy `Time<Real>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

/// Compact deterministic bit-index key code (`0..194`) mirroring Bevy's `KeyCode`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
pub struct FfiKeyCode(pub u16);

#[allow(missing_docs)]
impl FfiKeyCode {
    pub const BACKQUOTE: Self = Self(0);
    pub const BACKSLASH: Self = Self(1);
    pub const BRACKET_LEFT: Self = Self(2);
    pub const BRACKET_RIGHT: Self = Self(3);
    pub const COMMA: Self = Self(4);
    pub const DIGIT_0: Self = Self(5);
    pub const DIGIT_1: Self = Self(6);
    pub const DIGIT_2: Self = Self(7);
    pub const DIGIT_3: Self = Self(8);
    pub const DIGIT_4: Self = Self(9);
    pub const DIGIT_5: Self = Self(10);
    pub const DIGIT_6: Self = Self(11);
    pub const DIGIT_7: Self = Self(12);
    pub const DIGIT_8: Self = Self(13);
    pub const DIGIT_9: Self = Self(14);
    pub const EQUAL: Self = Self(15);
    pub const KEY_A: Self = Self(19);
    pub const KEY_B: Self = Self(20);
    pub const KEY_C: Self = Self(21);
    pub const KEY_D: Self = Self(22);
    pub const KEY_E: Self = Self(23);
    pub const KEY_F: Self = Self(24);
    pub const KEY_G: Self = Self(25);
    pub const KEY_H: Self = Self(26);
    pub const KEY_I: Self = Self(27);
    pub const KEY_J: Self = Self(28);
    pub const KEY_K: Self = Self(29);
    pub const KEY_L: Self = Self(30);
    pub const KEY_M: Self = Self(31);
    pub const KEY_N: Self = Self(32);
    pub const KEY_O: Self = Self(33);
    pub const KEY_P: Self = Self(34);
    pub const KEY_Q: Self = Self(35);
    pub const KEY_R: Self = Self(36);
    pub const KEY_S: Self = Self(37);
    pub const KEY_T: Self = Self(38);
    pub const KEY_U: Self = Self(39);
    pub const KEY_V: Self = Self(40);
    pub const KEY_W: Self = Self(41);
    pub const KEY_X: Self = Self(42);
    pub const KEY_Y: Self = Self(43);
    pub const KEY_Z: Self = Self(44);
    pub const MINUS: Self = Self(45);
    pub const PERIOD: Self = Self(46);
    pub const QUOTE: Self = Self(47);
    pub const SEMICOLON: Self = Self(48);
    pub const SLASH: Self = Self(49);
    pub const ALT_LEFT: Self = Self(50);
    pub const ALT_RIGHT: Self = Self(51);
    pub const BACKSPACE: Self = Self(52);
    pub const CAPS_LOCK: Self = Self(53);
    pub const CONTROL_LEFT: Self = Self(55);
    pub const CONTROL_RIGHT: Self = Self(56);
    pub const ENTER: Self = Self(57);
    pub const SUPER_LEFT: Self = Self(58);
    pub const SUPER_RIGHT: Self = Self(59);
    pub const SHIFT_LEFT: Self = Self(60);
    pub const SHIFT_RIGHT: Self = Self(61);
    pub const SPACE: Self = Self(62);
    pub const TAB: Self = Self(63);
    pub const DELETE: Self = Self(72);
    pub const END: Self = Self(73);
    pub const HOME: Self = Self(75);
    pub const INSERT: Self = Self(76);
    pub const PAGE_DOWN: Self = Self(77);
    pub const PAGE_UP: Self = Self(78);
    pub const ARROW_DOWN: Self = Self(79);
    pub const ARROW_LEFT: Self = Self(80);
    pub const ARROW_RIGHT: Self = Self(81);
    pub const ARROW_UP: Self = Self(82);
    pub const ESCAPE: Self = Self(114);
    pub const F1: Self = Self(159);
    pub const F2: Self = Self(160);
    pub const F3: Self = Self(161);
    pub const F4: Self = Self(162);
    pub const F5: Self = Self(163);
    pub const F6: Self = Self(164);
    pub const F7: Self = Self(165);
    pub const F8: Self = Self(166);
    pub const F9: Self = Self(167);
    pub const F10: Self = Self(168);
    pub const F11: Self = Self(169);
    pub const F12: Self = Self(170);
}

/// 256-bit keyboard state snapshot (`pressed`, `just_pressed`, `just_released`) projecting
/// Bevy's `ButtonInput<KeyCode>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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
    const fn get_bit(words: &[u64; 4], code: FfiKeyCode) -> bool {
        let idx = code.0 as usize;
        if idx >= 256 {
            false
        } else {
            (words[idx / 64] & (1u64 << (idx % 64))) != 0
        }
    }

    #[inline]
    fn set_bit(words: &mut [u64; 4], code: FfiKeyCode, active: bool) {
        let idx = code.0 as usize;
        if idx < 256 {
            let mask = 1u64 << (idx % 64);
            if active {
                words[idx / 64] |= mask;
            } else {
                words[idx / 64] &= !mask;
            }
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

    /// Sets or clears the `pressed` bit for `code`.
    #[inline]
    pub fn set_pressed(&mut self, code: FfiKeyCode, active: bool) {
        Self::set_bit(&mut self.pressed, code, active);
    }

    /// Sets or clears the `just_pressed` bit for `code`.
    #[inline]
    pub fn set_just_pressed(&mut self, code: FfiKeyCode, active: bool) {
        Self::set_bit(&mut self.just_pressed, code, active);
    }

    /// Sets or clears the `just_released` bit for `code`.
    #[inline]
    pub fn set_just_released(&mut self, code: FfiKeyCode, active: bool) {
        Self::set_bit(&mut self.just_released, code, active);
    }
}

/// Compact deterministic bit-index mouse button (`0..64`) mirroring Bevy's `MouseButton`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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

    /// Constructs a mouse button handle for `MouseButton::Other(n)` (`n < 59`).
    #[inline]
    pub const fn other(n: u16) -> Option<Self> {
        let idx = 5u16.saturating_add(n);
        if idx < 64 {
            Some(Self(idx as u8))
        } else {
            None
        }
    }
}

/// 64-bit mouse button state snapshot (`pressed`, `just_pressed`, `just_released`) projecting
/// Bevy's `ButtonInput<MouseButton>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
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
    fn set_bit(words: &mut u64, button: FfiMouseButton, active: bool) {
        if button.0 < 64 {
            let mask = 1u64 << button.0;
            if active {
                *words |= mask;
            } else {
                *words &= !mask;
            }
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

    /// Sets or clears the `pressed` bit for `button`.
    #[inline]
    pub fn set_pressed(&mut self, button: FfiMouseButton, active: bool) {
        Self::set_bit(&mut self.pressed, button, active);
    }

    /// Sets or clears the `just_pressed` bit for `button`.
    #[inline]
    pub fn set_just_pressed(&mut self, button: FfiMouseButton, active: bool) {
        Self::set_bit(&mut self.just_pressed, button, active);
    }

    /// Sets or clears the `just_released` bit for `button`.
    #[inline]
    pub fn set_just_released(&mut self, button: FfiMouseButton, active: bool) {
        Self::set_bit(&mut self.just_released, button, active);
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

/// Safe guest wrapper around the host's [`ValidatedStringVTable`] for `bevy_ecs::name::Name`.
#[derive(Clone, Copy)]
pub struct GuestNameBridge {
    vtable: ValidatedStringVTable,
}

impl GuestNameBridge {
    /// Resolves `"bam.v0.bevy_ecs::name::Name.vtable"` (falling back to `"bam.v0.Name.vtable"`)
    /// and validates its [`VTableHeader`](bevy_abi_mod_sys::VTableHeader).
    pub fn resolve(host: &Host) -> Result<Self, FfiResult> {
        let uuid = ComponentUuid::from_canonical_name(CANONICAL_NAME_NAME);
        let vtable = host
            .resolve_mapped_string_vtable(c"bam.v0.bevy_ecs::name::Name.vtable", uuid)
            .or_else(|_| host.resolve_mapped_string_vtable(c"bam.v0.Name.vtable", uuid))?;
        Ok(Self { vtable })
    }

    /// Returns the underlying validated [`ValidatedStringVTable`].
    #[inline]
    pub const fn vtable(&self) -> &ValidatedStringVTable {
        &self.vtable
    }

    /// Returns `true` if `Name` mutation is supported by the host bridge.
    #[inline]
    pub const fn supports_set(&self) -> bool {
        self.vtable.supports_set()
    }

    /// Reads `Name` at `(col, row)` of `chunk` into an owned guest `String`.
    #[inline]
    pub fn read_chunk_row(
        &self,
        chunk: &mut Chunk<'_, '_>,
        col: usize,
        row: usize,
    ) -> Result<Option<String>, FfiResult> {
        chunk.iter.read_mapped_string_row(col, row, &self.vtable)
    }

    /// Writes `text` into `Name` at `(col, row)` of `chunk`.
    #[inline]
    pub fn write_chunk_row(
        &self,
        chunk: &mut Chunk<'_, '_>,
        col: usize,
        row: usize,
        text: &str,
    ) -> FfiResult {
        chunk
            .iter
            .write_mapped_string_row(col, row, &self.vtable, text)
    }

    /// Reads `Name` at `(col, row)` of the active chunk in `iter`.
    #[inline]
    pub fn read_row(
        &self,
        iter: &mut ChunkIter<'_>,
        col: usize,
        row: usize,
    ) -> Result<Option<String>, FfiResult> {
        iter.inner.read_mapped_string_row(col, row, &self.vtable)
    }

    /// Writes `text` into `Name` at `(col, row)` of the active chunk in `iter`.
    #[inline]
    pub fn write_row(
        &self,
        iter: &mut ChunkIter<'_>,
        col: usize,
        row: usize,
        text: &str,
    ) -> FfiResult {
        iter.inner
            .write_mapped_string_row(col, row, &self.vtable, text)
    }

    /// Reads `Name` at `(col, row)` directly from a [`ValidatedIter`].
    #[inline]
    pub fn read_validated_row(
        &self,
        iter: &mut ValidatedIter<'_>,
        col: usize,
        row: usize,
    ) -> Result<Option<String>, FfiResult> {
        iter.read_mapped_string_row(col, row, &self.vtable)
    }

    /// Writes `text` into `Name` at `(col, row)` directly on a [`ValidatedIter`].
    #[inline]
    pub fn write_validated_row(
        &self,
        iter: &mut ValidatedIter<'_>,
        col: usize,
        row: usize,
        text: &str,
    ) -> FfiResult {
        iter.write_mapped_string_row(col, row, &self.vtable, text)
    }
}

/// Safe guest wrapper around a validated [`ValidatedWireVTable<W>`] for bridged Bevy components
/// and resources (available when `feature = "derive"` is enabled so wire structs implement [`AbiPod`]).
#[cfg(feature = "derive")]
pub struct GuestWireBridge<W: AbiPod> {
    vtable: ValidatedWireVTable<W>,
    _marker: PhantomData<W>,
}

#[cfg(feature = "derive")]
impl<W: AbiPod> Copy for GuestWireBridge<W> {}

#[cfg(feature = "derive")]
impl<W: AbiPod> Clone for GuestWireBridge<W> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(feature = "derive")]
impl<W: AbiPod> GuestWireBridge<W> {
    /// Resolves `canonical_vtable_symbol` (or optional `alias_vtable_symbol`) against
    /// `canonical_name`'s [`ComponentUuid`].
    pub fn resolve(
        host: &Host,
        canonical_name: &str,
        canonical_vtable_symbol: &CStr,
        alias_vtable_symbol: Option<&CStr>,
    ) -> Result<Self, FfiResult> {
        let uuid = ComponentUuid::from_canonical_name(canonical_name);
        let vtable = match host.resolve_mapped_wire_vtable::<W>(canonical_vtable_symbol, uuid) {
            Ok(vt) => vt,
            Err(err) => match alias_vtable_symbol {
                Some(alias) => host.resolve_mapped_wire_vtable::<W>(alias, uuid)?,
                None => return Err(err),
            },
        };
        Ok(Self {
            vtable,
            _marker: PhantomData,
        })
    }

    /// Returns the underlying [`ValidatedWireVTable<W>`].
    #[inline]
    pub const fn vtable(&self) -> &ValidatedWireVTable<W> {
        &self.vtable
    }

    /// Returns `true` if mutable `set` writes are supported by this bridge.
    #[inline]
    pub const fn supports_set(&self) -> bool {
        self.vtable.supports_set()
    }

    /// Reads row `row` of column `col` in `chunk` into `W`.
    #[inline]
    pub fn read_chunk_row(
        &self,
        chunk: &mut Chunk<'_, '_>,
        col: usize,
        row: usize,
    ) -> Result<Option<W>, FfiResult> {
        chunk.iter.read_mapped_wire_row(col, row, &self.vtable)
    }

    /// Writes `value` into row `row` of mutable column `col` in `chunk`.
    #[inline]
    pub fn write_chunk_row(
        &self,
        chunk: &mut Chunk<'_, '_>,
        col: usize,
        row: usize,
        value: &W,
    ) -> FfiResult {
        chunk
            .iter
            .write_mapped_wire_row(col, row, &self.vtable, value)
    }

    /// Reads row `row` of column `col` from the active chunk of `iter`.
    #[inline]
    pub fn read_row(
        &self,
        iter: &mut ChunkIter<'_>,
        col: usize,
        row: usize,
    ) -> Result<Option<W>, FfiResult> {
        iter.inner.read_mapped_wire_row(col, row, &self.vtable)
    }

    /// Writes `value` into row `row` of mutable column `col` in `iter`.
    #[inline]
    pub fn write_row(
        &self,
        iter: &mut ChunkIter<'_>,
        col: usize,
        row: usize,
        value: &W,
    ) -> FfiResult {
        iter.inner
            .write_mapped_wire_row(col, row, &self.vtable, value)
    }

    /// Reads row `row` of column `col` directly from a [`ValidatedIter`].
    #[inline]
    pub fn read_validated_row(
        &self,
        iter: &mut ValidatedIter<'_>,
        col: usize,
        row: usize,
    ) -> Result<Option<W>, FfiResult> {
        iter.read_mapped_wire_row(col, row, &self.vtable)
    }

    /// Writes `value` into row `row` of mutable column `col` directly on a [`ValidatedIter`].
    #[inline]
    pub fn write_validated_row(
        &self,
        iter: &mut ValidatedIter<'_>,
        col: usize,
        row: usize,
        value: &W,
    ) -> FfiResult {
        iter.write_mapped_wire_row(col, row, &self.vtable, value)
    }

    /// Reads a bridged singleton resource declared in query `0` at `term_index`.
    #[inline]
    pub fn read_resource(
        &self,
        ctx: &mut SystemContext<'_>,
        term_index: usize,
    ) -> Result<W, FfiResult> {
        ctx.iter()
            .inner
            .read_mapped_wire_resource(term_index, &self.vtable)
    }

    /// Reads a bridged singleton resource declared in query `query_index` at `term_index`.
    #[inline]
    pub fn read_query_resource(
        &self,
        ctx: &mut SystemContext<'_>,
        query_index: usize,
        term_index: usize,
    ) -> Result<W, FfiResult> {
        let iter = ctx.query_mut(query_index).ok_or(FfiResult::OUT_OF_RANGE)?;
        iter.inner
            .read_mapped_wire_resource(term_index, &self.vtable)
    }

    /// Writes `value` into a mutable bridged singleton resource declared in query `0` at `term_index`.
    #[inline]
    pub fn write_resource(
        &self,
        ctx: &mut SystemContext<'_>,
        term_index: usize,
        value: &W,
    ) -> FfiResult {
        ctx.iter()
            .inner
            .write_mapped_wire_resource(term_index, &self.vtable, value)
    }

    /// Writes `value` into a mutable bridged singleton resource declared in `query_index` at `term_index`.
    #[inline]
    pub fn write_query_resource(
        &self,
        ctx: &mut SystemContext<'_>,
        query_index: usize,
        term_index: usize,
        value: &W,
    ) -> FfiResult {
        let Some(iter) = ctx.query_mut(query_index) else {
            return FfiResult::OUT_OF_RANGE;
        };
        iter.inner
            .write_mapped_wire_resource(term_index, &self.vtable, value)
    }

    /// Reads a bridged singleton resource by pre-resolved `BamComponent` handle via [`ValidatedSystemRef`].
    #[inline]
    pub fn read_resource_by_handle(
        &self,
        sys_ref: &ValidatedSystemRef<'_>,
        component: BamComponent,
    ) -> Result<(W, BamChangeTicks), FfiResult> {
        sys_ref.read_mapped_wire_resource_by_handle(component, &self.vtable)
    }

    /// Writes a bridged singleton resource by pre-resolved `BamComponent` handle via [`ValidatedSystemRef`].
    #[inline]
    pub fn write_resource_by_handle(
        &self,
        sys_ref: &mut ValidatedSystemRef<'_>,
        component: BamComponent,
        value: &W,
    ) -> Result<BamChangeTicks, FfiResult> {
        sys_ref.write_mapped_wire_resource_by_handle(component, &self.vtable, value)
    }
}

/// Guest wire bridge for Bevy `Transform`.
#[cfg(feature = "derive")]
pub type GuestTransformBridge = GuestWireBridge<FfiTransform>;
/// Guest wire bridge for Bevy `GlobalTransform`.
#[cfg(feature = "derive")]
pub type GuestGlobalTransformBridge = GuestWireBridge<FfiGlobalTransform>;
/// Guest wire bridge for Bevy `Visibility`.
#[cfg(feature = "derive")]
pub type GuestVisibilityBridge = GuestWireBridge<FfiVisibility>;
/// Guest wire bridge for Bevy `Time<Virtual>`.
#[cfg(feature = "derive")]
pub type GuestTimeVirtualBridge = GuestWireBridge<FfiTimeVirtual>;
/// Guest wire bridge for Bevy `Time<Fixed>`.
#[cfg(feature = "derive")]
pub type GuestTimeFixedBridge = GuestWireBridge<FfiTimeFixed>;
/// Guest wire bridge for Bevy `Time<Real>`.
#[cfg(feature = "derive")]
pub type GuestTimeRealBridge = GuestWireBridge<FfiTimeReal>;
/// Guest wire bridge for Bevy `ButtonInput<KeyCode>`.
#[cfg(feature = "derive")]
pub type GuestKeyboardInputBridge = GuestWireBridge<FfiKeyboardInput>;
/// Guest wire bridge for Bevy `ButtonInput<MouseButton>`.
#[cfg(feature = "derive")]
pub type GuestMouseInputBridge = GuestWireBridge<FfiMouseInput>;

#[cfg(feature = "derive")]
impl GuestTransformBridge {
    /// Resolves the host `Transform` VTable (`"bam.v0.Transform.vtable"`).
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_TRANSFORM_NAME,
            c"bam.v0.bevy_transform::components::transform::Transform.vtable",
            Some(c"bam.v0.Transform.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestGlobalTransformBridge {
    /// Resolves the host `GlobalTransform` VTable (`"bam.v0.GlobalTransform.vtable"`).
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_GLOBAL_TRANSFORM_NAME,
            c"bam.v0.bevy_transform::components::global_transform::GlobalTransform.vtable",
            Some(c"bam.v0.GlobalTransform.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestVisibilityBridge {
    /// Resolves the host `Visibility` VTable (`"bam.v0.Visibility.vtable"`).
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_VISIBILITY_NAME,
            c"bam.v0.bevy_camera::visibility::Visibility.vtable",
            Some(c"bam.v0.Visibility.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestTimeVirtualBridge {
    /// Resolves the host `Time<Virtual>` VTable.
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_TIME_VIRTUAL_NAME,
            c"bam.v0.bevy_time::Time<Virtual>.vtable",
            Some(c"bam.v0.Time<Virtual>.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestTimeFixedBridge {
    /// Resolves the host `Time<Fixed>` VTable.
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_TIME_FIXED_NAME,
            c"bam.v0.bevy_time::Time<Fixed>.vtable",
            Some(c"bam.v0.Time<Fixed>.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestTimeRealBridge {
    /// Resolves the host `Time<Real>` VTable.
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_TIME_REAL_NAME,
            c"bam.v0.bevy_time::Time<Real>.vtable",
            Some(c"bam.v0.Time<Real>.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestKeyboardInputBridge {
    /// Resolves the host `ButtonInput<KeyCode>` VTable.
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_KEYBOARD_INPUT_NAME,
            c"bam.v0.bevy_input::ButtonInput<KeyCode>.vtable",
            Some(c"bam.v0.ButtonInput<KeyCode>.vtable"),
        )
    }
}

#[cfg(feature = "derive")]
impl GuestMouseInputBridge {
    /// Resolves the host `ButtonInput<MouseButton>` VTable.
    pub fn resolve_std(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve(
            host,
            CANONICAL_MOUSE_INPUT_NAME,
            c"bam.v0.bevy_input::ButtonInput<MouseButton>.vtable",
            Some(c"bam.v0.ButtonInput<MouseButton>.vtable"),
        )
    }
}

/// Resolves a `MappedStringVTable` directly via `Host::resolve_mapped_string_vtable` for custom
/// host string components.
#[inline]
pub fn resolve_string_vtable(
    host: &Host,
    symbol: &CStr,
    canonical_name: &str,
) -> Result<ValidatedStringVTable, FfiResult> {
    host.resolve_mapped_string_vtable(symbol, ComponentUuid::from_canonical_name(canonical_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_sys::{
        ABI_VERSION, BamChangeTicks, BamColumnInfo, BamComponent, BamHostApi, BamHostCtx,
        BamLoadCtx, MappedStringVTable, StrView, VTableHeader,
    };
    use core::ffi::{c_char, c_void};

    #[test]
    fn ffi_wire_helpers_and_bitsets_work_without_unsafe() {
        let tf = FfiTransform::from_xyz(1.0, 2.0, 3.0).with_scale([2.0, 2.0, 2.0]);
        assert_eq!(tf.translation, [1.0, 2.0, 3.0]);
        assert_eq!(tf.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(tf.scale, [2.0, 2.0, 2.0]);

        let gtf = FfiGlobalTransform {
            matrix_cols: [
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [4.0, 5.0, 6.0],
            ],
        };
        assert_eq!(gtf.translation(), [4.0, 5.0, 6.0]);
        assert_eq!(gtf.to_mat4()[3], [4.0, 5.0, 6.0, 1.0]);

        assert!(FfiVisibility::INHERITED.is_inherited());
        assert!(FfiVisibility::HIDDEN.is_hidden());
        assert!(FfiVisibility::VISIBLE.is_visible());

        let mut kb = FfiKeyboardInput::default();
        kb.set_pressed(FfiKeyCode::SPACE, true);
        kb.set_just_pressed(FfiKeyCode::F12, true);
        kb.set_just_released(FfiKeyCode::KEY_W, true);
        assert!(kb.pressed(FfiKeyCode::SPACE));
        assert!(!kb.pressed(FfiKeyCode::KEY_W));
        assert!(kb.just_pressed(FfiKeyCode::F12));
        assert!(kb.just_released(FfiKeyCode::KEY_W));

        let mut ms = FfiMouseInput::default();
        ms.set_pressed(FfiMouseButton::LEFT, true);
        ms.set_just_pressed(FfiMouseButton::other(2).unwrap(), true);
        assert!(ms.pressed(FfiMouseButton::LEFT));
        assert!(!ms.pressed(FfiMouseButton::RIGHT));
        assert!(ms.just_pressed(FfiMouseButton::other(2).unwrap()));
    }

    #[test]
    fn guest_name_bridge_vtable_header_matches_host_uuid() {
        static NAME_VTABLE: MappedStringVTable = MappedStringVTable {
            header: VTableHeader::from_raw_parts(
                core::mem::size_of::<MappedStringVTable>() as u32,
                ComponentUuid::from_canonical_name(CANONICAL_NAME_NAME),
                24,
                8,
            ),
            get: Some(mock_name_get),
            set: Some(mock_name_set),
        };

        extern "C" fn mock_name_get(_ptr: *const c_void) -> StrView {
            StrView::from_str("MockName")
        }
        extern "C" fn mock_name_set(_ptr: *mut c_void, _view: StrView) -> FfiResult {
            FfiResult::OK
        }
        extern "C" fn mock_get_proc(_ctx: *const BamHostCtx, _sym: *const c_char) -> *const c_void {
            (&raw const NAME_VTABLE).cast::<c_void>()
        }

        assert_eq!(
            NAME_VTABLE.header.uuid,
            ComponentUuid::from_canonical_name("bevy_ecs::name::Name")
        );
        let _ = (
            ABI_VERSION,
            BamChangeTicks::default(),
            core::mem::size_of::<BamColumnInfo>(),
            BamComponent::INVALID,
            BamHostApi::MIN_V0_2_SIZE,
            core::mem::size_of::<BamLoadCtx>(),
            mock_get_proc,
        );
    }

    #[cfg(feature = "derive")]
    #[test]
    fn guest_wire_bridge_validates_mapped_wire_vtable_with_derive() {
        // Compile-time proof that all std wire structs implement `AbiPod` when `derive` is enabled.
        bevy_abi_mod_sys::assert_abi_pod::<FfiTransform>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiGlobalTransform>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiVisibility>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiTimeVirtual>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiTimeFixed>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiTimeReal>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiKeyboardInput>();
        bevy_abi_mod_sys::assert_abi_pod::<FfiMouseInput>();
        assert_eq!(
            core::mem::size_of::<bevy_abi_mod_sys::MappedWireVTable<FfiTransform>>(),
            core::mem::size_of::<MappedStringVTable>()
        );
    }
}
