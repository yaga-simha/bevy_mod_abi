//! ABI-safe 8-bit boolean newtype (`typedef uint8_t BamBool;`).

/// ABI-safe 8-bit boolean flag (`0` = `false`, any non-zero `u8` = `true`).
///
/// Rust's `bool` only permits byte values `0` and `1`; passing any other byte (`2..=255`) from
/// C, C++, or Zig into an `extern "C"` Rust `bool` parameter is undefined behavior.
/// `FfiBool` wraps `u8` transparently so every 8-bit pattern is valid in Rust.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::FfiBool;
///
/// assert!(!FfiBool::FALSE.get());
/// assert!(FfiBool::TRUE.get());
/// assert!(FfiBool(42).get());
/// assert_eq!(FfiBool::from(true), FfiBool::TRUE);
/// assert!(!bool::from(FfiBool::FALSE));
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiBool(pub u8);

impl FfiBool {
    /// Canonical `false` constant (`0`, C `BAM_FALSE`).
    pub const FALSE: Self = Self(0);

    /// Canonical `true` constant (`1`, C `BAM_TRUE`).
    pub const TRUE: Self = Self(1);

    /// Converts a Rust `bool` into a canonical `FfiBool` (`0` or `1`) in `const` contexts.
    #[inline]
    pub const fn from_bool(value: bool) -> Self {
        Self(value as u8)
    }

    /// Converts this `FfiBool` into a Rust `bool` (`0` = `false`, any non-zero `u8` = `true`).
    #[inline]
    pub const fn get(self) -> bool {
        self.0 != 0
    }
}

impl From<bool> for FfiBool {
    #[inline]
    fn from(value: bool) -> Self {
        Self(value as u8)
    }
}

impl From<FfiBool> for bool {
    #[inline]
    fn from(value: FfiBool) -> Self {
        value.get()
    }
}

const _: () = {
    assert!(core::mem::size_of::<FfiBool>() == 1);
    assert!(core::mem::align_of::<FfiBool>() == 1);
};
