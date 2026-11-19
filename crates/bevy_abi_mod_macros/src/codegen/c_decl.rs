//! C99, C++17 (`#ifdef __cplusplus`), and Zig declaration emission (`ModComponent::C_DECL` and `ModComponent::ZIG_DECL`).

use crate::attrs::{IntRepr, to_snake_case};
use crate::classify::FieldKind;
use crate::codegen::CodegenState;
use std::fmt::Write as _;
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

fn map_type_name(ty: &Type, fallback_c: &str, fallback_zig: &str) -> (String, String) {
    if let Some((c, z)) = map_scalar_type(ty) {
        (c.to_owned(), z.to_owned())
    } else if let Type::Path(tp) = ty
        && tp.qself.is_none()
        && let Some(last) = tp.path.segments.last()
        && last.arguments.is_empty()
    {
        let n = last.ident.to_string();
        (n.clone(), n)
    } else {
        (fallback_c.to_owned(), fallback_zig.to_owned())
    }
}

fn format_c_zig_field_decl(field_name: &str, ty: &Type) -> Option<(String, String)> {
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

#[derive(Clone)]
pub(crate) enum DeclFieldKind {
    Bool,
    Primitive,
    HostString,
    HostVec { elem_ty: Box<Type> },
    FfiOption { inner_ty: Box<Type> },
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
            FieldKind::FfiOption { inner_ty, .. } => DeclFieldKind::FfiOption {
                inner_ty: inner_ty.clone(),
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

pub(crate) struct DeclEnumVariantField {
    pub(crate) field_name: String,
    pub(crate) c_field_ident: String,
    pub(crate) kind: DeclFieldKind,
    pub(crate) raw_ty: Type,
}

pub(crate) struct DeclEnumVariant {
    pub(crate) name: String,
    pub(crate) snake_name: String,
    pub(crate) discriminant: i128,
    pub(crate) has_skipped: bool,
    pub(crate) fields: Vec<DeclEnumVariantField>,
    pub(crate) schema_fields: Vec<(String, String)>,
}

pub(crate) struct DeclEnumInfo {
    pub(crate) is_fieldless: bool,
    pub(crate) int_repr: IntRepr,
    pub(crate) variants: Vec<DeclEnumVariant>,
}

fn option_typedef_name(struct_name: &str, sfx: &str) -> String {
    format!("{struct_name}_{sfx}_Option")
}

fn slice_typedef_name(vt_name: &str, sfx: &str) -> String {
    format!("{vt_name}_{sfx}_Slice")
}

fn emit_option_typedef(
    c_out: &mut String,
    zig_out: &mut String,
    opt_ty_name: &str,
    inner_ty: &Type,
) -> Option<()> {
    let (c_val_line, zig_val_line) = format_c_zig_field_decl("value", inner_ty)?;
    let _ = writeln!(c_out, "typedef struct {opt_ty_name} {{");
    let _ = writeln!(c_out, "    BamBool is_some;");
    let _ = writeln!(c_out, "{c_val_line}");
    let _ = writeln!(c_out, "}} {opt_ty_name};\n");

    let _ = writeln!(zig_out, "pub const {opt_ty_name} = extern struct {{");
    let _ = writeln!(zig_out, "    is_some: bam.BamBool,");
    let _ = writeln!(zig_out, "{zig_val_line}");
    let _ = writeln!(zig_out, "}};\n");
    Some(())
}

fn emit_cpp_shared_helpers(cpp: &mut String) {
    cpp.push_str(
        r#"namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
"#,
    );
}

#[allow(clippy::too_many_lines)]
fn generate_enum_decls(
    state: &CodegenState,
    enum_info: &DeclEnumInfo,
    vtable_ident: &Ident,
) -> (String, String) {
    let enum_name = state.struct_ident.to_string();
    let vt_name = vtable_ident.to_string();
    let c_tag_ty = enum_info.int_repr.c_ty_str();
    let zig_tag_ty = enum_info.int_repr.zig_ty_str();

    let mut c_out = String::new();
    let mut zig_out = String::new();

    // 1. Enum type & tag constants
    if enum_info.is_fieldless {
        if !enum_info.int_repr.is_some() {
            let _ = writeln!(c_out, "typedef enum {enum_name} {{");
            for (idx, var) in enum_info.variants.iter().enumerate() {
                let comma = if idx + 1 < enum_info.variants.len() {
                    ","
                } else {
                    ""
                };
                let _ = writeln!(
                    c_out,
                    "    {enum_name}_{} = {}{comma}",
                    var.name, var.discriminant
                );
            }
            let _ = writeln!(c_out, "}} {enum_name};\n");
        } else {
            let _ = writeln!(c_out, "typedef {c_tag_ty} {enum_name};");
            for var in &enum_info.variants {
                let _ = writeln!(
                    c_out,
                    "#define {enum_name}_{} (({c_tag_ty}){})",
                    var.name, var.discriminant
                );
            }
            c_out.push('\n');
        }

        let _ = writeln!(zig_out, "pub const {enum_name} = enum({zig_tag_ty}) {{");
        for var in &enum_info.variants {
            let _ = writeln!(zig_out, "    {} = {},", var.name, var.discriminant);
        }
        let _ = writeln!(zig_out, "}};\n");
    } else {
        let _ = writeln!(c_out, "typedef struct {enum_name} {enum_name};");
        for var in &enum_info.variants {
            let _ = writeln!(
                c_out,
                "#define {enum_name}_Tag_{} (({c_tag_ty}){})",
                var.name, var.discriminant
            );
        }
        c_out.push('\n');

        let _ = writeln!(zig_out, "pub const {enum_name} = opaque {{}};");
        let _ = writeln!(zig_out, "pub const {enum_name}Tag = enum({zig_tag_ty}) {{");
        for var in &enum_info.variants {
            let _ = writeln!(zig_out, "    {} = {},", var.name, var.discriminant);
        }
        let _ = writeln!(zig_out, "}};\n");
    }

    // 2. Helper slice / option typedefs for variant fields
    for var in &enum_info.variants {
        for f in &var.fields {
            let pfx = format!("{}_{}", var.snake_name, f.field_name);
            match &f.kind {
                DeclFieldKind::HostVec { elem_ty } => {
                    let (elem_c, elem_zig) = map_type_name(elem_ty, "uint8_t", "u8");
                    let slice_ty = slice_typedef_name(&vt_name, &pfx);
                    let _ = writeln!(
                        c_out,
                        "typedef struct {slice_ty} {{\n    const {elem_c}* ptr;\n    size_t len;\n}} {slice_ty};\n"
                    );
                    let _ = writeln!(
                        zig_out,
                        "pub const {slice_ty} = extern struct {{\n    ptr: ?[*]const {elem_zig},\n    len: usize,\n}};\n"
                    );
                }
                DeclFieldKind::FfiOption { inner_ty } => {
                    let opt_ty = option_typedef_name(&enum_name, &pfx);
                    let _ = emit_option_typedef(&mut c_out, &mut zig_out, &opt_ty, inner_ty);
                }
                _ => {}
            }
        }
    }

    // 3. VTable struct slots
    let mut c_vt_lines = Vec::new();
    let mut zig_vt_lines = Vec::new();
    let mut slot_count: usize = 0;

    c_vt_lines.push(format!("    {c_tag_ty} (*tag)(const void* ptr);"));
    zig_vt_lines.push(format!(
        "    tag: ?*const fn (?*const anyopaque) callconv(.c) {zig_tag_ty},"
    ));
    slot_count += 1;

    for var in &enum_info.variants {
        let v_snake = &var.snake_name;
        if !var.has_skipped {
            let mut c_params = String::from("void* ptr");
            let mut zig_params = String::from("?*anyopaque");
            for f in &var.fields {
                let pfx = format!("{v_snake}_{}", f.field_name);
                match &f.kind {
                    DeclFieldKind::Bool => {
                        let _ = write!(c_params, ", BamBool {}", f.c_field_ident);
                        zig_params.push_str(", bam.BamBool");
                    }
                    DeclFieldKind::Primitive => {
                        let (c_ty, zig_ty) = map_type_name(&f.raw_ty, "uintptr_t", "usize");
                        let _ = write!(c_params, ", {c_ty} {}", f.c_field_ident);
                        let _ = write!(zig_params, ", {zig_ty}");
                    }
                    DeclFieldKind::HostString => {
                        let _ = write!(c_params, ", BamStrView {}", f.c_field_ident);
                        zig_params.push_str(", bam.BamStrView");
                    }
                    DeclFieldKind::HostVec { .. } => {
                        let slice_ty = slice_typedef_name(&vt_name, &pfx);
                        let _ = write!(c_params, ", {slice_ty} {}", f.c_field_ident);
                        let _ = write!(zig_params, ", {slice_ty}");
                    }
                    DeclFieldKind::FfiOption { .. } => {
                        let opt_ty = option_typedef_name(&enum_name, &pfx);
                        let _ = write!(c_params, ", {opt_ty} {}", f.c_field_ident);
                        let _ = write!(zig_params, ", {opt_ty}");
                    }
                    DeclFieldKind::Nested => {
                        let _ = write!(c_params, ", const void* {}", f.c_field_ident);
                        zig_params.push_str(", ?*const anyopaque");
                    }
                }
            }
            c_vt_lines.push(format!("    BamResult (*set_{v_snake})({c_params});"));
            zig_vt_lines.push(format!(
                "    set_{v_snake}: ?*const fn ({zig_params}) callconv(.c) bam.BamResult,"
            ));
            slot_count += 1;
        }

        for f in &var.fields {
            let pfx = format!("{v_snake}_{}", f.field_name);
            let slot = format!("try_get_{pfx}");
            let (c_out_ty, zig_out_ty) = match &f.kind {
                DeclFieldKind::Bool => ("BamBool".to_owned(), "bam.BamBool".to_owned()),
                DeclFieldKind::Primitive => map_type_name(&f.raw_ty, "uintptr_t", "usize"),
                DeclFieldKind::HostString => ("BamStrView".to_owned(), "bam.BamStrView".to_owned()),
                DeclFieldKind::HostVec { .. } => {
                    let s = slice_typedef_name(&vt_name, &pfx);
                    (s.clone(), s)
                }
                DeclFieldKind::FfiOption { .. } => {
                    let o = option_typedef_name(&enum_name, &pfx);
                    (o.clone(), o)
                }
                DeclFieldKind::Nested => ("const void*".to_owned(), "?*const anyopaque".to_owned()),
            };
            c_vt_lines.push(format!(
                "    BamResult (*{slot})(const void* ptr, {c_out_ty}* out_val);"
            ));
            zig_vt_lines.push(format!(
                "    {slot}: ?*const fn (?*const anyopaque, ?*{zig_out_ty}) callconv(.c) bam.BamResult,"
            ));
            slot_count += 1;
        }
    }

    let _ = writeln!(c_out, "typedef struct {vt_name} {{");
    c_out.push_str("    BamVTableHeader header;\n");
    for line in &c_vt_lines {
        c_out.push_str(line);
        c_out.push('\n');
    }
    let _ = writeln!(c_out, "}} {vt_name};");

    let size64 = 40 + slot_count * 8;
    let size32 = 32 + slot_count * 4;
    let assert_tag = crate::attrs::sanitize_symbol_segment(&vt_name).to_ascii_lowercase();
    let _ = writeln!(
        c_out,
        "BAM_STATIC_ASSERT(offsetof({vt_name}, header) == 0u, {assert_tag}_header_off);"
    );
    let _ = writeln!(
        c_out,
        "#if UINTPTR_MAX == UINT64_MAX\nBAM_STATIC_ASSERT(sizeof({vt_name}) == {size64}u, {assert_tag}_size64);\n#elif UINTPTR_MAX == UINT32_MAX\nBAM_STATIC_ASSERT(sizeof({vt_name}) == {size32}u, {assert_tag}_size32);\n#endif\n"
    );

    // 4. C++17 noexcept view classes for enum
    c_out.push_str("#ifdef __cplusplus\nextern \"C++\" {\n");
    emit_cpp_shared_helpers(&mut c_out);

    let mut cpp_const_methods = String::new();
    let mut cpp_mut_methods = String::new();

    let _ = writeln!(
        cpp_const_methods,
        "    {c_tag_ty} tag() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->tag != nullptr) ? vt_->tag(ptr_) : static_cast<{c_tag_ty}>(0);\n    }}"
    );
    let _ = writeln!(
        cpp_mut_methods,
        "    {c_tag_ty} tag() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->tag != nullptr) ? vt_->tag(ptr_) : static_cast<{c_tag_ty}>(0);\n    }}"
    );

    for var in &enum_info.variants {
        let v_snake = &var.snake_name;
        if !var.has_skipped {
            let mut sig_params = String::new();
            let mut call_args = String::from("ptr_");
            for (idx, f) in var.fields.iter().enumerate() {
                if idx > 0 {
                    sig_params.push_str(", ");
                }
                let pfx = format!("{v_snake}_{}", f.field_name);
                let arg_name = &f.c_field_ident;
                match &f.kind {
                    DeclFieldKind::Bool => {
                        let _ = write!(sig_params, "bool {arg_name}");
                        let _ = write!(call_args, ", {arg_name} ? BAM_TRUE : BAM_FALSE");
                    }
                    DeclFieldKind::Primitive => {
                        let (c_ty, _) = map_type_name(&f.raw_ty, "uintptr_t", "usize");
                        let _ = write!(sig_params, "{c_ty} {arg_name}");
                        let _ = write!(call_args, ", {arg_name}");
                    }
                    DeclFieldKind::HostString => {
                        let _ = write!(sig_params, "BamStrView {arg_name}");
                        let _ = write!(call_args, ", {arg_name}");
                    }
                    DeclFieldKind::HostVec { .. } => {
                        let slice_ty = slice_typedef_name(&vt_name, &pfx);
                        let _ = write!(sig_params, "{slice_ty} {arg_name}");
                        let _ = write!(call_args, ", {arg_name}");
                    }
                    DeclFieldKind::FfiOption { .. } => {
                        let opt_ty = option_typedef_name(&enum_name, &pfx);
                        let _ = write!(sig_params, "{opt_ty} {arg_name}");
                        let _ = write!(call_args, ", {arg_name}");
                    }
                    DeclFieldKind::Nested => {
                        let _ = write!(sig_params, "const void* {arg_name}");
                        let _ = write!(call_args, ", {arg_name}");
                    }
                }
            }
            let _ = writeln!(
                cpp_mut_methods,
                "    BamResult set_{v_snake}({sig_params}) noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_{v_snake} != nullptr) ? vt_->set_{v_snake}({call_args}) : BAM_RESULT_NULL_POINTER;\n    }}"
            );
        }

        for f in &var.fields {
            let pfx = format!("{v_snake}_{}", f.field_name);
            let slot = format!("try_get_{pfx}");
            let c_out_ty = match &f.kind {
                DeclFieldKind::Bool => "BamBool".to_owned(),
                DeclFieldKind::Primitive => map_type_name(&f.raw_ty, "uintptr_t", "usize").0,
                DeclFieldKind::HostString => "BamStrView".to_owned(),
                DeclFieldKind::HostVec { .. } => slice_typedef_name(&vt_name, &pfx),
                DeclFieldKind::FfiOption { .. } => option_typedef_name(&enum_name, &pfx),
                DeclFieldKind::Nested => "const void*".to_owned(),
            };
            let method = format!(
                "    BamResult {slot}({c_out_ty}* out_val) const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->{slot} != nullptr) ? vt_->{slot}(ptr_, out_val) : BAM_RESULT_NULL_POINTER;\n    }}\n"
            );
            cpp_const_methods.push_str(&method);
            cpp_mut_methods.push_str(&method);
        }
    }

    let _ = writeln!(
        c_out,
        "class {enum_name}ConstView {{\npublic:\n    {enum_name}ConstView() noexcept = default;\n    {enum_name}ConstView(const void* ptr, const {vt_name}* vt) noexcept : ptr_(ptr), vt_(vt) {{}}\n    bool valid() const noexcept {{ return ptr_ != nullptr && vt_ != nullptr; }}\n    explicit operator bool() const noexcept {{ return valid(); }}\n    const void* raw() const noexcept {{ return ptr_; }}\n    const {vt_name}* vtable() const noexcept {{ return vt_; }}\n{cpp_const_methods}private:\n    const void* ptr_{{nullptr}};\n    const {vt_name}* vt_{{nullptr}};\n}};\n"
    );
    let _ = writeln!(
        c_out,
        "class {enum_name}View {{\npublic:\n    {enum_name}View() noexcept = default;\n    {enum_name}View(void* ptr, const {vt_name}* vt) noexcept : ptr_(ptr), vt_(vt) {{}}\n    bool valid() const noexcept {{ return ptr_ != nullptr && vt_ != nullptr; }}\n    explicit operator bool() const noexcept {{ return valid(); }}\n    void* raw() const noexcept {{ return ptr_; }}\n    const {vt_name}* vtable() const noexcept {{ return vt_; }}\n    {enum_name}ConstView as_const() const noexcept {{ return {enum_name}ConstView(ptr_, vt_); }}\n{cpp_mut_methods}private:\n    void* ptr_{{nullptr}};\n    const {vt_name}* vt_{{nullptr}};\n}};\n"
    );
    c_out.push_str("} // extern \"C++\"\n#endif /* __cplusplus */\n");

    // 5. Zig VTable + comptime View wrapper
    let _ = writeln!(zig_out, "pub const {vt_name} = extern struct {{");
    zig_out.push_str("    header: bam.BamVTableHeader,\n");
    for line in &zig_vt_lines {
        zig_out.push_str(line);
        zig_out.push('\n');
    }
    zig_out.push_str("};\n\n");

    let mut zig_view_methods = String::new();
    let _ = writeln!(
        zig_view_methods,
        "    pub fn tag(self: @This()) {zig_tag_ty} {{\n        const f = self.vt.tag orelse return 0;\n        return f(self.ptr);\n    }}"
    );
    for var in &enum_info.variants {
        let v_snake = &var.snake_name;
        if !var.has_skipped {
            let mut zig_sig = String::new();
            let mut zig_args = String::from("self.ptr");
            for f in &var.fields {
                let pfx = format!("{v_snake}_{}", f.field_name);
                let arg_name = &f.c_field_ident;
                let zig_ty = match &f.kind {
                    DeclFieldKind::Bool => "bam.BamBool".to_owned(),
                    DeclFieldKind::Primitive => map_type_name(&f.raw_ty, "uintptr_t", "usize").1,
                    DeclFieldKind::HostString => "bam.BamStrView".to_owned(),
                    DeclFieldKind::HostVec { .. } => slice_typedef_name(&vt_name, &pfx),
                    DeclFieldKind::FfiOption { .. } => option_typedef_name(&enum_name, &pfx),
                    DeclFieldKind::Nested => "?*const anyopaque".to_owned(),
                };
                let _ = write!(zig_sig, ", {arg_name}: {zig_ty}");
                let _ = write!(zig_args, ", {arg_name}");
            }
            let _ = writeln!(
                zig_view_methods,
                "    pub fn set_{v_snake}(self: @This(){zig_sig}) bam.BamResult {{\n        const f = self.vt.set_{v_snake} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f({zig_args});\n    }}"
            );
        }
        for f in &var.fields {
            let pfx = format!("{v_snake}_{}", f.field_name);
            let slot = format!("try_get_{pfx}");
            let zig_out_ty = match &f.kind {
                DeclFieldKind::Bool => "bam.BamBool".to_owned(),
                DeclFieldKind::Primitive => map_type_name(&f.raw_ty, "uintptr_t", "usize").1,
                DeclFieldKind::HostString => "bam.BamStrView".to_owned(),
                DeclFieldKind::HostVec { .. } => slice_typedef_name(&vt_name, &pfx),
                DeclFieldKind::FfiOption { .. } => option_typedef_name(&enum_name, &pfx),
                DeclFieldKind::Nested => "?*const anyopaque".to_owned(),
            };
            let _ = writeln!(
                zig_view_methods,
                "    pub fn {slot}(self: @This(), out_val: *{zig_out_ty}) bam.BamResult {{\n        const f = self.vt.{slot} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, out_val);\n    }}"
            );
        }
    }

    let _ = writeln!(
        zig_out,
        "pub const {enum_name}View = struct {{\n    ptr: *anyopaque,\n    vt: *const {vt_name},\n\n    comptime {{\n        if (@offsetOf({vt_name}, \"header\") != 0) @compileError(\"{vt_name} header must be at offset 0\");\n    }}\n\n    pub fn resolveVTable(\n        api: *const bam.BamHostApi,\n        host_ctx: ?*const bam.BamHostCtx,\n        symbol: [*:0]const u8,\n    ) !*const {vt_name} {{\n        const get_proc = api.get_proc_address orelse return error.NullPointer;\n        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;\n        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));\n        if (hdr.struct_size < @sizeOf({vt_name}) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {{\n            return error.InvalidVersion;\n        }}\n        return @ptrCast(@alignCast(raw));\n    }}\n\n    pub fn init(ptr: *anyopaque, vt: *const {vt_name}) @This() {{\n        return .{{ .ptr = ptr, .vt = vt }};\n    }}\n\n{zig_view_methods}}};\n"
    );

    // Prevent unused helper warning when enum has no PascalCase->snake collisions
    let _ = to_snake_case;

    (c_out, zig_out)
}

/// Builds `(c_decl_string, zig_decl_string)` for a `#[derive(ModComponent)]` struct or enum.
#[allow(clippy::too_many_lines)]
pub(crate) fn generate_decls(
    state: &CodegenState,
    vtable_ident: &Ident,
    is_abi_pod: bool,
) -> (String, String) {
    if let Some(ref enum_info) = state.decl_enum {
        return generate_enum_decls(state, enum_info, vtable_ident);
    }

    let struct_name = state.struct_ident.to_string();
    let vt_name = vtable_ident.to_string();
    let mut c_out = String::new();
    let mut zig_out = String::new();

    // 1. Emit helper `FfiOption<T>` struct typedefs BEFORE the enclosing struct definition so POD
    //    structs containing `FfiOption<T>` fields can embed them directly by value in C99 and Zig.
    for f in &state.decl_fields {
        if let DeclFieldKind::FfiOption { inner_ty } = &f.kind {
            let opt_ty_name = option_typedef_name(&struct_name, &f.slot_suffix);
            let _ = emit_option_typedef(&mut c_out, &mut zig_out, &opt_ty_name, inner_ty);
        }
    }

    // 2. Struct declaration: full `typedef struct` / `extern struct` when `IS_ABI_POD` and non-empty
    //    (with mapped fields), otherwise an opaque C99 forward typedef (`typedef struct X X;`) so
    //    zero-sized types and `#[repr(Rust)]` components remain strictly valid under `-std=c99 -pedantic`.
    let pod_lines: Option<Vec<(String, String)>> = if is_abi_pod && !state.decl_fields.is_empty() {
        state
            .decl_fields
            .iter()
            .map(|f| match &f.kind {
                DeclFieldKind::FfiOption { .. } => {
                    let opt_ty = option_typedef_name(&struct_name, &f.slot_suffix);
                    Some((
                        format!("    {opt_ty} {};", f.c_field_ident),
                        format!("    {}: {opt_ty},", f.c_field_ident),
                    ))
                }
                _ => format_c_zig_field_decl(&f.c_field_ident, &f.raw_ty),
            })
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

    // 3. Helper slice typedefs & VTable slots.
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
                let (c_ty, zig_ty) = map_type_name(&f.raw_ty, "uintptr_t", "usize");
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
            DeclFieldKind::FfiOption { .. } => {
                let opt_ty = option_typedef_name(&struct_name, sfx);
                c_vt_lines.push(format!("    {opt_ty} (*get_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!(
                    "    BamResult (*set_{sfx})(void* ptr, {opt_ty} val);"
                ));
                c_vt_lines.push(format!("    BamResult (*clear_{sfx})(void* ptr);"));
                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) {opt_ty},"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, {opt_ty}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    clear_{sfx}: ?*const fn (?*anyopaque) callconv(.c) bam.BamResult,"
                ));
                slot_count += 3;
            }
            DeclFieldKind::HostVec { elem_ty } => {
                let (elem_c, elem_zig) = map_type_name(elem_ty, "uint8_t", "u8");
                let slice_ty_name = slice_typedef_name(&vt_name, sfx);
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
                c_vt_lines.push(format!("    size_t (*len_{sfx})(const void* ptr);"));
                c_vt_lines.push(format!("    BamResult (*clear_{sfx})(void* ptr);"));
                c_vt_lines.push(format!(
                    "    BamResult (*push_{sfx})(void* ptr, {elem_c} val);"
                ));
                c_vt_lines.push(format!(
                    "    BamBool (*pop_{sfx})(void* ptr, {elem_c}* out_elem);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*remove_{sfx})(void* ptr, size_t index, {elem_c}* out_elem);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*swap_remove_{sfx})(void* ptr, size_t index, {elem_c}* out_elem);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*truncate_{sfx})(void* ptr, size_t new_len);"
                ));
                c_vt_lines.push(format!(
                    "    BamResult (*reserve_{sfx})(void* ptr, size_t additional);"
                ));
                c_vt_lines.push(format!(
                    "    {elem_c}* (*as_mut_slice_{sfx})(void* ptr, size_t* out_len);"
                ));

                zig_vt_lines.push(format!(
                    "    get_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) {slice_ty_name},"
                ));
                zig_vt_lines.push(format!(
                    "    set_{sfx}: ?*const fn (?*anyopaque, {slice_ty_name}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    len_{sfx}: ?*const fn (?*const anyopaque) callconv(.c) usize,"
                ));
                zig_vt_lines.push(format!(
                    "    clear_{sfx}: ?*const fn (?*anyopaque) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    push_{sfx}: ?*const fn (?*anyopaque, {elem_zig}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    pop_{sfx}: ?*const fn (?*anyopaque, ?*{elem_zig}) callconv(.c) bam.BamBool,"
                ));
                zig_vt_lines.push(format!(
                    "    remove_{sfx}: ?*const fn (?*anyopaque, usize, ?*{elem_zig}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    swap_remove_{sfx}: ?*const fn (?*anyopaque, usize, ?*{elem_zig}) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    truncate_{sfx}: ?*const fn (?*anyopaque, usize) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    reserve_{sfx}: ?*const fn (?*anyopaque, usize) callconv(.c) bam.BamResult,"
                ));
                zig_vt_lines.push(format!(
                    "    as_mut_slice_{sfx}: ?*const fn (?*anyopaque, ?*usize) callconv(.c) ?[*]{elem_zig},"
                ));
                slot_count += 11;
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

    // 4. VTable declaration + static size/offset assertions (`VTableHeader` is 40 bytes on 64-bit,
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

    // 5. `#ifdef __cplusplus` C++17 noexcept RAII & typed view classes
    c_out.push_str("#ifdef __cplusplus\nextern \"C++\" {\n");
    emit_cpp_shared_helpers(&mut c_out);

    let mut cpp_vec_ops_decls = String::new();
    let mut cpp_const_methods = String::new();
    let mut cpp_mut_methods = String::new();

    for f in &state.decl_fields {
        let sfx = &f.slot_suffix;
        let method_name = &f.c_field_ident;
        match &f.kind {
            DeclFieldKind::Bool => {
                let _ = writeln!(
                    cpp_const_methods,
                    "    bool get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? (vt_->get_{sfx}(ptr_) != BAM_FALSE) : false;\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    bool get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? (vt_->get_{sfx}(ptr_) != BAM_FALSE) : false;\n    }}\n    BamResult set_{method_name}(bool val) noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_{sfx} != nullptr) ? vt_->set_{sfx}(ptr_, val ? BAM_TRUE : BAM_FALSE) : BAM_RESULT_NULL_POINTER;\n    }}"
                );
            }
            DeclFieldKind::Primitive => {
                let (c_ty, _) = map_type_name(&f.raw_ty, "uintptr_t", "usize");
                let _ = writeln!(
                    cpp_const_methods,
                    "    {c_ty} get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : {c_ty}{{}};\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    {c_ty} get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : {c_ty}{{}};\n    }}\n    BamResult set_{method_name}({c_ty} val) noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_{sfx} != nullptr) ? vt_->set_{sfx}(ptr_, val) : BAM_RESULT_NULL_POINTER;\n    }}"
                );
            }
            DeclFieldKind::HostString => {
                let _ = writeln!(
                    cpp_const_methods,
                    "    BamStrView get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : BamStrView{{nullptr, 0u}};\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    BamStrView get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : BamStrView{{nullptr, 0u}};\n    }}\n    BamResult set_{method_name}(BamStrView view) noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_{sfx} != nullptr) ? vt_->set_{sfx}(ptr_, view) : BAM_RESULT_NULL_POINTER;\n    }}\n    BamResult set_{method_name}(const char* cstr) noexcept {{\n        const BamStrView view{{reinterpret_cast<const uint8_t*>(cstr), cstr != nullptr ? strlen(cstr) : 0u}};\n        return set_{method_name}(view);\n    }}"
                );
            }
            DeclFieldKind::FfiOption { inner_ty } => {
                let opt_ty = option_typedef_name(&struct_name, sfx);
                let (elem_c, _) = map_type_name(inner_ty, "uint8_t", "u8");
                let _ = writeln!(
                    cpp_const_methods,
                    "    {opt_ty} get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : {opt_ty}{{BAM_FALSE, {{}}}};\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    {opt_ty} get_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_{sfx} != nullptr) ? vt_->get_{sfx}(ptr_) : {opt_ty}{{BAM_FALSE, {{}}}};\n    }}\n    BamResult set_{method_name}({opt_ty} val) noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_{sfx} != nullptr) ? vt_->set_{sfx}(ptr_, val) : BAM_RESULT_NULL_POINTER;\n    }}\n    BamResult set_{method_name}_some({elem_c} val) noexcept {{\n        return set_{method_name}({opt_ty}{{BAM_TRUE, val}});\n    }}\n    BamResult clear_{method_name}() noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->clear_{sfx} != nullptr) ? vt_->clear_{sfx}(ptr_) : BAM_RESULT_NULL_POINTER;\n    }}"
                );
            }
            DeclFieldKind::HostVec { elem_ty } => {
                let (elem_c, _) = map_type_name(elem_ty, "uint8_t", "u8");
                let slice_ty = slice_typedef_name(&vt_name, sfx);
                let ops_name = format!("{vt_name}_{sfx}_VecOps");
                let const_ops_name = format!("{vt_name}_{sfx}_ConstVecOps");
                let _ = writeln!(
                    cpp_vec_ops_decls,
                    "struct {const_ops_name} {{\n    {slice_ty} (*get)(const void*) = nullptr;\n    size_t (*len)(const void*) = nullptr;\n    static {const_ops_name} from_vtable(const {vt_name}* vt) noexcept {{\n        if (vt == nullptr) return {const_ops_name}{{}};\n        return {const_ops_name}{{vt->get_{sfx}, vt->len_{sfx}}};\n    }}\n}};\nstruct {ops_name} {{\n    {slice_ty} (*get)(const void*) = nullptr;\n    BamResult (*set)(void*, {slice_ty}) = nullptr;\n    size_t (*len)(const void*) = nullptr;\n    BamResult (*clear)(void*) = nullptr;\n    BamResult (*push)(void*, {elem_c}) = nullptr;\n    BamBool (*pop)(void*, {elem_c}*) = nullptr;\n    BamResult (*remove)(void*, size_t, {elem_c}*) = nullptr;\n    BamResult (*swap_remove)(void*, size_t, {elem_c}*) = nullptr;\n    BamResult (*truncate)(void*, size_t) = nullptr;\n    BamResult (*reserve)(void*, size_t) = nullptr;\n    {elem_c}* (*as_mut_slice)(void*, size_t*) = nullptr;\n    bool valid() const noexcept {{\n        return get != nullptr && set != nullptr && len != nullptr && clear != nullptr &&\n               push != nullptr && pop != nullptr && remove != nullptr && swap_remove != nullptr &&\n               truncate != nullptr && reserve != nullptr && as_mut_slice != nullptr;\n    }}\n    static {ops_name} from_vtable(const {vt_name}* vt) noexcept {{\n        if (vt == nullptr) return {ops_name}{{}};\n        return {ops_name}{{\n            vt->get_{sfx}, vt->set_{sfx}, vt->len_{sfx}, vt->clear_{sfx},\n            vt->push_{sfx}, vt->pop_{sfx}, vt->remove_{sfx}, vt->swap_remove_{sfx},\n            vt->truncate_{sfx}, vt->reserve_{sfx}, vt->as_mut_slice_{sfx}\n        }};\n    }}\n}};\nusing {vt_name}_{sfx}_ConstVecView = bam::ConstHostVecView<{elem_c}, {slice_ty}, {const_ops_name}>;\nusing {vt_name}_{sfx}_VecView = bam::HostVecView<{elem_c}, {slice_ty}, {ops_name}>;\n"
                );
                let _ = writeln!(
                    cpp_const_methods,
                    "    {vt_name}_{sfx}_ConstVecView {method_name}() const noexcept {{\n        return {vt_name}_{sfx}_ConstVecView(ptr_, {const_ops_name}::from_vtable(vt_));\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    {vt_name}_{sfx}_VecView {method_name}() noexcept {{\n        return {vt_name}_{sfx}_VecView(ptr_, {ops_name}::from_vtable(vt_));\n    }}\n    {vt_name}_{sfx}_ConstVecView {method_name}() const noexcept {{\n        return {vt_name}_{sfx}_ConstVecView(ptr_, {const_ops_name}::from_vtable(vt_));\n    }}"
                );
            }
            DeclFieldKind::Nested => {
                let _ = writeln!(
                    cpp_const_methods,
                    "    const void* ptr_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_{sfx} != nullptr) ? vt_->ptr_{sfx}(ptr_) : nullptr;\n    }}"
                );
                let _ = writeln!(
                    cpp_mut_methods,
                    "    const void* ptr_{method_name}() const noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_{sfx} != nullptr) ? vt_->ptr_{sfx}(ptr_) : nullptr;\n    }}\n    void* ptr_mut_{method_name}() noexcept {{\n        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_mut_{sfx} != nullptr) ? vt_->ptr_mut_{sfx}(ptr_) : nullptr;\n    }}"
                );
            }
        }
    }

    c_out.push_str(&cpp_vec_ops_decls);
    let _ = writeln!(
        c_out,
        "class {struct_name}ConstView {{\npublic:\n    {struct_name}ConstView() noexcept = default;\n    {struct_name}ConstView(const void* ptr, const {vt_name}* vt) noexcept : ptr_(ptr), vt_(vt) {{}}\n    bool valid() const noexcept {{ return ptr_ != nullptr && vt_ != nullptr; }}\n    explicit operator bool() const noexcept {{ return valid(); }}\n    const void* raw() const noexcept {{ return ptr_; }}\n    const {vt_name}* vtable() const noexcept {{ return vt_; }}\n{cpp_const_methods}private:\n    const void* ptr_{{nullptr}};\n    const {vt_name}* vt_{{nullptr}};\n}};\n"
    );
    let _ = writeln!(
        c_out,
        "class {struct_name}View {{\npublic:\n    {struct_name}View() noexcept = default;\n    {struct_name}View(void* ptr, const {vt_name}* vt) noexcept : ptr_(ptr), vt_(vt) {{}}\n    bool valid() const noexcept {{ return ptr_ != nullptr && vt_ != nullptr; }}\n    explicit operator bool() const noexcept {{ return valid(); }}\n    void* raw() const noexcept {{ return ptr_; }}\n    const {vt_name}* vtable() const noexcept {{ return vt_; }}\n    {struct_name}ConstView as_const() const noexcept {{ return {struct_name}ConstView(ptr_, vt_); }}\n{cpp_mut_methods}private:\n    void* ptr_{{nullptr}};\n    const {vt_name}* vt_{{nullptr}};\n}};\n"
    );
    c_out.push_str("} // extern \"C++\"\n#endif /* __cplusplus */\n");

    // 6. Zig VTable + comptime View & HostVecView wrappers
    zig_out.push_str(&format!("pub const {vt_name} = extern struct {{\n"));
    zig_out.push_str("    header: bam.BamVTableHeader,\n");
    for line in &zig_vt_lines {
        zig_out.push_str(line);
        zig_out.push('\n');
    }
    zig_out.push_str("};\n\n");

    let mut zig_extra_types = String::new();
    let mut zig_view_methods = String::new();

    for f in &state.decl_fields {
        let sfx = &f.slot_suffix;
        let method_name = &f.c_field_ident;
        match &f.kind {
            DeclFieldKind::Bool => {
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn get_{method_name}(self: @This()) bool {{\n        const f = self.vt.get_{sfx} orelse return false;\n        return f(self.ptr) != bam.BAM_FALSE;\n    }}\n    pub fn set_{method_name}(self: @This(), val: bool) bam.BamResult {{\n        const f = self.vt.set_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, if (val) bam.BAM_TRUE else bam.BAM_FALSE);\n    }}"
                );
            }
            DeclFieldKind::Primitive => {
                let (_, zig_ty) = map_type_name(&f.raw_ty, "uintptr_t", "usize");
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn get_{method_name}(self: @This()) {zig_ty} {{\n        const f = self.vt.get_{sfx} orelse return std.mem.zeroes({zig_ty});\n        return f(self.ptr);\n    }}\n    pub fn set_{method_name}(self: @This(), val: {zig_ty}) bam.BamResult {{\n        const f = self.vt.set_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, val);\n    }}"
                );
            }
            DeclFieldKind::HostString => {
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn get_{method_name}(self: @This()) []const u8 {{\n        const f = self.vt.get_{sfx} orelse return &[_]u8{{}};\n        const v = f(self.ptr);\n        const p = v.ptr orelse return &[_]u8{{}};\n        return p[0..v.len];\n    }}\n    pub fn set_{method_name}(self: @This(), text: []const u8) bam.BamResult {{\n        const f = self.vt.set_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, .{{ .ptr = if (text.len == 0) null else text.ptr, .len = text.len }});\n    }}"
                );
            }
            DeclFieldKind::FfiOption { .. } => {
                let opt_ty = option_typedef_name(&struct_name, sfx);
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn get_{method_name}(self: @This()) {opt_ty} {{\n        const f = self.vt.get_{sfx} orelse return std.mem.zeroes({opt_ty});\n        return f(self.ptr);\n    }}\n    pub fn set_{method_name}(self: @This(), val: {opt_ty}) bam.BamResult {{\n        const f = self.vt.set_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, val);\n    }}\n    pub fn clear_{method_name}(self: @This()) bam.BamResult {{\n        const f = self.vt.clear_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr);\n    }}"
                );
            }
            DeclFieldKind::HostVec { elem_ty } => {
                let (_, elem_zig) = map_type_name(elem_ty, "uint8_t", "u8");
                let slice_ty = slice_typedef_name(&vt_name, sfx);
                let vec_view_name = format!("{vt_name}_{sfx}_VecView");
                let _ = writeln!(
                    zig_extra_types,
                    "pub const {vec_view_name} = struct {{\n    ptr: *anyopaque,\n    vt: *const {vt_name},\n\n    pub fn len(self: @This()) usize {{\n        const f = self.vt.len_{sfx} orelse return 0;\n        return f(self.ptr);\n    }}\n    pub fn isEmpty(self: @This()) bool {{\n        return self.len() == 0;\n    }}\n    pub fn slice(self: @This()) []const {elem_zig} {{\n        const f = self.vt.get_{sfx} orelse return &[_]{elem_zig}{{}};\n        const s = f(self.ptr);\n        const p = s.ptr orelse return &[_]{elem_zig}{{}};\n        return p[0..s.len];\n    }}\n    pub fn mutSlice(self: @This()) []{elem_zig} {{\n        const f = self.vt.as_mut_slice_{sfx} orelse return &[_]{elem_zig}{{}};\n        var n: usize = 0;\n        const p = f(self.ptr, &n) orelse return &[_]{elem_zig}{{}};\n        return p[0..n];\n    }}\n    pub fn set(self: @This(), items: []const {elem_zig}) bam.BamResult {{\n        const f = self.vt.set_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, {slice_ty}{{ .ptr = if (items.len == 0) null else items.ptr, .len = items.len }});\n    }}\n    pub fn clear(self: @This()) bam.BamResult {{\n        const f = self.vt.clear_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr);\n    }}\n    pub fn push(self: @This(), val: {elem_zig}) bam.BamResult {{\n        const f = self.vt.push_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, val);\n    }}\n    pub fn pop(self: @This(), out_elem: *{elem_zig}) bool {{\n        const f = self.vt.pop_{sfx} orelse return false;\n        return f(self.ptr, out_elem) != bam.BAM_FALSE;\n    }}\n    pub fn remove(self: @This(), index: usize, out_elem: *{elem_zig}) bam.BamResult {{\n        const f = self.vt.remove_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, index, out_elem);\n    }}\n    pub fn swapRemove(self: @This(), index: usize, out_elem: *{elem_zig}) bam.BamResult {{\n        const f = self.vt.swap_remove_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, index, out_elem);\n    }}\n    pub fn truncate(self: @This(), new_len: usize) bam.BamResult {{\n        const f = self.vt.truncate_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, new_len);\n    }}\n    pub fn reserve(self: @This(), additional: usize) bam.BamResult {{\n        const f = self.vt.reserve_{sfx} orelse return bam.BAM_RESULT_NULL_POINTER;\n        return f(self.ptr, additional);\n    }}\n}};\n"
                );
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn {method_name}(self: @This()) {vec_view_name} {{\n        return .{{ .ptr = self.ptr, .vt = self.vt }};\n    }}"
                );
            }
            DeclFieldKind::Nested => {
                let _ = writeln!(
                    zig_view_methods,
                    "    pub fn ptr_{method_name}(self: @This()) ?*const anyopaque {{\n        const f = self.vt.ptr_{sfx} orelse return null;\n        return f(self.ptr);\n    }}\n    pub fn ptr_mut_{method_name}(self: @This()) ?*anyopaque {{\n        const f = self.vt.ptr_mut_{sfx} orelse return null;\n        return f(self.ptr);\n    }}"
                );
            }
        }
    }

    zig_out.push_str(&zig_extra_types);
    let _ = writeln!(
        zig_out,
        "pub const {struct_name}View = struct {{\n    ptr: *anyopaque,\n    vt: *const {vt_name},\n\n    comptime {{\n        if (@offsetOf({vt_name}, \"header\") != 0) @compileError(\"{vt_name} header must be at offset 0\");\n    }}\n\n    pub fn resolveVTable(\n        api: *const bam.BamHostApi,\n        host_ctx: ?*const bam.BamHostCtx,\n        symbol: [*:0]const u8,\n    ) !*const {vt_name} {{\n        const get_proc = api.get_proc_address orelse return error.NullPointer;\n        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;\n        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));\n        if (hdr.struct_size < @sizeOf({vt_name}) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {{\n            return error.InvalidVersion;\n        }}\n        return @ptrCast(@alignCast(raw));\n    }}\n\n    pub fn init(ptr: *anyopaque, vt: *const {vt_name}) @This() {{\n        return .{{ .ptr = ptr, .vt = vt }};\n    }}\n\n{zig_view_methods}}};\n"
    );

    (c_out, zig_out)
}
