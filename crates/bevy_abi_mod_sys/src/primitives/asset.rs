//! Cross-ABI asset handle (`FfiAssetHandle`) and load-state (`FfiAssetLoadState`) primitives.

use crate::primitives::{BamComponent, ComponentUuid, FfiResult};

/// Wire representation of a Bevy asset identifier (`typedef struct BamAssetHandle BamAssetHandle;`).
///
/// Models `Option<UntypedAssetId>` explicitly via `kind` (`NONE = 0`, `INDEX = 1`, `UUID = 2`)
/// without relying on Bevy's deprecated `AssetId::invalid()` sentinel. Strong handle ownership
/// stays inside the host's asset retention table until released via `BamHostApi::asset_release`
/// or mod unload.
///
/// Layout: `32` bytes, `4`-byte alignment, zero implicit padding on both 64-bit and 32-bit targets.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{BamComponent, FfiAssetHandle, FfiResult};
///
/// let empty = FfiAssetHandle::default();
/// assert!(empty.is_none());
/// assert_eq!(empty, FfiAssetHandle::NONE);
/// assert_eq!(empty.validate(), FfiResult::OK);
///
/// let indexed = FfiAssetHandle::from_index(BamComponent::from_raw(3), 42, 1);
/// assert!(indexed.is_some());
/// assert_eq!(indexed.validate(), FfiResult::OK);
/// ```
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct FfiAssetHandle {
    /// Discriminant: [`Self::KIND_NONE`] (`0`), [`Self::KIND_INDEX`] (`1`), or [`Self::KIND_UUID`] (`2`) (`offset 0`).
    pub kind: u8,
    /// Explicit zero-reserved alignment bytes (`must be [0; 3]`, `offset 1`).
    pub reserved0: [u8; 3],
    /// Pre-resolved [`BamComponent`] handle of the asset type (`offset 4`).
    pub asset_type: BamComponent,
    /// Generational `AssetIndex` slot index when `kind == KIND_INDEX` (`offset 8`).
    pub index: u32,
    /// Generational `AssetIndex` generation counter when `kind == KIND_INDEX` (`offset 12`).
    pub generation: u32,
    /// 128-bit UUID asset identifier when `kind == KIND_UUID` (`offset 16`, 16 bytes).
    pub uuid: ComponentUuid,
}

impl FfiAssetHandle {
    /// Disengaged (`None`) asset handle discriminant (`0`, C `BAM_ASSET_HANDLE_KIND_NONE`).
    pub const KIND_NONE: u8 = 0;
    /// Generational index (`AssetId::Index`) discriminant (`1`, C `BAM_ASSET_HANDLE_KIND_INDEX`).
    pub const KIND_INDEX: u8 = 1;
    /// 128-bit UUID (`AssetId::Uuid`) discriminant (`2`, C `BAM_ASSET_HANDLE_KIND_UUID`).
    pub const KIND_UUID: u8 = 2;

    /// Canonical disengaged (`None`) asset handle constant (all bytes zero).
    pub const NONE: Self = Self::none();

    /// Constructs a disengaged (`None`) asset handle (`kind = KIND_NONE`).
    #[inline]
    pub const fn none() -> Self {
        Self {
            kind: Self::KIND_NONE,
            reserved0: [0u8; 3],
            asset_type: BamComponent::INVALID,
            index: 0,
            generation: 0,
            uuid: ComponentUuid::ZERO,
        }
    }

    /// Constructs an index-backed asset handle (`kind = KIND_INDEX`).
    #[inline]
    pub const fn from_index(asset_type: BamComponent, index: u32, generation: u32) -> Self {
        Self {
            kind: Self::KIND_INDEX,
            reserved0: [0u8; 3],
            asset_type,
            index,
            generation,
            uuid: ComponentUuid::ZERO,
        }
    }

    /// Constructs an index-backed asset handle from packed 64-bit `AssetIndex::to_bits()`
    /// (`(generation as u64) << 32 | (index as u64)`).
    #[inline]
    pub const fn from_index_bits(asset_type: BamComponent, bits: u64) -> Self {
        Self::from_index(asset_type, bits as u32, (bits >> 32) as u32)
    }

    /// Packs `(index, generation)` into 64-bit `AssetIndex::to_bits()` format when `kind == KIND_INDEX`.
    #[inline]
    pub const fn to_index_bits(&self) -> Option<u64> {
        if self.kind == Self::KIND_INDEX {
            Some(((self.generation as u64) << 32) | (self.index as u64))
        } else {
            None
        }
    }

    /// Constructs a UUID-backed asset handle (`kind = KIND_UUID`).
    #[inline]
    pub const fn from_uuid(asset_type: BamComponent, uuid: ComponentUuid) -> Self {
        Self {
            kind: Self::KIND_UUID,
            reserved0: [0u8; 3],
            asset_type,
            index: 0,
            generation: 0,
            uuid,
        }
    }

    /// Returns `true` if `self.kind == Self::KIND_NONE`.
    #[inline]
    pub const fn is_none(&self) -> bool {
        self.kind == Self::KIND_NONE
    }

    /// Returns `true` if `self.kind` names an active `INDEX` or `UUID` asset identifier.
    #[inline]
    pub const fn is_some(&self) -> bool {
        self.kind == Self::KIND_INDEX || self.kind == Self::KIND_UUID
    }

    /// Validates `reserved0 == [0; 3]`, `kind` discriminant range, and `asset_type != INVALID` when `is_some()`.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if self.reserved0[0] != 0 || self.reserved0[1] != 0 || self.reserved0[2] != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        match self.kind {
            Self::KIND_NONE => FfiResult::OK,
            Self::KIND_INDEX | Self::KIND_UUID => {
                if !self.asset_type.is_valid() {
                    FfiResult::INVALID_COMPONENT
                } else {
                    FfiResult::OK
                }
            }
            _ => FfiResult::INVALID_ARGUMENT,
        }
    }
}

impl Default for FfiAssetHandle {
    #[inline]
    fn default() -> Self {
        Self::NONE
    }
}

/// FFI-safe asset load state (`typedef uint32_t BamAssetLoadState;`).
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiAssetLoadState(pub u32);

impl FfiAssetLoadState {
    /// Asset is not currently tracked or is disengaged (`0`, C `BAM_ASSET_LOAD_STATE_NOT_LOADED`).
    pub const NOT_LOADED: Self = Self(0);
    /// Asset is queued or actively loading (`1`, C `BAM_ASSET_LOAD_STATE_LOADING`).
    pub const LOADING: Self = Self(1);
    /// Asset and its dependencies have finished loading (`2`, C `BAM_ASSET_LOAD_STATE_LOADED`).
    pub const LOADED: Self = Self(2);
    /// Asset loading failed (`3`, C `BAM_ASSET_LOAD_STATE_FAILED`).
    pub const FAILED: Self = Self(3);

    /// Returns `true` if this value is one of the recognized load states (`0..=3`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::FAILED.0
    }

    /// Returns `true` if `self == Self::LOADED`.
    #[inline]
    pub const fn is_loaded(self) -> bool {
        self.0 == Self::LOADED.0
    }

    /// Returns `true` if `self == Self::FAILED`.
    #[inline]
    pub const fn is_failed(self) -> bool {
        self.0 == Self::FAILED.0
    }
}

const _: () = {
    assert!(core::mem::size_of::<FfiAssetHandle>() == 32);
    assert!(core::mem::align_of::<FfiAssetHandle>() == 4);
    assert!(core::mem::offset_of!(FfiAssetHandle, kind) == 0);
    assert!(core::mem::offset_of!(FfiAssetHandle, reserved0) == 1);
    assert!(core::mem::offset_of!(FfiAssetHandle, asset_type) == 4);
    assert!(core::mem::offset_of!(FfiAssetHandle, index) == 8);
    assert!(core::mem::offset_of!(FfiAssetHandle, generation) == 12);
    assert!(core::mem::offset_of!(FfiAssetHandle, uuid) == 16);
    assert!(core::mem::size_of::<FfiAssetLoadState>() == 4);
    assert!(core::mem::align_of::<FfiAssetLoadState>() == 4);
};
