//! ABI-safe `#[repr(C)]` optional value wrapper (`FfiOption<T>`).

use core::{
    fmt,
    hash::{Hash, Hasher},
    mem::MaybeUninit,
};

use crate::{primitives::FfiBool, traits::AbiPod};

/// Cross-language `#[repr(C)]` optional value (`is_some: FfiBool` + `value: T`).
///
/// Rust's `Option<T>` does not have a guaranteed cross-compiler layout unless `T` has a specific
/// pointer niche, and never matches a C99 tagged struct. `FfiOption<T>` provides a deterministic
/// `#[repr(C)]` wire representation across Rust, C99, C++17, and Zig.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::FfiOption;
///
/// let some_val = FfiOption::some(42u32);
/// assert!(some_val.is_some());
/// assert_eq!(some_val.as_ref(), Some(&42));
/// assert_eq!(some_val.into_option(), Some(42));
///
/// let none_val = FfiOption::<u32>::none();
/// assert!(none_val.is_none());
/// assert_eq!(none_val.as_ref(), None);
/// assert_eq!(none_val.into_option(), None);
/// ```
#[repr(C)]
#[derive(Copy, Clone)]
pub struct FfiOption<T> {
    /// Presence tag (`0` = `None`, any non-zero `u8` = `Some(value)`).
    pub is_some: FfiBool,
    /// Payload slot (initialized when `is_some` is non-zero; zero-filled by [`Self::none`]).
    pub value: T,
}

impl<T> FfiOption<T> {
    /// Constructs an engaged `FfiOption::Some(value)` (`is_some = FfiBool::TRUE`).
    #[inline]
    pub const fn some(value: T) -> Self {
        Self {
            is_some: FfiBool::TRUE,
            value,
        }
    }

    /// Constructs a disengaged `FfiOption::None` for `T: AbiPod` by zero-filling `value`.
    #[inline]
    pub const fn none() -> Self
    where
        T: AbiPod,
    {
        // SAFETY: `T: AbiPod` guarantees every field is valid when all-bits-zero and has no drop glue.
        let value = unsafe { MaybeUninit::<T>::zeroed().assume_init() };
        Self {
            is_some: FfiBool::FALSE,
            value,
        }
    }

    /// Constructs a disengaged `FfiOption::None` using `T::default()` as the placeholder payload.
    #[inline]
    pub fn none_default() -> Self
    where
        T: Default,
    {
        Self {
            is_some: FfiBool::FALSE,
            value: T::default(),
        }
    }

    /// Returns `true` if `self.is_some` is non-zero.
    #[inline]
    pub const fn is_some(&self) -> bool {
        self.is_some.get()
    }

    /// Returns `true` if `self.is_some` is zero (`FfiBool::FALSE`).
    #[inline]
    pub const fn is_none(&self) -> bool {
        !self.is_some.get()
    }

    /// Converts this `FfiOption<T>` into a standard Rust `Option<T>`.
    #[inline]
    pub fn into_option(self) -> Option<T> {
        if self.is_some.get() {
            Some(self.value)
        } else {
            None
        }
    }

    /// Converts a standard Rust `Option<T>` (`T: AbiPod`) into an `FfiOption<T>`.
    #[inline]
    pub fn from_option(opt: Option<T>) -> Self
    where
        T: AbiPod,
    {
        match opt {
            Some(v) => Self::some(v),
            None => Self::none(),
        }
    }

    /// Converts a standard Rust `Option<T>` (`T: Default`) into an `FfiOption<T>`.
    #[inline]
    pub fn from_option_default(opt: Option<T>) -> Self
    where
        T: Default,
    {
        match opt {
            Some(v) => Self::some(v),
            None => Self::none_default(),
        }
    }

    /// Borrows the inner payload as `Option<&T>`.
    #[inline]
    pub const fn as_ref(&self) -> Option<&T> {
        if self.is_some.get() {
            Some(&self.value)
        } else {
            None
        }
    }

    /// Mutably borrows the inner payload as `Option<&mut T>`.
    #[inline]
    pub const fn as_mut(&mut self) -> Option<&mut T> {
        if self.is_some.get() {
            Some(&mut self.value)
        } else {
            None
        }
    }
}

impl<T: Default> Default for FfiOption<T> {
    #[inline]
    fn default() -> Self {
        Self::none_default()
    }
}

impl<T: AbiPod> From<Option<T>> for FfiOption<T> {
    #[inline]
    fn from(opt: Option<T>) -> Self {
        Self::from_option(opt)
    }
}

impl<T> From<FfiOption<T>> for Option<T> {
    #[inline]
    fn from(opt: FfiOption<T>) -> Self {
        opt.into_option()
    }
}

impl<T: fmt::Debug> fmt::Debug for FfiOption<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_ref() {
            Some(val) => f.debug_tuple("Some").field(val).finish(),
            None => f.write_str("None"),
        }
    }
}

impl<T: PartialEq> PartialEq for FfiOption<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<T: Eq> Eq for FfiOption<T> {}

impl<T: Hash> Hash for FfiOption<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

const _: () = {
    assert!(core::mem::size_of::<FfiOption<u8>>() == 2);
    assert!(core::mem::align_of::<FfiOption<u8>>() == 1);
    assert!(core::mem::offset_of!(FfiOption<u32>, is_some) == 0);
    assert!(core::mem::offset_of!(FfiOption<u32>, value) == 4);
    assert!(core::mem::size_of::<FfiOption<u32>>() == 8);
    assert!(core::mem::align_of::<FfiOption<u32>>() == 4);
};
