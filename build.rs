//! Build script generating inline MCP icon data URIs and, on Windows targets, a deterministic
//! pure-Rust COFF `.rsrc` object containing the Proxenos icon and version metadata.

#[path = "src/winres.rs"]
mod winres;

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let mut out = std::io::stdout().lock();
    for path in [
        "assets/proxenos.svg",
        "assets/proxenos.ico",
        "src/winres.rs",
    ] {
        writeln!(out, "cargo::rerun-if-changed={path}").expect("stdout directive");
    }
    for var in [
        "CARGO_PKG_VERSION",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_LICENSE",
    ] {
        writeln!(out, "cargo::rerun-if-env-changed={var}").expect("stdout directive");
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));

    let svg_bytes =
        fs::read(manifest_dir.join("assets/proxenos.svg")).expect("read assets/proxenos.svg");
    let ico_bytes =
        fs::read(manifest_dir.join("assets/proxenos.ico")).expect("read assets/proxenos.ico");

    let ico_entries = winres::parse_ico(&ico_bytes).expect("parse assets/proxenos.ico");
    let png_64 = winres::find_png_frame(&ico_entries, 64, 64).expect("find 64x64 PNG in ICO");

    let svg_uri = format!(
        "data:image/svg+xml;base64,{}",
        winres::base64_encode(&svg_bytes)
    );
    let png_uri = format!("data:image/png;base64,{}", winres::base64_encode(png_64));
    let mcp_icons = format!(
        "pub const SVG_DATA_URI: &str = {svg_uri:?};\npub const PNG_64_DATA_URI: &str = {png_uri:?};\n"
    );
    fs::write(out_dir.join("mcp_icons.rs"), mcp_icons).expect("write mcp_icons.rs");

    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
        let arch = winres::CoffArch::from_cargo_arch(&target_arch).expect("supported Windows arch");
        let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
        let description = env::var("CARGO_PKG_DESCRIPTION").expect("CARGO_PKG_DESCRIPTION");
        let license = env::var("CARGO_PKG_LICENSE").expect("CARGO_PKG_LICENSE");
        let info = winres::VersionInfo {
            version: &version,
            product_name: "Proxenos",
            file_description: &description,
            internal_name: "proxenos",
            original_filename: "proxenos.exe",
            legal_copyright: &license,
        };
        let coff = winres::build_rsrc_coff(arch, &ico_bytes, &info).expect("build COFF .rsrc");
        let obj_path = out_dir.join("proxenos-resources.o");
        fs::write(&obj_path, coff).expect("write proxenos-resources.o");
        writeln!(
            out,
            "cargo::rustc-link-arg-bin=proxenos={}",
            obj_path.display()
        )
        .expect("stdout link-arg directive");
    }
}
