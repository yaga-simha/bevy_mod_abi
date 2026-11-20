//! Safe (`#![forbid(unsafe_code)]`) Bevy `0.20` retained UI, `Val::Em`/`Val::Rem`,
//! `InlineBox`/`InlineImage`, headless widget (`bevy_ui_widgets`), and optional Feathers widget
//! (`bevy_feathers`) host bridge (`ModUiBridgePlugin`).
//!
//! Exposes zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` descriptors synchronized cleanly
//! to Bevy `0.20` UI, text, widget, and theme components:
//!
//! - Layout & geometry: [`FfiVal`] (`Auto`, `Px`, `Percent`, `Vw`, `Vh`, `VMin`, `VMax`, `Em`,
//!   `Rem`), [`FfiUiRect`], [`FfiCornerRadius`], [`FfiBorderRadius`], [`FfiUiNode`] (including
//!   [`FixedNode`] viewport-root pinning), [`FfiRgba`], and [`FfiBorderColor`].
//! - Text & inline content: [`FfiFontSize`] (`Px`, `Vw`, `Vh`, `VMin`, `VMax`, `Rem`),
//!   [`FfiUiTextStyle`], [`FfiEditableTextConfig`] (bridging Bevy `0.20`'s split
//!   [`EditableText`] + [`TextInput`]), [`FfiInlineBox`], and [`FfiInlineImage`] (backed by
//!   [`FfiAssetHandle`]).
//! - Headless widgets & observers: [`FfiTabList`], [`FfiTab`], [`FfiSelectedTab`], and the
//!   [`FfiValueChange`] entity-event observer bridge translating `bevy_ui_widgets::ValueChange<T>`
//!   (`Entity`, `Option<Entity>`, `f32`, `bool`, plus `f64`/`i32`/`i64`/`Color` under
//!   `bridge-feathers`) into `AbiPod` guest observer payloads.
//! - Optional `bevy_feathers` controls & contextual theming (`feature = "bridge-feathers"`):
//!   `FfiThemeContext` (`Base`, `Higher`, `Highest`, `Floating`), `FfiFeathersColorInput`,
//!   `FfiFeathersDropdown`, `FfiFeathersScrollableList`, `FfiFeathersListRow`, and
//!   scrubbable `FfiFeathersNumberInput`.
//! - Deterministic retained-UI teardown on `unload_mod` via [`ModScoped`], [`ModUiRegistry`],
//!   and [`bevy_abi_mod_host::ModUnloadHooks`].

use core::marker::PhantomData;
use core::time::Duration;

use bevy_abi_mod_host::{ModWorldExt, register_mod_unload_hook_in_world};
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, HostStringBridge, HostWireBridge, ModId,
    ModRegistry, ModScoped,
};
use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{FfiAssetHandle, FfiBool, FfiEntity, FfiResult};
use bevy_app::{App, Plugin, PostUpdate};
use bevy_asset::uuid::Uuid;
use bevy_asset::{Asset, AssetId, AssetIndex, AssetServer, Assets, Handle};
#[cfg(feature = "bridge-feathers")]
use bevy_asset::{AssetApp, AssetEvent, AssetLoadFailedEvent};
#[cfg(feature = "bridge-feathers")]
use bevy_color::Alpha;
use bevy_color::Color;
use bevy_ecs::change_detection::DetectChangesMut;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::error::{ErrorContext, error as log_ecs_error};
use bevy_ecs::event::{EntityEvent, EntityTrigger, Event};
use bevy_ecs::hierarchy::Children;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::message::Messages;
use bevy_ecs::observer::On;
use bevy_ecs::prelude::{
    Added, Changed, Commands, Component, Has, Or, Query, Res, ResMut, Resource,
};
use bevy_ecs::query::{Allow, With};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::world::World;
use bevy_image::{Image, TRANSPARENT_IMAGE_HANDLE};
use bevy_input_focus::{InputFocus, InputFocusSystems};
use bevy_math::Vec2;
use bevy_platform::collections::{HashMap, HashSet};
use bevy_text::{
    EditableText, EmSize, Font, FontSize, FontSmoothing, FontSource, FontStyle, FontWeight,
    InlineBox, InlineBoxKind, Justify, LineBreak, RemSize, TextColor, TextFont, TextLayout,
    TextReadWriteMode, TextSpan,
};
use bevy_ui::widget::{InlineImage, Text};
use bevy_ui::{
    AlignItems, BackgroundColor, BorderColor, BorderRadius, CornerRadius, Display, FixedNode,
    FlexDirection, FlexWrap, InteractionDisabled, JustifyContent, Node, PositionType, UiRect,
    UiSystems, Val,
};
#[cfg(feature = "bridge-feathers")]
use bevy_ui::{Overflow, Selected, ZIndex};
use bevy_ui_widgets::{
    Activate, ControlOrientation, MenuFocusSystem, SelectedTab, Tab, TabActivation, TabList,
    TabPlugin, TextInput, ValueChange,
};

#[cfg(feature = "bridge-feathers")]
use bevy_ecs::bundle::BundleScratch;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::change_detection::DetectChanges;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::hierarchy::ChildOf;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::name::Name;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::query::Without;
#[cfg(feature = "bridge-feathers")]
use bevy_ecs::world::Ref;
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::controls::{
    AngleDegrees, ColorInputMode, ColorInputSettings, ColorInputValue, ColorSwatchFg,
    Dimensionless, FeathersColorInput, FeathersColorInputProps, FeathersColorSwatch,
    FeathersListRow, FeathersListView, FeathersListViewProps, FeathersMenuButton,
    FeathersNumberInput, FeathersNumberInputProps, FeathersScrollbar, FeathersSelect,
    FeathersSelectProps, HardLimit, LengthMeters, NumberInputPrecision, NumberInputRange,
    NumberInputStep, NumberInputValue, NumberInputWrap, OptionIndex, ScrollbarGutter, SoftLimit,
    TimeSeconds, UnitsRegistry,
};
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::rounded_corners::RoundedCorners;
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::theme::{SurfaceLevel, ThemeContext, UiTheme};
#[cfg(feature = "bridge-feathers")]
use bevy_scene::{ResolvedSceneRoot, Scene, ScenePatch, bsn, bsn_list};
#[cfg(feature = "bridge-feathers")]
use bevy_ui_widgets::{ListBox, ScrollArea, Scrollbar, SetSelected};

/// Canonical ABI name for bridged `bevy_ui::widget::Text`.
pub const CANONICAL_UI_TEXT_NAME: &str = "bevy_ui::widget::text::Text";
/// Canonical ABI name for bridged `bevy_text::TextSpan`.
pub const CANONICAL_TEXT_SPAN_NAME: &str = "bevy_text::text::TextSpan";
/// Canonical ABI name for bridged `bevy_ui::BackgroundColor`.
pub const CANONICAL_BACKGROUND_COLOR_NAME: &str = "bevy_ui::ui_node::BackgroundColor";
/// Canonical ABI name for bridged `bevy_ui::BorderColor`.
pub const CANONICAL_BORDER_COLOR_NAME: &str = "bevy_ui::ui_node::BorderColor";
/// Canonical ABI name for bridged `bevy_text::RemSize` resource.
pub const CANONICAL_REM_SIZE_NAME: &str = "bevy_text::text::RemSize";
/// Canonical ABI name for bridged `bevy_text::EmSize` component.
pub const CANONICAL_EM_SIZE_NAME: &str = "bevy_text::text::EmSize";
/// Canonical ABI name for bridged `bevy_ui_widgets::Activate` entity event.
pub const CANONICAL_ACTIVATE_EVENT_NAME: &str = "bevy_ui_widgets::Activate";
/// Canonical ABI name for [`FfiValueChange`] entity event.
pub const CANONICAL_VALUE_CHANGE_EVENT_NAME: &str = "bevy_abi_mod_ui::FfiValueChange";
/// Canonical ABI name for bridged `bevy_feathers::theme::ThemeContext`.
#[cfg(feature = "bridge-feathers")]
pub const CANONICAL_THEME_CONTEXT_NAME: &str = "bevy_feathers::theme::ThemeContext";

const UI_UNLOAD_HOOK_KEY: &str = "bevy_abi_mod_bridges::ui";

/// Schedule [`SystemSet`]s ordering retained-UI descriptor sync, host-state mirroring, and
/// `ModScoped` UI root tracking inside [`PostUpdate`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModUiBridgeSet {
    /// Synchronizes guest `Ffi*` descriptors into native Bevy `0.20` UI, text, widget, and
    /// Feathers components prior to [`UiSystems::Prepare`] and `TabPlugin` derived-state updates.
    Sync,
    /// Mirrors host-updated widget state (e.g. [`SelectedTab`], and Feathers `ColorInputValue`,
    /// `NumberInputValue`, and `ThemeContext` when `bridge-feathers` is enabled) back into
    /// companion `Ffi*` descriptors.
    Mirror,
    /// Tracks newly spawned [`ModScoped`] UI entities in [`ModUiRegistry`] for unload teardown.
    Track,
}

// ============================================================================
// Core Geometry & Color Wire Primitives
// ============================================================================

/// Fixed-layout `#[repr(C)]` `AbiPod` wire representation of Bevy `0.20` [`Val`], including
/// `Val::Em` and `Val::Rem`.
///
/// Layout: `8` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiVal")]
pub struct FfiVal {
    /// Unit discriminant (`0 = Auto`, `1 = Px`, `2 = Percent`, `3 = Vw`, `4 = Vh`,
    /// `5 = VMin`, `6 = VMax`, `7 = Em`, `8 = Rem`).
    pub unit: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
    /// Scalar magnitude (ignored when `unit == Self::UNIT_AUTO`).
    pub value: f32,
}

impl FfiVal {
    /// Discriminant for [`Val::Auto`] (`0`).
    pub const UNIT_AUTO: u8 = 0;
    /// Discriminant for [`Val::Px`] (`1`).
    pub const UNIT_PX: u8 = 1;
    /// Discriminant for [`Val::Percent`] (`2`).
    pub const UNIT_PERCENT: u8 = 2;
    /// Discriminant for [`Val::Vw`] (`3`).
    pub const UNIT_VW: u8 = 3;
    /// Discriminant for [`Val::Vh`] (`4`).
    pub const UNIT_VH: u8 = 4;
    /// Discriminant for [`Val::VMin`] (`5`).
    pub const UNIT_VMIN: u8 = 5;
    /// Discriminant for [`Val::VMax`] (`6`).
    pub const UNIT_VMAX: u8 = 6;
    /// Discriminant for [`Val::Em`] (`7`).
    pub const UNIT_EM: u8 = 7;
    /// Discriminant for [`Val::Rem`] (`8`).
    pub const UNIT_REM: u8 = 8;

    /// Canonical [`Val::Auto`] value.
    pub const AUTO: Self = Self {
        unit: Self::UNIT_AUTO,
        reserved0: [0; 3],
        value: 0.0,
    };

    /// Canonical zero value (`Val::Px(0.0)`).
    pub const ZERO: Self = Self::px(0.0);

    /// Constructs a [`Val::Px`] wire value.
    #[inline]
    pub const fn px(value: f32) -> Self {
        Self {
            unit: Self::UNIT_PX,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::Percent`] wire value.
    #[inline]
    pub const fn percent(value: f32) -> Self {
        Self {
            unit: Self::UNIT_PERCENT,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::Vw`] wire value.
    #[inline]
    pub const fn vw(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VW,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::Vh`] wire value.
    #[inline]
    pub const fn vh(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VH,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::VMin`] wire value.
    #[inline]
    pub const fn vmin(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VMIN,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::VMax`] wire value.
    #[inline]
    pub const fn vmax(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VMAX,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::Em`] wire value (relative to the node's own `EmSize`).
    #[inline]
    pub const fn em(value: f32) -> Self {
        Self {
            unit: Self::UNIT_EM,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`Val::Rem`] wire value (relative to the root `RemSize` resource).
    #[inline]
    pub const fn rem(value: f32) -> Self {
        Self {
            unit: Self::UNIT_REM,
            reserved0: [0; 3],
            value,
        }
    }

    /// Projects a Bevy [`Val`] into [`FfiVal`].
    #[inline]
    pub const fn from_bevy(val: Val) -> Self {
        match val {
            Val::Auto => Self::AUTO,
            Val::Px(v) => Self::px(v),
            Val::Percent(v) => Self::percent(v),
            Val::Vw(v) => Self::vw(v),
            Val::Vh(v) => Self::vh(v),
            Val::VMin(v) => Self::vmin(v),
            Val::VMax(v) => Self::vmax(v),
            Val::Em(v) => Self::em(v),
            Val::Rem(v) => Self::rem(v),
        }
    }

    /// Validates and converts this wire value into a Bevy [`Val`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<Val, FfiResult> {
        if self.reserved0 != [0; 3] {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if self.unit != Self::UNIT_AUTO && !self.value.is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        match self.unit {
            Self::UNIT_AUTO => Ok(Val::Auto),
            Self::UNIT_PX => Ok(Val::Px(self.value)),
            Self::UNIT_PERCENT => Ok(Val::Percent(self.value)),
            Self::UNIT_VW => Ok(Val::Vw(self.value)),
            Self::UNIT_VH => Ok(Val::Vh(self.value)),
            Self::UNIT_VMIN => Ok(Val::VMin(self.value)),
            Self::UNIT_VMAX => Ok(Val::VMax(self.value)),
            Self::UNIT_EM => Ok(Val::Em(self.value)),
            Self::UNIT_REM => Ok(Val::Rem(self.value)),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

impl Default for FfiVal {
    #[inline]
    fn default() -> Self {
        Self::AUTO
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` wire representation of Bevy's [`UiRect`].
///
/// Layout: `32` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiUiRect")]
pub struct FfiUiRect {
    /// Left edge inset.
    pub left: FfiVal,
    /// Right edge inset.
    pub right: FfiVal,
    /// Top edge inset.
    pub top: FfiVal,
    /// Bottom edge inset.
    pub bottom: FfiVal,
}

impl FfiUiRect {
    /// Zero inset on all four edges (`Val::Px(0.0)`), matching [`UiRect::DEFAULT`].
    pub const ZERO: Self = Self::all(FfiVal::ZERO);

    /// Constructs a [`FfiUiRect`] with identical insets on all four edges.
    #[inline]
    pub const fn all(value: FfiVal) -> Self {
        Self {
            left: value,
            right: value,
            top: value,
            bottom: value,
        }
    }

    /// Constructs a [`FfiUiRect`] with explicit horizontal and vertical axis insets.
    #[inline]
    pub const fn axes(horizontal: FfiVal, vertical: FfiVal) -> Self {
        Self {
            left: horizontal,
            right: horizontal,
            top: vertical,
            bottom: vertical,
        }
    }

    /// Constructs a [`FfiUiRect`] from explicit edge values.
    #[inline]
    pub const fn new(left: FfiVal, right: FfiVal, top: FfiVal, bottom: FfiVal) -> Self {
        Self {
            left,
            right,
            top,
            bottom,
        }
    }

    /// Projects a Bevy [`UiRect`] into [`FfiUiRect`].
    #[inline]
    pub const fn from_bevy(rect: UiRect) -> Self {
        Self {
            left: FfiVal::from_bevy(rect.left),
            right: FfiVal::from_bevy(rect.right),
            top: FfiVal::from_bevy(rect.top),
            bottom: FfiVal::from_bevy(rect.bottom),
        }
    }

    /// Validates and converts this wire rect into a Bevy [`UiRect`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<UiRect, FfiResult> {
        Ok(UiRect {
            left: self.left.try_to_bevy()?,
            right: self.right.try_to_bevy()?,
            top: self.top.try_to_bevy()?,
            bottom: self.bottom.try_to_bevy()?,
        })
    }
}

impl Default for FfiUiRect {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` wire representation of Bevy `0.20`'s elliptical
/// [`CornerRadius`].
///
/// Layout: `16` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiCornerRadius")]
pub struct FfiCornerRadius {
    /// Horizontal corner radius (`CornerRadius::x`).
    pub x: FfiVal,
    /// Vertical corner radius (`CornerRadius::y`; [`FfiVal::AUTO`] produces a circular corner).
    pub y: FfiVal,
}

impl FfiCornerRadius {
    /// Sharp right-angled corner (`x = 0px, y = Auto`).
    pub const ZERO: Self = Self {
        x: FfiVal::ZERO,
        y: FfiVal::AUTO,
    };

    /// Constructs a circular corner radius matching [`CornerRadius::circular`].
    #[inline]
    pub const fn circular(radius: FfiVal) -> Self {
        Self {
            x: radius,
            y: FfiVal::AUTO,
        }
    }

    /// Constructs an elliptical corner radius with distinct horizontal (`x`) and vertical (`y`)
    /// radii.
    #[inline]
    pub const fn elliptical(x: FfiVal, y: FfiVal) -> Self {
        Self { x, y }
    }

    /// Projects a Bevy [`CornerRadius`] into [`FfiCornerRadius`].
    #[inline]
    pub const fn from_bevy(corner: CornerRadius) -> Self {
        Self {
            x: FfiVal::from_bevy(corner.x),
            y: FfiVal::from_bevy(corner.y),
        }
    }

    /// Validates and converts this wire corner radius into a Bevy [`CornerRadius`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<CornerRadius, FfiResult> {
        Ok(CornerRadius {
            x: self.x.try_to_bevy()?,
            y: self.y.try_to_bevy()?,
        })
    }
}

impl Default for FfiCornerRadius {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor for Bevy `0.20`'s elliptical
/// [`BorderRadius`].
///
/// Can be embedded inside [`FfiUiNode`] or attached standalone to any UI entity to synchronize
/// directly into [`Node::border_radius`].
///
/// Layout: `64` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiBorderRadius")]
pub struct FfiBorderRadius {
    /// Top-left elliptical corner radius.
    pub top_left: FfiCornerRadius,
    /// Top-right elliptical corner radius.
    pub top_right: FfiCornerRadius,
    /// Bottom-right elliptical corner radius.
    pub bottom_right: FfiCornerRadius,
    /// Bottom-left elliptical corner radius.
    pub bottom_left: FfiCornerRadius,
}

impl FfiBorderRadius {
    /// Zero curvature on all four corners.
    pub const ZERO: Self = Self::all(FfiCornerRadius::ZERO);

    /// Sets all four corners to the same [`FfiCornerRadius`].
    #[inline]
    pub const fn all(corner: FfiCornerRadius) -> Self {
        Self {
            top_left: corner,
            top_right: corner,
            bottom_right: corner,
            bottom_left: corner,
        }
    }

    /// Sets all four corners to the same elliptical `(x, y)` radius.
    #[inline]
    pub const fn elliptical_all(x: FfiVal, y: FfiVal) -> Self {
        Self::all(FfiCornerRadius::elliptical(x, y))
    }

    /// Sets each corner to a circular logical-pixel radius.
    #[inline]
    pub const fn px(top_left: f32, top_right: f32, bottom_right: f32, bottom_left: f32) -> Self {
        Self {
            top_left: FfiCornerRadius::circular(FfiVal::px(top_left)),
            top_right: FfiCornerRadius::circular(FfiVal::px(top_right)),
            bottom_right: FfiCornerRadius::circular(FfiVal::px(bottom_right)),
            bottom_left: FfiCornerRadius::circular(FfiVal::px(bottom_left)),
        }
    }

    /// Projects a Bevy [`BorderRadius`] into [`FfiBorderRadius`].
    #[inline]
    pub const fn from_bevy(radius: BorderRadius) -> Self {
        Self {
            top_left: FfiCornerRadius::from_bevy(radius.top_left),
            top_right: FfiCornerRadius::from_bevy(radius.top_right),
            bottom_right: FfiCornerRadius::from_bevy(radius.bottom_right),
            bottom_left: FfiCornerRadius::from_bevy(radius.bottom_left),
        }
    }

    /// Validates and converts this wire border radius into a Bevy [`BorderRadius`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<BorderRadius, FfiResult> {
        Ok(BorderRadius {
            top_left: self.top_left.try_to_bevy()?,
            top_right: self.top_right.try_to_bevy()?,
            bottom_right: self.bottom_right.try_to_bevy()?,
            bottom_left: self.bottom_left.try_to_bevy()?,
        })
    }
}

impl Default for FfiBorderRadius {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` sRGBA color wire struct.
///
/// Layout: `16` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiRgba")]
pub struct FfiRgba {
    /// Red channel in sRGB space (`0.0..=1.0`).
    pub r: f32,
    /// Green channel in sRGB space (`0.0..=1.0`).
    pub g: f32,
    /// Blue channel in sRGB space (`0.0..=1.0`).
    pub b: f32,
    /// Alpha channel (`0.0..=1.0`).
    pub a: f32,
}

impl FfiRgba {
    /// Fully transparent black (`[0, 0, 0, 0]`).
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);
    /// Opaque white (`[1, 1, 1, 1]`).
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);
    /// Opaque black (`[0, 0, 0, 1]`).
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    /// Constructs a [`FfiRgba`] color from sRGBA channels.
    #[inline]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Projects a Bevy [`Color`] into sRGBA [`FfiRgba`].
    #[inline]
    pub fn from_bevy(color: Color) -> Self {
        let s = color.to_srgba();
        Self {
            r: s.red,
            g: s.green,
            b: s.blue,
            a: s.alpha,
        }
    }

    /// Validates finite channels and converts into a Bevy [`Color`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<Color, FfiResult> {
        if !(self.r.is_finite() && self.g.is_finite() && self.b.is_finite() && self.a.is_finite()) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        Ok(Color::srgba(self.r, self.g, self.b, self.a))
    }
}

impl Default for FfiRgba {
    #[inline]
    fn default() -> Self {
        Self::WHITE
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` wire projection of Bevy's [`BorderColor`].
///
/// Layout: `64` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiBorderColor")]
pub struct FfiBorderColor {
    /// Top border color.
    pub top: FfiRgba,
    /// Right border color.
    pub right: FfiRgba,
    /// Bottom border color.
    pub bottom: FfiRgba,
    /// Left border color.
    pub left: FfiRgba,
}

impl FfiBorderColor {
    /// Sets all four borders to `color`.
    #[inline]
    pub const fn all(color: FfiRgba) -> Self {
        Self {
            top: color,
            right: color,
            bottom: color,
            left: color,
        }
    }

    /// Projects a Bevy [`BorderColor`] into [`FfiBorderColor`].
    #[inline]
    pub fn from_bevy(bc: &BorderColor) -> Self {
        Self {
            top: FfiRgba::from_bevy(bc.top),
            right: FfiRgba::from_bevy(bc.right),
            bottom: FfiRgba::from_bevy(bc.bottom),
            left: FfiRgba::from_bevy(bc.left),
        }
    }

    /// Validates and converts into a Bevy [`BorderColor`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<BorderColor, FfiResult> {
        Ok(BorderColor {
            top: self.top.try_to_bevy()?,
            right: self.right.try_to_bevy()?,
            bottom: self.bottom.try_to_bevy()?,
            left: self.left.try_to_bevy()?,
        })
    }
}

impl Default for FfiBorderColor {
    #[inline]
    fn default() -> Self {
        Self::all(FfiRgba::TRANSPARENT)
    }
}

/// Fixed-layout `#[repr(transparent)]` `AbiPod` wire projection of Bevy `0.20`'s [`RemSize`]
/// root font-size resource (in logical pixels).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct FfiRemSize(pub f32);

impl Default for FfiRemSize {
    #[inline]
    fn default() -> Self {
        Self(RemSize::default().0)
    }
}

/// Fixed-layout `#[repr(transparent)]` `AbiPod` wire projection of Bevy `0.20`'s [`EmSize`]
/// per-node font-size component (in logical pixels).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
pub struct FfiEmSize(pub f32);

impl Default for FfiEmSize {
    #[inline]
    fn default() -> Self {
        Self(EmSize::default().0)
    }
}

// ============================================================================
// Retained UI Node Descriptor (`Node` + `BorderRadius` + `FixedNode`)
// ============================================================================

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// [`Node`], elliptical [`BorderRadius`], and [`FixedNode`] viewport-pinning marker.
///
/// Layout: `280` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiUiNode")]
pub struct FfiUiNode {
    /// Preferred node width ([`Node::width`]).
    pub width: FfiVal,
    /// Preferred node height ([`Node::height`]).
    pub height: FfiVal,
    /// Minimum node width ([`Node::min_width`]).
    pub min_width: FfiVal,
    /// Minimum node height ([`Node::min_height`]).
    pub min_height: FfiVal,
    /// Maximum node width ([`Node::max_width`]).
    pub max_width: FfiVal,
    /// Maximum node height ([`Node::max_height`]).
    pub max_height: FfiVal,
    /// Left offset ([`Node::left`]).
    pub left: FfiVal,
    /// Right offset ([`Node::right`]).
    pub right: FfiVal,
    /// Top offset ([`Node::top`]).
    pub top: FfiVal,
    /// Bottom offset ([`Node::bottom`]).
    pub bottom: FfiVal,
    /// Flex basis ([`Node::flex_basis`]).
    pub flex_basis: FfiVal,
    /// Gutter between rows ([`Node::row_gap`]).
    pub row_gap: FfiVal,
    /// Gutter between columns ([`Node::column_gap`]).
    pub column_gap: FfiVal,
    /// Inner padding ([`Node::padding`]).
    pub padding: FfiUiRect,
    /// Outer margin ([`Node::margin`]).
    pub margin: FfiUiRect,
    /// Border thickness ([`Node::border`]).
    pub border: FfiUiRect,
    /// Elliptical corner radii ([`Node::border_radius`]).
    pub border_radius: FfiBorderRadius,
    /// Flex grow factor ([`Node::flex_grow`]).
    pub flex_grow: f32,
    /// Flex shrink factor ([`Node::flex_shrink`], default `1.0`).
    pub flex_shrink: f32,
    /// Layout algorithm (`0 = Flex`, `1 = Grid`, `2 = Block`, `3 = None`).
    pub display: u8,
    /// Positioning mode (`0 = Relative`, `1 = Absolute`).
    pub position_type: u8,
    /// Flexbox main axis (`0 = Row`, `1 = Column`, `2 = RowReverse`, `3 = ColumnReverse`).
    pub flex_direction: u8,
    /// Flexbox wrapping (`0 = NoWrap`, `1 = Wrap`, `2 = WrapReverse`).
    pub flex_wrap: u8,
    /// Cross-axis alignment (`0 = Default`, `1 = Start`, ..., `12 = Stretch`).
    pub align_items: u8,
    /// Main-axis justification (`0 = Default`, `1 = Start`, ..., `14 = SpaceAround`).
    pub justify_content: u8,
    /// When non-zero, attaches Bevy `0.20`'s [`FixedNode`] marker so this node is laid out as a
    /// viewport-pinned root even when parented in a UI hierarchy.
    pub fixed_node: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiUiNode {
    /// [`Display::Flex`] (`0`).
    pub const DISPLAY_FLEX: u8 = 0;
    /// [`Display::Grid`] (`1`).
    pub const DISPLAY_GRID: u8 = 1;
    /// [`Display::Block`] (`2`).
    pub const DISPLAY_BLOCK: u8 = 2;
    /// [`Display::None`] (`3`).
    pub const DISPLAY_NONE: u8 = 3;

    /// [`PositionType::Relative`] (`0`).
    pub const POSITION_RELATIVE: u8 = 0;
    /// [`PositionType::Absolute`] (`1`).
    pub const POSITION_ABSOLUTE: u8 = 1;

    /// [`FlexDirection::Row`] (`0`).
    pub const FLEX_ROW: u8 = 0;
    /// [`FlexDirection::Column`] (`1`).
    pub const FLEX_COLUMN: u8 = 1;
    /// [`FlexDirection::RowReverse`] (`2`).
    pub const FLEX_ROW_REVERSE: u8 = 2;
    /// [`FlexDirection::ColumnReverse`] (`3`).
    pub const FLEX_COLUMN_REVERSE: u8 = 3;

    /// [`FlexWrap::NoWrap`] (`0`).
    pub const WRAP_NO_WRAP: u8 = 0;
    /// [`FlexWrap::Wrap`] (`1`).
    pub const WRAP_WRAP: u8 = 1;
    /// [`FlexWrap::WrapReverse`] (`2`).
    pub const WRAP_WRAP_REVERSE: u8 = 2;

    /// [`AlignItems::Default`] (`0`).
    pub const ALIGN_DEFAULT: u8 = 0;
    /// [`AlignItems::Start`] (`1`).
    pub const ALIGN_START: u8 = 1;
    /// [`AlignItems::StartSafe`] (`2`).
    pub const ALIGN_START_SAFE: u8 = 2;
    /// [`AlignItems::End`] (`3`).
    pub const ALIGN_END: u8 = 3;
    /// [`AlignItems::EndSafe`] (`4`).
    pub const ALIGN_END_SAFE: u8 = 4;
    /// [`AlignItems::FlexStart`] (`5`).
    pub const ALIGN_FLEX_START: u8 = 5;
    /// [`AlignItems::FlexStartSafe`] (`6`).
    pub const ALIGN_FLEX_START_SAFE: u8 = 6;
    /// [`AlignItems::FlexEnd`] (`7`).
    pub const ALIGN_FLEX_END: u8 = 7;
    /// [`AlignItems::FlexEndSafe`] (`8`).
    pub const ALIGN_FLEX_END_SAFE: u8 = 8;
    /// [`AlignItems::Center`] (`9`).
    pub const ALIGN_CENTER: u8 = 9;
    /// [`AlignItems::CenterSafe`] (`10`).
    pub const ALIGN_CENTER_SAFE: u8 = 10;
    /// [`AlignItems::Baseline`] (`11`).
    pub const ALIGN_BASELINE: u8 = 11;
    /// [`AlignItems::Stretch`] (`12`).
    pub const ALIGN_STRETCH: u8 = 12;

    /// [`JustifyContent::Default`] (`0`).
    pub const JUSTIFY_DEFAULT: u8 = 0;
    /// [`JustifyContent::Start`] (`1`).
    pub const JUSTIFY_START: u8 = 1;
    /// [`JustifyContent::StartSafe`] (`2`).
    pub const JUSTIFY_START_SAFE: u8 = 2;
    /// [`JustifyContent::End`] (`3`).
    pub const JUSTIFY_END: u8 = 3;
    /// [`JustifyContent::EndSafe`] (`4`).
    pub const JUSTIFY_END_SAFE: u8 = 4;
    /// [`JustifyContent::FlexStart`] (`5`).
    pub const JUSTIFY_FLEX_START: u8 = 5;
    /// [`JustifyContent::FlexStartSafe`] (`6`).
    pub const JUSTIFY_FLEX_START_SAFE: u8 = 6;
    /// [`JustifyContent::FlexEnd`] (`7`).
    pub const JUSTIFY_FLEX_END: u8 = 7;
    /// [`JustifyContent::FlexEndSafe`] (`8`).
    pub const JUSTIFY_FLEX_END_SAFE: u8 = 8;
    /// [`JustifyContent::Center`] (`9`).
    pub const JUSTIFY_CENTER: u8 = 9;
    /// [`JustifyContent::CenterSafe`] (`10`).
    pub const JUSTIFY_CENTER_SAFE: u8 = 10;
    /// [`JustifyContent::Stretch`] (`11`).
    pub const JUSTIFY_STRETCH: u8 = 11;
    /// [`JustifyContent::SpaceBetween`] (`12`).
    pub const JUSTIFY_SPACE_BETWEEN: u8 = 12;
    /// [`JustifyContent::SpaceEvenly`] (`13`).
    pub const JUSTIFY_SPACE_EVENLY: u8 = 13;
    /// [`JustifyContent::SpaceAround`] (`14`).
    pub const JUSTIFY_SPACE_AROUND: u8 = 14;

    /// Applies this descriptor's layout and border-radius fields onto an existing Bevy [`Node`].
    pub fn apply_to_node(&self, node: &mut Node) -> Result<(), FfiResult> {
        if self.reserved0 != 0 || !self.flex_grow.is_finite() || !self.flex_shrink.is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let display = match self.display {
            Self::DISPLAY_FLEX => Display::Flex,
            Self::DISPLAY_GRID => Display::Grid,
            Self::DISPLAY_BLOCK => Display::Block,
            Self::DISPLAY_NONE => Display::None,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let position_type = match self.position_type {
            Self::POSITION_RELATIVE => PositionType::Relative,
            Self::POSITION_ABSOLUTE => PositionType::Absolute,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let flex_direction = match self.flex_direction {
            Self::FLEX_ROW => FlexDirection::Row,
            Self::FLEX_COLUMN => FlexDirection::Column,
            Self::FLEX_ROW_REVERSE => FlexDirection::RowReverse,
            Self::FLEX_COLUMN_REVERSE => FlexDirection::ColumnReverse,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let flex_wrap = match self.flex_wrap {
            Self::WRAP_NO_WRAP => FlexWrap::NoWrap,
            Self::WRAP_WRAP => FlexWrap::Wrap,
            Self::WRAP_WRAP_REVERSE => FlexWrap::WrapReverse,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let align_items = match self.align_items {
            Self::ALIGN_DEFAULT => AlignItems::Default,
            Self::ALIGN_START => AlignItems::Start,
            Self::ALIGN_START_SAFE => AlignItems::StartSafe,
            Self::ALIGN_END => AlignItems::End,
            Self::ALIGN_END_SAFE => AlignItems::EndSafe,
            Self::ALIGN_FLEX_START => AlignItems::FlexStart,
            Self::ALIGN_FLEX_START_SAFE => AlignItems::FlexStartSafe,
            Self::ALIGN_FLEX_END => AlignItems::FlexEnd,
            Self::ALIGN_FLEX_END_SAFE => AlignItems::FlexEndSafe,
            Self::ALIGN_CENTER => AlignItems::Center,
            Self::ALIGN_CENTER_SAFE => AlignItems::CenterSafe,
            Self::ALIGN_BASELINE => AlignItems::Baseline,
            Self::ALIGN_STRETCH => AlignItems::Stretch,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let justify_content = match self.justify_content {
            Self::JUSTIFY_DEFAULT => JustifyContent::Default,
            Self::JUSTIFY_START => JustifyContent::Start,
            Self::JUSTIFY_START_SAFE => JustifyContent::StartSafe,
            Self::JUSTIFY_END => JustifyContent::End,
            Self::JUSTIFY_END_SAFE => JustifyContent::EndSafe,
            Self::JUSTIFY_FLEX_START => JustifyContent::FlexStart,
            Self::JUSTIFY_FLEX_START_SAFE => JustifyContent::FlexStartSafe,
            Self::JUSTIFY_FLEX_END => JustifyContent::FlexEnd,
            Self::JUSTIFY_FLEX_END_SAFE => JustifyContent::FlexEndSafe,
            Self::JUSTIFY_CENTER => JustifyContent::Center,
            Self::JUSTIFY_CENTER_SAFE => JustifyContent::CenterSafe,
            Self::JUSTIFY_STRETCH => JustifyContent::Stretch,
            Self::JUSTIFY_SPACE_BETWEEN => JustifyContent::SpaceBetween,
            Self::JUSTIFY_SPACE_EVENLY => JustifyContent::SpaceEvenly,
            Self::JUSTIFY_SPACE_AROUND => JustifyContent::SpaceAround,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };

        let width = self.width.try_to_bevy()?;
        let height = self.height.try_to_bevy()?;
        let min_width = self.min_width.try_to_bevy()?;
        let min_height = self.min_height.try_to_bevy()?;
        let max_width = self.max_width.try_to_bevy()?;
        let max_height = self.max_height.try_to_bevy()?;
        let left = self.left.try_to_bevy()?;
        let right = self.right.try_to_bevy()?;
        let top = self.top.try_to_bevy()?;
        let bottom = self.bottom.try_to_bevy()?;
        let flex_basis = self.flex_basis.try_to_bevy()?;
        let row_gap = self.row_gap.try_to_bevy()?;
        let column_gap = self.column_gap.try_to_bevy()?;
        let padding = self.padding.try_to_bevy()?;
        let margin = self.margin.try_to_bevy()?;
        let border = self.border.try_to_bevy()?;
        let border_radius = self.border_radius.try_to_bevy()?;

        node.display = display;
        node.position_type = position_type;
        node.flex_direction = flex_direction;
        node.flex_wrap = flex_wrap;
        node.align_items = align_items;
        node.justify_content = justify_content;
        node.width = width;
        node.height = height;
        node.min_width = min_width;
        node.min_height = min_height;
        node.max_width = max_width;
        node.max_height = max_height;
        node.left = left;
        node.right = right;
        node.top = top;
        node.bottom = bottom;
        node.flex_basis = flex_basis;
        node.row_gap = row_gap;
        node.column_gap = column_gap;
        node.padding = padding;
        node.margin = margin;
        node.border = border;
        node.border_radius = border_radius;
        node.flex_grow = self.flex_grow;
        node.flex_shrink = self.flex_shrink;
        Ok(())
    }

    /// Projects a Bevy [`Node`] (and optional [`FixedNode`] presence) into [`FfiUiNode`].
    pub fn from_bevy(node: &Node, fixed_node: bool) -> Self {
        Self {
            width: FfiVal::from_bevy(node.width),
            height: FfiVal::from_bevy(node.height),
            min_width: FfiVal::from_bevy(node.min_width),
            min_height: FfiVal::from_bevy(node.min_height),
            max_width: FfiVal::from_bevy(node.max_width),
            max_height: FfiVal::from_bevy(node.max_height),
            left: FfiVal::from_bevy(node.left),
            right: FfiVal::from_bevy(node.right),
            top: FfiVal::from_bevy(node.top),
            bottom: FfiVal::from_bevy(node.bottom),
            flex_basis: FfiVal::from_bevy(node.flex_basis),
            row_gap: FfiVal::from_bevy(node.row_gap),
            column_gap: FfiVal::from_bevy(node.column_gap),
            padding: FfiUiRect::from_bevy(node.padding),
            margin: FfiUiRect::from_bevy(node.margin),
            border: FfiUiRect::from_bevy(node.border),
            border_radius: FfiBorderRadius::from_bevy(node.border_radius),
            flex_grow: node.flex_grow,
            flex_shrink: node.flex_shrink,
            display: match node.display {
                Display::Flex => Self::DISPLAY_FLEX,
                Display::Grid => Self::DISPLAY_GRID,
                Display::Block => Self::DISPLAY_BLOCK,
                Display::None => Self::DISPLAY_NONE,
            },
            position_type: match node.position_type {
                PositionType::Relative => Self::POSITION_RELATIVE,
                PositionType::Absolute => Self::POSITION_ABSOLUTE,
            },
            flex_direction: match node.flex_direction {
                FlexDirection::Row => Self::FLEX_ROW,
                FlexDirection::Column => Self::FLEX_COLUMN,
                FlexDirection::RowReverse => Self::FLEX_ROW_REVERSE,
                FlexDirection::ColumnReverse => Self::FLEX_COLUMN_REVERSE,
            },
            flex_wrap: match node.flex_wrap {
                FlexWrap::NoWrap => Self::WRAP_NO_WRAP,
                FlexWrap::Wrap => Self::WRAP_WRAP,
                FlexWrap::WrapReverse => Self::WRAP_WRAP_REVERSE,
            },
            align_items: match node.align_items {
                AlignItems::Default => Self::ALIGN_DEFAULT,
                AlignItems::Start => Self::ALIGN_START,
                AlignItems::StartSafe => Self::ALIGN_START_SAFE,
                AlignItems::End => Self::ALIGN_END,
                AlignItems::EndSafe => Self::ALIGN_END_SAFE,
                AlignItems::FlexStart => Self::ALIGN_FLEX_START,
                AlignItems::FlexStartSafe => Self::ALIGN_FLEX_START_SAFE,
                AlignItems::FlexEnd => Self::ALIGN_FLEX_END,
                AlignItems::FlexEndSafe => Self::ALIGN_FLEX_END_SAFE,
                AlignItems::Center => Self::ALIGN_CENTER,
                AlignItems::CenterSafe => Self::ALIGN_CENTER_SAFE,
                AlignItems::Baseline => Self::ALIGN_BASELINE,
                AlignItems::Stretch => Self::ALIGN_STRETCH,
            },
            justify_content: match node.justify_content {
                JustifyContent::Default => Self::JUSTIFY_DEFAULT,
                JustifyContent::Start => Self::JUSTIFY_START,
                JustifyContent::StartSafe => Self::JUSTIFY_START_SAFE,
                JustifyContent::End => Self::JUSTIFY_END,
                JustifyContent::EndSafe => Self::JUSTIFY_END_SAFE,
                JustifyContent::FlexStart => Self::JUSTIFY_FLEX_START,
                JustifyContent::FlexStartSafe => Self::JUSTIFY_FLEX_START_SAFE,
                JustifyContent::FlexEnd => Self::JUSTIFY_FLEX_END,
                JustifyContent::FlexEndSafe => Self::JUSTIFY_FLEX_END_SAFE,
                JustifyContent::Center => Self::JUSTIFY_CENTER,
                JustifyContent::CenterSafe => Self::JUSTIFY_CENTER_SAFE,
                JustifyContent::Stretch => Self::JUSTIFY_STRETCH,
                JustifyContent::SpaceBetween => Self::JUSTIFY_SPACE_BETWEEN,
                JustifyContent::SpaceEvenly => Self::JUSTIFY_SPACE_EVENLY,
                JustifyContent::SpaceAround => Self::JUSTIFY_SPACE_AROUND,
            },
            fixed_node: FfiBool::from_bool(fixed_node),
            reserved0: 0,
        }
    }
}

impl Default for FfiUiNode {
    fn default() -> Self {
        Self::from_bevy(&Node::DEFAULT, false)
    }
}

// ============================================================================
// Text, EditableText + TextInput Split, InlineBox & InlineImage Descriptors
// ============================================================================

/// Fixed-layout `#[repr(C)]` `AbiPod` wire representation of Bevy `0.20`'s [`FontSize`]
/// (`Px`, `Vw`, `Vh`, `VMin`, `VMax`, `Rem`).
///
/// Layout: `8` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFontSize")]
pub struct FfiFontSize {
    /// Unit discriminant (`0 = Px`, `1 = Vw`, `2 = Vh`, `3 = VMin`, `4 = VMax`, `5 = Rem`).
    pub unit: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
    /// Scalar font-size magnitude.
    pub value: f32,
}

impl FfiFontSize {
    /// Discriminant for [`FontSize::Px`] (`0`).
    pub const UNIT_PX: u8 = 0;
    /// Discriminant for [`FontSize::Vw`] (`1`).
    pub const UNIT_VW: u8 = 1;
    /// Discriminant for [`FontSize::Vh`] (`2`).
    pub const UNIT_VH: u8 = 2;
    /// Discriminant for [`FontSize::VMin`] (`3`).
    pub const UNIT_VMIN: u8 = 3;
    /// Discriminant for [`FontSize::VMax`] (`4`).
    pub const UNIT_VMAX: u8 = 4;
    /// Discriminant for [`FontSize::Rem`] (`5`).
    pub const UNIT_REM: u8 = 5;

    /// Constructs a [`FontSize::Px`] wire value.
    #[inline]
    pub const fn px(value: f32) -> Self {
        Self {
            unit: Self::UNIT_PX,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`FontSize::Vw`] wire value.
    #[inline]
    pub const fn vw(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VW,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`FontSize::Vh`] wire value.
    #[inline]
    pub const fn vh(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VH,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`FontSize::VMin`] wire value.
    #[inline]
    pub const fn vmin(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VMIN,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`FontSize::VMax`] wire value.
    #[inline]
    pub const fn vmax(value: f32) -> Self {
        Self {
            unit: Self::UNIT_VMAX,
            reserved0: [0; 3],
            value,
        }
    }

    /// Constructs a [`FontSize::Rem`] wire value (relative to [`RemSize`]).
    #[inline]
    pub const fn rem(value: f32) -> Self {
        Self {
            unit: Self::UNIT_REM,
            reserved0: [0; 3],
            value,
        }
    }

    /// Projects a Bevy [`FontSize`] into [`FfiFontSize`].
    #[inline]
    pub const fn from_bevy(size: FontSize) -> Self {
        match size {
            FontSize::Px(v) => Self::px(v),
            FontSize::Vw(v) => Self::vw(v),
            FontSize::Vh(v) => Self::vh(v),
            FontSize::VMin(v) => Self::vmin(v),
            FontSize::VMax(v) => Self::vmax(v),
            FontSize::Rem(v) => Self::rem(v),
        }
    }

    /// Validates and converts into a Bevy [`FontSize`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<FontSize, FfiResult> {
        if self.reserved0 != [0; 3] || !self.value.is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        match self.unit {
            Self::UNIT_PX => Ok(FontSize::Px(self.value)),
            Self::UNIT_VW => Ok(FontSize::Vw(self.value)),
            Self::UNIT_VH => Ok(FontSize::Vh(self.value)),
            Self::UNIT_VMIN => Ok(FontSize::VMin(self.value)),
            Self::UNIT_VMAX => Ok(FontSize::VMax(self.value)),
            Self::UNIT_REM => Ok(FontSize::Rem(self.value)),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

impl Default for FfiFontSize {
    #[inline]
    fn default() -> Self {
        Self::px(20.0)
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy's
/// [`TextFont`], [`TextColor`], [`TextLayout`], and optionally [`Text`].
///
/// Layout: `64` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiUiTextStyle")]
pub struct FfiUiTextStyle {
    /// Optional [`Font`] asset handle (`FfiAssetHandle::NONE` preserves the current font source).
    pub font_handle: FfiAssetHandle,
    /// Foreground text color ([`TextColor`]).
    pub color: FfiRgba,
    /// Font size ([`TextFont::font_size`]), supporting `FontSize::Rem` and viewport units.
    pub font_size: FfiFontSize,
    /// Variable font weight (`1..=1000`, default `400` = [`FontWeight::NORMAL`]).
    pub weight: u16,
    /// Horizontal justification (`0 = Left`, `1 = Center`, `2 = Right`, `3 = Justified`,
    /// `4 = Start`, `5 = End`).
    pub justify: u8,
    /// Line breaking strategy (`0 = WordBoundary`, `1 = AnyCharacter`, `2 = WordOrCharacter`,
    /// `3 = NoWrap`).
    pub linebreak: u8,
    /// Font smoothing (`0 = AntiAliased`, `1 = None`).
    pub font_smoothing: u8,
    /// Font slant style (`0 = Normal`, `1 = Italic`, `2 = Oblique`).
    pub font_style: u8,
    /// When non-zero, ensures a [`Text`] component exists on the entity if absent.
    pub ensure_text: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiUiTextStyle {
    /// [`Justify::Left`] (`0`).
    pub const JUSTIFY_LEFT: u8 = 0;
    /// [`Justify::Center`] (`1`).
    pub const JUSTIFY_CENTER: u8 = 1;
    /// [`Justify::Right`] (`2`).
    pub const JUSTIFY_RIGHT: u8 = 2;
    /// [`Justify::Justified`] (`3`).
    pub const JUSTIFY_JUSTIFIED: u8 = 3;
    /// [`Justify::Start`] (`4`).
    pub const JUSTIFY_START: u8 = 4;
    /// [`Justify::End`] (`5`).
    pub const JUSTIFY_END: u8 = 5;

    /// [`LineBreak::WordBoundary`] (`0`).
    pub const LINEBREAK_WORD_BOUNDARY: u8 = 0;
    /// [`LineBreak::AnyCharacter`] (`1`).
    pub const LINEBREAK_ANY_CHARACTER: u8 = 1;
    /// [`LineBreak::WordOrCharacter`] (`2`).
    pub const LINEBREAK_WORD_OR_CHARACTER: u8 = 2;
    /// [`LineBreak::NoWrap`] (`3`).
    pub const LINEBREAK_NO_WRAP: u8 = 3;

    /// [`FontSmoothing::AntiAliased`] (`0`).
    pub const SMOOTHING_ANTI_ALIASED: u8 = 0;
    /// [`FontSmoothing::None`] (`1`).
    pub const SMOOTHING_NONE: u8 = 1;

    /// [`FontStyle::Normal`] (`0`).
    pub const STYLE_NORMAL: u8 = 0;
    /// [`FontStyle::Italic`] (`1`).
    pub const STYLE_ITALIC: u8 = 1;
    /// [`FontStyle::Oblique`] (`2`).
    pub const STYLE_OBLIQUE: u8 = 2;

    /// Validates and decodes the [`Justify`] setting.
    pub fn try_justify(self) -> Result<Justify, FfiResult> {
        match self.justify {
            Self::JUSTIFY_LEFT => Ok(Justify::Left),
            Self::JUSTIFY_CENTER => Ok(Justify::Center),
            Self::JUSTIFY_RIGHT => Ok(Justify::Right),
            Self::JUSTIFY_JUSTIFIED => Ok(Justify::Justified),
            Self::JUSTIFY_START => Ok(Justify::Start),
            Self::JUSTIFY_END => Ok(Justify::End),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    /// Validates and decodes the [`LineBreak`] setting.
    pub fn try_linebreak(self) -> Result<LineBreak, FfiResult> {
        match self.linebreak {
            Self::LINEBREAK_WORD_BOUNDARY => Ok(LineBreak::WordBoundary),
            Self::LINEBREAK_ANY_CHARACTER => Ok(LineBreak::AnyCharacter),
            Self::LINEBREAK_WORD_OR_CHARACTER => Ok(LineBreak::WordOrCharacter),
            Self::LINEBREAK_NO_WRAP => Ok(LineBreak::NoWrap),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    /// Validates and decodes the [`FontSmoothing`] setting.
    pub fn try_font_smoothing(self) -> Result<FontSmoothing, FfiResult> {
        match self.font_smoothing {
            Self::SMOOTHING_ANTI_ALIASED => Ok(FontSmoothing::AntiAliased),
            Self::SMOOTHING_NONE => Ok(FontSmoothing::None),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    /// Validates and decodes the [`FontStyle`] setting.
    pub fn try_font_style(self) -> Result<FontStyle, FfiResult> {
        match self.font_style {
            Self::STYLE_NORMAL => Ok(FontStyle::Normal),
            Self::STYLE_ITALIC => Ok(FontStyle::Italic),
            Self::STYLE_OBLIQUE => Ok(FontStyle::Oblique(None)),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

impl Default for FfiUiTextStyle {
    fn default() -> Self {
        Self {
            font_handle: FfiAssetHandle::NONE,
            color: FfiRgba::WHITE,
            font_size: FfiFontSize::rem(1.0),
            weight: FontWeight::NORMAL.0,
            justify: Self::JUSTIFY_LEFT,
            linebreak: Self::LINEBREAK_WORD_BOUNDARY,
            font_smoothing: Self::SMOOTHING_ANTI_ALIASED,
            font_style: Self::STYLE_NORMAL,
            ensure_text: FfiBool::FALSE,
            reserved0: 0,
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor bridging Bevy `0.20`'s split
/// [`EditableText`] (in `bevy_text`) and [`TextInput`] (in `bevy_ui_widgets`) architecture.
///
/// Setting `interactive = FfiBool::TRUE` attaches [`TextInput`] alongside [`EditableText`];
/// setting `interactive = FfiBool::FALSE` preserves headless [`EditableText`] without attaching
/// [`TextInput`] (removing [`TextInput`] if previously attached).
///
/// Layout: `36` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiEditableTextConfig")]
pub struct FfiEditableTextConfig {
    /// Cursor reveal margins as fractions of viewport size (`0.0..=0.5`).
    pub cursor_margin: [f32; 2],
    /// Cursor width relative to font size (default `0.2`).
    pub cursor_width: f32,
    /// Cursor blink period in seconds (default `1.0`).
    pub cursor_blink_period_secs: f32,
    /// Height in number of visible lines when `has_visible_lines` is set.
    pub visible_lines: f32,
    /// Width in number of visible glyphs when `has_visible_width` is set.
    pub visible_width: f32,
    /// Maximum character count when `has_max_characters` is set.
    pub max_characters: u32,
    /// When non-zero, attaches `bevy_ui_widgets::TextInput`; when zero, keeps pure headless
    /// `bevy_text::EditableText` without `TextInput`.
    pub interactive: FfiBool,
    /// Whether multi-line input (`\n`) is permitted ([`EditableText::allow_newlines`]).
    pub allow_newlines: FfiBool,
    /// [`TextReadWriteMode`] discriminant (`0 = Editable`, `1 = ReadOnly`, `2 = Static`).
    pub read_write_mode: u8,
    /// Whether `max_characters` is engaged (`0 = None`, `1 = Some`).
    pub has_max_characters: FfiBool,
    /// Whether `visible_lines` is engaged (`0 = None`, `1 = Some`).
    pub has_visible_lines: FfiBool,
    /// Whether `visible_width` is engaged (`0 = None`, `1 = Some`).
    pub has_visible_width: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 2],
}

impl FfiEditableTextConfig {
    /// [`TextReadWriteMode::Editable`] (`0`).
    pub const MODE_EDITABLE: u8 = 0;
    /// [`TextReadWriteMode::ReadOnly`] (`1`).
    pub const MODE_READ_ONLY: u8 = 1;
    /// [`TextReadWriteMode::Static`] (`2`).
    pub const MODE_STATIC: u8 = 2;

    /// Validates and decodes [`TextReadWriteMode`].
    pub fn try_read_write_mode(self) -> Result<TextReadWriteMode, FfiResult> {
        match self.read_write_mode {
            Self::MODE_EDITABLE => Ok(TextReadWriteMode::Editable),
            Self::MODE_READ_ONLY => Ok(TextReadWriteMode::ReadOnly),
            Self::MODE_STATIC => Ok(TextReadWriteMode::Static),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    /// Applies this configuration onto an [`EditableText`] instance without resetting its text
    /// buffer or cursor state.
    pub fn apply_to_editable_text(&self, editable: &mut EditableText) -> Result<(), FfiResult> {
        if self.reserved0 != [0; 2]
            || !self.cursor_margin[0].is_finite()
            || !self.cursor_margin[1].is_finite()
            || !self.cursor_width.is_finite()
            || !self.cursor_blink_period_secs.is_finite()
            || self.cursor_blink_period_secs < 0.0
            || (self.has_visible_lines.get() && !self.visible_lines.is_finite())
            || (self.has_visible_width.get() && !self.visible_width.is_finite())
        {
            return Err(FfiResult::INVALID_ARGUMENT);
        }

        editable.cursor_margin = Vec2::new(self.cursor_margin[0], self.cursor_margin[1]);
        editable.cursor_width = self.cursor_width;
        editable.cursor_blink_period = Duration::from_secs_f32(self.cursor_blink_period_secs);
        editable.max_characters = self
            .has_max_characters
            .get()
            .then_some(self.max_characters as usize);
        editable.visible_lines = self.has_visible_lines.get().then_some(self.visible_lines);
        editable.visible_width = self.has_visible_width.get().then_some(self.visible_width);
        editable.allow_newlines = self.allow_newlines.get();
        Ok(())
    }
}

impl Default for FfiEditableTextConfig {
    fn default() -> Self {
        Self {
            cursor_margin: [0.2, 0.2],
            cursor_width: 0.2,
            cursor_blink_period_secs: 1.0,
            visible_lines: 1.0,
            visible_width: 0.0,
            max_characters: 0,
            interactive: FfiBool::TRUE,
            allow_newlines: FfiBool::FALSE,
            read_write_mode: Self::MODE_EDITABLE,
            has_max_characters: FfiBool::FALSE,
            has_visible_lines: FfiBool::TRUE,
            has_visible_width: FfiBool::FALSE,
            reserved0: [0; 2],
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// [`InlineBox`] (`bevy_text::InlineBox`).
///
/// Layout: `12` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiInlineBox")]
pub struct FfiInlineBox {
    /// Size of the inline box in logical pixels `[width, height]`.
    pub size: [f32; 2],
    /// Flow mode (`0 = OutOfFlow`, `1 = InFlow`), matching [`InlineBoxKind`].
    pub kind: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

impl FfiInlineBox {
    /// [`InlineBoxKind::OutOfFlow`] (`0`, default).
    pub const KIND_OUT_OF_FLOW: u8 = 0;
    /// [`InlineBoxKind::InFlow`] (`1`).
    pub const KIND_IN_FLOW: u8 = 1;

    /// Constructs an in-flow [`FfiInlineBox`] with logical pixel dimensions `(width, height)`.
    #[inline]
    pub const fn in_flow(width: f32, height: f32) -> Self {
        Self {
            size: [width, height],
            kind: Self::KIND_IN_FLOW,
            reserved0: [0; 3],
        }
    }

    /// Constructs an out-of-flow [`FfiInlineBox`] with logical pixel dimensions `(width, height)`.
    #[inline]
    pub const fn out_of_flow(width: f32, height: f32) -> Self {
        Self {
            size: [width, height],
            kind: Self::KIND_OUT_OF_FLOW,
            reserved0: [0; 3],
        }
    }

    /// Projects a Bevy [`InlineBox`] into [`FfiInlineBox`].
    #[inline]
    pub const fn from_bevy(inline_box: InlineBox) -> Self {
        Self {
            size: [inline_box.size.x, inline_box.size.y],
            kind: match inline_box.kind {
                InlineBoxKind::OutOfFlow => Self::KIND_OUT_OF_FLOW,
                InlineBoxKind::InFlow => Self::KIND_IN_FLOW,
            },
            reserved0: [0; 3],
        }
    }

    /// Validates and converts into a Bevy [`InlineBox`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<InlineBox, FfiResult> {
        if self.reserved0 != [0; 3] || !self.size[0].is_finite() || !self.size[1].is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let kind = match self.kind {
            Self::KIND_OUT_OF_FLOW => InlineBoxKind::OutOfFlow,
            Self::KIND_IN_FLOW => InlineBoxKind::InFlow,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        Ok(InlineBox {
            kind,
            size: Vec2::new(self.size[0], self.size[1]),
        })
    }
}

impl Default for FfiInlineBox {
    #[inline]
    fn default() -> Self {
        Self::out_of_flow(0.0, 0.0)
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// [`InlineImage`] (`bevy_ui::widget::InlineImage`) via [`FfiAssetHandle`].
///
/// Layout: `60` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiInlineImage")]
pub struct FfiInlineImage {
    /// Image texture handle (`FfiAssetHandle::NONE` resolves to [`TRANSPARENT_IMAGE_HANDLE`]).
    pub image: FfiAssetHandle,
    /// Multiplicative tint color ([`InlineImage::color`]).
    pub color: FfiRgba,
    /// Fixed width in logical pixels when `has_width` is non-zero.
    pub width: f32,
    /// Fixed height in logical pixels when `has_height` is non-zero.
    pub height: f32,
    /// Whether `width` is engaged (`0 = None`, `1 = Some`).
    pub has_width: FfiBool,
    /// Whether `height` is engaged (`0 = None`, `1 = Some`).
    pub has_height: FfiBool,
    /// Flip horizontally ([`InlineImage::flip_x`]).
    pub flip_x: FfiBool,
    /// Flip vertically ([`InlineImage::flip_y`]).
    pub flip_y: FfiBool,
}

impl Default for FfiInlineImage {
    fn default() -> Self {
        Self {
            image: FfiAssetHandle::NONE,
            color: FfiRgba::WHITE,
            width: 0.0,
            height: 0.0,
            has_width: FfiBool::FALSE,
            has_height: FfiBool::FALSE,
            flip_x: FfiBool::FALSE,
            flip_y: FfiBool::FALSE,
        }
    }
}

// ============================================================================
// Headless Widget Descriptors (`TabList`, `Tab`, `SelectedTab`) & `FfiValueChange`
// ============================================================================

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// headless [`TabList`].
///
/// Layout: `4` bytes, `1`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiTabList")]
pub struct FfiTabList {
    /// Navigation orientation (`0 = Horizontal`, `1 = Vertical`).
    pub orientation: u8,
    /// Selection activation policy (`0 = Manual`, `1 = Automatic`).
    pub activation: u8,
    /// When non-zero, automatically applies emitted `ValueChange<Option<Entity>>` updates onto
    /// [`SelectedTab`] (mirroring `tablist_self_update`).
    pub self_update: FfiBool,
    /// When non-zero, attaches [`InteractionDisabled`] onto the tab list.
    pub disabled: FfiBool,
}

impl FfiTabList {
    /// [`ControlOrientation::Horizontal`] (`0`, default for [`TabList`]).
    pub const ORIENTATION_HORIZONTAL: u8 = 0;
    /// [`ControlOrientation::Vertical`] (`1`).
    pub const ORIENTATION_VERTICAL: u8 = 1;

    /// [`TabActivation::Manual`] (`0`).
    pub const ACTIVATION_MANUAL: u8 = 0;
    /// [`TabActivation::Automatic`] (`1`).
    pub const ACTIVATION_AUTOMATIC: u8 = 1;

    /// Validates and converts into a Bevy [`TabList`].
    pub fn try_to_bevy(self) -> Result<TabList, FfiResult> {
        let orientation = match self.orientation {
            Self::ORIENTATION_HORIZONTAL => ControlOrientation::Horizontal,
            Self::ORIENTATION_VERTICAL => ControlOrientation::Vertical,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        let activation = match self.activation {
            Self::ACTIVATION_MANUAL => TabActivation::Manual,
            Self::ACTIVATION_AUTOMATIC => TabActivation::Automatic,
            _ => return Err(FfiResult::INVALID_ARGUMENT),
        };
        Ok(TabList {
            orientation,
            activation,
        })
    }
}

impl Default for FfiTabList {
    fn default() -> Self {
        Self {
            orientation: Self::ORIENTATION_HORIZONTAL,
            activation: Self::ACTIVATION_MANUAL,
            self_update: FfiBool::FALSE,
            disabled: FfiBool::FALSE,
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// headless [`Tab`] header component.
///
/// Layout: `4` bytes, `1`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiTab")]
pub struct FfiTab {
    /// When non-zero, attaches [`InteractionDisabled`] onto this tab.
    pub disabled: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor bidirectionally synchronized
/// with Bevy `0.20`'s [`SelectedTab`].
///
/// Layout: `8` bytes, `8`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiSelectedTab")]
pub struct FfiSelectedTab {
    /// Currently selected tab entity (`FfiEntity::NULL` represents `None`).
    pub selected: FfiEntity,
}

impl FfiSelectedTab {
    /// Constructs a [`FfiSelectedTab`] from an `Option<Entity>`.
    #[inline]
    pub const fn from_option(entity: Option<Entity>) -> Self {
        Self {
            selected: option_entity_to_ffi(entity),
        }
    }

    /// Converts this descriptor into an `Option<Entity>`.
    #[inline]
    pub const fn to_option(self) -> Option<Entity> {
        ffi_entity_to_option(self.selected)
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` [`EntityEvent`] payload translating Bevy
/// `0.20` [`ValueChange<T>`] notifications (`Entity`, `Option<Entity>`, `f32`, `bool`, plus
/// `f64`, `i32`, `i64`, and `Color` when `bridge-feathers` is enabled) for C-ABI guest observers.
///
/// Layout: `56` bytes, `8`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiValueChange")]
pub struct FfiValueChange {
    /// Source widget entity that emitted this [`ValueChange`] (`EntityEvent::event_target`).
    pub source: FfiEntity,
    /// Entity value for `KIND_ENTITY` and `KIND_OPTIONAL_ENTITY` (`FfiEntity::NULL` = `None`).
    pub entity_value: FfiEntity,
    /// Floating-point payload (`f64`, also populated for `KIND_F32`).
    pub f64_value: f64,
    /// Signed 64-bit integer payload (`i64`, also populated for `KIND_I32`).
    pub i64_value: i64,
    /// sRGBA color payload for `KIND_COLOR`.
    pub color_value: FfiRgba,
    /// 32-bit floating-point payload for `KIND_F32`.
    pub f32_value: f32,
    /// Discriminant tag identifying the active payload variant (`KIND_*`).
    pub kind: u8,
    /// Boolean payload for `KIND_BOOL`.
    pub bool_value: FfiBool,
    /// Mirrors [`ValueChange::is_final`].
    pub is_final: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

impl FfiValueChange {
    /// Payload carries `entity_value` (`ValueChange<Entity>`, `0`).
    pub const KIND_ENTITY: u8 = 0;
    /// Payload carries optional `entity_value` (`ValueChange<Option<Entity>>`, `1`).
    pub const KIND_OPTIONAL_ENTITY: u8 = 1;
    /// Payload carries `f32_value` (`ValueChange<f32>`, `2`).
    pub const KIND_F32: u8 = 2;
    /// Payload carries `bool_value` (`ValueChange<bool>`, `3`).
    pub const KIND_BOOL: u8 = 3;
    /// Payload carries `f64_value` (`ValueChange<f64>`, `4`).
    pub const KIND_F64: u8 = 4;
    /// Payload carries `i64_value as i32` (`ValueChange<i32>`, `5`).
    pub const KIND_I32: u8 = 5;
    /// Payload carries `i64_value` (`ValueChange<i64>`, `6`).
    pub const KIND_I64: u8 = 6;
    /// Payload carries `color_value` (`ValueChange<Color>`, `7`).
    pub const KIND_COLOR: u8 = 7;

    /// Constructs a [`FfiValueChange`] from [`ValueChange<Entity>`].
    #[inline]
    pub const fn from_entity(source: Entity, value: Entity, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: entity_to_ffi(value),
            f64_value: 0.0,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: 0.0,
            kind: Self::KIND_ENTITY,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<Option<Entity>>`].
    #[inline]
    pub const fn from_optional_entity(
        source: Entity,
        value: Option<Entity>,
        is_final: bool,
    ) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: option_entity_to_ffi(value),
            f64_value: 0.0,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: 0.0,
            kind: Self::KIND_OPTIONAL_ENTITY,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<f32>`].
    #[inline]
    pub const fn from_f32(source: Entity, value: f32, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: value as f64,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: value,
            kind: Self::KIND_F32,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<bool>`].
    #[inline]
    pub const fn from_bool(source: Entity, value: bool, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: 0.0,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: 0.0,
            kind: Self::KIND_BOOL,
            bool_value: FfiBool::from_bool(value),
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<f64>`].
    #[inline]
    pub const fn from_f64(source: Entity, value: f64, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: value,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: value as f32,
            kind: Self::KIND_F64,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<i32>`].
    #[inline]
    pub const fn from_i32(source: Entity, value: i32, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: value as f64,
            i64_value: value as i64,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: value as f32,
            kind: Self::KIND_I32,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<i64>`].
    #[inline]
    pub const fn from_i64(source: Entity, value: i64, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: value as f64,
            i64_value: value,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: value as f32,
            kind: Self::KIND_I64,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }

    /// Constructs a [`FfiValueChange`] from [`ValueChange<Color>`].
    #[inline]
    pub fn from_color(source: Entity, color: Color, is_final: bool) -> Self {
        Self {
            source: entity_to_ffi(source),
            entity_value: FfiEntity::NULL,
            f64_value: 0.0,
            i64_value: 0,
            color_value: FfiRgba::from_bevy(color),
            f32_value: 0.0,
            kind: Self::KIND_COLOR,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::from_bool(is_final),
            reserved0: 0,
        }
    }
}

impl Default for FfiValueChange {
    fn default() -> Self {
        Self {
            source: FfiEntity::NULL,
            entity_value: FfiEntity::NULL,
            f64_value: 0.0,
            i64_value: 0,
            color_value: FfiRgba::TRANSPARENT,
            f32_value: 0.0,
            kind: Self::KIND_ENTITY,
            bool_value: FfiBool::FALSE,
            is_final: FfiBool::TRUE,
            reserved0: 0,
        }
    }
}

impl Event for FfiValueChange {
    type Trigger<'a> = EntityTrigger;
}

impl EntityEvent for FfiValueChange {
    #[inline]
    fn event_target(&self) -> Entity {
        ffi_entity_to_option(self.source).unwrap_or(Entity::PLACEHOLDER)
    }
}

// ============================================================================
// Optional `bevy_feathers` Descriptors & Contextual Theming (`bridge-feathers`)
// ============================================================================

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor mapping Bevy `0.20`
/// `bevy_feathers::theme::ThemeContext` (`SurfaceLevel::{Base, Higher, Highest, Floating}`).
///
/// Layout: `4` bytes, `1`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiThemeContext")]
pub struct FfiThemeContext {
    /// Surface elevation discriminant (`0 = Base`, `1 = Higher`, `2 = Highest`, `3 = Floating`).
    pub surface_level: u8,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 3],
}

#[cfg(feature = "bridge-feathers")]
impl FfiThemeContext {
    /// [`SurfaceLevel::Base`] (`0`) — window and pane header.
    pub const BASE: Self = Self {
        surface_level: 0,
        reserved0: [0; 3],
    };
    /// [`SurfaceLevel::Higher`] (`1`) — pane and subpane bodies.
    pub const HIGHER: Self = Self {
        surface_level: 1,
        reserved0: [0; 3],
    };
    /// [`SurfaceLevel::Highest`] (`2`) — groups and subpane headers.
    pub const HIGHEST: Self = Self {
        surface_level: 2,
        reserved0: [0; 3],
    };
    /// [`SurfaceLevel::Floating`] (`3`) — overlays such as dialogs and menus.
    pub const FLOATING: Self = Self {
        surface_level: 3,
        reserved0: [0; 3],
    };

    /// Projects a [`SurfaceLevel`] into [`FfiThemeContext`].
    #[inline]
    pub const fn from_surface_level(level: SurfaceLevel) -> Self {
        match level {
            SurfaceLevel::Base => Self::BASE,
            SurfaceLevel::Higher => Self::HIGHER,
            SurfaceLevel::Highest => Self::HIGHEST,
            SurfaceLevel::Floating => Self::FLOATING,
        }
    }

    /// Projects a Bevy [`ThemeContext`] into [`FfiThemeContext`].
    #[inline]
    pub const fn from_bevy(ctx: &ThemeContext) -> Self {
        Self::from_surface_level(ctx.0)
    }

    /// Validates and decodes into a Bevy [`SurfaceLevel`].
    #[inline]
    pub fn try_to_surface_level(self) -> Result<SurfaceLevel, FfiResult> {
        if self.reserved0 != [0; 3] {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        match self.surface_level {
            0 => Ok(SurfaceLevel::Base),
            1 => Ok(SurfaceLevel::Higher),
            2 => Ok(SurfaceLevel::Highest),
            3 => Ok(SurfaceLevel::Floating),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    /// Validates and decodes into a Bevy [`ThemeContext`].
    #[inline]
    pub fn try_to_bevy(self) -> Result<ThemeContext, FfiResult> {
        self.try_to_surface_level().map(ThemeContext)
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// `bevy_feathers::controls::FeathersColorInput`, [`ColorInputValue`], and optional
/// [`ColorInputSettings`] color-editing mode.
///
/// Layout: `24` bytes, `4`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFeathersColorInput")]
pub struct FfiFeathersColorInput {
    /// Current sRGBA color synchronized with [`ColorInputValue`].
    pub color: FfiRgba,
    /// Fraction/percentage of the swatch showing the opaque color preview
    /// (`FeathersColorInputProps::opaque_color_percentage`).
    pub opaque_color_percentage: f32,
    /// Color picker editing mode (`0 = Wheel`, `1 = RGPlane`, `2 = HSPlane`).
    pub mode: u8,
    /// When non-zero, also applies `mode` onto the global [`ColorInputSettings`] resource.
    pub apply_mode_to_settings: FfiBool,
    /// When non-zero, attaches [`InteractionDisabled`].
    pub disabled: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

#[cfg(feature = "bridge-feathers")]
impl FfiFeathersColorInput {
    /// [`ColorInputMode::Wheel`] (`0`).
    pub const MODE_WHEEL: u8 = 0;
    /// [`ColorInputMode::RGPlane`] (`1`).
    pub const MODE_RG_PLANE: u8 = 1;
    /// [`ColorInputMode::HSPlane`] (`2`).
    pub const MODE_HS_PLANE: u8 = 2;

    /// Validates and decodes [`ColorInputMode`].
    pub fn try_mode(self) -> Result<ColorInputMode, FfiResult> {
        match self.mode {
            Self::MODE_WHEEL => Ok(ColorInputMode::Wheel),
            Self::MODE_RG_PLANE => Ok(ColorInputMode::RGPlane),
            Self::MODE_HS_PLANE => Ok(ColorInputMode::HSPlane),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

#[cfg(feature = "bridge-feathers")]
impl Default for FfiFeathersColorInput {
    fn default() -> Self {
        Self {
            color: FfiRgba::WHITE,
            opaque_color_percentage: 0.0,
            mode: Self::MODE_WHEEL,
            apply_mode_to_settings: FfiBool::FALSE,
            disabled: FfiBool::FALSE,
            reserved0: 0,
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// Feathers dropdown select control (`bevy_feathers::controls::FeathersSelect` + [`OptionIndex`]).
///
/// Layout: `24` bytes, `8`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFeathersDropdown")]
pub struct FfiFeathersDropdown {
    /// Selected option row entity (`FfiEntity::NULL` = none). When non-null on change, triggers
    /// [`SetSelected`] targeting this dropdown.
    pub selected_row: FfiEntity,
    /// Selected option index (synchronized with [`OptionIndex`] when `has_selected_index` is set).
    pub selected_index: u32,
    /// Maximum visible rows before scrolling (`FeathersSelectProps::max_visible`, default `8`).
    pub max_visible: u32,
    /// Whether `selected_index` is engaged (`0 = None`, `1 = Some`).
    pub has_selected_index: FfiBool,
    /// Rounded-corner preset matching [`RoundedCorners`] (`0 = None`, `1 = All`, ..., `10 = Left`).
    pub rounded_corners: u8,
    /// When non-zero, attaches [`InteractionDisabled`].
    pub disabled: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 5],
}

/// Alias for [`FfiFeathersDropdown`] matching Bevy `0.20`'s `FeathersSelect` naming.
#[cfg(feature = "bridge-feathers")]
pub type FfiFeathersSelect = FfiFeathersDropdown;

#[cfg(feature = "bridge-feathers")]
impl FfiFeathersDropdown {
    /// [`RoundedCorners::None`] (`0`).
    pub const CORNERS_NONE: u8 = 0;
    /// [`RoundedCorners::All`] (`1`, default).
    pub const CORNERS_ALL: u8 = 1;
    /// [`RoundedCorners::TopLeft`] (`2`).
    pub const CORNERS_TOP_LEFT: u8 = 2;
    /// [`RoundedCorners::TopRight`] (`3`).
    pub const CORNERS_TOP_RIGHT: u8 = 3;
    /// [`RoundedCorners::BottomRight`] (`4`).
    pub const CORNERS_BOTTOM_RIGHT: u8 = 4;
    /// [`RoundedCorners::BottomLeft`] (`5`).
    pub const CORNERS_BOTTOM_LEFT: u8 = 5;
    /// [`RoundedCorners::Top`] (`6`).
    pub const CORNERS_TOP: u8 = 6;
    /// [`RoundedCorners::Right`] (`7`).
    pub const CORNERS_RIGHT: u8 = 7;
    /// [`RoundedCorners::Bottom`] (`8`).
    pub const CORNERS_BOTTOM: u8 = 8;
    /// [`RoundedCorners::Left`] (`9`).
    pub const CORNERS_LEFT: u8 = 9;

    /// Validates and decodes [`RoundedCorners`].
    pub fn try_rounded_corners(self) -> Result<RoundedCorners, FfiResult> {
        match self.rounded_corners {
            Self::CORNERS_NONE => Ok(RoundedCorners::None),
            Self::CORNERS_ALL => Ok(RoundedCorners::All),
            Self::CORNERS_TOP_LEFT => Ok(RoundedCorners::TopLeft),
            Self::CORNERS_TOP_RIGHT => Ok(RoundedCorners::TopRight),
            Self::CORNERS_BOTTOM_RIGHT => Ok(RoundedCorners::BottomRight),
            Self::CORNERS_BOTTOM_LEFT => Ok(RoundedCorners::BottomLeft),
            Self::CORNERS_TOP => Ok(RoundedCorners::Top),
            Self::CORNERS_RIGHT => Ok(RoundedCorners::Right),
            Self::CORNERS_BOTTOM => Ok(RoundedCorners::Bottom),
            Self::CORNERS_LEFT => Ok(RoundedCorners::Left),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

#[cfg(feature = "bridge-feathers")]
impl Default for FfiFeathersDropdown {
    fn default() -> Self {
        Self {
            selected_row: FfiEntity::NULL,
            selected_index: 0,
            max_visible: 8,
            has_selected_index: FfiBool::FALSE,
            rounded_corners: Self::CORNERS_ALL,
            disabled: FfiBool::FALSE,
            reserved0: [0; 5],
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// scrollable list control (`bevy_feathers::controls::FeathersListView`, [`ScrollbarGutter`],
/// [`ListBox`], and [`ScrollArea`]).
///
/// Layout: `12` bytes, `4`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFeathersScrollableList")]
pub struct FfiFeathersScrollableList {
    /// Scrollbar gutter inset ([`ScrollbarGutter`], default `Val::Px(14.0)`).
    pub scrollbar_gutter: FfiVal,
    /// Scroll orientation (`0 = Horizontal`, `1 = Vertical`, default `Vertical`).
    pub orientation: u8,
    /// When non-zero, attaches headless [`ListBox`] selection management.
    pub selectable: FfiBool,
    /// When non-zero, attaches headless [`ScrollArea`] scrolling behavior.
    pub scrollable: FfiBool,
    /// When non-zero, attaches [`InteractionDisabled`].
    pub disabled: FfiBool,
}

/// Alias for [`FfiFeathersScrollableList`] matching Bevy `0.20`'s `FeathersListView` naming.
#[cfg(feature = "bridge-feathers")]
pub type FfiFeathersListView = FfiFeathersScrollableList;

#[cfg(feature = "bridge-feathers")]
impl FfiFeathersScrollableList {
    /// [`ControlOrientation::Horizontal`] (`0`).
    pub const ORIENTATION_HORIZONTAL: u8 = 0;
    /// [`ControlOrientation::Vertical`] (`1`, default).
    pub const ORIENTATION_VERTICAL: u8 = 1;
}

#[cfg(feature = "bridge-feathers")]
impl Default for FfiFeathersScrollableList {
    fn default() -> Self {
        Self {
            scrollbar_gutter: FfiVal::px(14.0),
            orientation: Self::ORIENTATION_VERTICAL,
            selectable: FfiBool::TRUE,
            scrollable: FfiBool::TRUE,
            disabled: FfiBool::FALSE,
        }
    }
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// [`FeathersListRow`], `bevy_ui_widgets::ListItem`, optional [`OptionIndex`], and [`Selected`]
/// state.
///
/// Layout: `8` bytes, `4`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFeathersListRow")]
pub struct FfiFeathersListRow {
    /// Option index synchronized with [`OptionIndex`] when `has_option_index` is non-zero.
    pub option_index: u32,
    /// Whether `option_index` should be attached as [`OptionIndex`].
    pub has_option_index: FfiBool,
    /// When non-zero, attaches [`Selected`]; when zero, removes [`Selected`].
    pub selected: FfiBool,
    /// When non-zero, attaches [`InteractionDisabled`].
    pub disabled: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

/// Fixed-layout `#[repr(C)]` `AbiPod` + `ModComponent` descriptor synchronized with Bevy `0.20`'s
/// scrubbable [`FeathersNumberInput`], [`NumberInputValue`], [`SoftLimit`], [`HardLimit`],
/// [`NumberInputPrecision`], [`NumberInputStep`], and [`NumberInputWrap`].
///
/// Layout: `72` bytes, `8`-byte alignment, zero implicit padding.
#[cfg(feature = "bridge-feathers")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod_ui::FfiFeathersNumberInput")]
pub struct FfiFeathersNumberInput {
    /// Current floating-point value for `FORMAT_F32` / `FORMAT_F64`.
    pub value_f64: f64,
    /// Current integer value for `FORMAT_I32` / `FORMAT_I64`.
    pub value_i64: i64,
    /// Soft scrub-drag lower bound ([`SoftLimit`]).
    pub soft_min: f64,
    /// Soft scrub-drag upper bound ([`SoftLimit`]).
    pub soft_max: f64,
    /// Hard clamp/wrap lower bound ([`HardLimit`]).
    pub hard_min: f64,
    /// Hard clamp/wrap upper bound ([`HardLimit`]).
    pub hard_max: f64,
    /// Increment/decrement and scrub heuristic step size ([`NumberInputStep`]).
    pub step: f64,
    /// Decimal precision during scrubbing ([`NumberInputPrecision`], default `2`).
    pub precision: i32,
    /// Numeric format (`0 = F32`, `1 = F64`, `2 = I32`, `3 = I64`).
    pub format: u8,
    /// When non-zero, attaches [`SoftLimit`] defining the scrub-drag range.
    pub has_soft_limit: FfiBool,
    /// When non-zero, attaches [`HardLimit`] defining the hard clamp/wrap range.
    pub has_hard_limit: FfiBool,
    /// When non-zero, sets [`NumberInputWrap::Wrap`]; otherwise [`NumberInputWrap::NoWrap`].
    pub wrap: FfiBool,
    /// When non-zero, attaches [`InteractionDisabled`].
    pub disabled: FfiBool,
    /// Explicit zero-reserved alignment bytes.
    pub reserved0: [u8; 7],
}

#[cfg(feature = "bridge-feathers")]
impl FfiFeathersNumberInput {
    /// `f32` numeric format (`0`).
    pub const FORMAT_F32: u8 = 0;
    /// `f64` numeric format (`1`).
    pub const FORMAT_F64: u8 = 1;
    /// `i32` numeric format (`2`).
    pub const FORMAT_I32: u8 = 2;
    /// `i64` numeric format (`3`).
    pub const FORMAT_I64: u8 = 3;

    /// Constructs an `f32` scrubbable number-input descriptor.
    pub fn f32(value: f32) -> Self {
        Self {
            value_f64: value as f64,
            format: Self::FORMAT_F32,
            ..Self::default()
        }
    }

    /// Constructs an `f64` scrubbable number-input descriptor.
    pub fn f64(value: f64) -> Self {
        Self {
            value_f64: value,
            format: Self::FORMAT_F64,
            ..Self::default()
        }
    }

    /// Constructs an `i32` scrubbable number-input descriptor.
    pub fn i32(value: i32) -> Self {
        Self {
            value_i64: value as i64,
            format: Self::FORMAT_I32,
            precision: 0,
            ..Self::default()
        }
    }

    /// Constructs an `i64` scrubbable number-input descriptor.
    pub fn i64(value: i64) -> Self {
        Self {
            value_i64: value,
            format: Self::FORMAT_I64,
            precision: 0,
            ..Self::default()
        }
    }

    /// Configures a scrub-drag [`SoftLimit`] range.
    pub const fn with_soft_limit(mut self, min: f64, max: f64) -> Self {
        self.soft_min = min;
        self.soft_max = max;
        self.has_soft_limit = FfiBool::TRUE;
        self
    }

    /// Configures a hard clamp/wrap [`HardLimit`] range.
    pub const fn with_hard_limit(mut self, min: f64, max: f64) -> Self {
        self.hard_min = min;
        self.hard_max = max;
        self.has_hard_limit = FfiBool::TRUE;
        self
    }

    /// Validates and decodes [`NumberInputValue`].
    pub fn try_value(self) -> Result<NumberInputValue, FfiResult> {
        if self.reserved0 != [0; 7] || !self.step.is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        match self.format {
            Self::FORMAT_F32 => {
                if !self.value_f64.is_finite() {
                    return Err(FfiResult::INVALID_ARGUMENT);
                }
                Ok(NumberInputValue::F32(self.value_f64 as f32))
            }
            Self::FORMAT_F64 => {
                if !self.value_f64.is_finite() {
                    return Err(FfiResult::INVALID_ARGUMENT);
                }
                Ok(NumberInputValue::F64(self.value_f64))
            }
            Self::FORMAT_I32 => Ok(NumberInputValue::I32(
                self.value_i64.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            )),
            Self::FORMAT_I64 => Ok(NumberInputValue::I64(self.value_i64)),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }

    fn try_range(self, min: f64, max: f64) -> Result<NumberInputRange, FfiResult> {
        if !min.is_finite() || !max.is_finite() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
        match self.format {
            Self::FORMAT_F32 => Ok(NumberInputRange::F32((lo as f32)..=(hi as f32))),
            Self::FORMAT_F64 => Ok(NumberInputRange::F64(lo..=hi)),
            Self::FORMAT_I32 => Ok(NumberInputRange::I32(
                (lo.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32)
                    ..=(hi.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32),
            )),
            Self::FORMAT_I64 => Ok(NumberInputRange::I64(
                (lo.round() as i64)..=(hi.round() as i64),
            )),
            _ => Err(FfiResult::INVALID_ARGUMENT),
        }
    }
}

#[cfg(feature = "bridge-feathers")]
impl Default for FfiFeathersNumberInput {
    fn default() -> Self {
        Self {
            value_f64: 0.0,
            value_i64: 0,
            soft_min: 0.0,
            soft_max: 0.0,
            hard_min: 0.0,
            hard_max: 0.0,
            step: 1.0,
            precision: 2,
            format: Self::FORMAT_F32,
            has_soft_limit: FfiBool::FALSE,
            has_hard_limit: FfiBool::FALSE,
            wrap: FfiBool::FALSE,
            disabled: FfiBool::FALSE,
            reserved0: [0; 7],
        }
    }
}

// ============================================================================
// Retained-UI Ownership Tracking & Mod-Unload Cleanup (`ModUiRegistry`)
// ============================================================================

/// Host [`Resource`] tracking mod-owned retained UI root hierarchies and UI observer entities so
/// [`ModWorldExt::unload_mod`] tears down every retained UI node and observer cleanly with zero
/// dangling entities or focus references.
#[derive(Resource, Default, Debug, Clone)]
pub struct ModUiRegistry {
    roots: HashMap<ModId, HashSet<Entity>>,
    observers: HashMap<ModId, HashSet<Entity>>,
}

impl ModUiRegistry {
    /// Creates an empty [`ModUiRegistry`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `entity` as a tracked retained-UI root/node owned by `mod_id`.
    pub fn register_ui_root(&mut self, mod_id: ModId, entity: Entity) {
        self.roots.entry(mod_id).or_default().insert(entity);
    }

    /// Registers `observer` as a tracked UI observer entity owned by `mod_id`.
    pub fn register_ui_observer(&mut self, mod_id: ModId, observer: Entity) {
        self.observers.entry(mod_id).or_default().insert(observer);
    }

    /// Unregisters `entity` from `mod_id`'s tracked UI root set.
    pub fn unregister_ui_root(&mut self, mod_id: ModId, entity: Entity) -> bool {
        self.roots
            .get_mut(&mod_id)
            .is_some_and(|set| set.remove(&entity))
    }

    /// Returns the number of tracked UI roots/entities currently registered for `mod_id`.
    pub fn tracked_ui_root_count(&self, mod_id: ModId) -> usize {
        self.roots.get(&mod_id).map_or(0, HashSet::len)
    }

    /// Returns the number of tracked UI observers currently registered for `mod_id`.
    pub fn tracked_ui_observer_count(&self, mod_id: ModId) -> usize {
        self.observers.get(&mod_id).map_or(0, HashSet::len)
    }
}

/// Extension methods on [`World`] for tracking and cleaning up mod-owned retained UI hierarchies.
pub trait ModUiWorldExt {
    /// Registers `entity` as a retained-UI root owned by `mod_id`, attaching [`ModScoped`] if
    /// absent so mod unload despawns the entire hierarchy deterministically.
    fn register_mod_ui_root(&mut self, mod_id: ModId, entity: Entity);

    /// Registers a UI observer entity owned by `mod_id` for deterministic teardown on unload.
    fn register_mod_ui_observer(&mut self, mod_id: ModId, observer: Entity);

    /// Performs immediate retained-UI cleanup for `mod_id`, returning the number of entities
    /// despawned by this hook.
    fn cleanup_mod_ui(&mut self, mod_id: ModId) -> usize;
}

impl ModUiWorldExt for World {
    fn register_mod_ui_root(&mut self, mod_id: ModId, entity: Entity) {
        if !self.contains_resource::<ModUiRegistry>() {
            self.insert_resource(ModUiRegistry::new());
        }
        if let Ok(mut ent) = self.get_entity_mut(entity)
            && !ent.contains::<ModScoped>()
        {
            ent.insert(ModScoped(mod_id));
        }
        self.resource_mut::<ModUiRegistry>()
            .register_ui_root(mod_id, entity);
    }

    fn register_mod_ui_observer(&mut self, mod_id: ModId, observer: Entity) {
        if !self.contains_resource::<ModUiRegistry>() {
            self.insert_resource(ModUiRegistry::new());
        }
        self.resource_mut::<ModUiRegistry>()
            .register_ui_observer(mod_id, observer);
    }

    fn cleanup_mod_ui(&mut self, mod_id: ModId) -> usize {
        cleanup_mod_ui_in_world(self, mod_id)
    }
}

/// Extension methods on [`App`] mirroring [`ModUiWorldExt`].
pub trait ModUiAppExt {
    /// Registers `entity` as a retained-UI root owned by `mod_id`.
    fn register_mod_ui_root(&mut self, mod_id: ModId, entity: Entity);

    /// Registers a UI observer entity owned by `mod_id`.
    fn register_mod_ui_observer(&mut self, mod_id: ModId, observer: Entity);
}

impl ModUiAppExt for App {
    #[inline]
    fn register_mod_ui_root(&mut self, mod_id: ModId, entity: Entity) {
        self.world_mut().register_mod_ui_root(mod_id, entity);
    }

    #[inline]
    fn register_mod_ui_observer(&mut self, mod_id: ModId, observer: Entity) {
        self.world_mut().register_mod_ui_observer(mod_id, observer);
    }
}

/// Deterministic retained-UI cleanup hook invoked automatically via `ModUnloadHooks` whenever
/// `mod_id` is unloaded.
///
/// Despawns every tracked UI observer, tracked UI root/descendant, and any remaining
/// [`ModScoped`] UI entity belonging to `mod_id` (including [`Disabled`] entities), and clears
/// [`InputFocus`] if it was targeting a torn-down entity.
pub fn cleanup_mod_ui_in_world(world: &mut World, mod_id: ModId) -> usize {
    let mut despawned = 0usize;

    let (tracked_observers, tracked_roots) =
        if let Some(mut ui_reg) = world.get_resource_mut::<ModUiRegistry>() {
            (
                ui_reg.observers.remove(&mod_id).unwrap_or_default(),
                ui_reg.roots.remove(&mod_id).unwrap_or_default(),
            )
        } else {
            (HashSet::new(), HashSet::new())
        };

    // 1. Despawn tracked UI observers first so they cannot fire during hierarchy teardown.
    for obs in tracked_observers {
        if let Ok(ent) = world.get_entity_mut(obs) {
            ent.despawn();
            despawned += 1;
        }
    }

    // 2. Collect any remaining `ModScoped(mod_id)` entities (including `Disabled` ones) alongside
    //    tracked roots and recursively despawn all still-alive entities and descendants.
    let mut to_despawn: Vec<Entity> = tracked_roots.into_iter().collect();
    {
        let mut scoped_q = world.query_filtered::<(Entity, &ModScoped), Allow<Disabled>>();
        for (entity, scoped) in scoped_q.iter(world) {
            if scoped.0 == mod_id {
                to_despawn.push(entity);
            }
        }
    }

    // Also collect explicit descendants before despawning so even partially-unlinked hierarchies
    // leave zero orphaned children.
    let mut expanded: Vec<Entity> = Vec::with_capacity(to_despawn.len());
    let mut seen: HashSet<Entity> = HashSet::new();
    let mut stack: Vec<Entity> = to_despawn;
    while let Some(entity) = stack.pop() {
        if !seen.insert(entity) {
            continue;
        }
        if let Some(children) = world.get::<Children>(entity) {
            for child in children.iter() {
                stack.push(*child);
            }
        }
        expanded.push(entity);
    }

    for entity in expanded {
        if let Ok(ent) = world.get_entity_mut(entity) {
            ent.despawn();
            despawned += 1;
        }
    }

    // 3. Clear `InputFocus` if the focused widget no longer exists after teardown.
    let stale_focus = world
        .get_resource::<InputFocus>()
        .and_then(InputFocus::get)
        .is_some_and(|focused| world.get_entity(focused).is_err());
    if stale_focus && let Some(mut focus) = world.get_resource_mut::<InputFocus>() {
        focus.clear();
    }

    despawned
}

fn cleanup_mod_ui_hook(world: &mut World, mod_id: ModId) {
    let _ = cleanup_mod_ui_in_world(world, mod_id);
}

// ============================================================================
// Native Bevy UI `HostWireBridge` / `HostStringBridge` Projections
// ============================================================================

struct UiTextBridge;
impl HostStringBridge<Text> for UiTextBridge {
    const CANONICAL_NAME: &'static str = CANONICAL_UI_TEXT_NAME;
    const ALIAS: Option<&'static str> = Some("Text");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn as_str(value: &Text) -> &str {
        value.0.as_str()
    }

    #[inline]
    fn set_str(value: &mut Text, text: &str) -> FfiResult {
        value.0.clear();
        value.0.push_str(text);
        FfiResult::OK
    }
}

struct TextSpanBridge;
impl HostStringBridge<TextSpan> for TextSpanBridge {
    const CANONICAL_NAME: &'static str = CANONICAL_TEXT_SPAN_NAME;
    const ALIAS: Option<&'static str> = Some("TextSpan");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn as_str(value: &TextSpan) -> &str {
        value.0.as_str()
    }

    #[inline]
    fn set_str(value: &mut TextSpan, text: &str) -> FfiResult {
        value.0.clear();
        value.0.push_str(text);
        FfiResult::OK
    }
}

struct BackgroundColorBridge;
impl HostWireBridge<BackgroundColor> for BackgroundColorBridge {
    type Wire = FfiRgba;
    const CANONICAL_NAME: &'static str = CANONICAL_BACKGROUND_COLOR_NAME;
    const ALIAS: Option<&'static str> = Some("BackgroundColor");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &BackgroundColor) -> FfiRgba {
        FfiRgba::from_bevy(value.0)
    }

    #[inline]
    fn apply_wire(value: &mut BackgroundColor, wire: FfiRgba) -> FfiResult {
        match wire.try_to_bevy() {
            Ok(color) => {
                value.0 = color;
                FfiResult::OK
            }
            Err(err) => err,
        }
    }
}

struct BorderColorBridge;
impl HostWireBridge<BorderColor> for BorderColorBridge {
    type Wire = FfiBorderColor;
    const CANONICAL_NAME: &'static str = CANONICAL_BORDER_COLOR_NAME;
    const ALIAS: Option<&'static str> = Some("BorderColor");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &BorderColor) -> FfiBorderColor {
        FfiBorderColor::from_bevy(value)
    }

    #[inline]
    fn apply_wire(value: &mut BorderColor, wire: FfiBorderColor) -> FfiResult {
        match wire.try_to_bevy() {
            Ok(bc) => {
                *value = bc;
                FfiResult::OK
            }
            Err(err) => err,
        }
    }
}

struct RemSizeBridge;
impl HostWireBridge<RemSize> for RemSizeBridge {
    type Wire = FfiRemSize;
    const CANONICAL_NAME: &'static str = CANONICAL_REM_SIZE_NAME;
    const ALIAS: Option<&'static str> = Some("RemSize");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &RemSize) -> FfiRemSize {
        FfiRemSize(value.0)
    }

    #[inline]
    fn apply_wire(value: &mut RemSize, wire: FfiRemSize) -> FfiResult {
        if !wire.0.is_finite() || wire.0 <= 0.0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        value.0 = wire.0;
        FfiResult::OK
    }
}

struct EmSizeBridge;
impl HostWireBridge<EmSize> for EmSizeBridge {
    type Wire = FfiEmSize;
    const CANONICAL_NAME: &'static str = CANONICAL_EM_SIZE_NAME;
    const ALIAS: Option<&'static str> = Some("EmSize");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &EmSize) -> FfiEmSize {
        FfiEmSize(value.0)
    }

    #[inline]
    fn apply_wire(value: &mut EmSize, wire: FfiEmSize) -> FfiResult {
        if !wire.0.is_finite() || wire.0 <= 0.0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        value.0 = wire.0;
        FfiResult::OK
    }
}

#[cfg(feature = "bridge-feathers")]
struct ThemeContextBridge;

#[cfg(feature = "bridge-feathers")]
impl HostWireBridge<ThemeContext> for ThemeContextBridge {
    type Wire = FfiThemeContext;
    const CANONICAL_NAME: &'static str = CANONICAL_THEME_CONTEXT_NAME;
    const ALIAS: Option<&'static str> = Some("ThemeContext");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &ThemeContext) -> FfiThemeContext {
        FfiThemeContext::from_bevy(value)
    }

    #[inline]
    fn apply_wire(value: &mut ThemeContext, wire: FfiThemeContext) -> FfiResult {
        match wire.try_to_bevy() {
            Ok(ctx) => {
                *value = ctx;
                FfiResult::OK
            }
            Err(err) => err,
        }
    }
}

// ============================================================================
// Registration & Plugin
// ============================================================================

/// Registers all Bevy `0.20` UI, text, headless widget, and (when `bridge-feathers` is enabled)
/// Feathers widget descriptors, wire/string bridges, entity events, and unload hooks in `world`.
pub fn register_ui_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<ModUiRegistry>() {
        world.insert_resource(ModUiRegistry::new());
    }
    if !world.contains_resource::<RemSize>() {
        world.insert_resource(RemSize::default());
    }
    #[cfg(feature = "bridge-feathers")]
    {
        ensure_feathers_runtime_resources(world);
    }

    register_mod_unload_hook_in_world(world, UI_UNLOAD_HOOK_KEY, cleanup_mod_ui_hook);

    // Helper wire sub-types exposed for proc-address / reflection lookup.
    world.register_mod_type::<FfiVal>();
    world.register_mod_type::<FfiUiRect>();
    world.register_mod_type::<FfiCornerRadius>();
    world.register_mod_type::<FfiRgba>();
    world.register_mod_type::<FfiBorderColor>();
    world.register_mod_type::<FfiFontSize>();

    // Descriptor components (`RawInsertable` + `Default`).
    world.register_mod_component::<FfiUiNode>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiBorderRadius>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiUiTextStyle>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiEditableTextConfig>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiInlineBox>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiInlineImage>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiTabList>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiTab>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    world.register_mod_component::<FfiSelectedTab>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;

    #[cfg(feature = "bridge-feathers")]
    {
        world.register_mod_component::<FfiThemeContext>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
        world.register_mod_component::<FfiFeathersColorInput>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
        world.register_mod_component::<FfiFeathersDropdown>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
        world.register_mod_component::<FfiFeathersScrollableList>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
        world.register_mod_component::<FfiFeathersListRow>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
        world.register_mod_component::<FfiFeathersNumberInput>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
    }

    // Entity events (`FfiValueChange` + `bevy_ui_widgets::Activate`).
    world.register_mod_entity_event::<FfiValueChange>()?;

    let text_id = world.register_component::<Text>();
    let text_span_id = world.register_component::<TextSpan>();
    let bg_id = world.register_component::<BackgroundColor>();
    let border_color_id = world.register_component::<BorderColor>();
    let rem_id = world.register_component::<RemSize>();
    let em_id = world.register_component::<EmSize>();
    #[cfg(feature = "bridge-feathers")]
    let theme_ctx_id = world.register_component::<ThemeContext>();

    world.resource_scope(
        |world,
         mut reg: bevy_ecs::prelude::Mut<'_, ModRegistry>|
         -> Result<(), ComponentCollisionError> {
            reg.register_bridged_string_component::<Text, UiTextBridge>(
                text_id,
                0,
                Some(ModRegistry::default_inserter_for::<Text>()),
            )?;
            reg.register_bridged_string_component::<TextSpan, TextSpanBridge>(
                text_span_id,
                0,
                Some(ModRegistry::default_inserter_for::<TextSpan>()),
            )?;
            reg.register_bridged_component::<BackgroundColor, BackgroundColorBridge>(
                bg_id,
                0,
                Some(ModRegistry::default_inserter_for::<BackgroundColor>()),
            )?;
            reg.register_bridged_component::<BorderColor, BorderColorBridge>(
                border_color_id,
                0,
                Some(ModRegistry::default_inserter_for::<BorderColor>()),
            )?;
            reg.register_bridged_resource::<RemSize, RemSizeBridge>(rem_id)?;
            reg.register_bridged_component::<EmSize, EmSizeBridge>(
                em_id,
                0,
                Some(ModRegistry::default_inserter_for::<EmSize>()),
            )?;
            #[cfg(feature = "bridge-feathers")]
            reg.register_bridged_component::<ThemeContext, ThemeContextBridge>(
                theme_ctx_id,
                0,
                Some(ModRegistry::default_inserter_for::<ThemeContext>()),
            )?;
            if reg.get_by_name(CANONICAL_ACTIVATE_EVENT_NAME).is_none() {
                reg.register_bridged_entity_event_with::<Activate>(
                    world,
                    CANONICAL_ACTIVATE_EVENT_NAME,
                    Some("Activate"),
                    |w, entity| {
                        w.trigger(Activate { entity });
                    },
                )?;
            }
            Ok(())
        },
    )?;

    Ok(())
}

/// Safe (`#![forbid(unsafe_code)]`), headless-safe Bevy [`Plugin`] bridging Bevy `0.20` retained
/// UI (`Val::Em`/`Val::Rem`, [`FixedNode`], elliptical [`BorderRadius`]/[`CornerRadius`]), text
/// (`FontSize::Rem`, [`InlineBox`], [`InlineImage`], split [`EditableText`] + [`TextInput`]),
/// headless widgets ([`TabList`], [`Tab`], [`SelectedTab`], [`ValueChange`]), optional
/// `bevy_feathers` controls + `ThemeContext`, and deterministic `unload_mod` UI teardown.
#[derive(Default, Debug, Clone, Copy)]
pub struct ModUiBridgePlugin;

impl Plugin for ModUiBridgePlugin {
    fn build(&self, app: &mut App) {
        if let Err(err) = register_ui_bridges(app.world_mut()) {
            log_ecs_error(
                err.into(),
                ErrorContext::Command {
                    name: "ModUiBridgePlugin::build".into(),
                },
            );
        }

        if !app.is_plugin_added::<TabPlugin>() {
            app.add_plugins(TabPlugin);
        }

        // Order descriptor sync before `UiSystems::Prepare` and `MenuFocusSystem` (which precedes
        // `TabPlugin`'s `update_tablist_derived_state`), and mirror host-driven state afterward.
        app.configure_sets(
            PostUpdate,
            (
                ModUiBridgeSet::Sync
                    .before(UiSystems::Prepare)
                    .before(MenuFocusSystem),
                ModUiBridgeSet::Mirror
                    .after(ModUiBridgeSet::Sync)
                    .after(InputFocusSystems::FocusChangeEvents),
                ModUiBridgeSet::Track.after(ModUiBridgeSet::Sync),
            ),
        );

        app.add_systems(
            PostUpdate,
            (
                sync_ffi_ui_nodes,
                sync_ffi_border_radius.after(sync_ffi_ui_nodes),
                sync_ffi_text_styles,
                sync_ffi_editable_text,
                sync_ffi_inline_images,
                sync_ffi_inline_boxes.after(sync_ffi_inline_images),
                sync_ffi_tablists,
                sync_ffi_tabs,
                sync_ffi_selected_tabs.after(sync_ffi_tablists),
            )
                .in_set(ModUiBridgeSet::Sync),
        )
        .add_systems(
            PostUpdate,
            mirror_selected_tabs_to_ffi.in_set(ModUiBridgeSet::Mirror),
        )
        .add_systems(
            PostUpdate,
            track_mod_scoped_ui_entities.in_set(ModUiBridgeSet::Track),
        )
        .add_observer(on_tablist_ffi_self_update)
        .add_observer(bridge_value_change_entity)
        .add_observer(bridge_value_change_optional_entity)
        .add_observer(bridge_value_change_f32)
        .add_observer(bridge_value_change_bool);

        #[cfg(feature = "bridge-feathers")]
        {
            app.add_systems(
                PostUpdate,
                (
                    sync_ffi_theme_context,
                    sync_ffi_feathers_color_input,
                    sync_ffi_feathers_dropdown,
                    sync_ffi_feathers_scrollable_list,
                    sync_ffi_feathers_list_row,
                    sync_ffi_feathers_number_input,
                )
                    .in_set(ModUiBridgeSet::Sync),
            )
            .add_systems(
                PostUpdate,
                (
                    mirror_theme_context_to_ffi,
                    mirror_color_input_value_to_ffi,
                    mirror_number_input_value_to_ffi,
                )
                    .in_set(ModUiBridgeSet::Mirror),
            )
            .add_observer(bridge_value_change_f64)
            .add_observer(bridge_value_change_i32)
            .add_observer(bridge_value_change_i64)
            .add_observer(bridge_value_change_color);
        }
    }

    fn finish(&self, app: &mut App) {
        #[cfg(feature = "bridge-feathers")]
        if app.world().contains_resource::<AssetServer>() {
            if !app.world().contains_resource::<Assets<Font>>() {
                app.init_asset::<Font>();
            }
            if !app.world().contains_resource::<Assets<Image>>() {
                app.init_asset::<Image>();
            }
            if !app.world().contains_resource::<Assets<ScenePatch>>() {
                app.init_asset::<ScenePatch>();
            }
        }
        #[cfg(not(feature = "bridge-feathers"))]
        let _ = app;
    }
}

// ============================================================================
// Sync & Mirror Systems
// ============================================================================

#[inline]
const fn entity_to_ffi(entity: Entity) -> FfiEntity {
    FfiEntity::from_bits(entity.to_bits())
}

#[inline]
const fn option_entity_to_ffi(entity: Option<Entity>) -> FfiEntity {
    match entity {
        Some(e) => entity_to_ffi(e),
        None => FfiEntity::NULL,
    }
}

#[inline]
const fn ffi_entity_to_option(entity: FfiEntity) -> Option<Entity> {
    if entity.is_null() {
        None
    } else {
        Entity::try_from_bits(entity.to_bits())
    }
}

fn resolve_ffi_asset_handle<A: Asset>(
    handle: &FfiAssetHandle,
    asset_server: Option<&AssetServer>,
    assets: Option<&mut Assets<A>>,
) -> Option<Handle<A>> {
    if !handle.validate().is_ok() {
        return None;
    }
    match handle.kind {
        FfiAssetHandle::KIND_UUID => Some(Handle::Uuid(
            Uuid::from_u128(handle.uuid.to_u128()),
            PhantomData,
        )),
        FfiAssetHandle::KIND_INDEX => {
            let bits = handle.to_index_bits()?;
            let id = AssetId::<A>::Index {
                index: AssetIndex::from_bits(bits),
                marker: PhantomData,
            };
            if let Some(server) = asset_server
                && let Some(strong) = server.get_id_handle(id)
            {
                return Some(strong);
            }
            assets.and_then(|store| store.get_strong_handle(id))
        }
        _ => None,
    }
}

fn sync_disabled_marker(commands: &mut Commands, entity: Entity, desired: bool, has_now: bool) {
    if desired && !has_now {
        commands.entity(entity).insert(InteractionDisabled);
    } else if !desired && has_now {
        commands.entity(entity).remove::<InteractionDisabled>();
    }
}

fn sync_ffi_ui_nodes(
    mut q: Query<(Entity, &FfiUiNode, Option<&mut Node>, Has<FixedNode>), Changed<FfiUiNode>>,
    mut commands: Commands,
) {
    for (entity, desc, node_opt, has_fixed) in &mut q {
        match node_opt {
            Some(mut node) => {
                let mut updated = node.clone();
                if desc.apply_to_node(&mut updated).is_ok() {
                    node.set_if_neq(updated);
                }
            }
            None => {
                let mut node = Node::DEFAULT;
                if desc.apply_to_node(&mut node).is_ok() {
                    commands.entity(entity).insert(node);
                }
            }
        }
        let want_fixed = desc.fixed_node.get();
        if want_fixed && !has_fixed {
            commands.entity(entity).insert(FixedNode);
        } else if !want_fixed && has_fixed {
            commands.entity(entity).remove::<FixedNode>();
        }
    }
}

fn sync_ffi_border_radius(
    mut q: Query<(Entity, &FfiBorderRadius, Option<&mut Node>), Changed<FfiBorderRadius>>,
    mut commands: Commands,
) {
    for (entity, desc, node_opt) in &mut q {
        let Ok(border_radius) = desc.try_to_bevy() else {
            continue;
        };
        match node_opt {
            Some(mut node) => {
                if node.border_radius != border_radius {
                    node.border_radius = border_radius;
                }
            }
            None => {
                commands.entity(entity).insert(Node {
                    border_radius,
                    ..Node::DEFAULT
                });
            }
        }
    }
}

type TextStyleSyncItem<'a> = (
    Entity,
    &'a FfiUiTextStyle,
    Option<&'a mut TextFont>,
    Option<&'a mut TextColor>,
    Option<&'a mut TextLayout>,
    Has<Text>,
);

fn sync_ffi_text_styles(
    mut q: Query<TextStyleSyncItem<'_>, Changed<FfiUiTextStyle>>,
    asset_server: Option<Res<AssetServer>>,
    mut fonts: Option<ResMut<Assets<Font>>>,
    mut commands: Commands,
) {
    for (entity, desc, font_opt, color_opt, layout_opt, has_text) in &mut q {
        if desc.reserved0 != 0 {
            continue;
        }
        let (Ok(font_size), Ok(color), Ok(justify), Ok(linebreak), Ok(smoothing), Ok(style)) = (
            desc.font_size.try_to_bevy(),
            desc.color.try_to_bevy(),
            desc.try_justify(),
            desc.try_linebreak(),
            desc.try_font_smoothing(),
            desc.try_font_style(),
        ) else {
            continue;
        };

        let resolved_font = if desc.font_handle.is_some() {
            resolve_ffi_asset_handle::<Font>(
                &desc.font_handle,
                asset_server.as_deref(),
                fonts.as_deref_mut(),
            )
        } else {
            None
        };

        let weight = FontWeight(desc.weight.clamp(1, 1000));
        match font_opt {
            Some(mut tf) => {
                let mut updated = tf.clone();
                if let Some(handle) = resolved_font {
                    updated.font = FontSource::Handle(handle);
                }
                updated.font_size = font_size;
                updated.weight = weight;
                updated.font_smoothing = smoothing;
                updated.style = style;
                tf.set_if_neq(updated);
            }
            None => {
                let mut tf = TextFont {
                    font_size,
                    weight,
                    font_smoothing: smoothing,
                    style,
                    ..TextFont::default()
                };
                if let Some(handle) = resolved_font {
                    tf.font = FontSource::Handle(handle);
                }
                commands.entity(entity).insert(tf);
            }
        }

        match color_opt {
            Some(mut tc) => {
                tc.set_if_neq(TextColor(color));
            }
            None => {
                commands.entity(entity).insert(TextColor(color));
            }
        }

        let desired_layout = TextLayout::new(justify, linebreak);
        match layout_opt {
            Some(mut tl) => {
                if tl.justify != desired_layout.justify || tl.linebreak != desired_layout.linebreak
                {
                    *tl = desired_layout;
                }
            }
            None => {
                commands.entity(entity).insert(desired_layout);
            }
        }

        if desc.ensure_text.get() && !has_text {
            commands.entity(entity).insert(Text::default());
        }
    }
}

type EditableTextSyncItem<'a> = (
    Entity,
    &'a FfiEditableTextConfig,
    Option<&'a mut EditableText>,
    Option<&'a mut TextReadWriteMode>,
    Has<TextInput>,
);

fn sync_ffi_editable_text(
    mut q: Query<EditableTextSyncItem<'_>, Changed<FfiEditableTextConfig>>,
    mut commands: Commands,
) {
    for (entity, desc, editable_opt, rw_opt, has_text_input) in &mut q {
        let Ok(rw_mode) = desc.try_read_write_mode() else {
            continue;
        };
        let want_interactive = desc.interactive.get();

        match editable_opt {
            Some(mut editable) => {
                if desc.apply_to_editable_text(&mut editable).is_err() {
                    continue;
                }
                match rw_opt {
                    Some(mut rw) => {
                        rw.set_if_neq(rw_mode);
                    }
                    None => {
                        commands.entity(entity).insert(rw_mode);
                    }
                }
                if want_interactive && !has_text_input {
                    commands.entity(entity).insert(TextInput);
                } else if !want_interactive && has_text_input {
                    commands.entity(entity).remove::<TextInput>();
                }
            }
            None => {
                let mut editable = EditableText::default();
                if desc.apply_to_editable_text(&mut editable).is_err() {
                    continue;
                }
                if want_interactive {
                    commands
                        .entity(entity)
                        .insert((TextInput, editable, rw_mode));
                } else {
                    commands.entity(entity).insert((editable, rw_mode));
                    if has_text_input {
                        commands.entity(entity).remove::<TextInput>();
                    }
                }
            }
        }
    }
}

fn sync_ffi_inline_images(
    mut q: Query<(Entity, &FfiInlineImage, Option<&mut InlineImage>), Changed<FfiInlineImage>>,
    asset_server: Option<Res<AssetServer>>,
    mut images: Option<ResMut<Assets<Image>>>,
    mut commands: Commands,
) {
    for (entity, desc, inline_opt) in &mut q {
        let Ok(color) = desc.color.try_to_bevy() else {
            continue;
        };
        if (desc.has_width.get() && !desc.width.is_finite())
            || (desc.has_height.get() && !desc.height.is_finite())
        {
            continue;
        }
        let image_handle = resolve_ffi_asset_handle::<Image>(
            &desc.image,
            asset_server.as_deref(),
            images.as_deref_mut(),
        )
        .unwrap_or(TRANSPARENT_IMAGE_HANDLE);

        let desired = InlineImage {
            color,
            image: image_handle,
            width: desc.has_width.get().then_some(desc.width),
            height: desc.has_height.get().then_some(desc.height),
            flip_x: desc.flip_x.get(),
            flip_y: desc.flip_y.get(),
        };

        match inline_opt {
            Some(mut img) => {
                img.set_if_neq(desired);
            }
            None => {
                commands.entity(entity).insert(desired);
            }
        }
    }
}

fn sync_ffi_inline_boxes(
    mut q: Query<(Entity, &FfiInlineBox, Option<&mut InlineBox>), Changed<FfiInlineBox>>,
    mut commands: Commands,
) {
    for (entity, desc, box_opt) in &mut q {
        let Ok(desired) = desc.try_to_bevy() else {
            continue;
        };
        match box_opt {
            Some(mut inline_box) => {
                inline_box.set_if_neq(desired);
            }
            None => {
                commands.entity(entity).insert(desired);
            }
        }
    }
}

fn sync_ffi_tablists(
    mut q: Query<
        (
            Entity,
            &FfiTabList,
            Option<&mut TabList>,
            Has<InteractionDisabled>,
        ),
        Changed<FfiTabList>,
    >,
    mut commands: Commands,
) {
    for (entity, desc, tablist_opt, has_disabled) in &mut q {
        let Ok(desired) = desc.try_to_bevy() else {
            continue;
        };
        match tablist_opt {
            Some(mut tablist) => {
                tablist.set_if_neq(desired);
            }
            None => {
                commands.entity(entity).insert(desired);
            }
        }
        sync_disabled_marker(&mut commands, entity, desc.disabled.get(), has_disabled);
    }
}

fn sync_ffi_tabs(
    q: Query<(Entity, &FfiTab, Has<Tab>, Has<InteractionDisabled>), Changed<FfiTab>>,
    mut commands: Commands,
) {
    for (entity, desc, has_tab, has_disabled) in &q {
        if desc.reserved0 != [0; 3] {
            continue;
        }
        if !has_tab {
            commands.entity(entity).insert(Tab);
        }
        sync_disabled_marker(&mut commands, entity, desc.disabled.get(), has_disabled);
    }
}

fn sync_ffi_selected_tabs(
    mut q: Query<(Entity, &FfiSelectedTab, Option<&mut SelectedTab>), Changed<FfiSelectedTab>>,
    mut commands: Commands,
) {
    for (entity, desc, selected_opt) in &mut q {
        let desired = SelectedTab(desc.to_option());
        match selected_opt {
            Some(mut sel) => {
                sel.set_if_neq(desired);
            }
            None => {
                commands.entity(entity).insert(desired);
            }
        }
    }
}

fn mirror_selected_tabs_to_ffi(
    mut q: Query<(&SelectedTab, &mut FfiSelectedTab), Changed<SelectedTab>>,
) {
    for (sel, mut ffi) in &mut q {
        let desired = FfiSelectedTab::from_option(sel.0);
        if *ffi != desired {
            *ffi.bypass_change_detection() = desired;
        }
    }
}

#[cfg(not(feature = "bridge-feathers"))]
type TrackedUiFilter = (
    Added<ModScoped>,
    Or<(
        With<FfiUiNode>,
        With<Node>,
        With<FfiBorderRadius>,
        With<FfiUiTextStyle>,
        With<FfiEditableTextConfig>,
        With<FfiInlineBox>,
        With<FfiInlineImage>,
        With<FfiTabList>,
        With<FfiTab>,
        With<TabList>,
    )>,
);

#[cfg(feature = "bridge-feathers")]
type TrackedUiFilter = (
    Added<ModScoped>,
    Or<(
        Or<(
            With<FfiUiNode>,
            With<Node>,
            With<FfiBorderRadius>,
            With<FfiUiTextStyle>,
            With<FfiEditableTextConfig>,
            With<FfiInlineBox>,
            With<FfiInlineImage>,
            With<FfiTabList>,
            With<FfiTab>,
            With<TabList>,
        )>,
        Or<(
            With<FfiThemeContext>,
            With<FfiFeathersColorInput>,
            With<FfiFeathersDropdown>,
            With<FfiFeathersScrollableList>,
            With<FfiFeathersListRow>,
            With<FfiFeathersNumberInput>,
        )>,
    )>,
);

fn track_mod_scoped_ui_entities(
    q: Query<(Entity, &ModScoped), TrackedUiFilter>,
    mut registry: ResMut<ModUiRegistry>,
) {
    for (entity, scoped) in &q {
        registry.register_ui_root(scoped.0, entity);
    }
}

// ============================================================================
// Headless Widget Observers (`TabList` self-update + `ValueChange<T>` Bridge)
// ============================================================================

fn on_tablist_ffi_self_update(
    change: On<ValueChange<Option<Entity>>>,
    q_tablists: Query<&FfiTabList, With<TabList>>,
    mut commands: Commands,
) {
    if let Ok(ffi_tablist) = q_tablists.get(change.source)
        && ffi_tablist.self_update.get()
    {
        commands
            .entity(change.source)
            .insert(SelectedTab(change.value));
    }
}

fn bridge_value_change_entity(change: On<ValueChange<Entity>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_entity(
        change.source,
        change.value,
        change.is_final,
    ));
}

fn bridge_value_change_optional_entity(
    change: On<ValueChange<Option<Entity>>>,
    mut commands: Commands,
) {
    commands.trigger(FfiValueChange::from_optional_entity(
        change.source,
        change.value,
        change.is_final,
    ));
}

fn bridge_value_change_f32(change: On<ValueChange<f32>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_f32(
        change.source,
        change.value,
        change.is_final,
    ));
}

fn bridge_value_change_bool(change: On<ValueChange<bool>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_bool(
        change.source,
        change.value,
        change.is_final,
    ));
}

// ============================================================================
// Optional `bevy_feathers` Sync, Mirror & Observer Functions
// ============================================================================

#[cfg(feature = "bridge-feathers")]
const FEATHERS_SELECT_ROW_PX: f32 = 28.0;

/// Internal marker recording that a Feathers `SceneComponent` BSN hierarchy has already been
/// hydrated onto an FFI widget entity so subsequent descriptor edits update state/props in place
/// without duplicating child hierarchies.
#[cfg(feature = "bridge-feathers")]
#[derive(Component, Debug, Default, Clone, Copy)]
struct FeathersSceneHydrated;

#[cfg(feature = "bridge-feathers")]
fn ensure_registered_asset<A: Asset>(world: &mut World, server: &AssetServer) {
    if !world.contains_resource::<Assets<A>>() {
        let assets = Assets::<A>::default();
        server.register_asset(&assets);
        world.insert_resource(assets);
    }
    world.init_resource::<Messages<AssetEvent<A>>>();
    world.init_resource::<Messages<AssetLoadFailedEvent<A>>>();
}

#[cfg(feature = "bridge-feathers")]
fn ensure_feathers_runtime_resources(world: &mut World) {
    if !world.contains_resource::<UiTheme>() {
        world.insert_resource(UiTheme::default());
    }
    if !world.contains_resource::<ColorInputSettings>() {
        world.insert_resource(ColorInputSettings::default());
    }
    if !world.contains_resource::<InputFocus>() {
        world.insert_resource(InputFocus::default());
    }
    if let Some(server) = world.get_resource::<AssetServer>().cloned() {
        ensure_registered_asset::<Font>(world, &server);
        ensure_registered_asset::<Image>(world, &server);
        ensure_registered_asset::<ScenePatch>(world, &server);
    } else if !world.contains_resource::<Assets<ScenePatch>>() {
        world.insert_resource(Assets::<ScenePatch>::default());
    }
    if !world.contains_resource::<UnitsRegistry>() {
        let mut units = UnitsRegistry::default();
        units.insert(&Dimensionless);
        units.insert(&LengthMeters);
        units.insert(&TimeSeconds);
        units.insert(&AngleDegrees);
        world.insert_resource(units);
    }
}

#[cfg(feature = "bridge-feathers")]
fn find_descendant_with<C: Component>(world: &World, root: Entity) -> Option<Entity> {
    let mut queue = std::collections::VecDeque::new();
    if let Some(children) = world.get::<Children>(root) {
        for &child in children.iter() {
            queue.push_back(child);
        }
    }
    while let Some(current) = queue.pop_front() {
        if world.get::<C>(current).is_some() {
            return Some(current);
        }
        if let Some(children) = world.get::<Children>(current) {
            for &child in children.iter() {
                queue.push_back(child);
            }
        }
    }
    None
}

#[cfg(feature = "bridge-feathers")]
fn hydrate_feathers_scene<S: Scene>(
    world: &mut World,
    entity: Entity,
    scene: S,
    reparent_preexisting_into_scroll_area: bool,
) -> bool {
    if world.get_entity(entity).is_err() {
        return false;
    }
    if world.get::<FeathersSceneHydrated>(entity).is_some() {
        return true;
    }
    ensure_feathers_runtime_resources(world);
    let Some(server) = world.get_resource::<AssetServer>().cloned() else {
        return false;
    };

    // Capture any pre-existing children (such as mod-spawned `FfiFeathersListRow` entities
    // attached via `ChildOf`) before `ResolvedSceneRoot::apply` replaces `Children`.
    let preexisting_children: Vec<Entity> = if reparent_preexisting_into_scroll_area {
        world
            .get::<Children>(entity)
            .map(|children| children.iter().copied().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let resolved = {
        let Some(patches) = world.get_resource::<Assets<ScenePatch>>() else {
            return false;
        };
        match ResolvedSceneRoot::resolve(Box::new(scene), &server, patches) {
            Ok(root) => root,
            Err(err) => {
                log_ecs_error(
                    err.into(),
                    ErrorContext::Command {
                        name: "ModUiBridgePlugin::hydrate_feathers_scene".into(),
                    },
                );
                return false;
            }
        }
    };

    let mut scratch = BundleScratch::default();
    let apply_result = {
        let Ok(mut ent_mut) = world.get_entity_mut(entity) else {
            return false;
        };
        resolved.apply(&mut ent_mut, &mut scratch)
    };
    if let Err(err) = apply_result {
        log_ecs_error(
            err.into(),
            ErrorContext::Command {
                name: "ModUiBridgePlugin::hydrate_feathers_scene".into(),
            },
        );
        return false;
    }

    if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
        ent_mut.insert(FeathersSceneHydrated);
    }

    if !preexisting_children.is_empty() {
        let target_parent = find_descendant_with::<ScrollArea>(world, entity).unwrap_or(entity);
        for child in preexisting_children {
            if world.get_entity(child).is_ok()
                && child != target_parent
                && let Ok(mut child_mut) = world.get_entity_mut(child)
            {
                child_mut.insert(ChildOf(target_parent));
            }
        }
    }

    true
}

#[cfg(feature = "bridge-feathers")]
fn apply_color_input_opaque_percentage(
    world: &mut World,
    root: Entity,
    opaque_color_percentage: f32,
    color: Color,
) {
    let Some(swatch_ent) = find_descendant_with::<FeathersColorSwatch>(world, root) else {
        return;
    };
    let children: Vec<Entity> = world
        .get::<Children>(swatch_ent)
        .map(|c| c.iter().copied().collect())
        .unwrap_or_default();
    let clamped = opaque_color_percentage.clamp(0.0, 100.0);
    if clamped > 0.0 {
        let target_left = Val::Percent(100.0 - clamped);
        let opaque_color = Color::from(color.to_srgba().with_alpha(1.0));
        if let Some(&opaque_child) = children.get(1) {
            if let Some(mut node) = world.get_mut::<Node>(opaque_child)
                && node.left != target_left
            {
                node.left = target_left;
            }
            if let Some(mut bg) = world.get_mut::<BackgroundColor>(opaque_child)
                && bg.0 != opaque_color
            {
                bg.0 = opaque_color;
            }
        } else {
            world.spawn((
                ChildOf(swatch_ent),
                Node {
                    position_type: PositionType::Absolute,
                    left: target_left,
                    top: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    right: Val::Px(0.0),
                    border_radius: RoundedCorners::Right.to_border_radius(5.0),
                    ..Default::default()
                },
                ColorSwatchFg,
                BackgroundColor(opaque_color),
                ZIndex(1),
            ));
        }
    } else if let Some(&opaque_child) = children.get(1) {
        let _ = world.despawn(opaque_child);
    }
}

#[cfg(feature = "bridge-feathers")]
fn apply_dropdown_visual_props(
    world: &mut World,
    root: Entity,
    corners: RoundedCorners,
    max_visible: usize,
) {
    if let Some(button_ent) = find_descendant_with::<FeathersMenuButton>(world, root)
        && let Some(mut node) = world.get_mut::<Node>(button_ent)
    {
        let desired_radius = corners.to_border_radius(4.0);
        if node.border_radius != desired_radius {
            node.border_radius = desired_radius;
        }
    }
    if let Some(list_ent) = find_descendant_with::<FeathersListView>(world, root)
        && let Some(mut node) = world.get_mut::<Node>(list_ent)
    {
        let desired_max_height = Val::Px(max_visible.max(1) as f32 * FEATHERS_SELECT_ROW_PX);
        if node.max_height != desired_max_height {
            node.max_height = desired_max_height;
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn apply_listview_layout_props(
    world: &mut World,
    root: Entity,
    orientation: ControlOrientation,
    gutter_val: Val,
    selectable: bool,
    scrollable: bool,
    disabled: bool,
) {
    if world.get_entity(root).is_err() {
        return;
    }

    if let Some(mut gutter) = world.get_mut::<ScrollbarGutter>(root) {
        if gutter.0 != gutter_val {
            gutter.0 = gutter_val;
        }
    } else if let Ok(mut ent_mut) = world.get_entity_mut(root) {
        ent_mut.insert(ScrollbarGutter(gutter_val));
    }

    if let Some(mut outer_node) = world.get_mut::<Node>(root) {
        let desired_padding = if scrollable {
            match orientation {
                ControlOrientation::Vertical => UiRect {
                    right: gutter_val,
                    ..Default::default()
                },
                ControlOrientation::Horizontal => UiRect {
                    bottom: gutter_val,
                    ..Default::default()
                },
            }
        } else {
            UiRect::ZERO
        };
        if outer_node.padding != desired_padding {
            outer_node.padding = desired_padding;
        }
    }

    let has_listbox = world.get::<ListBox>(root).is_some();
    if selectable && !has_listbox {
        if let Ok(mut ent_mut) = world.get_entity_mut(root) {
            ent_mut.insert(ListBox);
        }
    } else if !selectable
        && has_listbox
        && let Ok(mut ent_mut) = world.get_entity_mut(root)
    {
        ent_mut.remove::<ListBox>();
    }

    let has_disabled = world.get::<InteractionDisabled>(root).is_some();
    if disabled && !has_disabled {
        if let Ok(mut ent_mut) = world.get_entity_mut(root) {
            ent_mut.insert(InteractionDisabled);
        }
    } else if !disabled
        && has_disabled
        && let Ok(mut ent_mut) = world.get_entity_mut(root)
    {
        ent_mut.remove::<InteractionDisabled>();
    }

    let direct_children: Vec<Entity> = world
        .get::<Children>(root)
        .map(|c| c.iter().copied().collect())
        .unwrap_or_default();

    let inner_ent = direct_children
        .iter()
        .copied()
        .find(|&c| world.get::<Name>(c).is_some_and(|n| n.as_str() == "inner"))
        .or_else(|| {
            direct_children
                .iter()
                .copied()
                .find(|&c| world.get::<FeathersScrollbar>(c).is_none())
        });

    if let Some(inner) = inner_ent {
        if let Some(mut inner_node) = world.get_mut::<Node>(inner) {
            let desired_dir = match orientation {
                ControlOrientation::Horizontal => FlexDirection::Row,
                ControlOrientation::Vertical => FlexDirection::Column,
            };
            let desired_overflow = if scrollable {
                match orientation {
                    ControlOrientation::Horizontal => Overflow::scroll_x(),
                    ControlOrientation::Vertical => Overflow::scroll_y(),
                }
            } else {
                Overflow::clip()
            };
            if inner_node.flex_direction != desired_dir {
                inner_node.flex_direction = desired_dir;
            }
            if inner_node.overflow != desired_overflow {
                inner_node.overflow = desired_overflow;
            }
        }
        let has_scroll_area = world.get::<ScrollArea>(inner).is_some();
        if scrollable && !has_scroll_area {
            if let Ok(mut ent_mut) = world.get_entity_mut(inner) {
                ent_mut.insert(ScrollArea);
            }
        } else if !scrollable
            && has_scroll_area
            && let Ok(mut ent_mut) = world.get_entity_mut(inner)
        {
            ent_mut.remove::<ScrollArea>();
        }
    }

    let scrollbar_ent = direct_children
        .into_iter()
        .find(|&c| world.get::<FeathersScrollbar>(c).is_some());

    if let Some(sb_ent) = scrollbar_ent {
        if let Some(mut sb) = world.get_mut::<Scrollbar>(sb_ent)
            && sb.orientation != orientation
        {
            sb.orientation = orientation;
        }
        if let Some(mut sb_node) = world.get_mut::<Node>(sb_ent) {
            sb_node.display = if scrollable {
                Display::Flex
            } else {
                Display::None
            };
            sb_node.position_type = PositionType::Absolute;
            match orientation {
                ControlOrientation::Vertical => {
                    sb_node.left = Val::Auto;
                    sb_node.right = Val::Px(4.0);
                    sb_node.top = Val::Px(0.0);
                    sb_node.bottom = Val::Px(0.0);
                    sb_node.width = Val::Px(6.0);
                    sb_node.height = Val::Auto;
                }
                ControlOrientation::Horizontal => {
                    sb_node.left = Val::Px(0.0);
                    sb_node.right = Val::Px(0.0);
                    sb_node.top = Val::Auto;
                    sb_node.bottom = Val::Px(4.0);
                    sb_node.width = Val::Auto;
                    sb_node.height = Val::Px(6.0);
                }
            }
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_theme_context(
    mut q: Query<(Entity, &FfiThemeContext, Option<&mut ThemeContext>), Changed<FfiThemeContext>>,
    mut commands: Commands,
) {
    for (entity, desc, ctx_opt) in &mut q {
        let Ok(desired) = desc.try_to_bevy() else {
            continue;
        };
        match ctx_opt {
            Some(mut ctx) => {
                ctx.set_if_neq(desired);
            }
            None => {
                commands.entity(entity).insert(desired);
            }
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn mirror_theme_context_to_ffi(
    mut q: Query<(&ThemeContext, &mut FfiThemeContext), Changed<ThemeContext>>,
) {
    for (ctx, mut ffi) in &mut q {
        let desired = FfiThemeContext::from_bevy(ctx);
        if *ffi != desired {
            *ffi.bypass_change_detection() = desired;
        }
    }
}

#[cfg(feature = "bridge-feathers")]
type FeathersColorInputSyncItem<'a> = (
    Entity,
    Ref<'a, FfiFeathersColorInput>,
    Has<FeathersSceneHydrated>,
    Option<&'a ColorInputValue>,
    Has<InteractionDisabled>,
);

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_feathers_color_input(
    q: Query<
        FeathersColorInputSyncItem<'_>,
        Or<(
            Changed<FfiFeathersColorInput>,
            Without<FeathersSceneHydrated>,
        )>,
    >,
    asset_server: Option<Res<AssetServer>>,
    mut settings: Option<ResMut<ColorInputSettings>>,
    mut commands: Commands,
) {
    let has_asset_server = asset_server.is_some();
    for (entity, desc, is_hydrated, current_val, has_disabled) in &q {
        if !is_hydrated && !has_asset_server && !desc.is_changed() {
            continue;
        }
        if desc.reserved0 != 0 || !desc.opaque_color_percentage.is_finite() {
            continue;
        }
        let (Ok(color), Ok(mode)) = (desc.color.try_to_bevy(), desc.try_mode()) else {
            continue;
        };
        let opaque_color_percentage = desc.opaque_color_percentage;
        let disabled = desc.disabled.get();

        if !is_hydrated {
            if has_asset_server {
                commands.queue(move |world: &mut World| {
                    let props = FeathersColorInputProps {
                        opaque_color_percentage,
                    };
                    let scene = bsn! {
                        @FeathersColorInput {
                            @opaque_color_percentage: {props.opaque_color_percentage},
                        }
                    };
                    if hydrate_feathers_scene(world, entity, scene, false)
                        && let Ok(mut ent_mut) = world.get_entity_mut(entity)
                    {
                        ent_mut.insert(ColorInputValue(color));
                        if disabled {
                            ent_mut.insert(InteractionDisabled);
                        } else {
                            ent_mut.remove::<InteractionDisabled>();
                        }
                    }
                });
            }
        } else {
            if current_val.is_none_or(|v| v.0 != color) {
                // `ColorInputValue` is `#[component(immutable)]`, so replace via `insert`.
                commands.entity(entity).insert(ColorInputValue(color));
            }
            commands.queue(move |world: &mut World| {
                apply_color_input_opaque_percentage(world, entity, opaque_color_percentage, color);
            });
            sync_disabled_marker(&mut commands, entity, disabled, has_disabled);
        }

        if desc.apply_mode_to_settings.get()
            && let Some(settings) = settings.as_deref_mut()
            && settings.mode != mode
        {
            settings.mode = mode;
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn mirror_color_input_value_to_ffi(
    mut q: Query<(&ColorInputValue, &mut FfiFeathersColorInput), Changed<ColorInputValue>>,
) {
    for (val, mut ffi) in &mut q {
        let desired = FfiRgba::from_bevy(val.0);
        if ffi.color != desired {
            ffi.bypass_change_detection().color = desired;
        }
    }
}

#[cfg(feature = "bridge-feathers")]
type FeathersDropdownSyncItem<'a> = (
    Entity,
    Ref<'a, FfiFeathersDropdown>,
    Has<FeathersSceneHydrated>,
    Option<&'a OptionIndex>,
    Has<InteractionDisabled>,
);

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_feathers_dropdown(
    q: Query<
        FeathersDropdownSyncItem<'_>,
        Or<(Changed<FfiFeathersDropdown>, Without<FeathersSceneHydrated>)>,
    >,
    asset_server: Option<Res<AssetServer>>,
    mut commands: Commands,
) {
    let has_asset_server = asset_server.is_some();
    for (entity, desc, is_hydrated, current_idx, has_disabled) in &q {
        if !is_hydrated && !has_asset_server && !desc.is_changed() {
            continue;
        }
        if desc.reserved0 != [0; 5] {
            continue;
        }
        let Ok(corners) = desc.try_rounded_corners() else {
            continue;
        };
        let max_visible = (desc.max_visible as usize).max(1);
        let selected_index = desc
            .has_selected_index
            .get()
            .then_some(desc.selected_index as usize);
        let selected_row = ffi_entity_to_option(desc.selected_row);
        let disabled = desc.disabled.get();

        if !is_hydrated {
            if has_asset_server {
                commands.queue(move |world: &mut World| {
                    let props = FeathersSelectProps {
                        options: Box::new(bsn_list! {}),
                        corners,
                        max_visible,
                    };
                    let scene = bsn! {
                        @FeathersSelect {
                            @options: {props.options},
                            @corners: {props.corners},
                            @max_visible: {props.max_visible},
                        }
                    };
                    if hydrate_feathers_scene(world, entity, scene, true) {
                        if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
                            match selected_index {
                                Some(idx) => {
                                    ent_mut.insert(OptionIndex(idx));
                                }
                                None => {
                                    ent_mut.remove::<OptionIndex>();
                                }
                            }
                            if disabled {
                                ent_mut.insert(InteractionDisabled);
                            } else {
                                ent_mut.remove::<InteractionDisabled>();
                            }
                        }
                        if let Some(row) = selected_row {
                            world.trigger(SetSelected { entity, row });
                        }
                    }
                });
            }
        } else {
            commands.queue(move |world: &mut World| {
                apply_dropdown_visual_props(world, entity, corners, max_visible);
            });
            match selected_index {
                Some(desired_idx) => {
                    if current_idx.is_none_or(|i| i.0 != desired_idx) {
                        commands.entity(entity).insert(OptionIndex(desired_idx));
                    }
                }
                None if current_idx.is_some() => {
                    commands.entity(entity).remove::<OptionIndex>();
                }
                None => {}
            }
            if let Some(row) = selected_row {
                commands.trigger(SetSelected { entity, row });
            }
            sync_disabled_marker(&mut commands, entity, disabled, has_disabled);
        }
    }
}

#[cfg(feature = "bridge-feathers")]
type FeathersScrollableListSyncItem<'a> = (
    Entity,
    Ref<'a, FfiFeathersScrollableList>,
    Has<FeathersSceneHydrated>,
);

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_feathers_scrollable_list(
    q: Query<
        FeathersScrollableListSyncItem<'_>,
        Or<(
            Changed<FfiFeathersScrollableList>,
            Without<FeathersSceneHydrated>,
        )>,
    >,
    asset_server: Option<Res<AssetServer>>,
    mut commands: Commands,
) {
    let has_asset_server = asset_server.is_some();
    for (entity, desc, is_hydrated) in &q {
        if !is_hydrated && !has_asset_server && !desc.is_changed() {
            continue;
        }
        let orientation = match desc.orientation {
            FfiFeathersScrollableList::ORIENTATION_HORIZONTAL => ControlOrientation::Horizontal,
            FfiFeathersScrollableList::ORIENTATION_VERTICAL => ControlOrientation::Vertical,
            _ => continue,
        };
        let Ok(gutter_val) = desc.scrollbar_gutter.try_to_bevy() else {
            continue;
        };
        let selectable = desc.selectable.get();
        let scrollable = desc.scrollable.get();
        let disabled = desc.disabled.get();

        if !is_hydrated {
            if has_asset_server {
                commands.queue(move |world: &mut World| {
                    let props = FeathersListViewProps {
                        rows: Box::new(bsn_list! {}),
                    };
                    let scene = bsn! {
                        @FeathersListView {
                            @rows: {props.rows},
                        }
                    };
                    if hydrate_feathers_scene(world, entity, scene, true) {
                        apply_listview_layout_props(
                            world,
                            entity,
                            orientation,
                            gutter_val,
                            selectable,
                            scrollable,
                            disabled,
                        );
                    }
                });
            }
        } else {
            commands.queue(move |world: &mut World| {
                apply_listview_layout_props(
                    world,
                    entity,
                    orientation,
                    gutter_val,
                    selectable,
                    scrollable,
                    disabled,
                );
            });
        }
    }
}

#[cfg(feature = "bridge-feathers")]
type FeathersListRowSyncItem<'a> = (
    Entity,
    Ref<'a, FfiFeathersListRow>,
    Has<FeathersSceneHydrated>,
    Option<&'a OptionIndex>,
    Has<Selected>,
    Has<InteractionDisabled>,
);

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_feathers_list_row(
    q: Query<
        FeathersListRowSyncItem<'_>,
        Or<(Changed<FfiFeathersListRow>, Without<FeathersSceneHydrated>)>,
    >,
    asset_server: Option<Res<AssetServer>>,
    mut commands: Commands,
) {
    let has_asset_server = asset_server.is_some();
    for (entity, desc, is_hydrated, current_idx, has_selected, has_disabled) in &q {
        if !is_hydrated && !has_asset_server && !desc.is_changed() {
            continue;
        }
        if desc.reserved0 != 0 {
            continue;
        }
        let option_index = desc
            .has_option_index
            .get()
            .then_some(desc.option_index as usize);
        let want_selected = desc.selected.get();
        let disabled = desc.disabled.get();

        if !is_hydrated {
            if has_asset_server {
                commands.queue(move |world: &mut World| {
                    let scene = bsn! { @FeathersListRow };
                    if hydrate_feathers_scene(world, entity, scene, false)
                        && let Ok(mut ent_mut) = world.get_entity_mut(entity)
                    {
                        match option_index {
                            Some(idx) => {
                                ent_mut.insert(OptionIndex(idx));
                            }
                            None => {
                                ent_mut.remove::<OptionIndex>();
                            }
                        }
                        if want_selected {
                            ent_mut.insert(Selected);
                        } else {
                            ent_mut.remove::<Selected>();
                        }
                        if disabled {
                            ent_mut.insert(InteractionDisabled);
                        } else {
                            ent_mut.remove::<InteractionDisabled>();
                        }
                    }
                });
            }
        } else {
            match option_index {
                Some(desired_idx) => {
                    if current_idx.is_none_or(|i| i.0 != desired_idx) {
                        commands.entity(entity).insert(OptionIndex(desired_idx));
                    }
                }
                None if current_idx.is_some() => {
                    commands.entity(entity).remove::<OptionIndex>();
                }
                None => {}
            }
            if want_selected && !has_selected {
                commands.entity(entity).insert(Selected);
            } else if !want_selected && has_selected {
                commands.entity(entity).remove::<Selected>();
            }
            sync_disabled_marker(&mut commands, entity, disabled, has_disabled);
        }
    }
}

#[cfg(feature = "bridge-feathers")]
type FeathersNumberInputSyncItem<'a> = (
    Entity,
    Ref<'a, FfiFeathersNumberInput>,
    Has<FeathersSceneHydrated>,
    Option<&'a NumberInputValue>,
    Has<SoftLimit>,
    Has<HardLimit>,
    Has<InteractionDisabled>,
);

#[cfg(feature = "bridge-feathers")]
fn sync_ffi_feathers_number_input(
    q: Query<
        FeathersNumberInputSyncItem<'_>,
        Or<(
            Changed<FfiFeathersNumberInput>,
            Without<FeathersSceneHydrated>,
        )>,
    >,
    asset_server: Option<Res<AssetServer>>,
    mut commands: Commands,
) {
    let has_asset_server = asset_server.is_some();
    for (entity, desc, is_hydrated, current_val, has_soft, has_hard, has_disabled) in &q {
        if !is_hydrated && !has_asset_server && !desc.is_changed() {
            continue;
        }
        let Ok(value) = desc.try_value() else {
            continue;
        };
        let soft_limit = if desc.has_soft_limit.get() {
            let Ok(range) = desc.try_range(desc.soft_min, desc.soft_max) else {
                continue;
            };
            Some(SoftLimit(range))
        } else {
            None
        };
        let hard_limit = if desc.has_hard_limit.get() {
            let Ok(range) = desc.try_range(desc.hard_min, desc.hard_max) else {
                continue;
            };
            Some(HardLimit(range))
        } else {
            None
        };

        let wrap = if desc.wrap.get() {
            NumberInputWrap::Wrap
        } else {
            NumberInputWrap::NoWrap
        };
        let precision = NumberInputPrecision(desc.precision);
        let step = NumberInputStep(desc.step);
        let disabled = desc.disabled.get();

        if !is_hydrated {
            if has_asset_server {
                commands.queue(move |world: &mut World| {
                    let props = FeathersNumberInputProps::default();
                    let scene = bsn! {
                        @FeathersNumberInput {
                            @sigil_color: {props.sigil_color},
                            @label_text: {props.label_text},
                        }
                    };
                    if hydrate_feathers_scene(world, entity, scene, false)
                        && let Ok(mut ent_mut) = world.get_entity_mut(entity)
                    {
                        ent_mut.insert((precision, step, wrap));
                        match soft_limit {
                            Some(limit) => {
                                ent_mut.insert(limit);
                            }
                            None => {
                                ent_mut.remove::<SoftLimit>();
                            }
                        }
                        match hard_limit {
                            Some(limit) => {
                                ent_mut.insert(limit);
                            }
                            None => {
                                ent_mut.remove::<HardLimit>();
                            }
                        }
                        // Insert `NumberInputValue` after limits so `number_input_on_insert_value`
                        // formats and positions the slidebar using the active limits.
                        ent_mut.insert(value);
                        if disabled {
                            ent_mut.insert(InteractionDisabled);
                        } else {
                            ent_mut.remove::<InteractionDisabled>();
                        }
                    }
                });
            }
        } else {
            commands.entity(entity).insert((precision, step, wrap));
            match soft_limit {
                Some(limit) => {
                    commands.entity(entity).insert(limit);
                }
                None if has_soft => {
                    commands.entity(entity).remove::<SoftLimit>();
                }
                None => {}
            }

            match hard_limit {
                Some(limit) => {
                    commands.entity(entity).insert(limit);
                }
                None if has_hard => {
                    commands.entity(entity).remove::<HardLimit>();
                }
                None => {}
            }

            if current_val.is_none_or(|v| *v != value) {
                // `NumberInputValue` is `#[component(immutable)]`.
                commands.entity(entity).insert(value);
            }

            sync_disabled_marker(&mut commands, entity, disabled, has_disabled);
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn mirror_number_input_value_to_ffi(
    mut q: Query<(&NumberInputValue, &mut FfiFeathersNumberInput), Changed<NumberInputValue>>,
) {
    for (val, mut ffi) in &mut q {
        match *val {
            NumberInputValue::F32(v) => {
                let target = v as f64;
                if ffi.format != FfiFeathersNumberInput::FORMAT_F32
                    || (ffi.value_f64 as f32).to_bits() != v.to_bits()
                {
                    let raw = ffi.bypass_change_detection();
                    raw.format = FfiFeathersNumberInput::FORMAT_F32;
                    raw.value_f64 = target;
                }
            }
            NumberInputValue::F64(v) => {
                if ffi.format != FfiFeathersNumberInput::FORMAT_F64
                    || ffi.value_f64.to_bits() != v.to_bits()
                {
                    let raw = ffi.bypass_change_detection();
                    raw.format = FfiFeathersNumberInput::FORMAT_F64;
                    raw.value_f64 = v;
                }
            }
            NumberInputValue::I32(v) => {
                if ffi.format != FfiFeathersNumberInput::FORMAT_I32 || ffi.value_i64 != v as i64 {
                    let raw = ffi.bypass_change_detection();
                    raw.format = FfiFeathersNumberInput::FORMAT_I32;
                    raw.value_i64 = v as i64;
                }
            }
            NumberInputValue::I64(v) => {
                if ffi.format != FfiFeathersNumberInput::FORMAT_I64 || ffi.value_i64 != v {
                    let raw = ffi.bypass_change_detection();
                    raw.format = FfiFeathersNumberInput::FORMAT_I64;
                    raw.value_i64 = v;
                }
            }
        }
    }
}

#[cfg(feature = "bridge-feathers")]
fn bridge_value_change_f64(change: On<ValueChange<f64>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_f64(
        change.source,
        change.value,
        change.is_final,
    ));
}

#[cfg(feature = "bridge-feathers")]
fn bridge_value_change_i32(change: On<ValueChange<i32>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_i32(
        change.source,
        change.value,
        change.is_final,
    ));
}

#[cfg(feature = "bridge-feathers")]
fn bridge_value_change_i64(change: On<ValueChange<i64>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_i64(
        change.source,
        change.value,
        change.is_final,
    ));
}

#[cfg(feature = "bridge-feathers")]
fn bridge_value_change_color(change: On<ValueChange<Color>>, mut commands: Commands) {
    commands.trigger(FfiValueChange::from_color(
        change.source,
        change.value,
        change.is_final,
    ));
}
