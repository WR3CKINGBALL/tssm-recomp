//! Sharper text and sky. The game's fonts are small textures (letters about 13 texels tall
//! in a 256x256 atlas) drawn several times larger and unfiltered, which shows as jagged
//! steps; the flowers in Bikini Bottom's sky are thin outlines in a texture stretched over
//! the whole sky. Those textures on the disc are recognised by their contents when the game
//! uploads them, and are blown up 4x: the coverage (alpha) with a bicubic filter and then,
//! where the picture has a solid edge, pulled back to a sharp one, so letters and outlines
//! keep their shape with smooth edges. The sky's one blurred flower is drawn again as a copy
//! of a solid one (`redraw_blurred`).
//! They are then sampled with filtering (gx.rs). Only while a texture pack is in use (the
//! user's choice): it goes with the pack's sharper textures.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

/// The textures to sharpen, by width, height, GX format and XXH64 of their data.
static TEXTURES: OnceLock<HashMap<(u32, u32, u32, u64), Kind>> = OnceLock::new();

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Letters.
    Font,
    /// The sky's flowers, one of them drawn blurred, which is redrawn crisp.
    Flowers,
}

const SCALE: u32 = 4;
/// The sky flowers' texture chunks in BB/bb02.HOP on GGVE78 revision 1 (the other Bikini
/// Bottom levels have the same textures, which match by contents).
const FLOWERS: [u64; 2] = [0x323BC, 0x81DBC];

/// Reads the textures to sharpen from the disc's files folder; called once the disc is open.
pub fn find(files: &Path) {
    TEXTURES.get_or_init(|| {
        let mut found = HashMap::new();
        // The fonts: the paletted textures of 128 texels or more in font.HIP; the rest of it
        // is button pictures and a background.
        if let Ok(data) = std::fs::read(files.join("font.HIP")) {
            for at in (0..data.len()).step_by(4) {
                found.extend(
                    texture_at(&data, at)
                        .filter(|&(w, h, format, _)| crate::texture::palette_entries(format) > 0 && w >= 128 && h >= 128)
                        .map(|key| (key, Kind::Font)),
                );
            }
        }
        if let Ok(mut file) = std::fs::File::open(files.join("BB/bb02.HOP")) {
            for at in FLOWERS {
                let mut data = vec![0; 0x12000];
                if file.seek(SeekFrom::Start(at)).is_ok() && file.read_exact(&mut data).is_ok() {
                    found.extend(texture_at(&data, 0).map(|key| (key, Kind::Flowers)));
                }
            }
        }
        eprintln!("[sharpen] {} textures to sharpen", found.len());
        found
    });
}

/// The key of a RenderWare GameCube texture starting at `at`: a texture chunk (0x15) whose
/// struct chunk (1) is for platform 6, with the raster header 88 bytes into the struct and
/// the data after a word, the palette if any, and the size.
fn texture_at(data: &[u8], at: usize) -> Option<(u32, u32, u32, u64)> {
    let chunk = data.get(at..at + 24 + 104)?;
    if chunk[0..4] != [0x15, 0, 0, 0] || chunk[12..16] != [1, 0, 0, 0] || chunk[24..28] != [0, 0, 0, 6] {
        return None;
    }
    let header = &chunk[24 + 88..];
    let (w, h, format) = (u16::from_be_bytes([header[4], header[5]]) as u32, u16::from_be_bytes([header[6], header[7]]) as u32, header[10] as u32);
    let start = at + 24 + 88 + 16 + crate::texture::palette_entries(format) * 2 + 4;
    let size = crate::texture::data_size(format, w as usize, h as usize)?;
    Some((w, h, format, xxhash_rust::xxh64::xxh64(data.get(start..start + size)?, 0)))
}

/// Whether a texture the game uploads is one to sharpen, and which kind.
pub fn wanted(w: u32, h: u32, format: u32, data: &[u8]) -> Option<Kind> {
    let found = TEXTURES.get().filter(|found| !found.is_empty())?;
    found.get(&(w, h, format, xxhash_rust::xxh64::xxh64(data, 0))).copied()
}

/// A texture (RGBA8) 4x larger with smooth edges.
pub fn upscale(kind: Kind, w: u32, h: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    eprintln!("[sharpen] a {w}x{h} texture");
    let (w, h) = (w as usize, h as usize);
    let redrawn;
    let rgba = match kind {
        Kind::Flowers => {
            redrawn = redraw_blurred(rgba, w, h);
            &redrawn[..]
        }
        Kind::Font => rgba,
    };
    let (big_w, big_h) = (w * SCALE as usize, h * SCALE as usize);
    let texel = |x: isize, y: isize| {
        let (x, y) = (x.clamp(0, w as isize - 1) as usize, y.clamp(0, h as isize - 1) as usize);
        &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4]
    };
    let mut out = vec![0; big_w * big_h * 4];
    for y in 0..big_h {
        let sy = (y as f32 + 0.5) / SCALE as f32 - 0.5;
        let (y0, fy) = (sy.floor() as isize, sy - sy.floor());
        for x in 0..big_w {
            let sx = (x as f32 + 0.5) / SCALE as f32 - 0.5;
            let (x0, fx) = (sx.floor() as isize, sx - sx.floor());
            // Coverage: Catmull-Rom over the 4x4 texels around, then a steep smoothstep
            // around one half, which leaves an edge about a texel of the result wide. That
            // only where the texels around reach (nearly) full coverage: soft parts stay soft.
            let (wx, wy) = (catmull_rom(fx), catmull_rom(fy));
            let (mut cover, mut most) = (0.0, 0u8);
            for j in 0..4 {
                for i in 0..4 {
                    let a = texel(x0 + i as isize - 1, y0 + j as isize - 1)[3];
                    cover += wx[i] * wy[j] * a as f32;
                    most = most.max(a);
                }
            }
            let cover = (cover / 255.0).clamp(0.0, 1.0);
            let t = ((cover - 0.35) / 0.3).clamp(0.0, 1.0);
            let solid = ((most as f32 / 255.0 - 0.6) / 0.25).clamp(0.0, 1.0);
            let alpha = cover + (t * t * (3.0 - 2.0 * t) - cover) * solid;
            // Colour: the coverage-weighted bilinear mix of the 2x2 texels around.
            let mut colour = [0.0f32; 3];
            let mut weight = 0.0;
            for (dx, dy, k) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                let p = texel(x0 + dx, y0 + dy);
                let k = k * (p[3] as f32 + 1.0);
                for c in 0..3 {
                    colour[c] += p[c] as f32 * k;
                }
                weight += k;
            }
            let o = (y * big_w + x) * 4;
            for c in 0..3 {
                out[o + c] = (colour[c] / weight).round() as u8;
            }
            out[o + 3] = (alpha * 255.0).round() as u8;
        }
    }
    (big_w as u32, big_h as u32, out)
}

/// A flower in the sky textures: its bounding box and texels, and how solid it gets.
struct Shape {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    texels: Vec<usize>,
    peak: u8,
}

/// The sky textures have one flower drawn blurred, which no sharpening turns into a clean
/// outline (its petals run together in the middle). It is drawn again as a copy of the solid
/// flower closest to its size, fitted to its place inside the blur, in its own colour.
fn redraw_blurred(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    // Shapes: texels of some coverage joined to their 8 neighbours.
    const SOME: u8 = 26;
    let mut seen = vec![false; w * h];
    let mut shapes = Vec::new();
    for start in 0..w * h {
        if seen[start] || rgba[start * 4 + 3] < SOME {
            continue;
        }
        seen[start] = true;
        let (mut stack, mut shape) = (vec![start], Shape { left: w, top: h, right: 0, bottom: 0, texels: Vec::new(), peak: 0 });
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (shape.left, shape.top) = (shape.left.min(x), shape.top.min(y));
            (shape.right, shape.bottom) = (shape.right.max(x + 1), shape.bottom.max(y + 1));
            shape.peak = shape.peak.max(rgba[i * 4 + 3]);
            shape.texels.push(i);
            for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                    continue;
                }
                let n = ny as usize * w + nx as usize;
                if !seen[n] && rgba[n * 4 + 3] >= SOME {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        // Flowers, not the specks of light around them.
        if shape.right - shape.left >= 16 {
            shapes.push(shape);
        }
    }
    let (solid, blurred): (Vec<&Shape>, Vec<&Shape>) = shapes.iter().partition(|s| s.peak >= 230);
    let mut out = rgba.to_vec();
    for blur in blurred.iter().filter(|s| s.peak < 217) {
        // The blur spreads about 3 texels past the flower on every side.
        let (left, top) = (blur.left + 3, blur.top + 3);
        let (tw, th) = ((blur.right - blur.left).saturating_sub(6), (blur.bottom - blur.top).saturating_sub(6));
        let size = |s: &Shape| (s.right - s.left).abs_diff(tw) + (s.bottom - s.top).abs_diff(th);
        let Some(model) = solid.iter().min_by_key(|s| size(s)) else { continue };
        if tw < 8 || th < 8 {
            continue;
        }
        let mut colour = [0.0f32; 3];
        let mut total = 0.0;
        for &i in &blur.texels {
            let a = rgba[i * 4 + 3] as f32;
            for c in 0..3 {
                colour[c] += rgba[i * 4 + c] as f32 * a;
            }
            total += a;
            out[i * 4 + 3] = 0;
        }
        let colour = colour.map(|c| (c / total).round() as u8);
        // And the blur's faintest fringe, too faint to be part of the shape, around it.
        for y in blur.top.saturating_sub(3)..(blur.bottom + 3).min(h) {
            for x in blur.left.saturating_sub(3)..(blur.right + 3).min(w) {
                if rgba[(y * w + x) * 4 + 3] < SOME {
                    out[(y * w + x) * 4 + 3] = 0;
                }
            }
        }
        // The model's coverage scaled to the box, bilinearly.
        let (mw, mh) = (model.right - model.left, model.bottom - model.top);
        let coverage = |x: f32, y: f32| {
            let (x, y) = (x.clamp(0.0, mw as f32 - 1.0), y.clamp(0.0, mh as f32 - 1.0));
            let (x0, y0) = (x.floor() as usize, y.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(mw - 1), (y0 + 1).min(mh - 1));
            let (fx, fy) = (x - x0 as f32, y - y0 as f32);
            let a = |x: usize, y: usize| rgba[((model.top + y) * w + model.left + x) * 4 + 3] as f32;
            a(x0, y0) * (1.0 - fx) * (1.0 - fy) + a(x1, y0) * fx * (1.0 - fy) + a(x0, y1) * (1.0 - fx) * fy + a(x1, y1) * fx * fy
        };
        for y in 0..th {
            for x in 0..tw {
                let (sx, sy) = ((x as f32 + 0.5) * mw as f32 / tw as f32 - 0.5, (y as f32 + 0.5) * mh as f32 / th as f32 - 0.5);
                let a = coverage(sx, sy).round() as u8;
                let (px, py) = (left + x, top + y);
                if a > 0 && px < w && py < h {
                    let o = (py * w + px) * 4;
                    out[o..o + 4].copy_from_slice(&[colour[0], colour[1], colour[2], a]);
                }
            }
        }
    }
    out
}

fn catmull_rom(t: f32) -> [f32; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [
        (-t3 + 2.0 * t2 - t) / 2.0,
        (3.0 * t3 - 5.0 * t2 + 2.0) / 2.0,
        (-3.0 * t3 + 4.0 * t2 + t) / 2.0,
        (t3 - t2) / 2.0,
    ]
}
