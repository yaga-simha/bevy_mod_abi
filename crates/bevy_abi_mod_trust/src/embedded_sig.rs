//! Embedded `.bam_sig` / `__bam_sig` signature section extraction and verification across
//! ELF32, ELF64, PE32/PE32+, and Mach-O (32-bit, 64-bit, Fat) mod shared libraries.

use sha2::{Digest, Sha256};

use crate::{
    MAX_SIGNATURE_SIDECAR_BYTES, ModTrustPolicy, SignatureVerificationError, VerifiedModSignature,
};

/// Standard section name embedded inside ELF and PE binaries holding an OpenSSH `SSHSIG` PEM.
pub const BAM_SIG_SECTION_NAME: &str = ".bam_sig";

/// Mach-O section name (without leading dot, 16-byte limit) holding an OpenSSH `SSHSIG` PEM.
pub const BAM_SIG_MACHO_SECTION_NAME: &str = "__bam_sig";

const MAX_PE_SECTIONS: usize = 256;
const MAX_MACHO_LOAD_COMMANDS: usize = 4096;
const MAX_MACHO_SECTIONS_PER_SEGMENT: usize = 4096;
const MAX_MACHO_FAT_ARCHS: usize = 32;

/// Extracted raw embedded signature section metadata and payload from an ELF, PE, or Mach-O binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedSignatureSection {
    /// Byte offset in the outer binary of the signature section data (zeroed prior to hashing).
    pub offset: usize,
    /// Total raw byte span in the outer binary zeroed prior to verification.
    pub size: usize,
    /// Trimmed signature bytes stored inside the section (stripped of trailing NUL padding).
    pub payload: Vec<u8>,
}

#[inline]
fn spans_overlap(a_start: usize, a_len: usize, b_start: usize, b_len: usize) -> bool {
    if a_len == 0 || b_len == 0 {
        return false;
    }
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[inline]
fn malformed_sig(detail: impl Into<String>) -> SignatureVerificationError {
    SignatureVerificationError::MalformedSignature {
        detail: detail.into(),
    }
}

#[inline]
fn read_u16_end(bytes: &[u8], off: usize, le: bool) -> Option<u16> {
    let slice = bytes.get(off..off.checked_add(2)?)?;
    let arr: [u8; 2] = slice.try_into().ok()?;
    Some(if le {
        u16::from_le_bytes(arr)
    } else {
        u16::from_be_bytes(arr)
    })
}

#[inline]
fn read_u32_end(bytes: &[u8], off: usize, le: bool) -> Option<u32> {
    let slice = bytes.get(off..off.checked_add(4)?)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(arr)
    } else {
        u32::from_be_bytes(arr)
    })
}

#[inline]
fn read_u64_end(bytes: &[u8], off: usize, le: bool) -> Option<u64> {
    let slice = bytes.get(off..off.checked_add(8)?)?;
    let arr: [u8; 8] = slice.try_into().ok()?;
    Some(if le {
        u64::from_le_bytes(arr)
    } else {
        u64::from_be_bytes(arr)
    })
}

fn trim_trailing_nul(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }
    &bytes[..end]
}

fn extract_verified_section_payload(
    bytes: &[u8],
    off: usize,
    raw: usize,
    used: usize,
    format_label: &str,
) -> Result<Vec<u8>, SignatureVerificationError> {
    if raw == 0 || used == 0 {
        return Err(malformed_sig(format!(
            "empty {format_label} .bam_sig section"
        )));
    }
    // Every byte in `[off + used .. off + raw]` is zeroed before signature hashing; disallow any
    // non-zero bytes there so attackers cannot smuggle unverified payloads past VirtualSize.
    if used < raw && bytes[off + used..off + raw].iter().any(|&b| b != 0) {
        return Err(malformed_sig(format!(
            "unverified non-zero padding bytes in {format_label} .bam_sig section beyond payload size"
        )));
    }
    let payload = trim_trailing_nul(&bytes[off..off + used]);
    if payload.is_empty() {
        return Err(malformed_sig(format!(
            "{format_label} .bam_sig section contains only NUL bytes"
        )));
    }
    // An OpenSSH SSHSIG PEM is pure ASCII text; interior NUL bytes indicate smuggled trailing
    // bytes after NUL padding inside the zeroed signature span.
    if payload.contains(&0) {
        return Err(malformed_sig(format!(
            "{format_label} .bam_sig section contains interior NUL or smuggled trailing bytes"
        )));
    }
    Ok(payload.to_vec())
}

fn validate_section_span(
    binary_len: usize,
    offset: u64,
    raw_size: u64,
    used_size: u64,
) -> Result<(usize, usize, usize), SignatureVerificationError> {
    if raw_size == 0 || used_size == 0 {
        return Err(malformed_sig("embedded .bam_sig section is empty"));
    }
    if raw_size > MAX_SIGNATURE_SIDECAR_BYTES || used_size > MAX_SIGNATURE_SIDECAR_BYTES {
        return Err(SignatureVerificationError::SignatureFileTooLarge {
            size: raw_size.max(used_size),
            limit: MAX_SIGNATURE_SIDECAR_BYTES,
        });
    }
    let off = usize::try_from(offset)
        .map_err(|_| malformed_sig("embedded .bam_sig offset overflows usize"))?;
    let raw = usize::try_from(raw_size)
        .map_err(|_| malformed_sig("embedded .bam_sig size overflows usize"))?;
    let used = usize::try_from(used_size)
        .map_err(|_| malformed_sig("embedded .bam_sig payload size overflows usize"))?;
    let end = off
        .checked_add(raw)
        .filter(|&end| end <= binary_len && used <= raw)
        .ok_or_else(|| malformed_sig("embedded .bam_sig section extends past binary EOF"))?;
    let _ = end;
    Ok((off, raw, used))
}

/// Extracts an embedded `.bam_sig` / `__bam_sig` signature section from an ELF32, ELF64,
/// PE32/PE32+, or Mach-O shared library image, returning `Ok(None)` when no such section exists.
pub fn extract_embedded_bam_sig(
    binary_bytes: &[u8],
) -> Result<Option<EmbeddedSignatureSection>, SignatureVerificationError> {
    if binary_bytes.starts_with(b"\x7fELF") {
        return extract_elf_bam_sig(binary_bytes);
    }
    if binary_bytes.len() >= 4 {
        let magic_be = u32::from_be_bytes([
            binary_bytes[0],
            binary_bytes[1],
            binary_bytes[2],
            binary_bytes[3],
        ]);
        if magic_be == 0xcafe_babe || magic_be == 0xcafe_babf {
            return extract_macho_fat_bam_sig(binary_bytes, magic_be == 0xcafe_babf);
        }
        let magic_le = u32::from_le_bytes([
            binary_bytes[0],
            binary_bytes[1],
            binary_bytes[2],
            binary_bytes[3],
        ]);
        if matches!(
            magic_le,
            0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe
        ) {
            return extract_macho_thin_bam_sig(binary_bytes);
        }
    }
    if binary_bytes.starts_with(b"MZ") {
        return extract_pe_bam_sig(binary_bytes);
    }
    Ok(None)
}

fn extract_elf_bam_sig(
    bytes: &[u8],
) -> Result<Option<EmbeddedSignatureSection>, SignatureVerificationError> {
    const ELFCLASS32: u8 = 1;
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const ELFDATA2MSB: u8 = 2;
    const SHT_NULL: u32 = 0;
    const SHT_NOBITS: u32 = 8;
    const SHF_EXECINSTR: u64 = 0x4;

    let Some(&ei_class) = bytes.get(4) else {
        return Ok(None);
    };
    let Some(&ei_data) = bytes.get(5) else {
        return Ok(None);
    };
    let le = match ei_data {
        ELFDATA2LSB => true,
        ELFDATA2MSB => false,
        _ => return Ok(None),
    };

    let (is_64, ehdr_size, phoff, phentsize, phnum, shoff, shentsize, shnum, shstrndx) =
        match ei_class {
            ELFCLASS32 => {
                let Some(phoff) = read_u32_end(bytes, 28, le).map(u64::from) else {
                    return Ok(None);
                };
                let Some(shoff) = read_u32_end(bytes, 32, le).map(u64::from) else {
                    return Ok(None);
                };
                let Some(phentsize) = read_u16_end(bytes, 42, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(phnum) = read_u16_end(bytes, 44, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shentsize) = read_u16_end(bytes, 46, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shnum) = read_u16_end(bytes, 48, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shstrndx) = read_u16_end(bytes, 50, le).map(usize::from) else {
                    return Ok(None);
                };
                (
                    false, 52usize, phoff, phentsize, phnum, shoff, shentsize, shnum, shstrndx,
                )
            }
            ELFCLASS64 => {
                let Some(phoff) = read_u64_end(bytes, 32, le) else {
                    return Ok(None);
                };
                let Some(shoff) = read_u64_end(bytes, 40, le) else {
                    return Ok(None);
                };
                let Some(phentsize) = read_u16_end(bytes, 54, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(phnum) = read_u16_end(bytes, 56, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shentsize) = read_u16_end(bytes, 58, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shnum) = read_u16_end(bytes, 60, le).map(usize::from) else {
                    return Ok(None);
                };
                let Some(shstrndx) = read_u16_end(bytes, 62, le).map(usize::from) else {
                    return Ok(None);
                };
                (
                    true, 64usize, phoff, phentsize, phnum, shoff, shentsize, shnum, shstrndx,
                )
            }
            _ => return Ok(None),
        };

    let min_shdr = if is_64 { 64 } else { 40 };
    if shoff == 0 || shnum == 0 || shentsize < min_shdr || shstrndx >= shnum {
        return Ok(None);
    }
    let Ok(shoff_usize) = usize::try_from(shoff) else {
        return Ok(None);
    };

    let read_shdr_span = |off: usize| -> Option<(u64, u64)> {
        if is_64 {
            Some((
                read_u64_end(bytes, off + 24, le)?,
                read_u64_end(bytes, off + 32, le)?,
            ))
        } else {
            Some((
                read_u32_end(bytes, off + 16, le).map(u64::from)?,
                read_u32_end(bytes, off + 20, le).map(u64::from)?,
            ))
        }
    };
    let read_shdr_flags = |off: usize| -> Option<u64> {
        if is_64 {
            read_u64_end(bytes, off + 8, le)
        } else {
            read_u32_end(bytes, off + 8, le).map(u64::from)
        }
    };

    let Some(strtab_hdr_off) = shstrndx
        .checked_mul(shentsize)
        .and_then(|d| shoff_usize.checked_add(d))
    else {
        return Ok(None);
    };
    let Some((str_off_u64, str_size_u64)) = read_shdr_span(strtab_hdr_off) else {
        return Ok(None);
    };
    let (Ok(str_off), Ok(str_size)) = (usize::try_from(str_off_u64), usize::try_from(str_size_u64))
    else {
        return Ok(None);
    };
    let Some(str_end) = str_off.checked_add(str_size) else {
        return Ok(None);
    };
    let Some(shstrtab) = bytes.get(str_off..str_end) else {
        return Ok(None);
    };

    let sh_table_len = shnum
        .checked_mul(shentsize)
        .ok_or_else(|| malformed_sig("ELF section header table size overflows usize"))?;
    let ph_table_span = if phoff > 0 && phnum > 0 && phentsize > 0 {
        usize::try_from(phoff)
            .ok()
            .and_then(|o| phnum.checked_mul(phentsize).map(|l| (o, l)))
    } else {
        None
    };

    let mut found: Option<EmbeddedSignatureSection> = None;
    let mut other_spans: Vec<(usize, usize)> = Vec::new();

    for idx in 0..shnum {
        let Some(entry_off) = idx
            .checked_mul(shentsize)
            .and_then(|d| shoff_usize.checked_add(d))
        else {
            return Err(malformed_sig("ELF section header offset overflow"));
        };
        let Some(sh_name) = read_u32_end(bytes, entry_off, le).map(|v| v as usize) else {
            return Err(malformed_sig("truncated ELF section header table"));
        };
        let Some(sh_type) = read_u32_end(bytes, entry_off + 4, le) else {
            return Err(malformed_sig("truncated ELF section header table"));
        };

        let is_bam_sig = shstrtab.get(sh_name..).is_some_and(|name_tail| {
            let name_len = name_tail
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_tail.len());
            &name_tail[..name_len] == BAM_SIG_SECTION_NAME.as_bytes()
        });

        if is_bam_sig {
            if found.is_some() {
                return Err(malformed_sig("duplicate ELF .bam_sig sections"));
            }
            if sh_type == SHT_NOBITS {
                return Err(malformed_sig("ELF .bam_sig section must not be SHT_NOBITS"));
            }
            let Some(sh_flags) = read_shdr_flags(entry_off) else {
                return Err(malformed_sig("truncated ELF .bam_sig section flags"));
            };
            if (sh_flags & SHF_EXECINSTR) != 0 {
                return Err(malformed_sig(
                    "ELF .bam_sig section must not be executable (SHF_EXECINSTR)",
                ));
            }
            let Some((sec_off, sec_size)) = read_shdr_span(entry_off) else {
                return Err(malformed_sig("truncated ELF .bam_sig section header"));
            };
            let (off, raw, used) = validate_section_span(bytes.len(), sec_off, sec_size, sec_size)?;
            if spans_overlap(off, raw, 0, ehdr_size)
                || spans_overlap(off, raw, shoff_usize, sh_table_len)
                || ph_table_span.is_some_and(|(po, pl)| spans_overlap(off, raw, po, pl))
            {
                return Err(malformed_sig(
                    "ELF .bam_sig section overlaps ELF headers or header tables",
                ));
            }
            let payload = extract_verified_section_payload(bytes, off, raw, used, "ELF")?;
            found = Some(EmbeddedSignatureSection {
                offset: off,
                size: raw,
                payload,
            });
        } else if sh_type != SHT_NULL
            && sh_type != SHT_NOBITS
            && let Some((other_off, other_size)) = read_shdr_span(entry_off)
            && other_size > 0
            && let (Ok(o), Ok(s)) = (usize::try_from(other_off), usize::try_from(other_size))
        {
            other_spans.push((o, s));
        }
    }

    if let Some(ref sig_sec) = found {
        for &(other_off, other_size) in &other_spans {
            if spans_overlap(sig_sec.offset, sig_sec.size, other_off, other_size) {
                return Err(malformed_sig(
                    "ELF .bam_sig section overlaps another section",
                ));
            }
        }
    }

    Ok(found)
}

fn extract_pe_bam_sig(
    bytes: &[u8],
) -> Result<Option<EmbeddedSignatureSection>, SignatureVerificationError> {
    const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;
    const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;

    let Some(lfanew) = read_u32_end(bytes, 0x3c, true).map(|v| v as usize) else {
        return Ok(None);
    };
    let Some(pe_sig_end) = lfanew.checked_add(4) else {
        return Ok(None);
    };
    if bytes.get(lfanew..pe_sig_end) != Some(b"PE\0\0") {
        return Ok(None);
    }
    let coff_off = pe_sig_end;
    let Some(num_sections) = read_u16_end(bytes, coff_off + 2, true).map(usize::from) else {
        return Ok(None);
    };
    if num_sections > MAX_PE_SECTIONS {
        return Err(malformed_sig(format!(
            "PE section count ({num_sections}) exceeds maximum ({MAX_PE_SECTIONS})"
        )));
    }
    let Some(opt_size) = read_u16_end(bytes, coff_off + 16, true).map(usize::from) else {
        return Ok(None);
    };
    let Some(opt_off) = coff_off.checked_add(20) else {
        return Ok(None);
    };
    let Some(sections_off) = opt_off.checked_add(opt_size) else {
        return Ok(None);
    };
    let Some(section_table_end) = num_sections
        .checked_mul(40)
        .and_then(|sz| sections_off.checked_add(sz))
    else {
        return Err(malformed_sig("PE section table size overflows usize"));
    };
    let opt_size_of_headers = if opt_size >= 64 {
        read_u32_end(bytes, opt_off + 60, true)
            .map(|v| v as usize)
            .filter(|&v| v <= bytes.len())
            .unwrap_or(0)
    } else {
        0
    };
    let headers_end = section_table_end.max(opt_size_of_headers);

    let mut found: Option<EmbeddedSignatureSection> = None;
    let mut other_spans: Vec<(usize, usize)> = Vec::new();

    for idx in 0..num_sections {
        let Some(off) = idx
            .checked_mul(40)
            .and_then(|d| sections_off.checked_add(d))
        else {
            return Err(malformed_sig("PE section header offset overflow"));
        };
        let Some(name_slice) = bytes.get(off..off.saturating_add(8)) else {
            return Err(malformed_sig("truncated PE section header table"));
        };
        let name_len = name_slice.iter().position(|&b| b == 0).unwrap_or(8);
        let is_bam_sig = &name_slice[..name_len] == BAM_SIG_SECTION_NAME.as_bytes();

        let virt_size = read_u32_end(bytes, off + 8, true)
            .ok_or_else(|| malformed_sig("truncated PE section header"))?;
        let raw_size = read_u32_end(bytes, off + 16, true)
            .ok_or_else(|| malformed_sig("truncated PE section raw_size"))?;
        let raw_ptr = read_u32_end(bytes, off + 20, true)
            .ok_or_else(|| malformed_sig("truncated PE section raw_ptr"))?;
        let characteristics = read_u32_end(bytes, off + 36, true)
            .ok_or_else(|| malformed_sig("truncated PE section characteristics"))?;

        if is_bam_sig {
            if found.is_some() {
                return Err(malformed_sig("duplicate PE .bam_sig sections"));
            }
            if (characteristics & (IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_CNT_CODE)) != 0 {
                return Err(malformed_sig(
                    "PE .bam_sig section must not be executable or code",
                ));
            }
            if raw_size == 0 {
                return Err(malformed_sig("empty PE .bam_sig section"));
            }
            let used_size = if virt_size > 0 && virt_size < raw_size {
                virt_size
            } else {
                raw_size
            };
            let (sec_off, raw, used) = validate_section_span(
                bytes.len(),
                u64::from(raw_ptr),
                u64::from(raw_size),
                u64::from(used_size),
            )?;
            if spans_overlap(sec_off, raw, 0, headers_end) {
                return Err(malformed_sig(
                    "PE .bam_sig section overlaps PE headers or section table",
                ));
            }
            let payload = extract_verified_section_payload(bytes, sec_off, raw, used, "PE")?;
            found = Some(EmbeddedSignatureSection {
                offset: sec_off,
                size: raw,
                payload,
            });
        } else if raw_size > 0 && raw_ptr > 0 {
            other_spans.push((raw_ptr as usize, raw_size as usize));
        }
    }

    if let Some(ref sig_sec) = found {
        for &(other_off, other_size) in &other_spans {
            if spans_overlap(sig_sec.offset, sig_sec.size, other_off, other_size) {
                return Err(malformed_sig(
                    "PE .bam_sig section overlaps another section",
                ));
            }
        }
    }

    Ok(found)
}

fn extract_macho_fat_bam_sig(
    bytes: &[u8],
    is_64: bool,
) -> Result<Option<EmbeddedSignatureSection>, SignatureVerificationError> {
    let Some(nfat_arch) = read_u32_end(bytes, 4, false).map(|n| n as usize) else {
        return Ok(None);
    };
    if nfat_arch == 0 {
        return Ok(None);
    }
    if nfat_arch > MAX_MACHO_FAT_ARCHS {
        return Err(malformed_sig(format!(
            "Mach-O fat arch count ({nfat_arch}) exceeds maximum ({MAX_MACHO_FAT_ARCHS})"
        )));
    }
    let arch_size = if is_64 { 32 } else { 20 };
    let fat_hdr_end = nfat_arch
        .checked_mul(arch_size)
        .and_then(|d| 8usize.checked_add(d))
        .ok_or_else(|| malformed_sig("Mach-O fat header size overflows usize"))?;

    let mut found: Option<EmbeddedSignatureSection> = None;
    let mut slice_spans: Vec<(usize, usize)> = Vec::with_capacity(nfat_arch);

    for idx in 0..nfat_arch {
        let Some(arch_off) = idx
            .checked_mul(arch_size)
            .and_then(|d| 8usize.checked_add(d))
        else {
            return Err(malformed_sig("Mach-O fat arch offset overflow"));
        };
        let (slice_off, slice_size) = if is_64 {
            let Some(o) =
                read_u64_end(bytes, arch_off + 8, false).and_then(|v| usize::try_from(v).ok())
            else {
                return Err(malformed_sig("truncated Mach-O fat64 arch entry"));
            };
            let Some(s) =
                read_u64_end(bytes, arch_off + 16, false).and_then(|v| usize::try_from(v).ok())
            else {
                return Err(malformed_sig("truncated Mach-O fat64 arch entry"));
            };
            (o, s)
        } else {
            let Some(o) = read_u32_end(bytes, arch_off + 8, false).map(|v| v as usize) else {
                return Err(malformed_sig("truncated Mach-O fat arch entry"));
            };
            let Some(s) = read_u32_end(bytes, arch_off + 12, false).map(|v| v as usize) else {
                return Err(malformed_sig("truncated Mach-O fat arch entry"));
            };
            (o, s)
        };
        if spans_overlap(slice_off, slice_size, 0, fat_hdr_end) {
            return Err(malformed_sig("Mach-O fat slice overlaps fat header"));
        }
        for &(prev_off, prev_size) in &slice_spans {
            if spans_overlap(slice_off, slice_size, prev_off, prev_size) {
                return Err(malformed_sig("Mach-O fat slices overlap"));
            }
        }
        slice_spans.push((slice_off, slice_size));

        let Some(end) = slice_off.checked_add(slice_size) else {
            return Err(malformed_sig("Mach-O fat slice offset overflows usize"));
        };
        let Some(slice) = bytes.get(slice_off..end) else {
            return Err(malformed_sig("Mach-O fat slice extends past EOF"));
        };
        if let Some(mut inner) = extract_macho_thin_bam_sig(slice)? {
            if found.is_some() {
                return Err(malformed_sig(
                    "duplicate Mach-O __bam_sig sections across fat slices",
                ));
            }
            inner.offset = slice_off
                .checked_add(inner.offset)
                .ok_or_else(|| malformed_sig("Mach-O fat __bam_sig offset overflows usize"))?;
            found = Some(inner);
        }
    }
    Ok(found)
}

fn extract_macho_thin_bam_sig(
    bytes: &[u8],
) -> Result<Option<EmbeddedSignatureSection>, SignatureVerificationError> {
    const MH_MAGIC: u32 = 0xfeed_face;
    const MH_CIGAM: u32 = 0xcefa_edfe;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const MH_CIGAM_64: u32 = 0xcffa_edfe;
    const LC_SEGMENT: u32 = 0x01;
    const LC_SEGMENT_64: u32 = 0x19;
    const VM_PROT_EXECUTE: u32 = 0x04;
    const SECTION_TYPE_MASK: u32 = 0x0000_00ff;
    const S_ZEROFILL: u32 = 0x01;
    const S_GB_ZEROFILL: u32 = 0x0c;
    const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
    const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
    const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

    let Some(raw_magic_le) = read_u32_end(bytes, 0, true) else {
        return Ok(None);
    };
    let (is_64, le) = match raw_magic_le {
        MH_MAGIC => (false, true),
        MH_CIGAM => (false, false),
        MH_MAGIC_64 => (true, true),
        MH_CIGAM_64 => (true, false),
        _ => return Ok(None),
    };

    let hdr_size: usize = if is_64 { 32 } else { 28 };
    let Some(ncmds) = read_u32_end(bytes, 16, le).map(|v| v as usize) else {
        return Ok(None);
    };
    if ncmds > MAX_MACHO_LOAD_COMMANDS {
        return Err(malformed_sig(format!(
            "Mach-O load command count ({ncmds}) exceeds maximum ({MAX_MACHO_LOAD_COMMANDS})"
        )));
    }
    let Some(sizeofcmds) = read_u32_end(bytes, 20, le).map(|v| v as usize) else {
        return Ok(None);
    };
    let cmds_end = hdr_size
        .checked_add(sizeofcmds)
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| malformed_sig("Mach-O load commands extend past EOF"))?;
    let mut cursor = hdr_size;

    let mut found: Option<EmbeddedSignatureSection> = None;
    let mut other_spans: Vec<(usize, usize)> = Vec::new();

    for _ in 0..ncmds {
        if cursor + 8 > cmds_end {
            return Err(malformed_sig("truncated Mach-O load command table"));
        }
        let Some(cmd) = read_u32_end(bytes, cursor, le) else {
            return Err(malformed_sig("truncated Mach-O load command"));
        };
        let Some(cmdsize) = read_u32_end(bytes, cursor + 4, le).map(|v| v as usize) else {
            return Err(malformed_sig("truncated Mach-O load command size"));
        };
        if cmdsize < 8 || cursor.checked_add(cmdsize).is_none_or(|e| e > cmds_end) {
            return Err(malformed_sig("invalid Mach-O load command size"));
        }

        if (cmd == LC_SEGMENT && !is_64 && cmdsize >= 56)
            || (cmd == LC_SEGMENT_64 && is_64 && cmdsize >= 72)
        {
            let (seg_hdr_size, sect_size, initprot_off, nsects_off) = if is_64 {
                (72usize, 80usize, 60usize, 64usize)
            } else {
                (56usize, 68usize, 44usize, 48usize)
            };
            let seg_initprot = read_u32_end(bytes, cursor + initprot_off, le).unwrap_or(0);
            let nsects = read_u32_end(bytes, cursor + nsects_off, le).unwrap_or(0) as usize;
            if nsects > MAX_MACHO_SECTIONS_PER_SEGMENT {
                return Err(malformed_sig(format!(
                    "Mach-O segment section count ({nsects}) exceeds maximum ({MAX_MACHO_SECTIONS_PER_SEGMENT})"
                )));
            }
            let mut sec_cursor = cursor + seg_hdr_size;
            for _ in 0..nsects {
                if sec_cursor + sect_size > cursor + cmdsize {
                    return Err(malformed_sig("truncated Mach-O segment section header"));
                }
                let sect_name_bytes = &bytes[sec_cursor..sec_cursor + 16];
                let sect_name_len = sect_name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(sect_name_bytes.len());
                let sect_name = &sect_name_bytes[..sect_name_len];
                let (sec_size, sec_off, sec_flags) = if is_64 {
                    let sz = read_u64_end(bytes, sec_cursor + 40, le).unwrap_or(0);
                    let off = read_u32_end(bytes, sec_cursor + 48, le).unwrap_or(0);
                    let fl = read_u32_end(bytes, sec_cursor + 64, le).unwrap_or(0);
                    (sz, u64::from(off), fl)
                } else {
                    let sz = read_u32_end(bytes, sec_cursor + 36, le).unwrap_or(0);
                    let off = read_u32_end(bytes, sec_cursor + 40, le).unwrap_or(0);
                    let fl = read_u32_end(bytes, sec_cursor + 56, le).unwrap_or(0);
                    (u64::from(sz), u64::from(off), fl)
                };
                let sec_type = sec_flags & SECTION_TYPE_MASK;
                let is_zerofill = matches!(
                    sec_type,
                    S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL
                );

                if sect_name == BAM_SIG_MACHO_SECTION_NAME.as_bytes()
                    || sect_name == BAM_SIG_SECTION_NAME.as_bytes()
                {
                    if found.is_some() {
                        return Err(malformed_sig("duplicate Mach-O __bam_sig sections"));
                    }
                    if is_zerofill {
                        return Err(malformed_sig(
                            "Mach-O __bam_sig section must not be zerofill",
                        ));
                    }
                    if (seg_initprot & VM_PROT_EXECUTE) != 0
                        || (sec_flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS)) != 0
                    {
                        return Err(malformed_sig(
                            "Mach-O __bam_sig section must not be executable",
                        ));
                    }
                    let (off, raw, used) =
                        validate_section_span(bytes.len(), sec_off, sec_size, sec_size)?;
                    if spans_overlap(off, raw, 0, cmds_end) {
                        return Err(malformed_sig(
                            "Mach-O __bam_sig section overlaps Mach-O header or load commands",
                        ));
                    }
                    let payload =
                        extract_verified_section_payload(bytes, off, raw, used, "Mach-O")?;
                    found = Some(EmbeddedSignatureSection {
                        offset: off,
                        size: raw,
                        payload,
                    });
                } else if !is_zerofill
                    && sec_size > 0
                    && sec_off > 0
                    && let (Ok(o), Ok(s)) = (usize::try_from(sec_off), usize::try_from(sec_size))
                {
                    other_spans.push((o, s));
                }
                sec_cursor += sect_size;
            }
        }
        cursor += cmdsize;
    }

    if let Some(ref sig_sec) = found {
        for &(other_off, other_size) in &other_spans {
            if spans_overlap(sig_sec.offset, sig_sec.size, other_off, other_size) {
                return Err(malformed_sig(
                    "Mach-O __bam_sig section overlaps another section",
                ));
            }
        }
    }

    Ok(found)
}

/// Returns a copy of `binary_bytes` with `[section.offset .. section.offset + section.size]`
/// zeroed (`0x00`) so embedded `.bam_sig` verification hashes a canonical signature-free image.
#[must_use]
pub fn zeroed_binary_for_bam_sig(
    binary_bytes: &[u8],
    section: &EmbeddedSignatureSection,
) -> Vec<u8> {
    let mut copy = binary_bytes.to_vec();
    if let Some(end) = section.offset.checked_add(section.size)
        && let Some(slot) = copy.get_mut(section.offset..end)
    {
        slot.fill(0);
    }
    copy
}

impl ModTrustPolicy {
    /// Verifies an embedded `.bam_sig` / `__bam_sig` signature section inside `binary_bytes`.
    ///
    /// The signature is verified against a copy of `binary_bytes` whose `.bam_sig` raw byte span
    /// has been zero-filled (`zeroed_binary_for_bam_sig`), while the returned
    /// [`VerifiedModSignature::binary_sha256`] records the SHA-256 digest of the on-disk bytes.
    pub fn verify_embedded_bam_sig(
        &self,
        binary_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        if binary_bytes.is_empty() {
            return Err(SignatureVerificationError::EmptyBinary);
        }
        let bin_len = binary_bytes.len() as u64;
        if bin_len > self.max_binary_bytes() {
            return Err(SignatureVerificationError::BinaryTooLarge {
                size: bin_len,
                limit: self.max_binary_bytes(),
            });
        }
        let Some(section) = extract_embedded_bam_sig(binary_bytes)? else {
            return Err(SignatureVerificationError::MissingSignatureFile {
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no detached .sig sidecar and no embedded .bam_sig section found",
                ),
            });
        };
        let zeroed = zeroed_binary_for_bam_sig(binary_bytes, &section);
        let mut verified = self.verify_bytes(&zeroed, &section.payload)?;
        verified.binary_sha256 = Sha256::digest(binary_bytes).into();
        Ok(verified)
    }

    /// Verifies `binary_bytes` using a detached `sidecar_sig` when provided, or falling back to
    /// an embedded `.bam_sig` / `__bam_sig` section when `sidecar_sig` is `None`.
    pub fn verify_binary_or_sidecar(
        &self,
        binary_bytes: &[u8],
        sidecar_sig: Option<&[u8]>,
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        match sidecar_sig {
            Some(sig) => self.verify_bytes(binary_bytes, sig),
            None => self.verify_embedded_bam_sig(binary_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_SIGNATURE_NAMESPACE, sign_mod_bytes_with_key};
    use ssh_key::rand_core::OsRng;
    use ssh_key::{Algorithm, PrivateKey};

    fn test_key() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
    }

    fn build_synthetic_elf64_with_bam_sig(sig_slot_len: usize) -> (Vec<u8>, usize, usize) {
        let code_off = 64usize;
        let code_len = 32usize;
        let strtab = b"\0.text\0.shstrtab\0.bam_sig\0";
        let strtab_off = code_off + code_len;
        let sig_off = strtab_off + strtab.len();
        let shoff = sig_off + sig_slot_len;
        let shnum = 4usize; // null, .text, .shstrtab, .bam_sig
        let mut buf = vec![0u8; shoff + shnum * 64];

        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // ELFCLASS64
        buf[5] = 1; // ELFDATA2LSB
        buf[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        buf[58..60].copy_from_slice(&64u16.to_le_bytes());
        buf[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
        buf[62..64].copy_from_slice(&2u16.to_le_bytes()); // e_shstrndx = 2

        buf[code_off..code_off + code_len].fill(0x90);
        buf[strtab_off..strtab_off + strtab.len()].copy_from_slice(strtab);

        let write_shdr =
            |b: &mut [u8], idx: usize, name_off: u32, sh_type: u32, off: usize, sz: usize| {
                let s = shoff + idx * 64;
                b[s..s + 4].copy_from_slice(&name_off.to_le_bytes());
                b[s + 4..s + 8].copy_from_slice(&sh_type.to_le_bytes());
                b[s + 24..s + 32].copy_from_slice(&(off as u64).to_le_bytes());
                b[s + 32..s + 40].copy_from_slice(&(sz as u64).to_le_bytes());
            };
        write_shdr(&mut buf, 1, 1, 1, code_off, code_len);
        write_shdr(&mut buf, 2, 7, 3, strtab_off, strtab.len());
        write_shdr(&mut buf, 3, 17, 1, sig_off, sig_slot_len);
        (buf, sig_off, code_off)
    }

    fn build_synthetic_pe64_with_bam_sig(sig_slot_len: usize) -> (Vec<u8>, usize, usize) {
        let lfanew = 0x80usize;
        let opt_size = 240usize;
        let sections_off = lfanew + 24 + opt_size;
        let text_off = 0x400usize;
        let sig_off = text_off + 0x100;
        let mut buf = vec![0u8; sig_off + sig_slot_len];

        buf[0..2].copy_from_slice(b"MZ");
        buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
        buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");
        buf[lfanew + 6..lfanew + 8].copy_from_slice(&2u16.to_le_bytes());
        buf[lfanew + 20..lfanew + 22].copy_from_slice(&(opt_size as u16).to_le_bytes());
        buf[lfanew + 24..lfanew + 26].copy_from_slice(&0x20bu16.to_le_bytes());

        // Section 0: .text
        buf[sections_off..sections_off + 5].copy_from_slice(b".text");
        buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x100u32.to_le_bytes());
        buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x100u32.to_le_bytes());
        buf[sections_off + 20..sections_off + 24].copy_from_slice(&(text_off as u32).to_le_bytes());
        buf[text_off..text_off + 16].fill(0xCC);

        // Section 1: .bam_sig
        let s1 = sections_off + 40;
        buf[s1..s1 + 8].copy_from_slice(b".bam_sig");
        buf[s1 + 8..s1 + 12].copy_from_slice(&(sig_slot_len as u32).to_le_bytes());
        buf[s1 + 16..s1 + 20].copy_from_slice(&(sig_slot_len as u32).to_le_bytes());
        buf[s1 + 20..s1 + 24].copy_from_slice(&(sig_off as u32).to_le_bytes());

        (buf, sig_off, text_off)
    }

    fn build_synthetic_macho64_with_bam_sig(sig_slot_len: usize) -> (Vec<u8>, usize, usize) {
        let seg_cmd_size = 72usize + 80usize;
        let hdr_and_cmds = 32 + seg_cmd_size;
        let sig_off = hdr_and_cmds + 32;
        let mut buf = vec![0u8; sig_off + sig_slot_len];

        buf[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        buf[16..20].copy_from_slice(&1u32.to_le_bytes());
        buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());

        let lc = 32usize;
        buf[lc..lc + 4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
        buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
        buf[lc + 64..lc + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1

        let sec = lc + 72;
        buf[sec..sec + 9].copy_from_slice(b"__bam_sig");
        buf[sec + 40..sec + 48].copy_from_slice(&(sig_slot_len as u64).to_le_bytes());
        buf[sec + 48..sec + 52].copy_from_slice(&(sig_off as u32).to_le_bytes());

        buf[hdr_and_cmds..hdr_and_cmds + 8].copy_from_slice(b"MACHO_OK");
        (buf, sig_off, hdr_and_cmds)
    }

    #[test]
    fn embedded_bam_sig_verifies_elf_pe_and_macho_and_rejects_tamper() {
        let key = test_key();
        let policy =
            ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
                .unwrap();

        let builders: [fn(usize) -> (Vec<u8>, usize, usize); 3] = [
            build_synthetic_elf64_with_bam_sig,
            build_synthetic_pe64_with_bam_sig,
            build_synthetic_macho64_with_bam_sig,
        ];

        for build_fn in builders {
            let (mut img, sig_off, code_off) = build_fn(1024);
            let pem =
                sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, &img).expect("sign");
            assert!(pem.len() <= 1024);
            img[sig_off..sig_off + pem.len()].copy_from_slice(pem.as_bytes());

            let verified = policy
                .verify_embedded_bam_sig(&img)
                .expect("embedded .bam_sig must verify");
            let expected_sha: [u8; 32] = Sha256::digest(&img).into();
            assert_eq!(verified.binary_sha256, expected_sha);

            // Fallback via verify_binary_or_sidecar(None) works cleanly.
            assert!(policy.verify_binary_or_sidecar(&img, None).is_ok());

            // 1-byte tamper inside code fails verification!
            let mut tampered = img.clone();
            tampered[code_off] ^= 0xff;
            assert!(matches!(
                policy.verify_embedded_bam_sig(&tampered),
                Err(SignatureVerificationError::InvalidSignature)
            ));
        }
    }

    #[test]
    fn missing_and_truncated_bam_sig_return_errors_without_panicking() {
        let key = test_key();
        let policy =
            ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
                .unwrap();

        assert!(matches!(
            policy.verify_embedded_bam_sig(b"plain-bytes-no-headers"),
            Err(SignatureVerificationError::MissingSignatureFile { .. })
        ));
        for len in 0..16 {
            let mut b = vec![0x7f, b'E', b'L', b'F'];
            b.resize(len, 0);
            let _ = extract_embedded_bam_sig(&b);
        }
    }
}
