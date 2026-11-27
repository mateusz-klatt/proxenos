//! Behavioural tests for Proxenos's MCP server icons and Windows PE executable resources.

#[allow(dead_code)]
mod common;

use common::Server;

const REPO_SVG: &[u8] = include_bytes!("../assets/proxenos.svg");
const REPO_ICO: &[u8] = include_bytes!("../assets/proxenos.ico");

#[test]
fn initialize_advertises_svg_and_png_server_icons_matching_repo_assets() {
    let expected_png64 = extract_ico_frame(REPO_ICO, 64, 64);
    assert!(
        expected_png64.starts_with(b"\x89PNG\r\n\x1a\n"),
        "64x64 ICO frame must be PNG-encoded"
    );
    // PNG IHDR width and height live at offsets 16..24 (big-endian u32).
    assert_eq!(
        u32::from_be_bytes(expected_png64[16..20].try_into().unwrap()),
        64
    );
    assert_eq!(
        u32::from_be_bytes(expected_png64[20..24].try_into().unwrap()),
        64
    );

    for version in ["2024-11-05", "2025-06-18", "2025-11-25"] {
        let (mut server, answer) = Server::initialized(version);
        let icons = answer["result"]["serverInfo"]["icons"]
            .as_array()
            .expect("serverInfo.icons is an array");
        assert_eq!(
            icons.len(),
            2,
            "expected SVG and 64x64 PNG icons ({version})"
        );

        let serialized_len = serde_json::to_string(icons).expect("icons serialize").len();
        assert!(
            serialized_len < 2048,
            "serverInfo.icons should stay compact ({serialized_len} bytes)"
        );

        let svg_icon = &icons[0];
        assert_eq!(svg_icon["mimeType"], "image/svg+xml");
        assert_eq!(svg_icon["sizes"], serde_json::json!(["any"]));
        let svg_src = svg_icon["src"].as_str().expect("SVG data URI");
        let svg_b64 = svg_src
            .strip_prefix("data:image/svg+xml;base64,")
            .expect("SVG icon uses base64 data URI");
        assert_eq!(base64_decode(svg_b64), REPO_SVG);

        let png_icon = &icons[1];
        assert_eq!(png_icon["mimeType"], "image/png");
        assert_eq!(png_icon["sizes"], serde_json::json!(["64x64"]));
        let png_src = png_icon["src"].as_str().expect("PNG data URI");
        let png_b64 = png_src
            .strip_prefix("data:image/png;base64,")
            .expect("PNG icon uses base64 data URI");
        assert_eq!(base64_decode(png_b64), expected_png64);

        let (status, _) = server.finish();
        assert!(status.success(), "{version}: {status}");
    }
}

fn extract_ico_frame(ico: &[u8], width: u8, height: u8) -> &[u8] {
    let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
    for idx in 0..count {
        let base = 6 + idx * 16;
        if ico[base] == width && ico[base + 1] == height {
            let size = u32::from_le_bytes(ico[base + 8..base + 12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(ico[base + 12..base + 16].try_into().unwrap()) as usize;
            return &ico[offset..offset + size];
        }
    }
    panic!("missing {width}x{height} frame in assets/proxenos.ico");
}

fn base64_decode(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let (chunks, remainder) = bytes.as_chunks::<4>();
    assert!(remainder.is_empty(), "valid base64 length");
    let val = |b: u8| -> u8 {
        match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("unexpected base64 byte {other:#x}"),
        }
    };
    let mut out = Vec::with_capacity(chunks.len() * 3);
    for chunk in chunks {
        let a = u32::from(val(chunk[0]));
        let b = u32::from(val(chunk[1]));
        let pad1 = chunk[2] == b'=';
        let pad2 = chunk[3] == b'=';
        let c = if pad1 { 0 } else { u32::from(val(chunk[2])) };
        let d = if pad2 { 0 } else { u32::from(val(chunk[3])) };
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((triple >> 16) & 0xFF) as u8);
        if !pad1 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if !pad2 {
            out.push((triple & 0xFF) as u8);
        }
    }
    out
}

#[cfg(windows)]
mod windows_pe {
    use std::collections::BTreeMap;
    use std::fs;

    use super::REPO_ICO;

    const RT_ICON: u32 = 3;
    const RT_GROUP_ICON: u32 = 14;
    const RT_VERSION: u32 = 16;
    const LANG_EN_US: u32 = 0x0409;

    fn u16_le(buf: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([buf[off], buf[off + 1]])
    }

    fn u32_le(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }

    fn parse_pe_resources(pe: &[u8]) -> BTreeMap<(u32, u32, u32), Vec<u8>> {
        assert_eq!(&pe[0..2], b"MZ", "missing DOS header");
        let pe_off = u32_le(pe, 0x3C) as usize;
        assert_eq!(&pe[pe_off..pe_off + 4], b"PE\0\0", "missing PE signature");

        let num_sections = usize::from(u16_le(pe, pe_off + 6));
        let opt_size = usize::from(u16_le(pe, pe_off + 20));
        let opt_off = pe_off + 24;
        let magic = u16_le(pe, opt_off);
        let data_dir_off = match magic {
            0x010B => opt_off + 96,  // PE32
            0x020B => opt_off + 112, // PE32+
            other => panic!("unexpected PE optional header magic {other:#x}"),
        };
        let rsrc_rva = u32_le(pe, data_dir_off + 2 * 8);
        let rsrc_size = u32_le(pe, data_dir_off + 2 * 8 + 4);
        if rsrc_rva == 0 || rsrc_size == 0 {
            return BTreeMap::new();
        }

        let sec_table = opt_off + opt_size;
        let mut sections = Vec::with_capacity(num_sections);
        for i in 0..num_sections {
            let s = sec_table + i * 40;
            let vsize = u32_le(pe, s + 8);
            let va = u32_le(pe, s + 12);
            let raw_size = u32_le(pe, s + 16);
            let raw_ptr = u32_le(pe, s + 20);
            sections.push((va, vsize.max(raw_size), raw_ptr, raw_size));
        }

        let rva_to_off = |rva: u32, len: usize| -> usize {
            for &(va, span, raw_ptr, raw_size) in &sections {
                if rva >= va && (rva - va) < span {
                    let rel = (rva - va) as usize;
                    assert!(
                        rel + len <= raw_size as usize,
                        "resource slice at RVA {rva:#x} exceeds section raw data"
                    );
                    return raw_ptr as usize + rel;
                }
            }
            panic!("RVA {rva:#x} is not mapped in any PE section");
        };

        let rsrc_base = rva_to_off(rsrc_rva, rsrc_size as usize);
        let rsrc = &pe[rsrc_base..rsrc_base + rsrc_size as usize];
        let dir_entries = |dir_off: usize| -> Vec<(u32, u32)> {
            let named = usize::from(u16_le(rsrc, dir_off + 12));
            let ids = usize::from(u16_le(rsrc, dir_off + 14));
            let mut list = Vec::with_capacity(named + ids);
            for i in 0..(named + ids) {
                let e = dir_off + 16 + i * 8;
                list.push((u32_le(rsrc, e), u32_le(rsrc, e + 4)));
            }
            list
        };

        let mut resources = BTreeMap::new();
        for (type_id, type_next) in dir_entries(0) {
            if (type_id & 0x8000_0000) != 0 || (type_next & 0x8000_0000) == 0 {
                continue;
            }
            for (item_id, item_next) in dir_entries((type_next & 0x7FFF_FFFF) as usize) {
                if (item_id & 0x8000_0000) != 0 || (item_next & 0x8000_0000) == 0 {
                    continue;
                }
                for (lang_id, data_off) in dir_entries((item_next & 0x7FFF_FFFF) as usize) {
                    if (data_off & 0x8000_0000) != 0 {
                        continue;
                    }
                    let de = data_off as usize;
                    let data_rva = u32_le(rsrc, de);
                    let data_size = u32_le(rsrc, de + 4) as usize;
                    let file_off = rva_to_off(data_rva, data_size);
                    resources.insert(
                        (type_id, item_id, lang_id),
                        pe[file_off..file_off + data_size].to_vec(),
                    );
                }
            }
        }
        resources
    }

    fn utf16_nul(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn compiled_proxenos_exe_embeds_icon_and_version_resources() {
        let exe = fs::read(env!("CARGO_BIN_EXE_proxenos")).expect("read proxenos.exe");
        let resources = parse_pe_resources(&exe);

        let group = resources
            .get(&(RT_GROUP_ICON, 1, LANG_EN_US))
            .expect("RT_GROUP_ICON resource embedded in proxenos.exe");
        let ico_count = usize::from(u16_le(REPO_ICO, 4));
        assert_eq!(u16_le(group, 0), 0);
        assert_eq!(u16_le(group, 2), 1);
        assert_eq!(usize::from(u16_le(group, 4)), ico_count);
        assert_eq!(group.len(), 6 + ico_count * 14);

        for i in 0..ico_count {
            let ico_entry = 6 + i * 16;
            let grp_entry = 6 + i * 14;
            let size = u32_le(REPO_ICO, ico_entry + 8) as usize;
            let offset = u32_le(REPO_ICO, ico_entry + 12) as usize;
            assert_eq!(group[grp_entry], REPO_ICO[ico_entry]);
            assert_eq!(group[grp_entry + 1], REPO_ICO[ico_entry + 1]);
            assert_eq!(u32_le(group, grp_entry + 8) as usize, size);
            let icon_id = u32::from(u16_le(group, grp_entry + 12));
            assert_eq!(icon_id, (i + 1) as u32);
            let embedded = resources
                .get(&(RT_ICON, icon_id, LANG_EN_US))
                .expect("RT_ICON entry embedded");
            assert_eq!(embedded.as_slice(), &REPO_ICO[offset..offset + size]);
        }

        let version = resources
            .get(&(RT_VERSION, 1, LANG_EN_US))
            .expect("RT_VERSION resource embedded in proxenos.exe");
        assert!(
            version
                .windows(4)
                .any(|w| w == 0xFEEF_04BD_u32.to_le_bytes()),
            "missing VS_FIXEDFILEINFO signature"
        );
        for expected in [
            "Proxenos",
            "proxenos.exe",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_DESCRIPTION"),
            env!("CARGO_PKG_LICENSE"),
        ] {
            let needle = utf16_nul(expected);
            assert!(
                version.windows(needle.len()).any(|w| w == needle),
                "missing UTF-16LE version string {expected:?}"
            );
        }
    }

    #[cfg(feature = "test-tools")]
    #[test]
    fn proxenos_stub_does_not_embed_proxenos_icon() {
        let stub = fs::read(env!("CARGO_BIN_EXE_proxenos-stub")).expect("read proxenos-stub.exe");
        let resources = parse_pe_resources(&stub);
        assert!(
            !resources.contains_key(&(RT_GROUP_ICON, 1, LANG_EN_US)),
            "proxenos-stub.exe must not inherit proxenos's icon"
        );
    }
}
