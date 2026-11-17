//! C99 and Zig declaration emission (`ModComponent::C_DECL` and `ModComponent::ZIG_DECL`).

use crate::classify::FieldKind;
use crate::codegen::CodegenState;
use syn::{Ident, Type};

/// Maps a syntactic Rust primitive or ABI primitive type to `(c_type, zig_type)`.
fn map_scalar_type(ty: &Type) -> Option<(&'static str, &'static str)> {
    if let Type::Path(tp) = ty
        && tp.qself.is_none()
        && let Some(last) = tp.path.segments.last()
        && last.arguments.is_empty()
    {
        return match last.ident.to_string().as_str() {
            "f32" => Some(("float", "f32")),
            "f64" => Some(("double", "f64")),
            "i8" => Some(("int8_t", "i8")),
            "i16" => Some(("int16_t", "i16")),
            "i32" => Some(("int32_t", "i32")),
            "i64" => Some(("int64_t", "i64")),
            "u8" => Some(("uint8_t", "u8")),
            "u16" => Some(("uint16_t", "u16")),
            "u32" => Some(("uint32_t", "u32")),
            "u64" => Some(("uint64_t", "u64")),
            "usize" => Some(("size_t", "usize")),
            "isize" => Some(("ptrdiff_t", "isize")),
            "FfiBool" => Some(("BamBool", "bam.BamBool")),
            "FfiEntity" | "TargetEntity" => Some(("BamEntity", "bam.BamEntity")),
            "BamComponent" => Some(("BamComponent", "bam.BamComponent")),
            "ComponentUuid" => Some(("BamComponentUuid", "bam.BamComponentUuid")),
            _ => None,
        };
    }
    None
}

/// Formats a `#[repr(C)]` `AbiPod` struct field in C99 (`"float x;"` or `"float weights[4];"`)
/// and Zig (`"x: f32,"` or `"weights: [4]f32,"`).
fn format_pod_struct_field(field_name: &str, ty: &Type) -> Option<(String, String)> {
    if let Some((c_ty, zig_ty)) = map_scalar_type(ty) {
        return Some((
            format!("    {c_ty} {field_name};"),
            format!("    {field_name}: {zig_ty},"),
        ));
    }
    match ty {
        Type::Array(arr) => {
            let len_tokens = &arr.len;
            let len_str = quote::quote!(#len_tokens).to_string().replace(' ', "");
            let elem_ty = &*arr.elem;
            let (elem_c, elem_zig) = if let Some((c, z)) = map_scalar_type(elem_ty) {
                (c.to_owned(), z.to_owned())
            } else if let Type::Path(tp) = elem_ty
                && tp.qself.is_none()
                && let Some(last) = tp.path.segments.last()
                && last.arguments.is_empty()
            {
                let n = last.ident.to_string();
                (n.clone(), n)
            } else {
                return None;
            };
            Some((
                format!("    {elem_c} {field_name}[{len_str}];"),
                format!("    {field_name}: [{len_str}]{elem_zig},"),
            ))
        }
        Type::Path(tp) if tp.qself.is_none() => {
            if let Some(last) = tp.path.segments.last()
                && last.arguments.is_empty()
            {
                let ident = last.ident.to_string();
                Some((
                    format!("    {ident} {field_name};"),
                    format!("    {field_name}: {ident},"),
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Recorded field metadata used solely to synthesize `ModComponent::C_DECL` and `ZIG_DECL`.
pub(crate) struct DeclFieldInfo {
    pub(crate) c_field_ident: String,
    pub(crate) slot_suffix: String,
    pub(crate) kind: DeclFieldKind,
    pub(crate) raw_ty: Type,
}

pub(crate) enum DeclFieldKind {
    Bool,
    Primitive,
    HostString,
    HostVec { elem_ty: Box<Type> },
    Nested,
}

impl DeclFieldInfo {
    pub(crate) fn from_classified(
        raw_field_name: &str,
        kind: &FieldKind,
        raw_ty: &Type,
    ) -> Option<Self> {
        let c_field_ident = if raw_field_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            format!("_{raw_field_name}")
        } else {
            raw_field_name.to_owned()
        };
        let decl_kind = match kind {
            FieldKind::Skip => return None,
            FieldKind::Bool(_) => DeclFieldKind::Bool,
            FieldKind::Primitive { .. } => DeclFieldKind::Primitive,
            FieldKind::HostString(_) => DeclFieldKind::HostString,
            FieldKind::HostVec { elem_ty, .. } => DeclFieldKind::HostVec {
                elem_ty: elem_ty.clone(),
            },
            FieldKind::Nested { .. } => DeclFieldKind::Nested,
        };
        Some(Self {
            c_field_ident,
            slot_suffix: raw_field_name.to_owned(),
            kind: decl_kind,
            raw_ty: raw_ty.clone(),
        })
    }
}

/// Builds `(c_decl_string, zig_decl_string)` for a `#[derive(ModComponent)]` struct.
pub(crate) fn generate_decls(
    state: &CodegenState,
    vtable_ident: &Ident,
    is_abi_pod: bool,
) -> (String, String) {
    let struct_name = state.struct_ident.to_string();
    let vt_name = vtable_ident.to_string();
    let mut c_out = String::new();
    let mut zig_out = String::new();

    // 1. Struct declaration: full `typedef struct` / `extern struct` when `IS_ABI_POD` and non-empty
    //    (with mapped fields), otherwise an opaque C99 forward typedef (`typedef struct X X;`) so
    //    zero-sized types and `#[repr(Rust)]` components remain strictly valid under `-std=c99 -pedantic`.
    let pod_lines: Option<Vec<(String, String)>> = if is_abi_pod && !state.decl_fields.is_empty() {
        state
            .decl_fields
            .iter()
            .map(|f| format_pod_struct_field(&f.c_field_ident, &f.raw_ty))
            .collect()
    } else {
        None
    };

    if let Some(lines) = pod_lines {
        c_out.push_str(&format!("typedef struct {struct_name} {{\n"));
        zig_out.push_str(&format!("pub const {struct_name} = extern struct {{\n"));
        for (c_line, zig_line) in lines {
            c_out.push_str(&c_line);
            c_out.push('\n');
            zig_out.push_str(&zig_line);
            zig_out.push('\n');
        }
        c_out.push_str(&format!("}} {struct_name};\n\n"));
        zig_out.push_str("};\n\n");
    } else {
        c_out.push_str(&format!("typedef struct {struct_name} {struct_name};\n\n"));
        zig_out.push_str(&format!("pub const {struct_name} = opaque {{}};\n\n"));
    }

    // 2. Helper slice typedefs for `Vec<Elem>` fields so ISO C99 VTables compile cleanly.
    let mut c_vt_lines = Vec::new();
    let mut zig_vt_lines = Vec::new();
    let mut slot_count: usize = 0;

    for f in &state.decl_fields {
        let sfx = &f.slot_suffix;
        match &f.kind {
            DeclFieldKind::Bool => {
                c_vt_lines.push(format!("    BamBool   (*get_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!(
                    "    BamResult (*set_{sfx})(void* ptr, BamBool val);"
                ));
                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) bam.BamBool,"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, bam.BamBool) callconv(.c) bam.BamResult,"
                ));
                slot_count += 2;
            }
            DeclFieldKind::Primitive => {
                let (c_ty, zig_ty): (String, String) =
                    if let Some((c, z)) = map_scalar_type(&f.raw_ty) {
                        (c.to_owned(), z.to_owned())
                    } else if let Type::Path(tp) = &f.raw_ty
                        && let Some(last) = tp.path.segments.last()
                    {
                        let n = last.ident.to_string();
                        (n.clone(), n)
                    } else {
                        ("uintptr_t".to_owned(), "usize".to_owned())
                    };
                c_vt_lines.push(format!("    {c_ty} (*get_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!(
                    "    BamResult (*set_{sfx})(void* ptr, {c_ty} val);"
                ));
                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) {zig_ty},"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, {zig_ty}) callconv(.c) bam.BamResult,"
                ));
                slot_count += 2;
            }
            DeclFieldKind::HostString => {
                c_vt_lines.push(format!("    BamStrView (*get_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!(
                    "    BamResult  (*set_{sfx})(void* ptr, BamStrView view);"
                ));
                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) bam.BamStrView,"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, bam.BamStrView) callconv(.c) bam.BamResult,"
                ));
                slot_count += 2;
            }
            DeclFieldKind::HostVec { elem_ty } => {
                let (elem_c, elem_zig): (String, String) =
                    if let Some((c, z)) = map_scalar_type(elem_ty) {
                        (c.to_owned(), z.to_owned())
                    } else if let Type::Path(tp) = &**elem_ty
                        && let Some(last) = tp.path.segments.last()
                    {
                        let n = last.ident.to_string();
                        (n.clone(), n)
                    } else {
                        ("uint8_t".to_owned(), "u8".to_owned())
                    };
                let slice_ty_name = format!("{vt_name}_{sfx}_Slice");
                c_out.push_str(&format!(
                    "typedef struct {slice_ty_name} {{\n    const {elem_c}* ptr;\n    size_t len;\n}} {slice_ty_name};\n\n"
                ));
                zig_out.push_str(&format!(
                    "pub const {slice_ty_name} = extern struct {{\n    ptr: ?[*]const {elem_zig},\n    len: usize,\n}};\n\n"
                ));

                c_vt_lines.push(format!(
                    "    {slice_ty_name} (*get_{sfx})(const void* ptr);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*set_{sfx})(void* ptr, {slice_ty_name} slice);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*push_{sfx})(void* ptr, {elem_c} val);"
                ));
                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) {slice_ty_name},"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, {slice_ty_name}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    push_{sfx}: ?*const fn (?*anyopaque, {elem_zig}) callconv(.c) bam.BamResult,"
                ));
                slot_count += 3;
            }
            DeclFieldKind::Nested => {
                c_vt_lines.push(format!("    const void* (*ptr_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!("    void*       (*ptr_mut_{sfx})(void* ptr);"));
                zig_vt_lines.push(format!(
                    "    ptr_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) ?*const anyopaque,"
                ));
                zig_vt_lines.push(format!(
                    "    ptr_mut_{sfx}: ?*const fn (?*anyopaque) callconv(.c) ?*anyopaque,"
                ));
                slot_count += 2;
            }
        }
    }

    // 3. VTable declaration + static size/offset assertions (`VTableHeader` is 40 bytes on 64-bit,
    //    32 bytes on 32-bit, followed by `slot_count` function pointers).
    c_out.push_str(&format!("typedef struct {vt_name} {{\n"));
    c_out.push_str("    BamVTableHeader header;\n");
    for line in &c_vt_lines {
        c_out.push_str(line);
        c_out.push('\n');
    }
    c_out.push_str(&format!("}} {vt_name};\n"));

    let size64 = 40 + slot_count * 8;
    let size32 = 32 + slot_count * 4;
    let assert_tag = crate::attrs::sanitize_symbol_segment(&vt_name).to_ascii_lowercase();
    c_out.push_str(&format!(
        "BAM_STATIC_ASSERT(offsetof({vt_name}, header) == 0u, {assert_tag}_header_off);\n"
    ));
    c_out.push_str(&format!(
        "#if UINTPTR_MAX == UINT64_MAX\nBAM_STATIC_ASSERT(sizeof({vt_name}) == {size64}u, {assert_tag}_size64);\n#elif UINTPTR_MAX == UINT32_MAX\nBAM_STATIC_ASSERT(sizeof({vt_name}) == {size32}u, {assert_tag}_size32);\n#endif\n"
    ));

    zig_out.push_str(&format!("pub const {vt_name} = extern struct {{\n"));
    zig_out.push_str("    header: bam.BamVTableHeader,\n");
    for line in &zig_vt_lines {
        zig_out.push_str(line);
        zig_out.push('\n');
    }
    zig_out.push_str("};\n");

    (c_out, zig_out)
}
