//! Bevy `0.20` flattened `bevy_picking` observer bridge and [`EntityCursor`] component bridge
//! (`feature = "bridge-picking"`).
//!
//! Translates Bevy's flattened `#[derive(EntityEvent)]` picking events
//! ([`PointerPress`], [`PointerRelease`], [`PointerMove`], [`PointerClick`], [`PointerOver`],
//! [`PointerOut`], [`PointerDrag`]) into zero-padding `#[repr(C)]` [`AbiPod`](bevy_abi_mod_sys::AbiPod)
//! and [`ModComponent`](bevy_abi_mod_sys::ModComponent) entity events delivered directly to native
//! guest observers registered via `register_observer` or runtime `cmd_observe`, and synchronizes
//! [`FfiEntityCursor`] bidirectionally with Bevy's [`EntityCursor`] component.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{ComponentCollisionError, ComponentOptions, ModRegistry};
use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{FfiBool, FfiEntity, assert_abi_pod};
use bevy_app::{App, Plugin, PostUpdate, PreUpdate};
use bevy_camera::NormalizedRenderTarget;
use bevy_ecs::component::Component;
use bevy_ecs::entity::{ContainsEntity, Entity};
use bevy_ecs::error::{ErrorContext, error as log_ecs_error};
use bevy_ecs::event::EntityEvent;
use bevy_ecs::lifecycle::RemovedComponents;
use bevy_ecs::observer::On;
use bevy_ecs::query::{Changed, Or, With, Without};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::system::{Commands, Query};
use bevy_ecs::world::World;
use bevy_math::Vec3;
use bevy_picking::backend::HitData;
use bevy_picking::cursor::EntityCursor;
use bevy_picking::events::{
    Pointer, PointerClick, PointerDrag, PointerMove, PointerOut, PointerOver, PointerPress,
    PointerRelease,
};
use bevy_picking::pointer::{PointerButton, PointerId};
use bevy_window::{SystemCursorIcon, Window};

/// Canonical ABI name for bridged [`PointerPress`] (`FfiPointerPress`).
pub const CANONICAL_POINTER_PRESS_NAME: &str = "bevy_picking::events::PointerPress";
/// Canonical ABI name for bridged [`PointerRelease`] (`FfiPointerRelease`).
pub const CANONICAL_POINTER_RELEASE_NAME: &str = "bevy_picking::events::PointerRelease";
/// Canonical ABI name for bridged [`PointerMove`] (`FfiPointerMove`).
pub const CANONICAL_POINTER_MOVE_NAME: &str = "bevy_picking::events::PointerMove";
/// Canonical ABI name for bridged [`PointerClick`] (`FfiPointerClick`).
pub const CANONICAL_POINTER_CLICK_NAME: &str = "bevy_picking::events::PointerClick";
/// Canonical ABI name for bridged [`PointerOver`] (`FfiPointerOver`).
pub const CANONICAL_POINTER_OVER_NAME: &str = "bevy_picking::events::PointerOver";
/// Canonical ABI name for bridged [`PointerOut`] (`FfiPointerOut`).
pub const CANONICAL_POINTER_OUT_NAME: &str = "bevy_picking::events::PointerOut";
/// Canonical ABI name for bridged [`PointerDrag`] (`FfiPointerDrag`).
pub const CANONICAL_POINTER_DRAG_NAME: &str = "bevy_picking::events::PointerDrag";
/// Canonical ABI name for bridged [`EntityCursor`] (`FfiEntityCursor`).
pub const CANONICAL_ENTITY_CURSOR_NAME: &str = "bevy_picking::cursor::EntityCursor";

/// Wire discriminant identifying the kind of Bevy [`PointerId`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiPointerKind(pub u8);

impl FfiPointerKind {
    /// Mouse pointer (`PointerId::Mouse` = `0`).
    pub const MOUSE: Self = Self(0);
    /// Touch pointer (`PointerId::Touch(u64)` = `1`).
    pub const TOUCH: Self = Self(1);
    /// Custom UUID-identified pointer (`PointerId::Custom(Uuid)` = `2`).
    pub const CUSTOM: Self = Self(2);

    /// Raw `u8` tag for [`Self::MOUSE`].
    pub const MOUSE_RAW: u8 = 0;
    /// Raw `u8` tag for [`Self::TOUCH`].
    pub const TOUCH_RAW: u8 = 1;
    /// Raw `u8` tag for [`Self::CUSTOM`].
    pub const CUSTOM_RAW: u8 = 2;

    /// Converts a Bevy [`PointerId`] into its [`FfiPointerKind`].
    #[inline]
    pub const fn from_bevy(id: PointerId) -> Self {
        match id {
            PointerId::Mouse => Self::MOUSE,
            PointerId::Touch(_) => Self::TOUCH,
            PointerId::Custom(_) => Self::CUSTOM,
        }
    }
}

/// Wire discriminant identifying a Bevy [`PointerButton`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, AbiPod)]
pub struct FfiPointerButton(pub u8);

impl FfiPointerButton {
    /// No pointer button (`0`).
    pub const NONE: Self = Self(0);
    /// Primary pointer button (`PointerButton::Primary` = `1`).
    pub const PRIMARY: Self = Self(1);
    /// Secondary pointer button (`PointerButton::Secondary` = `2`).
    pub const SECONDARY: Self = Self(2);
    /// Middle/tertiary pointer button (`PointerButton::Middle` = `3`).
    pub const MIDDLE: Self = Self(3);

    /// Raw `u8` tag for [`Self::NONE`].
    pub const NONE_RAW: u8 = 0;
    /// Raw `u8` tag for [`Self::PRIMARY`].
    pub const PRIMARY_RAW: u8 = 1;
    /// Raw `u8` tag for [`Self::SECONDARY`].
    pub const SECONDARY_RAW: u8 = 2;
    /// Raw `u8` tag for [`Self::MIDDLE`].
    pub const MIDDLE_RAW: u8 = 3;

    /// Converts a Bevy [`PointerButton`] into its [`FfiPointerButton`] wire value.
    #[inline]
    pub const fn from_bevy(button: PointerButton) -> Self {
        match button {
            PointerButton::Primary => Self::PRIMARY,
            PointerButton::Secondary => Self::SECONDARY,
            PointerButton::Middle => Self::MIDDLE,
        }
    }

    /// Converts this wire value back to a Bevy [`PointerButton`], returning `None` for
    /// [`Self::NONE`] or unknown discriminants.
    #[inline]
    pub const fn to_bevy(self) -> Option<PointerButton> {
        match self.0 {
            Self::PRIMARY_RAW => Some(PointerButton::Primary),
            Self::SECONDARY_RAW => Some(PointerButton::Secondary),
            Self::MIDDLE_RAW => Some(PointerButton::Middle),
            _ => None,
        }
    }
}

#[inline]
fn entity_to_ffi(entity: Entity) -> FfiEntity {
    if entity == Entity::PLACEHOLDER {
        FfiEntity::NULL
    } else {
        FfiEntity::from_bits(entity.to_bits())
    }
}

#[inline]
fn ffi_to_entity(entity: FfiEntity) -> Option<Entity> {
    if entity == FfiEntity::NULL {
        return None;
    }
    Entity::try_from_bits(entity.to_bits()).filter(|&e| e != Entity::PLACEHOLDER)
}

#[inline]
fn encode_pointer(pointer: &Pointer) -> (FfiEntity, u8, u64, u64, [f32; 2]) {
    let window = match pointer.target {
        NormalizedRenderTarget::Window(win_ref) => entity_to_ffi(win_ref.entity()),
        _ => FfiEntity::NULL,
    };
    let (kind, id_lo, id_hi) = match pointer.id {
        PointerId::Mouse => (FfiPointerKind::MOUSE_RAW, 0u64, 0u64),
        PointerId::Touch(id) => (FfiPointerKind::TOUCH_RAW, id, 0u64),
        PointerId::Custom(uuid) => {
            let b = *uuid.as_bytes();
            let lo = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            let hi = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
            (FfiPointerKind::CUSTOM_RAW, lo, hi)
        }
    };
    (
        window,
        kind,
        id_lo,
        id_hi,
        [pointer.position.x, pointer.position.y],
    )
}

#[inline]
const fn decode_uuid_bytes(lo: u64, hi: u64) -> [u8; 16] {
    let a = lo.to_le_bytes();
    let b = hi.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], b[0], b[1], b[2], b[3], b[4], b[5], b[6],
        b[7],
    ]
}

#[inline]
fn encode_hit(hit: &HitData) -> (FfiEntity, f32, [f32; 3], [f32; 3], FfiBool, FfiBool) {
    let camera = entity_to_ffi(hit.camera);
    let (pos, has_pos) = match hit.position {
        Some(p) => ([p.x, p.y, p.z], FfiBool::TRUE),
        None => ([0.0; 3], FfiBool::FALSE),
    };
    let (normal, has_normal) = match hit.normal {
        Some(n) => ([n.x, n.y, n.z], FfiBool::TRUE),
        None => ([0.0; 3], FfiBool::FALSE),
    };
    (camera, hit.depth, pos, normal, has_pos, has_normal)
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerPress`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerPress")]
pub struct FfiPointerPress {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity hit by the picking backend prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Pressed pointer button (`1 = Primary`, `2 = Secondary`, `3 = Middle`; see [`FfiPointerButton`]).
    pub button: u8,
    /// Number of consecutive presses, starting at `1`.
    pub count: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 7],
}

impl FfiPointerPress {
    /// Projects a Bevy [`PointerPress`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerPress, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            position,
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            button: FfiPointerButton::from_bevy(event.button).0,
            count: event.count,
            has_hit_position,
            has_hit_normal,
            reserved0: [0; 7],
        }
    }

    /// Returns the typed [`FfiPointerKind`].
    #[inline]
    pub const fn pointer_kind_typed(&self) -> FfiPointerKind {
        FfiPointerKind(self.pointer_kind)
    }

    /// Returns the 16-byte custom pointer UUID in little-endian wire order.
    #[inline]
    pub const fn pointer_uuid_bytes(&self) -> [u8; 16] {
        decode_uuid_bytes(self.pointer_id, self.pointer_id_hi)
    }

    /// Returns the decoded Bevy [`PointerButton`], if valid.
    #[inline]
    pub const fn button_bevy(&self) -> Option<PointerButton> {
        FfiPointerButton(self.button).to_bevy()
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }

    /// Returns the picking hit position as [`Vec3`] when present.
    #[inline]
    pub fn hit_position_vec3(&self) -> Option<Vec3> {
        self.has_hit_position
            .get()
            .then(|| Vec3::from_array(self.hit_position))
    }

    /// Returns the picking hit normal as [`Vec3`] when present.
    #[inline]
    pub fn hit_normal_vec3(&self) -> Option<Vec3> {
        self.has_hit_normal
            .get()
            .then(|| Vec3::from_array(self.hit_normal))
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerRelease`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerRelease")]
pub struct FfiPointerRelease {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity hit by the picking backend prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Released pointer button (`1 = Primary`, `2 = Secondary`, `3 = Middle`; see [`FfiPointerButton`]).
    pub button: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
}

impl FfiPointerRelease {
    /// Projects a Bevy [`PointerRelease`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerRelease, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            position,
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            button: FfiPointerButton::from_bevy(event.button).0,
            has_hit_position,
            has_hit_normal,
        }
    }

    /// Returns the decoded Bevy [`PointerButton`], if valid.
    #[inline]
    pub const fn button_bevy(&self) -> Option<PointerButton> {
        FfiPointerButton(self.button).to_bevy()
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerMove`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerMove")]
pub struct FfiPointerMove {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity hit by the picking backend prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Screen-pixel movement delta `[dx, dy]` since the previous move event.
    pub delta: [f32; 2],
    /// Picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiPointerMove {
    /// Projects a Bevy [`PointerMove`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerMove, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            position,
            delta: [event.delta.x, event.delta.y],
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            has_hit_position,
            has_hit_normal,
            reserved0: 0,
        }
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerClick`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerClick")]
pub struct FfiPointerClick {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity hit by the picking backend prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Duration between pointer press and release in seconds (`f64`).
    pub duration_secs: f64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Clicked pointer button (`1 = Primary`, `2 = Secondary`, `3 = Middle`; see [`FfiPointerButton`]).
    pub button: u8,
    /// Number of consecutive clicks, starting at `1`.
    pub count: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 7],
}

impl FfiPointerClick {
    /// Projects a Bevy [`PointerClick`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerClick, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            duration_secs: event.duration.as_secs_f64(),
            position,
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            button: FfiPointerButton::from_bevy(event.button).0,
            count: event.count,
            has_hit_position,
            has_hit_normal,
            reserved0: [0; 7],
        }
    }

    /// Returns the decoded Bevy [`PointerButton`], if valid.
    #[inline]
    pub const fn button_bevy(&self) -> Option<PointerButton> {
        FfiPointerButton(self.button).to_bevy()
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerOver`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerOver")]
pub struct FfiPointerOver {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity hit by the picking backend prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiPointerOver {
    /// Projects a Bevy [`PointerOver`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerOver, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            position,
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            has_hit_position,
            has_hit_normal,
            reserved0: 0,
        }
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerOut`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerOut")]
pub struct FfiPointerOut {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity exited by the pointer prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Camera entity that produced the prior picking hit (or [`FfiEntity::NULL`]).
    pub hit_camera: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Prior picking intersection depth reported by [`HitData::depth`].
    pub hit_depth: f32,
    /// Prior picking intersection position `[x, y, z]` when [`Self::has_hit_position`] is true.
    pub hit_position: [f32; 3],
    /// Prior picking intersection surface normal `[x, y, z]` when [`Self::has_hit_normal`] is true.
    pub hit_normal: [f32; 3],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Whether [`Self::hit_position`] contains a valid backend-reported position.
    pub has_hit_position: FfiBool,
    /// Whether [`Self::hit_normal`] contains a valid backend-reported normal.
    pub has_hit_normal: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiPointerOut {
    /// Projects a Bevy [`PointerOut`] event and its original hit entity into a wire payload.
    pub fn from_bevy(event: &PointerOut, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        let (hit_camera, hit_depth, hit_position, hit_normal, has_hit_position, has_hit_normal) =
            encode_hit(&event.hit);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            hit_camera,
            window,
            pointer_id,
            pointer_id_hi,
            position,
            hit_depth,
            hit_position,
            hit_normal,
            pointer_kind,
            has_hit_position,
            has_hit_normal,
            reserved0: 0,
        }
    }

    /// Returns the originally hit Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` wire projection of Bevy `0.20`'s flattened [`PointerDrag`] event.
#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "bevy_picking::events::PointerDrag")]
pub struct FfiPointerDrag {
    /// Target entity receiving this observer dispatch (updated along `PointerTraversal`).
    #[event_target]
    pub target: TargetEntity,
    /// Original entity being dragged prior to bubbling propagation.
    pub hit_entity: FfiEntity,
    /// Window entity associated with the pointer render target (or [`FfiEntity::NULL`]).
    pub window: FfiEntity,
    /// Pointer identifier: `0` for mouse, touch ID for touch, or lower 64 bits of custom UUID.
    pub pointer_id: u64,
    /// Upper 64 bits of custom pointer UUID (`0` for mouse/touch).
    pub pointer_id_hi: u64,
    /// Pointer logical position `[x, y]` in the render target.
    pub position: [f32; 2],
    /// Total drag vector `[dx, dy]` in screen pixels from drag start to the current position.
    pub distance: [f32; 2],
    /// Incremental drag delta `[dx, dy]` in screen pixels since the previous drag event.
    pub delta: [f32; 2],
    /// Pointer kind tag (`0 = Mouse`, `1 = Touch`, `2 = Custom`; see [`FfiPointerKind`]).
    pub pointer_kind: u8,
    /// Held pointer button (`1 = Primary`, `2 = Secondary`, `3 = Middle`; see [`FfiPointerButton`]).
    pub button: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 6],
}

impl FfiPointerDrag {
    /// Projects a Bevy [`PointerDrag`] event and its original dragged entity into a wire payload.
    pub fn from_bevy(event: &PointerDrag, original_target: Entity) -> Self {
        let (window, pointer_kind, pointer_id, pointer_id_hi, position) =
            encode_pointer(&event.pointer);
        Self {
            target: TargetEntity::from(event.entity),
            hit_entity: entity_to_ffi(original_target),
            window,
            pointer_id,
            pointer_id_hi,
            position,
            distance: [event.distance.x, event.distance.y],
            delta: [event.delta.x, event.delta.y],
            pointer_kind,
            button: FfiPointerButton::from_bevy(event.button).0,
            reserved0: [0; 6],
        }
    }

    /// Returns the decoded Bevy [`PointerButton`], if valid.
    #[inline]
    pub const fn button_bevy(&self) -> Option<PointerButton> {
        FfiPointerButton(self.button).to_bevy()
    }

    /// Returns the originally dragged Bevy [`Entity`], if valid.
    #[inline]
    pub fn hit_entity_bevy(&self) -> Option<Entity> {
        ffi_to_entity(self.hit_entity)
    }
}

/// Zero-padding `#[repr(C)]` POD component mirroring Bevy's [`EntityCursor`] (`SystemCursorIcon`).
///
/// Guests may query, insert, mutate, or remove `FfiEntityCursor` directly on any entity;
/// [`ModPickingBridgePlugin`] keeps it bidirectionally synchronized with Bevy's native
/// [`EntityCursor`] component.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_picking::cursor::EntityCursor")]
pub struct FfiEntityCursor {
    /// Deterministic discriminant matching [`SystemCursorIcon`] (`0 = Default` .. `33 = ZoomOut`).
    pub icon: u8,
}

impl FfiEntityCursor {
    /// [`SystemCursorIcon::Default`] (`0`).
    pub const DEFAULT: Self = Self { icon: 0 };
    /// [`SystemCursorIcon::ContextMenu`] (`1`).
    pub const CONTEXT_MENU: Self = Self { icon: 1 };
    /// [`SystemCursorIcon::Help`] (`2`).
    pub const HELP: Self = Self { icon: 2 };
    /// [`SystemCursorIcon::Pointer`] (`3`).
    pub const POINTER: Self = Self { icon: 3 };
    /// [`SystemCursorIcon::Progress`] (`4`).
    pub const PROGRESS: Self = Self { icon: 4 };
    /// [`SystemCursorIcon::Wait`] (`5`).
    pub const WAIT: Self = Self { icon: 5 };
    /// [`SystemCursorIcon::Cell`] (`6`).
    pub const CELL: Self = Self { icon: 6 };
    /// [`SystemCursorIcon::Crosshair`] (`7`).
    pub const CROSSHAIR: Self = Self { icon: 7 };
    /// [`SystemCursorIcon::Text`] (`8`).
    pub const TEXT: Self = Self { icon: 8 };
    /// [`SystemCursorIcon::VerticalText`] (`9`).
    pub const VERTICAL_TEXT: Self = Self { icon: 9 };
    /// [`SystemCursorIcon::Alias`] (`10`).
    pub const ALIAS: Self = Self { icon: 10 };
    /// [`SystemCursorIcon::Copy`] (`11`).
    pub const COPY: Self = Self { icon: 11 };
    /// [`SystemCursorIcon::Move`] (`12`).
    pub const MOVE: Self = Self { icon: 12 };
    /// [`SystemCursorIcon::NoDrop`] (`13`).
    pub const NO_DROP: Self = Self { icon: 13 };
    /// [`SystemCursorIcon::NotAllowed`] (`14`).
    pub const NOT_ALLOWED: Self = Self { icon: 14 };
    /// [`SystemCursorIcon::Grab`] (`15`).
    pub const GRAB: Self = Self { icon: 15 };
    /// [`SystemCursorIcon::Grabbing`] (`16`).
    pub const GRABBING: Self = Self { icon: 16 };
    /// [`SystemCursorIcon::EResize`] (`17`).
    pub const E_RESIZE: Self = Self { icon: 17 };
    /// [`SystemCursorIcon::NResize`] (`18`).
    pub const N_RESIZE: Self = Self { icon: 18 };
    /// [`SystemCursorIcon::NeResize`] (`19`).
    pub const NE_RESIZE: Self = Self { icon: 19 };
    /// [`SystemCursorIcon::NwResize`] (`20`).
    pub const NW_RESIZE: Self = Self { icon: 20 };
    /// [`SystemCursorIcon::SResize`] (`21`).
    pub const S_RESIZE: Self = Self { icon: 21 };
    /// [`SystemCursorIcon::SeResize`] (`22`).
    pub const SE_RESIZE: Self = Self { icon: 22 };
    /// [`SystemCursorIcon::SwResize`] (`23`).
    pub const SW_RESIZE: Self = Self { icon: 23 };
    /// [`SystemCursorIcon::WResize`] (`24`).
    pub const W_RESIZE: Self = Self { icon: 24 };
    /// [`SystemCursorIcon::EwResize`] (`25`).
    pub const EW_RESIZE: Self = Self { icon: 25 };
    /// [`SystemCursorIcon::NsResize`] (`26`).
    pub const NS_RESIZE: Self = Self { icon: 26 };
    /// [`SystemCursorIcon::NeswResize`] (`27`).
    pub const NESW_RESIZE: Self = Self { icon: 27 };
    /// [`SystemCursorIcon::NwseResize`] (`28`).
    pub const NWSE_RESIZE: Self = Self { icon: 28 };
    /// [`SystemCursorIcon::ColResize`] (`29`).
    pub const COL_RESIZE: Self = Self { icon: 29 };
    /// [`SystemCursorIcon::RowResize`] (`30`).
    pub const ROW_RESIZE: Self = Self { icon: 30 };
    /// [`SystemCursorIcon::AllScroll`] (`31`).
    pub const ALL_SCROLL: Self = Self { icon: 31 };
    /// [`SystemCursorIcon::ZoomIn`] (`32`).
    pub const ZOOM_IN: Self = Self { icon: 32 };
    /// [`SystemCursorIcon::ZoomOut`] (`33`).
    pub const ZOOM_OUT: Self = Self { icon: 33 };

    /// Converts a [`SystemCursorIcon`] into its deterministic [`FfiEntityCursor`].
    #[inline]
    pub const fn from_system_cursor(icon: SystemCursorIcon) -> Self {
        match icon {
            SystemCursorIcon::Default => Self::DEFAULT,
            SystemCursorIcon::ContextMenu => Self::CONTEXT_MENU,
            SystemCursorIcon::Help => Self::HELP,
            SystemCursorIcon::Pointer => Self::POINTER,
            SystemCursorIcon::Progress => Self::PROGRESS,
            SystemCursorIcon::Wait => Self::WAIT,
            SystemCursorIcon::Cell => Self::CELL,
            SystemCursorIcon::Crosshair => Self::CROSSHAIR,
            SystemCursorIcon::Text => Self::TEXT,
            SystemCursorIcon::VerticalText => Self::VERTICAL_TEXT,
            SystemCursorIcon::Alias => Self::ALIAS,
            SystemCursorIcon::Copy => Self::COPY,
            SystemCursorIcon::Move => Self::MOVE,
            SystemCursorIcon::NoDrop => Self::NO_DROP,
            SystemCursorIcon::NotAllowed => Self::NOT_ALLOWED,
            SystemCursorIcon::Grab => Self::GRAB,
            SystemCursorIcon::Grabbing => Self::GRABBING,
            SystemCursorIcon::EResize => Self::E_RESIZE,
            SystemCursorIcon::NResize => Self::N_RESIZE,
            SystemCursorIcon::NeResize => Self::NE_RESIZE,
            SystemCursorIcon::NwResize => Self::NW_RESIZE,
            SystemCursorIcon::SResize => Self::S_RESIZE,
            SystemCursorIcon::SeResize => Self::SE_RESIZE,
            SystemCursorIcon::SwResize => Self::SW_RESIZE,
            SystemCursorIcon::WResize => Self::W_RESIZE,
            SystemCursorIcon::EwResize => Self::EW_RESIZE,
            SystemCursorIcon::NsResize => Self::NS_RESIZE,
            SystemCursorIcon::NeswResize => Self::NESW_RESIZE,
            SystemCursorIcon::NwseResize => Self::NWSE_RESIZE,
            SystemCursorIcon::ColResize => Self::COL_RESIZE,
            SystemCursorIcon::RowResize => Self::ROW_RESIZE,
            SystemCursorIcon::AllScroll => Self::ALL_SCROLL,
            SystemCursorIcon::ZoomIn => Self::ZOOM_IN,
            SystemCursorIcon::ZoomOut => Self::ZOOM_OUT,
        }
    }

    /// Converts this wire discriminant back into a [`SystemCursorIcon`], returning `None` if out
    /// of range.
    #[inline]
    pub const fn to_system_cursor(self) -> Option<SystemCursorIcon> {
        match self.icon {
            0 => Some(SystemCursorIcon::Default),
            1 => Some(SystemCursorIcon::ContextMenu),
            2 => Some(SystemCursorIcon::Help),
            3 => Some(SystemCursorIcon::Pointer),
            4 => Some(SystemCursorIcon::Progress),
            5 => Some(SystemCursorIcon::Wait),
            6 => Some(SystemCursorIcon::Cell),
            7 => Some(SystemCursorIcon::Crosshair),
            8 => Some(SystemCursorIcon::Text),
            9 => Some(SystemCursorIcon::VerticalText),
            10 => Some(SystemCursorIcon::Alias),
            11 => Some(SystemCursorIcon::Copy),
            12 => Some(SystemCursorIcon::Move),
            13 => Some(SystemCursorIcon::NoDrop),
            14 => Some(SystemCursorIcon::NotAllowed),
            15 => Some(SystemCursorIcon::Grab),
            16 => Some(SystemCursorIcon::Grabbing),
            17 => Some(SystemCursorIcon::EResize),
            18 => Some(SystemCursorIcon::NResize),
            19 => Some(SystemCursorIcon::NeResize),
            20 => Some(SystemCursorIcon::NwResize),
            21 => Some(SystemCursorIcon::SResize),
            22 => Some(SystemCursorIcon::SeResize),
            23 => Some(SystemCursorIcon::SwResize),
            24 => Some(SystemCursorIcon::WResize),
            25 => Some(SystemCursorIcon::EwResize),
            26 => Some(SystemCursorIcon::NsResize),
            27 => Some(SystemCursorIcon::NeswResize),
            28 => Some(SystemCursorIcon::NwseResize),
            29 => Some(SystemCursorIcon::ColResize),
            30 => Some(SystemCursorIcon::RowResize),
            31 => Some(SystemCursorIcon::AllScroll),
            32 => Some(SystemCursorIcon::ZoomIn),
            33 => Some(SystemCursorIcon::ZoomOut),
            _ => None,
        }
    }

    /// Projects a Bevy [`EntityCursor`] into [`FfiEntityCursor`], returning `None` for custom
    /// image cursors when `bevy_picking/custom_cursor` is enabled.
    #[inline]
    pub fn from_bevy(cursor: &EntityCursor) -> Option<Self> {
        cursor
            .to_cursor_icon()
            .as_system()
            .copied()
            .map(Self::from_system_cursor)
    }

    /// Converts this wire cursor into Bevy's [`EntityCursor::System`], returning `None` if
    /// [`Self::icon`] is not a valid [`SystemCursorIcon`] discriminant.
    #[inline]
    pub const fn to_bevy(self) -> Option<EntityCursor> {
        match self.to_system_cursor() {
            Some(icon) => Some(EntityCursor::System(icon)),
            None => None,
        }
    }
}

impl From<SystemCursorIcon> for FfiEntityCursor {
    #[inline]
    fn from(icon: SystemCursorIcon) -> Self {
        Self::from_system_cursor(icon)
    }
}

const _: () = {
    assert_abi_pod::<FfiPointerKind>();
    assert_abi_pod::<FfiPointerButton>();
    assert_abi_pod::<FfiPointerPress>();
    assert_abi_pod::<FfiPointerRelease>();
    assert_abi_pod::<FfiPointerMove>();
    assert_abi_pod::<FfiPointerClick>();
    assert_abi_pod::<FfiPointerOver>();
    assert_abi_pod::<FfiPointerOut>();
    assert_abi_pod::<FfiPointerDrag>();
    assert_abi_pod::<FfiEntityCursor>();

    assert!(<FfiPointerPress as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerRelease as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerMove as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerClick as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerOver as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerOut as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiPointerDrag as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);
    assert!(<FfiEntityCursor as bevy_abi_mod_sys::ModComponent>::IS_ABI_POD);

    assert!(core::mem::size_of::<FfiPointerPress>() == 96);
    assert!(core::mem::align_of::<FfiPointerPress>() == 8);
    assert!(core::mem::size_of::<FfiPointerRelease>() == 88);
    assert!(core::mem::align_of::<FfiPointerRelease>() == 8);
    assert!(core::mem::size_of::<FfiPointerMove>() == 96);
    assert!(core::mem::align_of::<FfiPointerMove>() == 8);
    assert!(core::mem::size_of::<FfiPointerClick>() == 104);
    assert!(core::mem::align_of::<FfiPointerClick>() == 8);
    assert!(core::mem::size_of::<FfiPointerOver>() == 88);
    assert!(core::mem::align_of::<FfiPointerOver>() == 8);
    assert!(core::mem::size_of::<FfiPointerOut>() == 88);
    assert!(core::mem::align_of::<FfiPointerOut>() == 8);
    assert!(core::mem::size_of::<FfiPointerDrag>() == 72);
    assert!(core::mem::align_of::<FfiPointerDrag>() == 8);
    assert!(core::mem::size_of::<FfiEntityCursor>() == 1);
    assert!(core::mem::align_of::<FfiEntityCursor>() == 1);
};

/// System sets for [`ModPickingBridgePlugin`] synchronization.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PickingBridgeSet {
    /// Copies host [`EntityCursor`] updates into [`FfiEntityCursor`] during [`PreUpdate`].
    SyncFromHost,
    /// Applies guest [`FfiEntityCursor`] mutations and removals to [`EntityCursor`] during
    /// [`PostUpdate`].
    SyncToHost,
}

fn forward_pointer_press(on: On<PointerPress>, mut commands: Commands) {
    commands.trigger(FfiPointerPress::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_release(on: On<PointerRelease>, mut commands: Commands) {
    commands.trigger(FfiPointerRelease::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_move(on: On<PointerMove>, mut commands: Commands) {
    commands.trigger(FfiPointerMove::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_click(on: On<PointerClick>, mut commands: Commands) {
    commands.trigger(FfiPointerClick::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_over(on: On<PointerOver>, mut commands: Commands) {
    commands.trigger(FfiPointerOver::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_out(on: On<PointerOut>, mut commands: Commands) {
    commands.trigger(FfiPointerOut::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

fn forward_pointer_drag(on: On<PointerDrag>, mut commands: Commands) {
    commands.trigger(FfiPointerDrag::from_bevy(
        on.event(),
        on.original_event_target(),
    ));
}

#[allow(clippy::type_complexity)]
fn sync_host_entity_cursor_to_ffi(
    mut commands: Commands,
    mut host_cursors: Query<
        (Entity, &EntityCursor, Option<&mut FfiEntityCursor>),
        Or<(Changed<EntityCursor>, Without<FfiEntityCursor>)>,
    >,
    mut removed_host: RemovedComponents<EntityCursor>,
    ffi_only: Query<(), (With<FfiEntityCursor>, Without<EntityCursor>)>,
) {
    for (entity, cursor, ffi_opt) in &mut host_cursors {
        if let Some(desired) = FfiEntityCursor::from_bevy(cursor) {
            match ffi_opt {
                Some(mut existing) => {
                    if *existing != desired {
                        *existing = desired;
                    }
                }
                None => {
                    commands.entity(entity).insert(desired);
                }
            }
        }
    }

    for entity in removed_host.read() {
        if ffi_only.contains(entity) {
            commands.entity(entity).remove::<FfiEntityCursor>();
        }
    }
}

#[allow(clippy::type_complexity)]
fn sync_ffi_entity_cursor_to_host(
    mut commands: Commands,
    mut ffi_cursors: Query<
        (Entity, &FfiEntityCursor, Option<&mut EntityCursor>),
        Or<(Changed<FfiEntityCursor>, Without<EntityCursor>)>,
    >,
    mut removed_ffi: RemovedComponents<FfiEntityCursor>,
    host_only: Query<(), (With<EntityCursor>, Without<FfiEntityCursor>)>,
) {
    for (entity, ffi, host_opt) in &mut ffi_cursors {
        if let Some(desired) = ffi.to_bevy() {
            match host_opt {
                Some(mut existing) => {
                    if *existing != desired {
                        *existing = desired;
                    }
                }
                None => {
                    commands.entity(entity).insert(desired);
                }
            }
        }
    }

    for entity in removed_ffi.read() {
        if host_only.contains(entity) {
            commands.entity(entity).remove::<EntityCursor>();
        }
    }
}

/// Registers all flattened picking entity events (`FfiPointerPress`, `FfiPointerRelease`,
/// `FfiPointerMove`, `FfiPointerClick`, `FfiPointerOver`, `FfiPointerOut`, `FfiPointerDrag`) and
/// [`FfiEntityCursor`] inside `world`'s [`ModRegistry`].
pub fn register_picking_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    // Ensure `PointerTraversal`'s optional query terms (`ChildOf` and `Window`) are registered
    // in `world` so Bevy `0.20` pointer bubbling works even in headless worlds without a `Window`.
    world.register_component::<bevy_ecs::hierarchy::ChildOf>();
    world.register_component::<Window>();
    world.register_mod_entity_event::<FfiPointerPress>()?;
    world.register_mod_entity_event::<FfiPointerRelease>()?;
    world.register_mod_entity_event::<FfiPointerMove>()?;
    world.register_mod_entity_event::<FfiPointerClick>()?;
    world.register_mod_entity_event::<FfiPointerOver>()?;
    world.register_mod_entity_event::<FfiPointerOut>()?;
    world.register_mod_entity_event::<FfiPointerDrag>()?;
    world.register_mod_component::<FfiEntityCursor>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    Ok(())
}

/// Bevy [`Plugin`] bridging Bevy `0.20` flattened `bevy_picking` observer events and
/// [`EntityCursor`] into `bevy_abi_mod` (`#![forbid(unsafe_code)]`).
///
/// Registers:
/// - Entity events: [`FfiPointerPress`], [`FfiPointerRelease`], [`FfiPointerMove`],
///   [`FfiPointerClick`], [`FfiPointerOver`], [`FfiPointerOut`], [`FfiPointerDrag`].
/// - Component: [`FfiEntityCursor`] bidirectionally synced with [`EntityCursor`].
/// - Bevy observers forwarding every flattened `bevy_picking::events::Pointer*` entity event to
///   matching guest observers registered via `register_observer` or `cmd_observe`.
#[derive(Default, Debug, Clone, Copy)]
pub struct ModPickingBridgePlugin;

impl Plugin for ModPickingBridgePlugin {
    fn build(&self, app: &mut App) {
        if let Err(err) = register_picking_bridges(app.world_mut()) {
            log_ecs_error(
                err.into(),
                ErrorContext::Command {
                    name: "ModPickingBridgePlugin::build".into(),
                },
            );
        }

        app.add_observer(forward_pointer_press)
            .add_observer(forward_pointer_release)
            .add_observer(forward_pointer_move)
            .add_observer(forward_pointer_click)
            .add_observer(forward_pointer_over)
            .add_observer(forward_pointer_out)
            .add_observer(forward_pointer_drag)
            .add_systems(
                PreUpdate,
                sync_host_entity_cursor_to_ffi.in_set(PickingBridgeSet::SyncFromHost),
            )
            .add_systems(
                PostUpdate,
                sync_ffi_entity_cursor_to_host.in_set(PickingBridgeSet::SyncToHost),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_match_bevy_type_names() {
        assert_eq!(
            CANONICAL_POINTER_PRESS_NAME,
            core::any::type_name::<PointerPress>()
        );
        assert_eq!(
            CANONICAL_POINTER_RELEASE_NAME,
            core::any::type_name::<PointerRelease>()
        );
        assert_eq!(
            CANONICAL_POINTER_MOVE_NAME,
            core::any::type_name::<PointerMove>()
        );
        assert_eq!(
            CANONICAL_POINTER_CLICK_NAME,
            core::any::type_name::<PointerClick>()
        );
        assert_eq!(
            CANONICAL_POINTER_OVER_NAME,
            core::any::type_name::<PointerOver>()
        );
        assert_eq!(
            CANONICAL_POINTER_OUT_NAME,
            core::any::type_name::<PointerOut>()
        );
        assert_eq!(
            CANONICAL_POINTER_DRAG_NAME,
            core::any::type_name::<PointerDrag>()
        );
        assert_eq!(
            CANONICAL_ENTITY_CURSOR_NAME,
            core::any::type_name::<EntityCursor>()
        );
    }

    #[test]
    fn entity_cursor_round_trips_all_system_cursor_variants() {
        let all_icons = [
            SystemCursorIcon::Default,
            SystemCursorIcon::ContextMenu,
            SystemCursorIcon::Help,
            SystemCursorIcon::Pointer,
            SystemCursorIcon::Progress,
            SystemCursorIcon::Wait,
            SystemCursorIcon::Cell,
            SystemCursorIcon::Crosshair,
            SystemCursorIcon::Text,
            SystemCursorIcon::VerticalText,
            SystemCursorIcon::Alias,
            SystemCursorIcon::Copy,
            SystemCursorIcon::Move,
            SystemCursorIcon::NoDrop,
            SystemCursorIcon::NotAllowed,
            SystemCursorIcon::Grab,
            SystemCursorIcon::Grabbing,
            SystemCursorIcon::EResize,
            SystemCursorIcon::NResize,
            SystemCursorIcon::NeResize,
            SystemCursorIcon::NwResize,
            SystemCursorIcon::SResize,
            SystemCursorIcon::SeResize,
            SystemCursorIcon::SwResize,
            SystemCursorIcon::WResize,
            SystemCursorIcon::EwResize,
            SystemCursorIcon::NsResize,
            SystemCursorIcon::NeswResize,
            SystemCursorIcon::NwseResize,
            SystemCursorIcon::ColResize,
            SystemCursorIcon::RowResize,
            SystemCursorIcon::AllScroll,
            SystemCursorIcon::ZoomIn,
            SystemCursorIcon::ZoomOut,
        ];
        for (expected_idx, icon) in all_icons.into_iter().enumerate() {
            let ffi = FfiEntityCursor::from_system_cursor(icon);
            assert_eq!(ffi.icon as usize, expected_idx);
            assert_eq!(ffi.to_system_cursor(), Some(icon));
            let bevy_cursor = EntityCursor::System(icon);
            assert_eq!(FfiEntityCursor::from_bevy(&bevy_cursor), Some(ffi));
            assert_eq!(ffi.to_bevy(), Some(bevy_cursor));
        }
        assert_eq!(FfiEntityCursor { icon: 255 }.to_system_cursor(), None);
        assert_eq!(FfiEntityCursor { icon: 255 }.to_bevy(), None);
    }
}
