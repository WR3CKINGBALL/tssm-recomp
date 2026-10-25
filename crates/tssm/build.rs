// Gives the Windows executable the game's own icon: SpongeBob's round badge from the menus
// (a 128x128 CI8 texture in files\MN\mnui.HIP). It is game data, so it is not in the repo:
// the builder points TSSM_DISC at the extracted disc, and without it the disc next to the
// workspace is used. No disc, no icon; the build works either way.
//
// The icon goes in a .res file (the compiled form of a resource script) written here and
// handed to the MSVC linker, which takes .res files as input: an RT_GROUP_ICON (id 1, which
// lib.rs also gives the window) and one RT_ICON per size.

use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// Where the badge's RenderWare texture chunk starts in mnui.HIP on GGVE78 revision 1.
const BADGE_AT: usize = 0x3683C;
const SIZE: usize = 128;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TSSM_DISC");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    let disc = std::env::var_os("TSSM_DISC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
    let hip = disc.join("files/MN/mnui.HIP");
    let Ok(data) = std::fs::read(&hip) else {
        println!("cargo:warning=no icon: {} not found", hip.display());
        return;
    };
    println!("cargo:rerun-if-changed={}", hip.display());
    let Some(badge) = decode_badge(&data) else {
        println!("cargo:warning=no icon: the badge texture is not where it should be in {}", hip.display());
        return;
    };

    let sizes = [16, 24, 32, 48, 64, 128, 256];
    let mut res = Vec::new();
    entry(&mut res, 0, 0, &[]); // a .res file starts with an empty entry
    let mut group = vec![0, 0, 1, 0]; // reserved, type 1 (icon)
    group.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    for (i, &size) in sizes.iter().enumerate() {
        let dib = dib(size, &resize(&badge, size));
        let id = i as u16 + 1;
        group.extend_from_slice(&[(size % 256) as u8, (size % 256) as u8, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits a pixel
        group.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
        entry(&mut res, RT_ICON, id, &dib);
    }
    entry(&mut res, RT_GROUP_ICON, 1, &group);

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icon.res");
    std::fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// The badge as RGBA, after checking its texture header: a GameCube native texture
/// (platform 6), 128x128, CI8 (GX format 9) with an RGB5A3 palette (2). The struct holds
/// the platform, filtering, name and mask, the raster header (at +88), a word, the
/// palette, the image size, and the image in 8x4 texel tiles.
fn decode_badge(data: &[u8]) -> Option<Vec<[u8; 4]>> {
    let at = BADGE_AT + 24; // past the texture and struct chunk headers
    let header = data.get(at..at + 104)?;
    if data.get(BADGE_AT..BADGE_AT + 4)? != [0x15, 0, 0, 0]
        || header[0..4] != [0, 0, 0, 6]
        || header[92..100] != [0, 0x80, 0, 0x80, 8, 1, 9, 2]
    {
        return None;
    }
    let palette: Vec<[u8; 4]> = data
        .get(at + 104..at + 104 + 512)?
        .chunks(2)
        .map(|c| rgb5a3(u16::from_be_bytes([c[0], c[1]])))
        .collect();
    let mut at = at + 104 + 512 + 4;
    let mut pixels = vec![[0; 4]; SIZE * SIZE];
    for tile_y in (0..SIZE).step_by(4) {
        for tile_x in (0..SIZE).step_by(8) {
            for y in 0..4 {
                for x in 0..8 {
                    pixels[(tile_y + y) * SIZE + tile_x + x] = palette[*data.get(at)? as usize];
                    at += 1;
                }
            }
        }
    }
    Some(pixels)
}

fn rgb5a3(v: u16) -> [u8; 4] {
    let five = |s: u16| ((v >> s & 31) * 255 / 31) as u8;
    let four = |s: u16| ((v >> s & 15) * 17) as u8;
    if v & 0x8000 != 0 {
        [five(10), five(5), five(0), 255]
    } else {
        [four(8), four(4), four(0), ((v >> 12 & 7) * 255 / 7) as u8]
    }
}

/// The badge at another size: each output pixel a tent-weighted average of the source
/// pixels under it (wider for shrinking), in premultiplied alpha so that the transparent
/// corners do not darken the edge.
fn resize(pixels: &[[u8; 4]], size: usize) -> Vec<[u8; 4]> {
    let scale = SIZE as f32 / size as f32;
    let radius = scale.max(1.0);
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let (cx, cy) = ((x as f32 + 0.5) * scale - 0.5, (y as f32 + 0.5) * scale - 0.5);
            let mut sum = [0f32; 4];
            let mut total = 0.0;
            let (x0, x1) = ((cx - radius).floor().max(0.0) as usize, ((cx + radius).ceil() as usize).min(SIZE - 1));
            let (y0, y1) = ((cy - radius).floor().max(0.0) as usize, ((cy + radius).ceil() as usize).min(SIZE - 1));
            for sy in y0..=y1 {
                for sx in x0..=x1 {
                    let weight = (1.0 - (sx as f32 - cx).abs() / radius).max(0.0)
                        * (1.0 - (sy as f32 - cy).abs() / radius).max(0.0);
                    let p = pixels[sy * SIZE + sx];
                    let alpha = p[3] as f32 / 255.0;
                    for c in 0..3 {
                        sum[c] += p[c] as f32 * alpha * weight;
                    }
                    sum[3] += alpha * weight;
                    total += weight;
                }
            }
            let alpha = sum[3] / total;
            let colour = |c: usize| if sum[3] > 0.0 { (sum[c] / sum[3]).round().min(255.0) as u8 } else { 0 };
            out.push([colour(0), colour(1), colour(2), (alpha * 255.0).round() as u8]);
        }
    }
    out
}

/// An icon image as a device-independent bitmap: header, BGRA rows bottom up, AND mask.
fn dib(size: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in [40u32, size as u32, size as u32 * 2] {
        out.extend_from_slice(&value.to_le_bytes()); // header size, width, height (with mask)
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // uncompressed; sizes, resolutions and colours unused
    for y in (0..size).rev() {
        for p in &pixels[y * size..(y + 1) * size] {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    // The alpha channel decides transparency; the mask, rows padded to 4 bytes, is all clear.
    out.resize(out.len() + size.div_ceil(32) * 4 * size, 0);
    out
}

/// One resource: data size, header size, type and name as ordinals, then version, flags
/// (moveable, pure), language (US English), and the data padded to 4 bytes.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for ordinal in [kind, name] {
        res.extend_from_slice(&0xFFFFu16.to_le_bytes());
        res.extend_from_slice(&ordinal.to_le_bytes());
    }
    let (flags, language) = if kind == 0 { (0u16, 0u16) } else { (0x1030, 0x0409) };
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&language.to_le_bytes());
    res.extend_from_slice(&[0; 8]); // version, characteristics
    res.extend_from_slice(data);
    res.resize(res.len().next_multiple_of(4), 0);
}
