//! Deterministic 128-bit component identity via `const` SipHash-2-4-128 and layout fingerprinting.

/// Fixed SipHash-2-4-128 domain-separation key half 0 (`"BAM_UUID"` in big-endian ASCII).
pub const SIPHASH_KEY_0: u64 = 0x4241_4d5f_5555_4944;

/// Fixed SipHash-2-4-128 domain-separation key half 1 (`"V0_2SIPH"` in big-endian ASCII).
pub const SIPHASH_KEY_1: u64 = 0x5630_5f32_5349_5048;

#[inline]
const fn sipround(v0: u64, v1: u64, v2: u64, v3: u64) -> (u64, u64, u64, u64) {
    let v0 = v0.wrapping_add(v1);
    let v1 = v1.rotate_left(13) ^ v0;
    let v0 = v0.rotate_left(32);
    let v2 = v2.wrapping_add(v3);
    let v3 = v3.rotate_left(16) ^ v2;
    let v0 = v0.wrapping_add(v3);
    let v3 = v3.rotate_left(21) ^ v0;
    let v2 = v2.wrapping_add(v1);
    let v1 = v1.rotate_left(17) ^ v2;
    let v2 = v2.rotate_left(32);
    (v0, v1, v2, v3)
}

/// Streaming `const`-evaluatable SipHash-2-4-128 state matching the canonical Aumasson & Bernstein
/// 16-byte tag specification (`DOUBLE` mode).
#[derive(Copy, Clone)]
struct ConstSipHasher128 {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    tail: [u8; 8],
    tail_len: usize,
    total_len: usize,
}

impl ConstSipHasher128 {
    #[inline]
    const fn new(k0: u64, k1: u64) -> Self {
        Self {
            v0: 0x736f_6d65_7073_6575 ^ k0,
            v1: (0x646f_7261_6e64_6f6d ^ k1) ^ 0xee,
            v2: 0x6c79_6765_6e65_7261 ^ k0,
            v3: 0x7465_6462_7974_6573 ^ k1,
            tail: [0u8; 8],
            tail_len: 0,
            total_len: 0,
        }
    }

    #[inline]
    const fn compress_word(mut self, m: u64) -> Self {
        self.v3 ^= m;
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        self.v0 ^= m;
        self
    }

    #[inline]
    const fn write(mut self, bytes: &[u8]) -> Self {
        let mut idx = 0;
        self.total_len = self.total_len.wrapping_add(bytes.len());

        if self.tail_len > 0 {
            while idx < bytes.len() && self.tail_len < 8 {
                self.tail[self.tail_len] = bytes[idx];
                self.tail_len += 1;
                idx += 1;
            }
            if self.tail_len == 8 {
                let m = u64::from_le_bytes(self.tail);
                self = self.compress_word(m);
                self.tail = [0u8; 8];
                self.tail_len = 0;
            }
        }

        while idx + 8 <= bytes.len() {
            let m = u64::from_le_bytes([
                bytes[idx],
                bytes[idx + 1],
                bytes[idx + 2],
                bytes[idx + 3],
                bytes[idx + 4],
                bytes[idx + 5],
                bytes[idx + 6],
                bytes[idx + 7],
            ]);
            self = self.compress_word(m);
            idx += 8;
        }

        while idx < bytes.len() {
            self.tail[self.tail_len] = bytes[idx];
            self.tail_len += 1;
            idx += 1;
        }

        self
    }

    #[inline]
    const fn finish(mut self) -> [u8; 16] {
        let mut b = ((self.total_len as u64) & 0xff) << 56;
        let mut i = 0;
        while i < self.tail_len {
            b |= (self.tail[i] as u64) << (8 * i);
            i += 1;
        }
        self = self.compress_word(b);

        self.v2 ^= 0xee;
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        let h0 = self.v0 ^ self.v1 ^ self.v2 ^ self.v3;

        self.v1 ^= 0xdd;
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        (self.v0, self.v1, self.v2, self.v3) = sipround(self.v0, self.v1, self.v2, self.v3);
        let h1 = self.v0 ^ self.v1 ^ self.v2 ^ self.v3;

        let b0 = h0.to_le_bytes();
        let b1 = h1.to_le_bytes();
        [
            b0[0], b0[1], b0[2], b0[3], b0[4], b0[5], b0[6], b0[7], b1[0], b1[1], b1[2], b1[3],
            b1[4], b1[5], b1[6], b1[7],
        ]
    }
}

/// Computes a `const` 128-bit SipHash-2-4 digest over `data` using `(k0, k1)`.
#[inline]
pub const fn siphash24_128(data: &[u8], k0: u64, k1: u64) -> [u8; 16] {
    ConstSipHasher128::new(k0, k1).write(data).finish()
}

#[inline]
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Unique 128-bit component identifier (`typedef struct BamComponentUuid { uint8_t bytes[16]; } BamComponentUuid;`).
///
/// Stored as `[u8; 16]` (`size = 16`, `align = 1` across every 32-bit and 64-bit C ABI, avoiding
/// cross-compiler `u128` or 32-bit `uint64_t` alignment divergence).
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::ComponentUuid;
///
/// let mod_a_health = ComponentUuid::from_parts("mod_a::Health", 4, 4, 0);
/// let mod_b_health = ComponentUuid::from_parts("mod_b::Health", 4, 4, 0);
/// let mod_a_health_f64 = ComponentUuid::from_parts("mod_a::Health", 8, 8, 0);
/// assert_ne!(mod_a_health, mod_b_health);
/// assert_ne!(mod_a_health, mod_a_health_f64);
///
/// let explicit = ComponentUuid::parse_hex("00112233-4455-6677-8899-aabbccddeeff");
/// assert_eq!(explicit.to_u128(), 0x00112233_4455_6677_8899_aabbccddeeff);
/// ```
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ComponentUuid {
    /// Raw 16-byte UUID representation.
    pub bytes: [u8; 16],
}

impl ComponentUuid {
    /// All-zero UUID constant.
    pub const ZERO: Self = Self { bytes: [0u8; 16] };

    /// Constructs a `ComponentUuid` directly from a 16-byte array.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self { bytes }
    }

    /// Borrows the raw 16-byte UUID array.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    /// Constructs a `ComponentUuid` from a `u128` in standard big-endian UUID byte order.
    #[inline]
    pub const fn from_u128(value: u128) -> Self {
        Self {
            bytes: value.to_be_bytes(),
        }
    }

    /// Converts this `ComponentUuid` into a `u128` in standard big-endian UUID byte order.
    #[inline]
    pub const fn to_u128(self) -> u128 {
        u128::from_be_bytes(self.bytes)
    }

    /// Returns `true` if all 16 bytes are zero.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.bytes[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Computes a deterministic 128-bit UUID via `const` SipHash-2-4-128 over:
    /// - `canonical_name.as_bytes()` (`"<namespace>::<Name>"`) + `0xFF` separator,
    /// - `(size as u64).to_le_bytes()`,
    /// - `(align as u64).to_le_bytes()`,
    /// - `field_schema_hash.to_le_bytes()` (`0` for opaque VTable-only components).
    #[inline]
    pub const fn from_parts(
        canonical_name: &str,
        size: usize,
        align: usize,
        field_schema_hash: u64,
    ) -> Self {
        let size_le = (size as u64).to_le_bytes();
        let align_le = (align as u64).to_le_bytes();
        let schema_le = field_schema_hash.to_le_bytes();
        let bytes = ConstSipHasher128::new(SIPHASH_KEY_0, SIPHASH_KEY_1)
            .write(canonical_name.as_bytes())
            .write(&[0xFF])
            .write(&size_le)
            .write(&align_le)
            .write(&schema_le)
            .finish();
        Self { bytes }
    }

    /// Alias for [`Self::from_parts`] computing SipHash-2-4-128 over a namespaced component name
    /// and its `(size, align, field_schema_hash)` layout fingerprint.
    #[inline]
    pub const fn from_namespaced(
        canonical_name: &str,
        size: usize,
        align: usize,
        field_schema_hash: u64,
    ) -> Self {
        Self::from_parts(canonical_name, size, align, field_schema_hash)
    }

    /// Computes a deterministic UUID for an opaque or VTable-only component whose host layout is
    /// intentionally hidden across compilers (`size = 0, align = 0, field_schema_hash = 0`).
    #[inline]
    pub const fn from_canonical_name(canonical_name: &str) -> Self {
        Self::from_parts(canonical_name, 0, 0, 0)
    }

    /// Accumulates one `(field_name, field_type_tag, field_offset)` tuple into a deterministic
    /// 64-bit `field_schema_hash` for `#[repr(C)]` `AbiPod` structs.
    #[inline]
    pub const fn hash_field_schema(
        prev_hash: u64,
        field_name: &str,
        field_type_tag: &str,
        field_offset: usize,
    ) -> u64 {
        let prev_le = prev_hash.to_le_bytes();
        let offset_le = (field_offset as u64).to_le_bytes();
        let digest = ConstSipHasher128::new(SIPHASH_KEY_0, SIPHASH_KEY_1)
            .write(&prev_le)
            .write(field_name.as_bytes())
            .write(&[0xFE])
            .write(field_type_tag.as_bytes())
            .write(&[0xFD])
            .write(&offset_le)
            .finish();
        u64::from_le_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ])
    }

    /// Parses a UUID from either canonical 36-character hyphenated `"8-4-4-4-12"` hex form or
    /// contiguous 32-character hex form, returning `None` if malformed.
    #[inline]
    pub const fn try_parse_hex(hex: &str) -> Option<Self> {
        let src = hex.as_bytes();
        let hyphenated = match src.len() {
            36 => {
                if src[8] != b'-' || src[13] != b'-' || src[18] != b'-' || src[23] != b'-' {
                    return None;
                }
                true
            }
            32 => false,
            _ => return None,
        };

        let mut out = [0u8; 16];
        let mut src_idx = 0;
        let mut byte_idx = 0;
        while byte_idx < 16 {
            if hyphenated && (src_idx == 8 || src_idx == 13 || src_idx == 18 || src_idx == 23) {
                src_idx += 1;
            }
            let Some(hi) = hex_nibble(src[src_idx]) else {
                return None;
            };
            let Some(lo) = hex_nibble(src[src_idx + 1]) else {
                return None;
            };
            out[byte_idx] = (hi << 4) | lo;
            src_idx += 2;
            byte_idx += 1;
        }
        Some(Self { bytes: out })
    }

    /// Parses a compile-time UUID hex literal (`"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"` or 32 hex
    /// digits), panicking if the input is malformed.
    ///
    /// # Panics
    /// Panics if `hex` is not a valid 36-character `"8-4-4-4-12"` or 32-character hex UUID string.
    #[inline]
    pub const fn parse_hex(hex: &str) -> Self {
        match Self::try_parse_hex(hex) {
            Some(uuid) => uuid,
            None => panic!(
                "invalid ComponentUuid hex string (expected xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx or 32 hex digits)"
            ),
        }
    }
}

const _: () = {
    assert!(core::mem::size_of::<ComponentUuid>() == 16);
    assert!(core::mem::align_of::<ComponentUuid>() == 1);
};
