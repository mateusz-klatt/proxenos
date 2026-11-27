# Proxenos icon assets

- `proxenos.svg` — canonical 64×64 vector mark (interlocking amber `#ecb157` and teal `#4ab8af` Greek capital Π on `#0d1929` rounded square `rx="14"`).
- `proxenos.ico` — multi-resolution Windows icon containing 16×16, 24×24, 32×32, and 48×48 32-bit BGRA BMP DIB frames (with 1-bit AND masks) plus 64×64 and 256×256 lossless RGBA PNG frames, deterministically rasterized from `proxenos.svg`.

At build time, `build.rs` uses the pure-Rust writer in `src/winres.rs` to:

1. Generate an inline SVG (`any`) and 64×64 PNG (`64x64`, extracted verbatim from `proxenos.ico`) data URI table for the MCP `serverInfo.icons` field (`src/mcp.rs`).
2. On Windows targets (`windows-msvc`, `windows-gnu`, `windows-gnullvm`), emit a deterministic COFF `.rsrc` object file (`RT_ICON`, `RT_GROUP_ICON`, and `RT_VERSION` `VS_VERSION_INFO`) linked only into the `proxenos` binary without calling `rc.exe`, `windres`, or a C compiler.
