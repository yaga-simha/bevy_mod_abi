#![cfg(feature = "bridge-ui")]
#![allow(clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]

//! Headless integration tests for `bridge-ui` and `bridge-feathers` (`ModUiBridgePlugin`):
//! - Zero-padding `#[repr(C)]` `AbiPod` layout & `ModRegistry` registration verification.
//! - `Val::Em` / `Val::Rem`, `FixedNode`, elliptical `BorderRadius` / `CornerRadius`,
//!   `FontSize::Rem`, `InlineBox` / `InlineImage`, and split `EditableText` + `TextInput` sync.
//! - Headless `TabList` / `Tab` / `SelectedTab` derived state + `FfiValueChange` guest FFI
//!   observer dispatch (`ValueChange<Option<Entity>>`, `ValueChange<Entity>`, `ValueChange<f32>`,
//!   `ValueChange<bool>`).
//! - `bevy_feathers` widget descriptors (`FeathersColorInput`, `FeathersSelect` / dropdown,
//!   `FeathersListView` / `FeathersListRow`, scrubbable `FeathersNumberInput`) & `ThemeContext`
//!   (`Base`, `Higher`, `Highest`, `Floating`) under `#[cfg(feature = "bridge-feathers")]`.
//! - Full retained-UI mod-unload teardown (`app.unload_mod`) verifying zero leaked UI nodes,
//!   nested children, `Disabled` panels, tracked observers, or dangling `InputFocus`.

use core::ffi::c_void;
use core::mem::{align_of, size_of};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy_abi_mod_bridges::ui::{
    CANONICAL_ACTIVATE_EVENT_NAME, CANONICAL_BACKGROUND_COLOR_NAME, CANONICAL_BORDER_COLOR_NAME,
    CANONICAL_EM_SIZE_NAME, CANONICAL_REM_SIZE_NAME, CANONICAL_TEXT_SPAN_NAME,
    CANONICAL_UI_TEXT_NAME, CANONICAL_VALUE_CHANGE_EVENT_NAME, FfiBorderColor, FfiBorderRadius,
    FfiCornerRadius, FfiEditableTextConfig, FfiEmSize, FfiFontSize, FfiInlineBox, FfiInlineImage,
    FfiRemSize, FfiRgba, FfiSelectedTab, FfiTab, FfiTabList, FfiUiNode, FfiUiRect, FfiUiTextStyle,
    FfiVal, FfiValueChange, ModUiAppExt, ModUiBridgePlugin, ModUiRegistry,
};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, ModScoped, host_cmd_observe, host_sys_commands,
};
use bevy_abi_mod_sys::{
    BamComponent, BamObserverDesc, BamSystemCtx, ComponentUuid, FfiAssetHandle, FfiBool, FfiEntity,
    FfiResult, assert_abi_pod,
};
use bevy_app::{App, Update};
use bevy_asset::Assets;
use bevy_color::Color;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::observer::On;
use bevy_image::{Image, TRANSPARENT_IMAGE_HANDLE};
use bevy_input_focus::tab_navigation::TabIndex;
use bevy_input_focus::{FocusCause, InputFocus};
use bevy_math::Vec2;
use bevy_text::{
    EditableText, FontSize, FontSmoothing, FontStyle, FontWeight, InlineBox, InlineBoxKind,
    Justify, LineBreak, RemSize, TextColor, TextFont, TextLayout, TextReadWriteMode,
};
use bevy_ui::widget::{InlineImage, Text};
use bevy_ui::{
    AlignItems, BorderRadius, CornerRadius, Display, FixedNode, FlexDirection, FlexWrap,
    InteractionDisabled, JustifyContent, Node, PositionType, Selected, UiRect, Val,
};
use bevy_ui_widgets::{
    ControlOrientation, SelectedTab, Tab, TabActivation, TabList, TextInput, ValueChange,
};

#[cfg(feature = "bridge-feathers")]
use bevy_abi_mod_bridges::ui::{
    CANONICAL_THEME_CONTEXT_NAME, FfiFeathersColorInput, FfiFeathersDropdown, FfiFeathersListRow,
    FfiFeathersNumberInput, FfiFeathersScrollableList, FfiThemeContext,
};
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::controls::{
    ColorInputMode, ColorInputSettings, ColorInputValue, FeathersColorInput, FeathersListRow,
    FeathersListView, FeathersNumberInput, FeathersSelect, HardLimit, NumberInputPrecision,
    NumberInputRange, NumberInputStep, NumberInputValue, NumberInputWrap, OptionIndex,
    ScrollbarGutter, SoftLimit,
};
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::rounded_corners::RoundedCorners;
#[cfg(feature = "bridge-feathers")]
use bevy_feathers::theme::{
    SemanticToken, SurfaceLevel, ThemeContext, ThemeProps, ThemeToken, UiTheme,
};
#[cfg(feature = "bridge-feathers")]
use bevy_platform::collections::HashMap;
#[cfg(feature = "bridge-feathers")]
use bevy_ui_widgets::{ListBox, ListItem, ScrollArea};

fn setup_ui_app() -> App {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModUiBridgePlugin));
    app
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

struct TempCModDylib {
    path: PathBuf,
    dir: PathBuf,
}

impl TempCModDylib {
    /// Compiles a minimal zero-system ISO C99 shared library against `include/bevy_abi_mod.h`
    /// so `app.load_mod_with` and `app.unload_mod` run hermetically without pre-built fixtures.
    fn build(mod_name: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bam_ui_bridge_mod_{}_{}_{}",
            std::process::id(),
            mod_name,
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp mod dir");

        let src_path = dir.join(format!("{mod_name}.c"));
        let ext = if cfg!(target_os = "windows") {
            "dll"
        } else if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let dylib_path = dir.join(format!("lib{mod_name}.{ext}"));

        let c_source = format!(
            r#"#include <stddef.h>
#include <stdint.h>
#include "bevy_abi_mod.h"

static const BamManifest MANIFEST = {{
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "{mod_name}",
    .version = "0.1.0",
    .flags = 0u,
    .reserved0 = 0u
}};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {{
    return &MANIFEST;
}}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {{
    BamHostApi local_api;
    if (load_ctx == NULL || instance_out == NULL) {{
        return BAM_RESULT_NULL_POINTER;
    }}
    *instance_out = NULL;
    return bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
}}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {{
    (void)instance;
}}
"#
        );
        fs::write(&src_path, c_source).expect("write minimal C99 mod source");

        let include_dir = workspace_root().join("include");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
        let mut cmd = Command::new(&cc);
        cmd.arg("-std=c99")
            .arg("-shared")
            .arg("-O1")
            .arg(format!("-I{}", include_dir.display()));
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(&src_path).arg("-o").arg(&dylib_path);

        let output = cmd.output().expect("invoke C compiler for test mod");
        assert!(
            output.status.success(),
            "C compiler failed building minimal UI teardown mod:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        Self {
            path: dylib_path,
            dir,
        }
    }
}

impl Drop for TempCModDylib {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn ffi_ui_wire_types_have_zero_padding_abi_pod_layouts_and_registry_entries() {
    assert_abi_pod::<FfiVal>();
    assert_abi_pod::<FfiUiRect>();
    assert_abi_pod::<FfiCornerRadius>();
    assert_abi_pod::<FfiBorderRadius>();
    assert_abi_pod::<FfiRgba>();
    assert_abi_pod::<FfiBorderColor>();
    assert_abi_pod::<FfiRemSize>();
    assert_abi_pod::<FfiEmSize>();
    assert_abi_pod::<FfiUiNode>();
    assert_abi_pod::<FfiFontSize>();
    assert_abi_pod::<FfiUiTextStyle>();
    assert_abi_pod::<FfiEditableTextConfig>();
    assert_abi_pod::<FfiInlineBox>();
    assert_abi_pod::<FfiInlineImage>();
    assert_abi_pod::<FfiTabList>();
    assert_abi_pod::<FfiTab>();
    assert_abi_pod::<FfiSelectedTab>();
    assert_abi_pod::<FfiValueChange>();

    assert_eq!(size_of::<FfiVal>(), 8);
    assert_eq!(align_of::<FfiVal>(), 4);
    assert_eq!(size_of::<FfiUiRect>(), 32);
    assert_eq!(size_of::<FfiCornerRadius>(), 16);
    assert_eq!(size_of::<FfiBorderRadius>(), 64);
    assert_eq!(size_of::<FfiRgba>(), 16);
    assert_eq!(size_of::<FfiBorderColor>(), 64);
    assert_eq!(size_of::<FfiUiNode>(), 280);
    assert_eq!(size_of::<FfiFontSize>(), 8);
    assert_eq!(size_of::<FfiUiTextStyle>(), 64);
    assert_eq!(size_of::<FfiEditableTextConfig>(), 36);
    assert_eq!(size_of::<FfiInlineBox>(), 12);
    assert_eq!(size_of::<FfiInlineImage>(), 60);
    assert_eq!(size_of::<FfiTabList>(), 4);
    assert_eq!(size_of::<FfiTab>(), 4);
    assert_eq!(size_of::<FfiSelectedTab>(), 8);
    assert_eq!(size_of::<FfiValueChange>(), 56);
    assert_eq!(align_of::<FfiValueChange>(), 8);

    #[cfg(feature = "bridge-feathers")]
    {
        assert_abi_pod::<FfiThemeContext>();
        assert_abi_pod::<FfiFeathersColorInput>();
        assert_abi_pod::<FfiFeathersDropdown>();
        assert_abi_pod::<FfiFeathersScrollableList>();
        assert_abi_pod::<FfiFeathersListRow>();
        assert_abi_pod::<FfiFeathersNumberInput>();

        assert_eq!(size_of::<FfiThemeContext>(), 4);
        assert_eq!(size_of::<FfiFeathersColorInput>(), 24);
        assert_eq!(size_of::<FfiFeathersDropdown>(), 24);
        assert_eq!(size_of::<FfiFeathersScrollableList>(), 12);
        assert_eq!(size_of::<FfiFeathersListRow>(), 8);
        assert_eq!(size_of::<FfiFeathersNumberInput>(), 72);
    }

    let app = setup_ui_app();
    let reg = app.world().resource::<ModRegistry>();

    for canonical in [
        "bevy_abi_mod_ui::FfiUiNode",
        "bevy_abi_mod_ui::FfiBorderRadius",
        "bevy_abi_mod_ui::FfiUiTextStyle",
        "bevy_abi_mod_ui::FfiEditableTextConfig",
        "bevy_abi_mod_ui::FfiInlineBox",
        "bevy_abi_mod_ui::FfiInlineImage",
        "bevy_abi_mod_ui::FfiTabList",
        "bevy_abi_mod_ui::FfiTab",
        "bevy_abi_mod_ui::FfiSelectedTab",
        CANONICAL_VALUE_CHANGE_EVENT_NAME,
        CANONICAL_ACTIVATE_EVENT_NAME,
        CANONICAL_UI_TEXT_NAME,
        CANONICAL_TEXT_SPAN_NAME,
        CANONICAL_BACKGROUND_COLOR_NAME,
        CANONICAL_BORDER_COLOR_NAME,
        CANONICAL_REM_SIZE_NAME,
        CANONICAL_EM_SIZE_NAME,
    ] {
        assert!(
            reg.get_by_name(canonical).is_some(),
            "missing ModRegistry entry for {canonical}"
        );
    }

    assert!(reg.resolve_symbol("bam.v0.Text.vtable").is_some());
    assert!(reg.resolve_symbol("bam.v0.TextSpan.vtable").is_some());
    assert!(
        reg.resolve_symbol("bam.v0.BackgroundColor.vtable")
            .is_some()
    );
    assert!(reg.resolve_symbol("bam.v0.BorderColor.vtable").is_some());
    assert!(reg.resolve_symbol("bam.v0.RemSize.vtable").is_some());
    assert!(reg.resolve_symbol("bam.v0.EmSize.vtable").is_some());

    #[cfg(feature = "bridge-feathers")]
    {
        for canonical in [
            "bevy_abi_mod_ui::FfiThemeContext",
            "bevy_abi_mod_ui::FfiFeathersColorInput",
            "bevy_abi_mod_ui::FfiFeathersDropdown",
            "bevy_abi_mod_ui::FfiFeathersScrollableList",
            "bevy_abi_mod_ui::FfiFeathersListRow",
            "bevy_abi_mod_ui::FfiFeathersNumberInput",
            CANONICAL_THEME_CONTEXT_NAME,
        ] {
            assert!(
                reg.get_by_name(canonical).is_some(),
                "missing Feathers ModRegistry entry for {canonical}"
            );
        }
        assert!(reg.resolve_symbol("bam.v0.ThemeContext.vtable").is_some());
    }
}

#[test]
fn val_em_rem_fixed_node_and_elliptical_border_radius_sync() {
    // Exhaustive `FfiVal` roundtrip across all 9 Bevy 0.20 `Val` variants including Em & Rem.
    let all_vals = [
        Val::Auto,
        Val::Px(12.5),
        Val::Percent(50.0),
        Val::Vw(10.0),
        Val::Vh(25.0),
        Val::VMin(5.0),
        Val::VMax(75.0),
        Val::Em(1.75),
        Val::Rem(2.5),
    ];
    for val in all_vals {
        let wire = FfiVal::from_bevy(val);
        assert_eq!(wire.try_to_bevy(), Ok(val));
    }
    assert_eq!(
        FfiVal {
            unit: 99,
            reserved0: [0; 3],
            value: 1.0
        }
        .try_to_bevy(),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        FfiVal::em(f32::NAN).try_to_bevy(),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    let mut app = setup_ui_app();

    let elliptical = FfiBorderRadius {
        top_left: FfiCornerRadius::elliptical(FfiVal::px(24.0), FfiVal::em(1.5)),
        top_right: FfiCornerRadius::elliptical(FfiVal::rem(1.25), FfiVal::percent(30.0)),
        bottom_right: FfiCornerRadius::circular(FfiVal::px(8.0)),
        bottom_left: FfiCornerRadius::elliptical(FfiVal::vw(2.0), FfiVal::vh(3.0)),
    };

    let node_entity = app
        .world_mut()
        .spawn(FfiUiNode {
            width: FfiVal::rem(20.0),
            height: FfiVal::em(6.5),
            min_width: FfiVal::px(120.0),
            padding: FfiUiRect::axes(FfiVal::em(1.0), FfiVal::rem(0.5)),
            margin: FfiUiRect::all(FfiVal::rem(0.75)),
            row_gap: FfiVal::em(0.5),
            column_gap: FfiVal::rem(1.0),
            border_radius: elliptical,
            display: FfiUiNode::DISPLAY_FLEX,
            position_type: FfiUiNode::POSITION_ABSOLUTE,
            flex_direction: FfiUiNode::FLEX_COLUMN,
            flex_wrap: FfiUiNode::WRAP_WRAP,
            align_items: FfiUiNode::ALIGN_CENTER,
            justify_content: FfiUiNode::JUSTIFY_SPACE_BETWEEN,
            flex_grow: 1.0,
            flex_shrink: 0.0,
            fixed_node: FfiBool::TRUE,
            ..Default::default()
        })
        .id();

    app.update();

    let node = app
        .world()
        .get::<Node>(node_entity)
        .expect("Node synced from FfiUiNode");
    assert_eq!(node.width, Val::Rem(20.0));
    assert_eq!(node.height, Val::Em(6.5));
    assert_eq!(node.min_width, Val::Px(120.0));
    assert_eq!(node.padding, UiRect::axes(Val::Em(1.0), Val::Rem(0.5)));
    assert_eq!(node.margin, UiRect::all(Val::Rem(0.75)));
    assert_eq!(node.row_gap, Val::Em(0.5));
    assert_eq!(node.column_gap, Val::Rem(1.0));
    assert_eq!(node.display, Display::Flex);
    assert_eq!(node.position_type, PositionType::Absolute);
    assert_eq!(node.flex_direction, FlexDirection::Column);
    assert_eq!(node.flex_wrap, FlexWrap::Wrap);
    assert_eq!(node.align_items, AlignItems::Center);
    assert_eq!(node.justify_content, JustifyContent::SpaceBetween);
    assert_eq!(node.flex_grow, 1.0);
    assert_eq!(node.flex_shrink, 0.0);
    assert_eq!(
        node.border_radius,
        BorderRadius {
            top_left: CornerRadius::new(Val::Px(24.0), Val::Em(1.5)),
            top_right: CornerRadius::new(Val::Rem(1.25), Val::Percent(30.0)),
            bottom_right: CornerRadius::circular(Val::Px(8.0)),
            bottom_left: CornerRadius::new(Val::Vw(2.0), Val::Vh(3.0)),
        }
    );
    assert!(
        app.world().get::<FixedNode>(node_entity).is_some(),
        "FixedNode must be attached when FfiUiNode::fixed_node is true"
    );

    // Toggle `FixedNode` off and attach standalone `FfiBorderRadius` override.
    app.world_mut()
        .get_mut::<FfiUiNode>(node_entity)
        .unwrap()
        .fixed_node = FfiBool::FALSE;
    app.world_mut()
        .entity_mut(node_entity)
        .insert(FfiBorderRadius::elliptical_all(
            FfiVal::rem(2.0),
            FfiVal::em(0.75),
        ));

    app.update();

    assert!(
        app.world().get::<FixedNode>(node_entity).is_none(),
        "FixedNode must be removed when FfiUiNode::fixed_node becomes false"
    );
    let updated_node = app.world().get::<Node>(node_entity).unwrap();
    assert_eq!(
        updated_node.border_radius,
        BorderRadius::all(CornerRadius::new(Val::Rem(2.0), Val::Em(0.75)))
    );
}

#[test]
fn font_size_rem_inline_box_inline_image_and_split_editable_text_sync() {
    // Exhaustive `FfiFontSize` roundtrip across all 6 Bevy 0.20 variants.
    for fs in [
        FontSize::Px(14.0),
        FontSize::Vw(3.0),
        FontSize::Vh(4.0),
        FontSize::VMin(2.5),
        FontSize::VMax(5.0),
        FontSize::Rem(1.5),
    ] {
        let wire = FfiFontSize::from_bevy(fs);
        assert_eq!(wire.try_to_bevy(), Ok(fs));
    }

    let mut app = setup_ui_app();
    app.insert_resource(Assets::<Image>::default());
    app.insert_resource(RemSize(16.0));

    // 1. `FfiUiTextStyle` with `FontSize::Rem` + `ensure_text`.
    let text_ent = app
        .world_mut()
        .spawn(FfiUiTextStyle {
            font_size: FfiFontSize::rem(1.5),
            color: FfiRgba::new(0.25, 0.5, 0.75, 1.0),
            weight: FontWeight::BOLD.0,
            justify: FfiUiTextStyle::JUSTIFY_CENTER,
            linebreak: FfiUiTextStyle::LINEBREAK_NO_WRAP,
            font_smoothing: FfiUiTextStyle::SMOOTHING_NONE,
            font_style: FfiUiTextStyle::STYLE_ITALIC,
            ensure_text: FfiBool::TRUE,
            ..Default::default()
        })
        .id();

    // 2. `FfiInlineBox` standalone (`InFlow`).
    let box_ent = app
        .world_mut()
        .spawn(FfiInlineBox::in_flow(32.0, 24.0))
        .id();

    // 3. `FfiInlineImage` backed by an index-based `FfiAssetHandle` into `Assets<Image>` AND
    //    another backed by a deterministic UUID handle.
    let img_handle = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::default());
    let index_bits = match img_handle.id() {
        bevy_asset::AssetId::Index { index, .. } => index.to_bits(),
        bevy_asset::AssetId::Uuid { .. } => panic!("expected indexed image handle"),
    };
    let ffi_img_index = FfiAssetHandle::from_index_bits(BamComponent::from_raw(1), index_bits);
    let inline_img_index_ent = app
        .world_mut()
        .spawn(FfiInlineImage {
            image: ffi_img_index,
            color: FfiRgba::new(1.0, 0.8, 0.2, 1.0),
            width: 48.0,
            height: 36.0,
            has_width: FfiBool::TRUE,
            has_height: FfiBool::TRUE,
            flip_x: FfiBool::TRUE,
            flip_y: FfiBool::FALSE,
        })
        .id();

    let uuid = ComponentUuid::from_canonical_name("ui_test::badge_icon");
    let ffi_img_uuid = FfiAssetHandle::from_uuid(BamComponent::from_raw(1), uuid);
    let inline_img_uuid_ent = app
        .world_mut()
        .spawn((
            FfiInlineImage {
                image: ffi_img_uuid,
                ..Default::default()
            },
            FfiInlineBox::in_flow(18.0, 18.0),
        ))
        .id();

    let inline_img_default_ent = app.world_mut().spawn(FfiInlineImage::default()).id();

    // 4. Split `EditableText` + `TextInput`: start headless (`interactive = FALSE`).
    let editable_ent = app
        .world_mut()
        .spawn(FfiEditableTextConfig {
            cursor_margin: [0.15, 0.25],
            cursor_width: 0.35,
            cursor_blink_period_secs: 0.5,
            visible_lines: 4.0,
            visible_width: 30.0,
            max_characters: 64,
            interactive: FfiBool::FALSE,
            allow_newlines: FfiBool::TRUE,
            read_write_mode: FfiEditableTextConfig::MODE_READ_ONLY,
            has_max_characters: FfiBool::TRUE,
            has_visible_lines: FfiBool::TRUE,
            has_visible_width: FfiBool::TRUE,
            reserved0: [0; 2],
        })
        .id();

    app.update();

    // Verify `TextFont` / `FontSize::Rem` / `TextColor` / `TextLayout` / `Text`.
    let tf = app.world().get::<TextFont>(text_ent).expect("TextFont");
    assert_eq!(tf.font_size, FontSize::Rem(1.5));
    assert_eq!(
        tf.font_size
            .eval(Vec2::new(1280.0, 720.0), *app.world().resource::<RemSize>()),
        24.0
    );
    assert_eq!(tf.weight, FontWeight::BOLD);
    assert_eq!(tf.font_smoothing, FontSmoothing::None);
    assert_eq!(tf.style, FontStyle::Italic);
    assert_eq!(
        app.world().get::<TextColor>(text_ent).unwrap().0,
        Color::srgba(0.25, 0.5, 0.75, 1.0)
    );
    let tl = app.world().get::<TextLayout>(text_ent).unwrap();
    assert_eq!(tl.justify, Justify::Center);
    assert_eq!(tl.linebreak, LineBreak::NoWrap);
    assert!(app.world().get::<Text>(text_ent).is_some());

    // Verify `InlineBox`.
    let ib = app.world().get::<InlineBox>(box_ent).expect("InlineBox");
    assert_eq!(ib.kind, InlineBoxKind::InFlow);
    assert_eq!(ib.size, Vec2::new(32.0, 24.0));

    // Verify `InlineImage` (index-resolved, UUID-resolved + explicit InlineBox, and default).
    let ii_idx = app
        .world()
        .get::<InlineImage>(inline_img_index_ent)
        .expect("InlineImage (index)");
    assert_eq!(ii_idx.image.id(), img_handle.id());
    assert_eq!(ii_idx.width, Some(48.0));
    assert_eq!(ii_idx.height, Some(36.0));
    assert!(ii_idx.flip_x);
    assert!(!ii_idx.flip_y);
    assert_eq!(
        ii_idx.resolve_inline_box_size(Vec2::new(10.0, 10.0)),
        Vec2::new(48.0, 36.0)
    );

    let ii_uuid = app
        .world()
        .get::<InlineImage>(inline_img_uuid_ent)
        .expect("InlineImage (uuid)");
    assert_eq!(
        ii_uuid.image.id(),
        bevy_asset::AssetId::Uuid {
            uuid: bevy_asset::uuid::Uuid::from_u128(uuid.to_u128()),
        }
    );
    let ib_override = app
        .world()
        .get::<InlineBox>(inline_img_uuid_ent)
        .expect("InlineBox override on InlineImage");
    assert_eq!(ib_override.kind, InlineBoxKind::InFlow);
    assert_eq!(ib_override.size, Vec2::new(18.0, 18.0));

    let ii_def = app
        .world()
        .get::<InlineImage>(inline_img_default_ent)
        .expect("InlineImage (default)");
    assert_eq!(ii_def.image, TRANSPARENT_IMAGE_HANDLE);

    // Verify split `EditableText` (present) vs `TextInput` (absent when `interactive = FALSE`).
    {
        let ed = app
            .world()
            .get::<EditableText>(editable_ent)
            .expect("EditableText");
        assert!(ed.allow_newlines);
        assert_eq!(ed.max_characters, Some(64));
        assert_eq!(ed.visible_lines, Some(4.0));
        assert_eq!(ed.visible_width, Some(30.0));
        assert_eq!(ed.cursor_margin, Vec2::new(0.15, 0.25));
        assert_eq!(ed.cursor_width, 0.35);
        assert_eq!(
            *app.world().get::<TextReadWriteMode>(editable_ent).unwrap(),
            TextReadWriteMode::ReadOnly
        );
        assert!(
            app.world().get::<TextInput>(editable_ent).is_none(),
            "headless EditableText must NOT attach TextInput when interactive = FALSE"
        );
    }

    // Mutate live buffer, then flip `interactive = TRUE` and verify buffer is preserved AND
    // `TextInput` is attached.
    app.world_mut()
        .get_mut::<EditableText>(editable_ent)
        .unwrap()
        .editor
        .set_text("hello bevy 0.20");
    {
        let mut cfg = app
            .world_mut()
            .get_mut::<FfiEditableTextConfig>(editable_ent)
            .unwrap();
        cfg.interactive = FfiBool::TRUE;
        cfg.read_write_mode = FfiEditableTextConfig::MODE_EDITABLE;
        cfg.max_characters = 128;
    }

    app.update();

    assert!(
        app.world().get::<TextInput>(editable_ent).is_some(),
        "TextInput must attach when interactive = TRUE"
    );
    assert_eq!(
        *app.world().get::<TextReadWriteMode>(editable_ent).unwrap(),
        TextReadWriteMode::Editable
    );
    let ed_after = app.world().get::<EditableText>(editable_ent).unwrap();
    assert_eq!(ed_after.max_characters, Some(128));
    assert_eq!(ed_after.value().to_string(), "hello bevy 0.20");

    // Toggle `interactive` back to FALSE -> `TextInput` removed, buffer still intact.
    app.world_mut()
        .get_mut::<FfiEditableTextConfig>(editable_ent)
        .unwrap()
        .interactive = FfiBool::FALSE;
    app.update();
    assert!(app.world().get::<TextInput>(editable_ent).is_none());
    assert_eq!(
        app.world()
            .get::<EditableText>(editable_ent)
            .unwrap()
            .value()
            .to_string(),
        "hello bevy 0.20"
    );
}

#[test]
fn headless_tablist_selection_and_value_change_guest_observer_dispatch() {
    let mut app = setup_ui_app();

    let tablist = app
        .world_mut()
        .spawn((
            FfiTabList {
                orientation: FfiTabList::ORIENTATION_HORIZONTAL,
                activation: FfiTabList::ACTIVATION_AUTOMATIC,
                self_update: FfiBool::TRUE,
                disabled: FfiBool::FALSE,
            },
            FfiSelectedTab::default(),
        ))
        .id();
    let tab_a = app
        .world_mut()
        .spawn((ChildOf(tablist), FfiTab::default()))
        .id();
    let tab_b = app
        .world_mut()
        .spawn((ChildOf(tablist), FfiTab::default()))
        .id();
    let slider_ent = app.world_mut().spawn_empty().id();
    let checkbox_ent = app.world_mut().spawn_empty().id();
    let listbox_ent = app.world_mut().spawn_empty().id();

    // Host Bevy observer on `FfiValueChange`.
    let host_events: Arc<Mutex<Vec<FfiValueChange>>> = Arc::new(Mutex::new(Vec::new()));
    let host_events_clone = Arc::clone(&host_events);
    app.add_observer(move |ev: On<FfiValueChange>| {
        host_events_clone.lock().unwrap().push(*ev.event());
    });

    // Real C-ABI guest FFI observer watching `FfiValueChange` via `host_cmd_observe`.
    let guest_events: Arc<Mutex<Vec<(FfiEntity, FfiValueChange)>>> =
        Arc::new(Mutex::new(Vec::new()));

    unsafe extern "C" fn guest_value_change_cb(
        target: FfiEntity,
        event_data: *const c_void,
        _cmds: *mut bevy_abi_mod_sys::BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        assert!(!event_data.is_null());
        let payload = unsafe { *(event_data as *const FfiValueChange) };
        let log = unsafe { &*(user_data as *const Mutex<Vec<(FfiEntity, FfiValueChange)>>) };
        log.lock().unwrap().push((target, payload));
        FfiResult::OK
    }

    let value_change_handle = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(CANONICAL_VALUE_CHANGE_EVENT_NAME)
        .expect("FfiValueChange registered")
        .handle;

    struct InstallObserverState {
        event_handle: BamComponent,
        installed: bool,
        guest_log: *const Mutex<Vec<(FfiEntity, FfiValueChange)>>,
    }
    // SAFETY: `guest_log` points to an `Arc<Mutex<_>>` kept alive for the entire test.
    unsafe impl Send for InstallObserverState {}
    // SAFETY: Synchronized via `Mutex<InstallObserverState>`.
    unsafe impl Sync for InstallObserverState {}

    let install_state = Arc::new(Mutex::new(InstallObserverState {
        event_handle: value_change_handle,
        installed: false,
        guest_log: Arc::as_ptr(&guest_events),
    }));

    unsafe extern "C" fn install_observer_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut st = unsafe { &*(user_data as *const Mutex<InstallObserverState>) }
            .lock()
            .unwrap();
        if st.installed {
            return FfiResult::OK;
        }
        let mut cmds = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_commands(sys_ctx, &mut cmds) },
            FfiResult::OK
        );
        let desc = BamObserverDesc::new(
            st.event_handle,
            guest_value_change_cb,
            st.guest_log.cast_mut().cast::<c_void>(),
        );
        assert_eq!(
            unsafe { host_cmd_observe(cmds, &desc, core::ptr::null_mut()) },
            FfiResult::OK
        );
        st.installed = true;
        FfiResult::OK
    }

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    app.add_systems(
        Update,
        GuestSystem::new(
            "ui_value_change_observer_installer",
            vec![BamQuerySpecOwned::new(vec![])],
            install_observer_sys,
            Arc::as_ptr(&install_state) as *mut c_void,
            mod_reg,
        ),
    );

    // Frame 1: sync TabList/Tab descriptors & install guest observer.
    app.update();

    assert_eq!(
        *app.world().get::<TabList>(tablist).expect("TabList synced"),
        TabList {
            orientation: ControlOrientation::Horizontal,
            activation: TabActivation::Automatic,
        }
    );
    assert!(app.world().get::<Tab>(tab_a).is_some());
    assert!(app.world().get::<Tab>(tab_b).is_some());
    // Frame 2 ensures TabPlugin's derived roving `TabIndex` sees newly inserted `Tab`s.
    app.update();
    assert_eq!(*app.world().get::<TabIndex>(tab_a).unwrap(), TabIndex(0));
    assert_eq!(*app.world().get::<TabIndex>(tab_b).unwrap(), TabIndex(-1));

    // Trigger all 4 core `bevy_ui_widgets::ValueChange<T>` payload types.
    app.world_mut().trigger(ValueChange::<Option<Entity>> {
        source: tablist,
        value: Some(tab_b),
        is_final: true,
    });
    app.world_mut().trigger(ValueChange::<Entity> {
        source: listbox_ent,
        value: tab_a,
        is_final: true,
    });
    app.world_mut().trigger(ValueChange::<f32> {
        source: slider_ent,
        value: 0.75,
        is_final: false,
    });
    app.world_mut().trigger(ValueChange::<bool> {
        source: checkbox_ent,
        value: true,
        is_final: true,
    });

    app.update();

    // Verify `tablist_ffi_self_update` + `TabPlugin` + `mirror_selected_tabs_to_ffi`.
    assert_eq!(
        *app.world().get::<SelectedTab>(tablist).unwrap(),
        SelectedTab(Some(tab_b))
    );
    assert_eq!(
        *app.world().get::<FfiSelectedTab>(tablist).unwrap(),
        FfiSelectedTab::from_option(Some(tab_b))
    );
    assert!(app.world().get::<Selected>(tab_b).is_some());
    assert!(app.world().get::<Selected>(tab_a).is_none());

    // Verify BOTH Bevy host observer and C-ABI guest observer received all 4 `FfiValueChange`s!
    let expected = [
        FfiValueChange::from_optional_entity(tablist, Some(tab_b), true),
        FfiValueChange::from_entity(listbox_ent, tab_a, true),
        FfiValueChange::from_f32(slider_ent, 0.75, false),
        FfiValueChange::from_bool(checkbox_ent, true, true),
    ];
    let host_log = host_events.lock().unwrap();
    let guest_log = guest_events.lock().unwrap();
    assert_eq!(host_log.as_slice(), &expected);
    assert_eq!(guest_log.len(), expected.len());
    for ((target, got), want) in guest_log.iter().zip(expected.iter()) {
        assert_eq!(*target, want.source);
        assert_eq!(got, want);
    }

    // Mutate `FfiSelectedTab` from guest side -> switches `Selected` from `tab_b` to `tab_a`.
    *app.world_mut().get_mut::<FfiSelectedTab>(tablist).unwrap() =
        FfiSelectedTab::from_option(Some(tab_a));
    app.update();
    assert_eq!(
        *app.world().get::<SelectedTab>(tablist).unwrap(),
        SelectedTab(Some(tab_a))
    );
    assert!(app.world().get::<Selected>(tab_a).is_some());
    assert!(app.world().get::<Selected>(tab_b).is_none());
}

#[cfg(feature = "bridge-feathers")]
#[test]
fn feathers_widget_descriptors_and_theme_context_levels_sync() {
    let mut app = setup_ui_app();

    // Configure contextual `UiTheme` overrides for `Base`, `Higher`, `Highest`, `Floating`.
    let surface_token = ThemeToken::new_static("panel.surface");
    let semantic = SemanticToken::new_static("surface.panel");
    let mut props = ThemeProps::default();
    props
        .token_assignments
        .insert(surface_token.clone(), semantic.clone());
    props
        .semantic_base
        .insert(semantic.clone(), Color::srgba(0.1, 0.1, 0.1, 1.0));
    let mut higher_map = HashMap::new();
    higher_map.insert(semantic.clone(), Color::srgba(0.2, 0.2, 0.2, 1.0));
    props
        .semantic_overrides
        .insert(SurfaceLevel::Higher, higher_map);
    let mut highest_map = HashMap::new();
    highest_map.insert(semantic.clone(), Color::srgba(0.3, 0.3, 0.3, 1.0));
    props
        .semantic_overrides
        .insert(SurfaceLevel::Highest, highest_map);
    let mut floating_map = HashMap::new();
    floating_map.insert(semantic, Color::srgba(0.4, 0.4, 0.4, 1.0));
    props
        .semantic_overrides
        .insert(SurfaceLevel::Floating, floating_map);
    app.insert_resource(UiTheme(props));

    // 1. All 4 `FfiThemeContext` levels (`Base`, `Higher`, `Highest`, `Floating`).
    let theme_entities: Vec<(Entity, SurfaceLevel)> = [
        (FfiThemeContext::BASE, SurfaceLevel::Base),
        (FfiThemeContext::HIGHER, SurfaceLevel::Higher),
        (FfiThemeContext::HIGHEST, SurfaceLevel::Highest),
        (FfiThemeContext::FLOATING, SurfaceLevel::Floating),
    ]
    .into_iter()
    .map(|(ffi_ctx, expected)| (app.world_mut().spawn(ffi_ctx).id(), expected))
    .collect();

    // 2. `FfiFeathersColorInput`.
    let color_ent = app
        .world_mut()
        .spawn(FfiFeathersColorInput {
            color: FfiRgba::new(0.8, 0.25, 0.5, 0.9),
            opaque_color_percentage: 50.0,
            mode: FfiFeathersColorInput::MODE_HS_PLANE,
            apply_mode_to_settings: FfiBool::TRUE,
            disabled: FfiBool::FALSE,
            reserved0: 0,
        })
        .id();

    // 3. `FfiFeathersDropdown` (`FeathersSelect`).
    let dropdown_ent = app
        .world_mut()
        .spawn(FfiFeathersDropdown {
            selected_row: FfiEntity::NULL,
            selected_index: 2,
            max_visible: 6,
            has_selected_index: FfiBool::TRUE,
            rounded_corners: FfiFeathersDropdown::CORNERS_TOP,
            disabled: FfiBool::FALSE,
            reserved0: [0; 5],
        })
        .id();
    assert_eq!(
        FfiFeathersDropdown {
            rounded_corners: FfiFeathersDropdown::CORNERS_TOP,
            ..Default::default()
        }
        .try_rounded_corners(),
        Ok(RoundedCorners::Top)
    );

    // 4. `FfiFeathersScrollableList` (`FeathersListView`) + `FfiFeathersListRow`.
    let list_ent = app
        .world_mut()
        .spawn(FfiFeathersScrollableList {
            scrollbar_gutter: FfiVal::rem(1.0),
            orientation: FfiFeathersScrollableList::ORIENTATION_VERTICAL,
            selectable: FfiBool::TRUE,
            scrollable: FfiBool::TRUE,
            disabled: FfiBool::FALSE,
        })
        .id();
    let row_ent = app
        .world_mut()
        .spawn((
            ChildOf(list_ent),
            FfiFeathersListRow {
                option_index: 3,
                has_option_index: FfiBool::TRUE,
                selected: FfiBool::TRUE,
                disabled: FfiBool::FALSE,
                reserved0: 0,
            },
        ))
        .id();

    // 5. Scrubbable `FfiFeathersNumberInput` (F32 with SoftLimit + HardLimit + Wrap + Precision).
    let number_ent = app
        .world_mut()
        .spawn(FfiFeathersNumberInput {
            step: 0.25,
            precision: 3,
            wrap: FfiBool::TRUE,
            ..FfiFeathersNumberInput::f32(12.5)
                .with_soft_limit(0.0, 100.0)
                .with_hard_limit(-50.0, 200.0)
        })
        .id();

    // Observe `FfiValueChange` for Feathers `Color`, `f64`, `i32`, `i64` notifications.
    let captured: Arc<Mutex<Vec<FfiValueChange>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    app.add_observer(move |ev: On<FfiValueChange>| {
        captured_clone.lock().unwrap().push(*ev.event());
    });

    app.update();

    // Verify `ThemeContext` & contextual color lookup for all 4 surface levels.
    let theme = app.world().resource::<UiTheme>();
    for (idx, (ent, expected_level)) in theme_entities.iter().enumerate() {
        assert_eq!(
            *app.world().get::<ThemeContext>(*ent).expect("ThemeContext"),
            ThemeContext(*expected_level)
        );
        let expected_v = 0.1 * (idx + 1) as f32;
        let resolved = theme
            .context_color(&surface_token, *expected_level)
            .to_srgba();
        assert!((resolved.red - expected_v).abs() < 1e-5);
    }

    // Verify `FeathersColorInput`, `ColorInputValue`, and `ColorInputSettings`.
    assert!(app.world().get::<FeathersColorInput>(color_ent).is_some());
    assert_eq!(
        app.world().get::<ColorInputValue>(color_ent).unwrap().0,
        Color::srgba(0.8, 0.25, 0.5, 0.9)
    );
    assert!(app.world().resource::<ColorInputSettings>().mode == ColorInputMode::HSPlane);

    // Verify `FeathersSelect` dropdown + `OptionIndex`.
    assert!(app.world().get::<FeathersSelect>(dropdown_ent).is_some());
    assert_eq!(app.world().get::<OptionIndex>(dropdown_ent).unwrap().0, 2);

    // Verify `FeathersListView`, `ScrollbarGutter`, `ListBox`, `ScrollArea`, & `FeathersListRow`.
    assert!(app.world().get::<FeathersListView>(list_ent).is_some());
    assert_eq!(
        app.world().get::<ScrollbarGutter>(list_ent).unwrap().0,
        Val::Rem(1.0)
    );
    assert!(app.world().get::<ListBox>(list_ent).is_some());
    assert!(app.world().get::<ScrollArea>(list_ent).is_some());

    assert!(app.world().get::<FeathersListRow>(row_ent).is_some());
    assert!(app.world().get::<ListItem>(row_ent).is_some());
    assert_eq!(app.world().get::<OptionIndex>(row_ent).unwrap().0, 3);
    assert!(app.world().get::<Selected>(row_ent).is_some());

    // Verify scrubbable `FeathersNumberInput` + limits + precision + step + wrap.
    assert!(app.world().get::<FeathersNumberInput>(number_ent).is_some());
    assert_eq!(
        *app.world().get::<NumberInputValue>(number_ent).unwrap(),
        NumberInputValue::F32(12.5)
    );
    assert_eq!(
        app.world().get::<SoftLimit>(number_ent).unwrap().0,
        NumberInputRange::F32(0.0..=100.0)
    );
    assert_eq!(
        app.world().get::<HardLimit>(number_ent).unwrap().0,
        NumberInputRange::F32(-50.0..=200.0)
    );
    assert_eq!(
        app.world()
            .get::<NumberInputPrecision>(number_ent)
            .unwrap()
            .0,
        3
    );
    assert_eq!(
        app.world().get::<NumberInputStep>(number_ent).unwrap().0,
        0.25
    );
    assert_eq!(
        *app.world().get::<NumberInputWrap>(number_ent).unwrap(),
        NumberInputWrap::Wrap
    );

    // Verify host-side updates mirror back into `FfiThemeContext`, `FfiFeathersColorInput`,
    // and `FfiFeathersNumberInput`, and Feathers `ValueChange` events bridge into `FfiValueChange`.
    app.world_mut()
        .entity_mut(theme_entities[0].0)
        .insert(ThemeContext(SurfaceLevel::Floating));
    app.world_mut()
        .entity_mut(color_ent)
        .insert(ColorInputValue(Color::srgba(0.1, 0.9, 0.3, 1.0)));
    app.world_mut()
        .entity_mut(number_ent)
        .insert(NumberInputValue::I64(42));
    app.world_mut().trigger(ValueChange::<Color> {
        source: color_ent,
        value: Color::srgba(0.1, 0.9, 0.3, 1.0),
        is_final: true,
    });
    app.world_mut().trigger(ValueChange::<f64> {
        source: number_ent,
        value: 99.5,
        is_final: false,
    });
    app.world_mut().trigger(ValueChange::<i32> {
        source: number_ent,
        value: -7,
        is_final: true,
    });
    app.world_mut().trigger(ValueChange::<i64> {
        source: number_ent,
        value: 42,
        is_final: true,
    });

    app.update();

    assert_eq!(
        *app.world()
            .get::<FfiThemeContext>(theme_entities[0].0)
            .unwrap(),
        FfiThemeContext::FLOATING
    );
    assert_eq!(
        app.world()
            .get::<FfiFeathersColorInput>(color_ent)
            .unwrap()
            .color,
        FfiRgba::new(0.1, 0.9, 0.3, 1.0)
    );
    let num_desc = app
        .world()
        .get::<FfiFeathersNumberInput>(number_ent)
        .unwrap();
    assert_eq!(num_desc.format, FfiFeathersNumberInput::FORMAT_I64);
    assert_eq!(num_desc.value_i64, 42);

    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].kind, FfiValueChange::KIND_COLOR);
    assert_eq!(events[1].kind, FfiValueChange::KIND_F64);
    assert_eq!(events[1].f64_value, 99.5);
    assert_eq!(events[2].kind, FfiValueChange::KIND_I32);
    assert_eq!(events[2].i64_value, -7);
    assert_eq!(events[3].kind, FfiValueChange::KIND_I64);
    assert_eq!(events[3].i64_value, 42);
}

#[test]
fn retained_ui_mod_unload_teardown_leaves_zero_leaked_entities_observers_or_focus() {
    let mod_a_dylib = TempCModDylib::build("ui_teardown_a");
    let mod_b_dylib = TempCModDylib::build("ui_teardown_b");

    let mut app = setup_ui_app();
    app.init_resource::<InputFocus>();

    let mod_a = app
        .load_mod_with(&mod_a_dylib.path, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(&mod_b_dylib.path, LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    // Persistent host HUD root (must survive both mod unloads).
    let host_hud = app
        .world_mut()
        .spawn(FfiUiNode {
            width: FfiVal::percent(100.0),
            height: FfiVal::percent(100.0),
            ..Default::default()
        })
        .id();

    // Rich retained-UI tree owned by `mod_a`:
    // - `ModScoped(mod_a)` root with nested children that do NOT individually carry `ModScoped`
    // - `FixedNode` floating overlay registered via `register_mod_ui_root`
    // - `Disabled` hidden panel carrying `ModScoped(mod_a)`
    // - Tracked UI observer registered via `register_mod_ui_observer`
    let mod_a_root = app
        .world_mut()
        .spawn((
            ModScoped(mod_a),
            FfiUiNode {
                width: FfiVal::rem(30.0),
                height: FfiVal::rem(20.0),
                ..Default::default()
            },
        ))
        .id();
    let mod_a_tablist = app
        .world_mut()
        .spawn((
            ChildOf(mod_a_root),
            FfiTabList {
                self_update: FfiBool::TRUE,
                ..Default::default()
            },
            FfiSelectedTab::default(),
        ))
        .id();
    let mod_a_tab_1 = app
        .world_mut()
        .spawn((ChildOf(mod_a_tablist), FfiTab::default()))
        .id();
    let mod_a_text_input = app
        .world_mut()
        .spawn((
            ChildOf(mod_a_root),
            FfiEditableTextConfig::default(),
            FfiUiTextStyle {
                font_size: FfiFontSize::rem(1.25),
                ..Default::default()
            },
        ))
        .id();
    let mod_a_inline_img = app
        .world_mut()
        .spawn((
            ChildOf(mod_a_root),
            FfiInlineImage::default(),
            FfiInlineBox::in_flow(16.0, 16.0),
        ))
        .id();
    let mod_a_fixed_popup = app
        .world_mut()
        .spawn(FfiUiNode {
            fixed_node: FfiBool::TRUE,
            width: FfiVal::px(240.0),
            height: FfiVal::px(120.0),
            ..Default::default()
        })
        .id();
    let mod_a_popup_child = app
        .world_mut()
        .spawn((
            ChildOf(mod_a_fixed_popup),
            FfiUiTextStyle {
                ensure_text: FfiBool::TRUE,
                ..Default::default()
            },
        ))
        .id();
    app.register_mod_ui_root(mod_a, mod_a_fixed_popup);

    let mod_a_disabled_panel = app
        .world_mut()
        .spawn((
            ModScoped(mod_a),
            Disabled,
            FfiUiNode {
                width: FfiVal::px(100.0),
                height: FfiVal::px(50.0),
                ..Default::default()
            },
        ))
        .id();

    let obs_hits = Arc::new(AtomicU64::new(0));
    let obs_hits_clone = Arc::clone(&obs_hits);
    let mod_a_observer = app
        .world_mut()
        .add_observer(move |_ev: On<FfiValueChange>| {
            obs_hits_clone.fetch_add(1, Ordering::Relaxed);
        })
        .id();
    app.register_mod_ui_observer(mod_a, mod_a_observer);

    // Focus one of `mod_a`'s interactive widgets so unload must clear stale `InputFocus`.
    app.world_mut()
        .resource_mut::<InputFocus>()
        .set(mod_a_text_input, FocusCause::Navigated);

    // Separate retained-UI tree owned by `mod_b` (must remain alive when `mod_a` unloads).
    let mod_b_root = app
        .world_mut()
        .spawn((
            ModScoped(mod_b),
            FfiUiNode {
                width: FfiVal::rem(15.0),
                height: FfiVal::rem(10.0),
                ..Default::default()
            },
        ))
        .id();
    let mod_b_child = app
        .world_mut()
        .spawn((ChildOf(mod_b_root), FfiTab::default()))
        .id();

    app.update();

    assert!(
        app.world()
            .resource::<ModUiRegistry>()
            .tracked_ui_root_count(mod_a)
            >= 2
    );
    assert_eq!(
        app.world()
            .resource::<ModUiRegistry>()
            .tracked_ui_observer_count(mod_a),
        1
    );
    assert_eq!(
        app.world().resource::<InputFocus>().get(),
        Some(mod_a_text_input)
    );

    // Unload `mod_a`: every `mod_a` UI entity, nested child, FixedNode popup, Disabled panel,
    // tracked observer, and stale `InputFocus` MUST be cleaned up with zero leaks!
    app.unload_mod(mod_a).expect("unload mod_a");

    for dead in [
        mod_a_root,
        mod_a_tablist,
        mod_a_tab_1,
        mod_a_text_input,
        mod_a_inline_img,
        mod_a_fixed_popup,
        mod_a_popup_child,
        mod_a_disabled_panel,
        mod_a_observer,
    ] {
        assert!(
            app.world().get_entity(dead).is_err(),
            "leaked mod_a UI entity {dead:?} after unload_mod"
        );
    }

    let ui_reg = app.world().resource::<ModUiRegistry>();
    assert_eq!(ui_reg.tracked_ui_root_count(mod_a), 0);
    assert_eq!(ui_reg.tracked_ui_observer_count(mod_a), 0);
    assert_eq!(
        app.world().resource::<InputFocus>().get(),
        None,
        "stale InputFocus targeting unloaded mod_a widget must be cleared"
    );

    // Verify the torn-down observer cannot fire on post-unload events.
    let hits_before = obs_hits.load(Ordering::Relaxed);
    app.world_mut().trigger(ValueChange::<bool> {
        source: host_hud,
        value: true,
        is_final: true,
    });
    app.update();
    assert_eq!(obs_hits.load(Ordering::Relaxed), hits_before);

    // `mod_b` and persistent `host_hud` must remain completely intact.
    assert!(app.world().get_entity(host_hud).is_ok());
    assert!(app.world().get_entity(mod_b_root).is_ok());
    assert!(app.world().get_entity(mod_b_child).is_ok());

    // Now unload `mod_b` as well and confirm only `host_hud` survives.
    app.unload_mod(mod_b).expect("unload mod_b");
    assert!(app.world().get_entity(mod_b_root).is_err());
    assert!(app.world().get_entity(mod_b_child).is_err());
    assert!(app.world().get_entity(host_hud).is_ok());
    assert_eq!(
        app.world()
            .resource::<ModUiRegistry>()
            .tracked_ui_root_count(mod_b),
        0
    );

    // Post-unload update ticks remain clean.
    app.update();
}

#[test]
fn disabled_tabs_and_interaction_disabled_sync() {
    let mut app = setup_ui_app();
    let list = app
        .world_mut()
        .spawn(FfiTabList {
            disabled: FfiBool::TRUE,
            ..Default::default()
        })
        .id();
    let tab = app
        .world_mut()
        .spawn((
            ChildOf(list),
            FfiTab {
                disabled: FfiBool::TRUE,
                reserved0: [0; 3],
            },
        ))
        .id();

    app.update();
    assert!(app.world().get::<InteractionDisabled>(list).is_some());
    assert!(app.world().get::<InteractionDisabled>(tab).is_some());

    app.world_mut()
        .get_mut::<FfiTabList>(list)
        .unwrap()
        .disabled = FfiBool::FALSE;
    app.world_mut().get_mut::<FfiTab>(tab).unwrap().disabled = FfiBool::FALSE;
    app.update();
    assert!(app.world().get::<InteractionDisabled>(list).is_none());
    assert!(app.world().get::<InteractionDisabled>(tab).is_none());
}
