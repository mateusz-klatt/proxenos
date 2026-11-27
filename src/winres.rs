//! Pure-Rust Windows COFF `.rsrc` resource builder and asset helpers.
//!
//! Used by `build.rs` to generate:
//! - base64 data URIs for the MCP `serverInfo.icons` field on every platform, and
//! - a deterministic COFF `.rsrc` object file (`RT_ICON`, `RT_GROUP_ICON`, and `RT_VERSION`)
//!   linked into `proxenos.exe` on Windows targets without calling `rc.exe`, `windres`, or a C
//!   compiler.
//!
//! Also compiled under `#[cfg(test)]` in the crate so every line is covered and verified on every
//! platform.

/// Windows resource type for an individual icon image (`RT_ICON`).
pub const RT_ICON: u16 = 3;
/// Windows resource type for an icon directory (`RT_GROUP_ICON`).
pub const RT_GROUP_ICON: u16 = 14;
/// Windows resource type for version metadata (`RT_VERSION`).
pub const RT_VERSION: u16 = 16;
/// US English language ID (`MAKELANGID(LANG_ENGLISH, SUBLANG_ENGLISH_US)`).
pub const LANG_EN_US: u16 = 0x0409;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const COFF_HEADER_LEN: usize = 20;
const SECTION_HEADER_LEN: usize = 40;
const DIR_HEADER_LEN: usize = 16;
const DIR_ENTRY_LEN: usize = 8;
const DATA_ENTRY_LEN: usize = 16;
const RELOC_LEN: usize = 10;
const DIR_FLAG: u32 = 0x8000_0000;
const IMAGE_SCN_RSRC: u32 = 0x4030_0040; // CNT_INITIALIZED_DATA | MEM_READ | ALIGN_4BYTES
const IMAGE_SYM_CLASS_STATIC: u8 = 3;
const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;
const VS_FFI_STRUCVERSION: u32 = 0x0001_0000;
const VS_FFI_FILEFLAGSMASK: u32 = 0x0000_003F;
const VS_FF_PRERELEASE: u32 = 0x0000_0002;
const VOS_NT_WINDOWS32: u32 = 0x0004_0004;
const VFT_APP: u32 = 0x0000_0001;
const CODEPAGE_UNICODE: u16 = 0x04B0;

/// Windows target architecture supported by the COFF `.rsrc` writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoffArch {
    X86,
    X86_64,
    Aarch64,
}

impl CoffArch {
    pub fn from_cargo_arch(arch: &str) -> Result<Self, String> {
        match arch {
            "x86" => Ok(Self::X86),
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            other => Err(format!("unsupported Windows COFF architecture: {other}")),
        }
    }

    fn machine(self) -> u16 {
        match self {
            Self::X86 => 0x014C,
            Self::X86_64 => 0x8664,
            Self::Aarch64 => 0xAA64,
        }
    }

    fn addr32nb_reloc(self) -> u16 {
        match self {
            Self::X86 => 0x0007,
            Self::X86_64 => 0x0003,
            Self::Aarch64 => 0x0002,
        }
    }
}

/// One parsed icon image from a Windows `.ico` container.
#[derive(Debug, Eq, PartialEq)]
pub struct IcoEntry<'a> {
    pub width: u8,
    pub height: u8,
    pub color_count: u8,
    pub planes: u16,
    pub bit_count: u16,
    pub data: &'a [u8],
}

/// Metadata written into the Windows `VS_VERSION_INFO` resource.
pub struct VersionInfo<'a> {
    pub version: &'a str,
    pub product_name: &'a str,
    pub file_description: &'a str,
    pub internal_name: &'a str,
    pub original_filename: &'a str,
    pub legal_copyright: &'a str,
}

/// Parses a Windows `.ico` file and validates every image slice.
pub fn parse_ico(bytes: &[u8]) -> Result<Vec<IcoEntry<'_>>, String> {
    if bytes.len() < 6 {
        return Err("ICO file is truncated before header".into());
    }
    let reserved = read_u16(bytes, 0);
    let ico_type = read_u16(bytes, 2);
    let count = usize::from(read_u16(bytes, 4));
    if reserved != 0 || ico_type != 1 || count == 0 {
        return Err("ICO header is invalid or empty".into());
    }
    let table_end = 6 + count * 16;
    if bytes.len() < table_end {
        return Err("ICO directory table is truncated".into());
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = 6 + index * 16;
        let size = read_u32(bytes, base + 8) as usize;
        let offset = read_u32(bytes, base + 12) as usize;
        let end = offset
            .checked_add(size)
            .filter(|&end| size > 0 && end <= bytes.len())
            .ok_or_else(|| format!("ICO entry {index} has out-of-range image data"))?;
        entries.push(IcoEntry {
            width: bytes[base],
            height: bytes[base + 1],
            color_count: bytes[base + 2],
            planes: read_u16(bytes, base + 4),
            bit_count: read_u16(bytes, base + 6),
            data: &bytes[offset..end],
        });
    }
    Ok(entries)
}

/// Finds a PNG-encoded icon entry of the requested dimensions inside a parsed `.ico`.
pub fn find_png_frame<'a>(
    entries: &'a [IcoEntry<'a>],
    width: u8,
    height: u8,
) -> Result<&'a [u8], String> {
    entries
        .iter()
        .find(|entry| {
            entry.width == width && entry.height == height && entry.data.starts_with(PNG_SIGNATURE)
        })
        .map(|entry| entry.data)
        .ok_or_else(|| format!("no {width}x{height} PNG frame found in ICO"))
}

/// Encodes bytes as standard RFC 4648 base64 with `=` padding.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Parses `MAJOR.MINOR.PATCH[-prerelease][+build]` into Windows' four-part version tuple.
pub fn parse_semver_quad(version: &str) -> Result<([u16; 4], bool), String> {
    let without_build = version.split('+').next().unwrap_or("");
    let mut split_pre = without_build.splitn(2, '-');
    let core = split_pre.next().unwrap_or("");
    let prerelease = split_pre.next().is_some_and(|pre| !pre.is_empty());
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("invalid SemVer version: {version}"));
    }
    let mut nums = [0_u16; 4];
    for (slot, part) in nums[..3].iter_mut().zip(parts) {
        *slot = part
            .parse::<u16>()
            .map_err(|_| format!("invalid SemVer component {part:?} in {version}"))?;
    }
    Ok((nums, prerelease))
}

/// Builds the `RT_GROUP_ICON` (`GRPICONDIR`) resource payload referencing `RT_ICON` IDs `1..=N`.
pub fn build_group_icon(entries: &[IcoEntry<'_>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + entries.len() * 14);
    push_u16(&mut out, 0);
    push_u16(&mut out, 1);
    push_u16(&mut out, entries.len() as u16);
    for (index, entry) in entries.iter().enumerate() {
        out.push(entry.width);
        out.push(entry.height);
        out.push(entry.color_count);
        out.push(0);
        push_u16(&mut out, entry.planes);
        push_u16(&mut out, entry.bit_count);
        push_u32(&mut out, entry.data.len() as u32);
        push_u16(&mut out, (index + 1) as u16);
    }
    out
}

/// Builds the `RT_VERSION` (`VS_VERSION_INFO`) binary resource payload.
pub fn build_version_info(info: &VersionInfo<'_>) -> Result<Vec<u8>, String> {
    let ([major, minor, patch, build], prerelease) = parse_semver_quad(info.version)?;
    let ver_ms = (u32::from(major) << 16) | u32::from(minor);
    let ver_ls = (u32::from(patch) << 16) | u32::from(build);
    let flags = if prerelease { VS_FF_PRERELEASE } else { 0 };

    let mut fixed = Vec::with_capacity(52);
    for word in [
        VS_FFI_SIGNATURE,
        VS_FFI_STRUCVERSION,
        ver_ms,
        ver_ls,
        ver_ms,
        ver_ls,
        VS_FFI_FILEFLAGSMASK,
        flags,
        VOS_NT_WINDOWS32,
        VFT_APP,
        0,
        0,
        0,
    ] {
        push_u32(&mut fixed, word);
    }

    let strings = [
        ("FileDescription", info.file_description),
        ("FileVersion", info.version),
        ("InternalName", info.internal_name),
        ("LegalCopyright", info.legal_copyright),
        ("OriginalFilename", info.original_filename),
        ("ProductName", info.product_name),
        ("ProductVersion", info.version),
    ];

    let mut translation = Vec::with_capacity(4);
    push_u16(&mut translation, LANG_EN_US);
    push_u16(&mut translation, CODEPAGE_UNICODE);

    let mut out = Vec::new();
    write_version_node(&mut out, "VS_VERSION_INFO", 0, Some(&fixed), &mut |out| {
        write_version_node(out, "StringFileInfo", 1, None, &mut |out| {
            write_version_node(out, "040904b0", 1, None, &mut |out| {
                for (index, (key, value)) in strings.iter().enumerate() {
                    if index > 0 {
                        align4(out);
                    }
                    write_version_string(out, key, value);
                }
            });
        });
        align4(out);
        write_version_node(out, "VarFileInfo", 1, None, &mut |out| {
            write_version_node(out, "Translation", 0, Some(&translation), &mut |_| {});
        });
    });
    Ok(out)
}

/// Builds a deterministic Windows COFF object file containing `.rsrc` with the icon and version
/// resources.
pub fn build_rsrc_coff(
    arch: CoffArch,
    ico_bytes: &[u8],
    info: &VersionInfo<'_>,
) -> Result<Vec<u8>, String> {
    let entries = parse_ico(ico_bytes)?;
    let group_icon = build_group_icon(&entries);
    let version_blob = build_version_info(info)?;

    let icon_items: Vec<(u16, &[u8])> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| ((i + 1) as u16, e.data))
        .collect();
    let group_items = [(1_u16, group_icon.as_slice())];
    let version_items = [(1_u16, version_blob.as_slice())];

    // Resource types MUST appear in ascending numeric order per the PE/COFF specification.
    let groups = [
        ResourceGroup {
            type_id: RT_ICON,
            items: &icon_items,
        },
        ResourceGroup {
            type_id: RT_GROUP_ICON,
            items: &group_items,
        },
        ResourceGroup {
            type_id: RT_VERSION,
            items: &version_items,
        },
    ];

    let (rsrc, reloc_offsets) = build_rsrc_section(&groups);
    let rsrc_ptr = (COFF_HEADER_LEN + SECTION_HEADER_LEN) as u32;
    let reloc_ptr = rsrc_ptr + rsrc.len() as u32;
    let sym_ptr = reloc_ptr + (reloc_offsets.len() * RELOC_LEN) as u32;

    let mut obj = Vec::with_capacity(sym_ptr as usize + 18 + 4);
    // IMAGE_FILE_HEADER (20 bytes)
    push_u16(&mut obj, arch.machine());
    push_u16(&mut obj, 1); // NumberOfSections
    push_u32(&mut obj, 0); // TimeDateStamp (deterministic)
    push_u32(&mut obj, sym_ptr);
    push_u32(&mut obj, 1); // NumberOfSymbols
    push_u16(&mut obj, 0); // SizeOfOptionalHeader
    push_u16(&mut obj, 0); // Characteristics

    // IMAGE_SECTION_HEADER (40 bytes)
    obj.extend_from_slice(b".rsrc\0\0\0");
    push_u32(&mut obj, 0); // VirtualSize
    push_u32(&mut obj, 0); // VirtualAddress
    push_u32(&mut obj, rsrc.len() as u32);
    push_u32(&mut obj, rsrc_ptr);
    push_u32(&mut obj, reloc_ptr);
    push_u32(&mut obj, 0); // PointerToLinenumbers
    push_u16(&mut obj, reloc_offsets.len() as u16);
    push_u16(&mut obj, 0); // NumberOfLinenumbers
    push_u32(&mut obj, IMAGE_SCN_RSRC);

    // .rsrc raw data
    obj.extend_from_slice(&rsrc);

    // Relocations: one ADDR32NB relocation per IMAGE_RESOURCE_DATA_ENTRY.OffsetToData.
    let reloc_type = arch.addr32nb_reloc();
    for offset in reloc_offsets {
        push_u32(&mut obj, offset);
        push_u32(&mut obj, 0); // SymbolTableIndex (.rsrc)
        push_u16(&mut obj, reloc_type);
    }

    // Symbol table (18 bytes): static section symbol for `.rsrc` (SectionNumber = 1).
    obj.extend_from_slice(b".rsrc\0\0\0");
    push_u32(&mut obj, 0); // Value
    push_u16(&mut obj, 1); // SectionNumber
    push_u16(&mut obj, 0); // Type
    obj.push(IMAGE_SYM_CLASS_STATIC);
    obj.push(0); // NumberOfAuxSymbols

    // Empty COFF string table (4-byte self-inclusive length).
    push_u32(&mut obj, 4);
    Ok(obj)
}

struct ResourceGroup<'a> {
    type_id: u16,
    items: &'a [(u16, &'a [u8])],
}

fn build_rsrc_section(groups: &[ResourceGroup<'_>]) -> (Vec<u8>, Vec<u32>) {
    let total_items: usize = groups.iter().map(|group| group.items.len()).sum();

    let root_len = DIR_HEADER_LEN + groups.len() * DIR_ENTRY_LEN;
    let mut type_dir_offsets = Vec::with_capacity(groups.len());
    let mut cursor = root_len;
    for group in groups {
        type_dir_offsets.push(cursor as u32);
        cursor += DIR_HEADER_LEN + group.items.len() * DIR_ENTRY_LEN;
    }

    let lang_dirs_start = cursor;
    let lang_dir_len = DIR_HEADER_LEN + DIR_ENTRY_LEN;
    let data_entries_start = lang_dirs_start + total_items * lang_dir_len;
    let payloads_start = data_entries_start + total_items * DATA_ENTRY_LEN;

    let mut payload_offsets = Vec::with_capacity(total_items);
    let mut data_cursor = payloads_start;
    for group in groups {
        for (_, payload) in group.items {
            payload_offsets.push(data_cursor as u32);
            data_cursor = align4_len(data_cursor + payload.len());
        }
    }

    let mut rsrc = Vec::with_capacity(data_cursor);
    // Level 1 (Type) directory
    push_dir_header(&mut rsrc, groups.len() as u16);
    for (group, &dir_offset) in groups.iter().zip(&type_dir_offsets) {
        push_dir_entry(&mut rsrc, u32::from(group.type_id), DIR_FLAG | dir_offset);
    }

    // Level 2 (ID) directories
    let mut item_index = 0_usize;
    for group in groups {
        push_dir_header(&mut rsrc, group.items.len() as u16);
        for (item_id, _) in group.items {
            let lang_offset = (lang_dirs_start + item_index * lang_dir_len) as u32;
            push_dir_entry(&mut rsrc, u32::from(*item_id), DIR_FLAG | lang_offset);
            item_index += 1;
        }
    }

    // Level 3 (Language) directories
    for idx in 0..total_items {
        push_dir_header(&mut rsrc, 1);
        let data_entry_offset = (data_entries_start + idx * DATA_ENTRY_LEN) as u32;
        push_dir_entry(&mut rsrc, u32::from(LANG_EN_US), data_entry_offset);
    }

    // IMAGE_RESOURCE_DATA_ENTRY records
    let mut reloc_offsets = Vec::with_capacity(total_items);
    let mut idx = 0_usize;
    for group in groups {
        for (_, payload) in group.items {
            reloc_offsets.push(rsrc.len() as u32);
            push_u32(&mut rsrc, payload_offsets[idx]); // OffsetToData (relocated via ADDR32NB)
            push_u32(&mut rsrc, payload.len() as u32); // Size
            push_u32(&mut rsrc, 0); // CodePage
            push_u32(&mut rsrc, 0); // Reserved
            idx += 1;
        }
    }

    // Raw payloads, 4-byte aligned
    for group in groups {
        for (_, payload) in group.items {
            rsrc.extend_from_slice(payload);
            align4(&mut rsrc);
        }
    }

    (rsrc, reloc_offsets)
}

fn push_dir_header(out: &mut Vec<u8>, id_entries: u16) {
    push_u32(out, 0); // Characteristics
    push_u32(out, 0); // TimeDateStamp
    push_u16(out, 0); // MajorVersion
    push_u16(out, 0); // MinorVersion
    push_u16(out, 0); // NumberOfNamedEntries
    push_u16(out, id_entries);
}

fn push_dir_entry(out: &mut Vec<u8>, id: u32, offset: u32) {
    push_u32(out, id);
    push_u32(out, offset);
}

fn write_version_node(
    out: &mut Vec<u8>,
    key: &str,
    w_type: u16,
    binary_value: Option<&[u8]>,
    children: &mut dyn FnMut(&mut Vec<u8>),
) {
    let start = out.len();
    push_u16(out, 0); // wLength (patched below)
    push_u16(out, binary_value.map_or(0, |v| v.len() as u16));
    push_u16(out, w_type);
    push_utf16_nul(out, key);
    align4(out);
    if let Some(value) = binary_value {
        out.extend_from_slice(value);
    }
    let before_children = out.len();
    align4(out);
    let aligned_before_children = out.len();
    children(out);
    if out.len() == aligned_before_children {
        out.truncate(before_children);
    }
    let len = (out.len() - start) as u16;
    out[start..start + 2].copy_from_slice(&len.to_le_bytes());
}

fn write_version_string(out: &mut Vec<u8>, key: &str, value: &str) {
    let start = out.len();
    let utf16_units = value.encode_utf16().count() + 1;
    push_u16(out, 0); // wLength (patched below)
    push_u16(out, utf16_units as u16); // wValueLength is in UTF-16 code units including NUL
    push_u16(out, 1); // wType = text
    push_utf16_nul(out, key);
    align4(out);
    push_utf16_nul(out, value);
    let len = (out.len() - start) as u16;
    out[start..start + 2].copy_from_slice(&len.to_le_bytes());
}

fn push_utf16_nul(out: &mut Vec<u8>, s: &str) {
    for unit in s.encode_utf16() {
        push_u16(out, unit);
    }
    push_u16(out, 0);
}

fn align4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn align4_len(len: usize) -> usize {
    (len + 3) & !3
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_ICO: &[u8] = include_bytes!("../assets/proxenos.ico");
    const REPO_SVG: &[u8] = include_bytes!("../assets/proxenos.svg");

    fn sample_version_info(version: &str) -> VersionInfo<'_> {
        VersionInfo {
            version,
            product_name: "Proxenos",
            file_description: "MCP delegation between coding-agent CLIs",
            internal_name: "proxenos",
            original_filename: "proxenos.exe",
            legal_copyright: "Apache-2.0",
        }
    }

    #[test]
    fn coff_arch_maps_supported_architectures_and_rejects_unknown_ones() {
        assert_eq!(CoffArch::from_cargo_arch("x86"), Ok(CoffArch::X86));
        assert_eq!(CoffArch::from_cargo_arch("x86_64"), Ok(CoffArch::X86_64));
        assert_eq!(CoffArch::from_cargo_arch("aarch64"), Ok(CoffArch::Aarch64));
        assert!(CoffArch::from_cargo_arch("riscv64").is_err());

        assert_eq!(CoffArch::X86.machine(), 0x014C);
        assert_eq!(CoffArch::X86_64.machine(), 0x8664);
        assert_eq!(CoffArch::Aarch64.machine(), 0xAA64);
        assert_eq!(CoffArch::X86.addr32nb_reloc(), 0x0007);
        assert_eq!(CoffArch::X86_64.addr32nb_reloc(), 0x0003);
        assert_eq!(CoffArch::Aarch64.addr32nb_reloc(), 0x0002);
    }

    #[test]
    fn parse_ico_reads_checked_in_icon_and_rejects_malformed_inputs() {
        let entries = parse_ico(REPO_ICO).expect("repo ICO parses");
        let dims: Vec<(u8, u8)> = entries.iter().map(|e| (e.width, e.height)).collect();
        assert_eq!(
            dims,
            [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (0, 0)]
        );

        let png64 = find_png_frame(&entries, 64, 64).expect("64x64 PNG frame present");
        assert!(png64.starts_with(PNG_SIGNATURE));
        assert!(find_png_frame(&entries, 16, 16).is_err());

        assert!(parse_ico(&[0, 0, 1]).is_err());
        assert!(parse_ico(&[1, 0, 1, 0, 1, 0]).is_err());
        assert!(parse_ico(&[0, 0, 2, 0, 1, 0]).is_err());
        assert!(parse_ico(&[0, 0, 1, 0, 0, 0]).is_err());
        assert!(parse_ico(&[0, 0, 1, 0, 1, 0, 16, 16]).is_err());

        let mut bad_range = vec![0, 0, 1, 0, 1, 0];
        bad_range.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0]);
        bad_range.extend_from_slice(&4_u32.to_le_bytes());
        bad_range.extend_from_slice(&999_u32.to_le_bytes());
        assert!(parse_ico(&bad_range).is_err());
    }

    #[test]
    fn base64_encode_handles_all_padding_lengths() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert!(base64_encode(REPO_SVG).len() < 400);
    }

    #[test]
    fn parse_semver_quad_handles_release_prerelease_build_and_invalid_versions() {
        assert_eq!(parse_semver_quad("0.1.0"), Ok(([0, 1, 0, 0], false)));
        assert_eq!(
            parse_semver_quad("1.2.3-rc.1+build.9"),
            Ok(([1, 2, 3, 0], true))
        );
        assert!(parse_semver_quad("1.2").is_err());
        assert!(parse_semver_quad("1.2.nope").is_err());
    }

    #[test]
    fn build_version_info_encodes_fixed_info_and_utf16_strings() {
        let release = build_version_info(&sample_version_info("0.1.0")).expect("release version");
        assert_eq!(read_u16(&release, 0) as usize, release.len());
        assert_eq!(read_u16(&release, 2), 52);
        let pre = build_version_info(&sample_version_info("0.2.0-rc.1")).expect("prerelease");
        assert!(pre.windows(4).any(|w| w == VS_FFI_SIGNATURE.to_le_bytes()));
        // VS_FF_PRERELEASE is set in dwFileFlags (offset 28 inside VS_FIXEDFILEINFO).
        let sig_pos = pre
            .windows(4)
            .position(|w| w == VS_FFI_SIGNATURE.to_le_bytes())
            .expect("VS_FIXEDFILEINFO signature");
        assert_eq!(read_u32(&pre, sig_pos + 28), VS_FF_PRERELEASE);
        let rel_sig = release
            .windows(4)
            .position(|w| w == VS_FFI_SIGNATURE.to_le_bytes())
            .expect("VS_FIXEDFILEINFO signature");
        assert_eq!(read_u32(&release, rel_sig + 28), 0);

        let mut needle = Vec::new();
        push_utf16_nul(&mut needle, "proxenos.exe");
        assert!(release.windows(needle.len()).any(|w| w == needle));
        assert!(build_version_info(&sample_version_info("not-semver")).is_err());
    }

    #[test]
    fn build_rsrc_coff_is_deterministic_and_emits_valid_coff_layout() {
        let info = sample_version_info("0.1.0");
        for arch in [CoffArch::X86, CoffArch::X86_64, CoffArch::Aarch64] {
            let obj1 = build_rsrc_coff(arch, REPO_ICO, &info).expect("coff builds");
            let obj2 = build_rsrc_coff(arch, REPO_ICO, &info).expect("coff builds");
            assert_eq!(obj1, obj2, "COFF output must be deterministic");

            // COFF header
            assert_eq!(read_u16(&obj1, 0), arch.machine());
            assert_eq!(read_u16(&obj1, 2), 1);
            assert_eq!(read_u32(&obj1, 4), 0);
            let sym_ptr = read_u32(&obj1, 8) as usize;
            assert_eq!(read_u32(&obj1, 12), 1);
            assert_eq!(obj1.len(), sym_ptr + 18 + 4);

            // Section header
            assert_eq!(&obj1[20..28], b".rsrc\0\0\0");
            let raw_size = read_u32(&obj1, 36) as usize;
            let raw_ptr = read_u32(&obj1, 40) as usize;
            let reloc_ptr = read_u32(&obj1, 44) as usize;
            let reloc_count = read_u16(&obj1, 52) as usize;
            assert_eq!(raw_ptr, 60);
            assert_eq!(reloc_ptr, raw_ptr + raw_size);
            assert_eq!(reloc_count, 8); // 6 RT_ICON + 1 RT_GROUP_ICON + 1 RT_VERSION
            assert_eq!(sym_ptr, reloc_ptr + reloc_count * RELOC_LEN);
            assert_eq!(read_u32(&obj1, 56), IMAGE_SCN_RSRC);

            // Every relocation points at an OffsetToData entry inside .rsrc with symbol 0.
            for idx in 0..reloc_count {
                let rbase = reloc_ptr + idx * RELOC_LEN;
                let va = read_u32(&obj1, rbase) as usize;
                assert!(va + 4 <= raw_size);
                assert_eq!(read_u32(&obj1, rbase + 4), 0);
                assert_eq!(read_u16(&obj1, rbase + 8), arch.addr32nb_reloc());
            }

            // Static `.rsrc` symbol and 4-byte empty string table.
            assert_eq!(&obj1[sym_ptr..sym_ptr + 8], b".rsrc\0\0\0");
            assert_eq!(read_u16(&obj1, sym_ptr + 12), 1);
            assert_eq!(obj1[sym_ptr + 16], IMAGE_SYM_CLASS_STATIC);
            assert_eq!(read_u32(&obj1, sym_ptr + 18), 4);

            // Walk the 3-level resource directory inside `.rsrc` and verify every payload.
            let rsrc = &obj1[raw_ptr..raw_ptr + raw_size];
            let entries = parse_ico(REPO_ICO).expect("repo ICO");
            let mut found = std::collections::BTreeMap::new();
            let dir_entries = |off: usize| -> Vec<(u32, u32)> {
                assert_eq!(read_u16(rsrc, off + 12), 0, "no named entries expected");
                let count = usize::from(read_u16(rsrc, off + 14));
                (0..count)
                    .map(|i| {
                        (
                            read_u32(rsrc, off + 16 + i * 8),
                            read_u32(rsrc, off + 20 + i * 8),
                        )
                    })
                    .collect()
            };
            let types = dir_entries(0);
            assert_eq!(
                types.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                [
                    u32::from(RT_ICON),
                    u32::from(RT_GROUP_ICON),
                    u32::from(RT_VERSION)
                ]
            );
            for (type_id, type_next) in types {
                assert_ne!(type_next & DIR_FLAG, 0);
                for (item_id, item_next) in dir_entries((type_next & !DIR_FLAG) as usize) {
                    assert_ne!(item_next & DIR_FLAG, 0);
                    for (lang_id, data_off) in dir_entries((item_next & !DIR_FLAG) as usize) {
                        assert_eq!(data_off & DIR_FLAG, 0);
                        let de = data_off as usize;
                        let payload_off = read_u32(rsrc, de) as usize;
                        let payload_len = read_u32(rsrc, de + 4) as usize;
                        found.insert(
                            (type_id, item_id, lang_id),
                            rsrc[payload_off..payload_off + payload_len].to_vec(),
                        );
                    }
                }
            }
            assert_eq!(found.len(), entries.len() + 2);
            for (i, entry) in entries.iter().enumerate() {
                assert_eq!(
                    found[&(u32::from(RT_ICON), (i + 1) as u32, u32::from(LANG_EN_US))],
                    entry.data
                );
            }
            assert_eq!(
                found[&(u32::from(RT_GROUP_ICON), 1, u32::from(LANG_EN_US))],
                build_group_icon(&entries)
            );
            assert_eq!(
                found[&(u32::from(RT_VERSION), 1, u32::from(LANG_EN_US))],
                build_version_info(&info).expect("version")
            );
        }

        assert!(build_rsrc_coff(CoffArch::X86_64, b"bad", &info).is_err());
        assert!(build_rsrc_coff(CoffArch::X86_64, REPO_ICO, &sample_version_info("bad")).is_err());
    }
}
