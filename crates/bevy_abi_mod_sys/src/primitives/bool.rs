//! ABI-safe 8-bit boolean representation.

/// ABI-safe 8-bit boolean flag (`0` = `false`, non-zero = `true`).
///
/// Rust's `bool` only permits byte values `0` and `1`; passing any other byte (`2..=255`) from
/// C, C++, or Zig into an `extern "C"` Rust `bool` parameter is immediate undefined behavior.
/// All boolean values crossing the C-ABI use `FfiBool` (`u8`) instead.
pub type FfiBool = u8;

/// Canonical `false` value for [`FfiBool`].
pub const FFI_BOOL_FALSE: FfiBool = 0;
/// Canonical `true` value for [`FfiBool`].
pub const FFI_BOOL_TRUE: FfiBool = 1;

/// Converts an [`FfiBool`] byte (`0` = `false`, any non-zero byte = `true`) into a Rust `bool`
/// without undefined behavior on non-canonical foreign truthy values (`2..=255`).
#[inline]
pub const fn ffi_bool_to_rust(flag: FfiBool) -> bool {
    flag != FFI_BOOL_FALSE
}

/// Converts a Rust `bool` into a canonical [`FfiBool`] (`0` or `1`).
#[inline]
pub const fn rust_bool_to_ffi(val: bool) -> FfiBool {
    if val { FFI_BOOL_TRUE } else { FFI_BOOL_FALSE }
}
