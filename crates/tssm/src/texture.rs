//! Decoders for the GameCube's texture formats. Texels are stored in small tiles (blocks),
//! big-endian, and everything is decoded to tightly packed RGBA8.

fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}

fn expand(v: u16, bits: u32) -> u8 {
    (v as u32 * 255 / ((1 << bits) - 1)) as u8
}

fn rgb565(v: u16) -> [u8; 4] {
    [expand(v >> 11, 5), expand((v >> 5) & 63, 6), expand(v & 31, 5), 255]
}

/// Either opaque RGB555 or RGB444 with 3-bit alpha, chosen by the top bit.
fn rgb5a3(v: u16) -> [u8; 4] {
    if v & 0x8000 != 0 {
        [expand((v >> 10) & 31, 5), expand((v >> 5) & 31, 5), expand(v & 31, 5), 255]
    } else {
        [expand((v >> 8) & 15, 4), expand((v >> 4) & 15, 4), expand(v & 15, 4), expand(v >> 12, 3)]
    }
}

fn ia8(v: u16) -> [u8; 4] {
    let (a, i) = ((v >> 8) as u8, v as u8);
    [i, i, i, a]
}

/// Palette entry formats: 0 = IA8, 1 = RGB565, 2 = RGB5A3.
pub fn palette_color(format: u32, v: u16) -> [u8; 4] {
    match format {
        0 => ia8(v),
        1 => rgb565(v),
        _ => rgb5a3(v),
    }
}

/// (block width, block height, bytes per block) for a texture format, or None if unknown.
pub fn block_layout(format: u32) -> Option<(usize, usize, usize)> {
    Some(match format {
        0 | 8 | 14 => (8, 8, 32),  // I4, C4, CMPR
        1 | 2 | 9 => (8, 4, 32),   // I8, IA4, C8
        3 | 4 | 5 | 10 => (4, 4, 32), // IA8, RGB565, RGB5A3, C14X2
        6 => (4, 4, 64),           // RGBA8
        _ => return None,
    })
}

/// Bytes of source data a `w` x `h` texture occupies.
pub fn data_size(format: u32, w: usize, h: usize) -> Option<usize> {
    let (bw, bh, bytes) = block_layout(format)?;
    Some(w.div_ceil(bw) * h.div_ceil(bh) * bytes)
}

/// Number of palette entries a paletted format can reference (0 for direct-color formats).
pub fn palette_entries(format: u32) -> usize {
    match format {
        8 => 16,
        9 => 256,
        10 => 16384,
        _ => 0,
    }
}

/// One 4x4 S3TC/DXT1 block in the GameCube's bit order.
fn dxt_block(src: &[u8], mut put: impl FnMut(usize, usize, [u8; 4])) {
    let (c0, c1) = (be16(src, 0), be16(src, 2));
    let (a, b) = (rgb565(c0), rgb565(c1));
    let mix = |x: u8, y: u8, wx: u32, wy: u32| ((x as u32 * wx + y as u32 * wy) / (wx + wy)) as u8;
    let blend = |wx, wy| [mix(a[0], b[0], wx, wy), mix(a[1], b[1], wx, wy), mix(a[2], b[2], wx, wy), 255];
    let colors = if c0 > c1 {
        [a, b, blend(2, 1), blend(1, 2)]
    } else {
        let half = blend(1, 1);
        [a, b, half, [half[0], half[1], half[2], 0]]
    };
    for y in 0..4 {
        for x in 0..4 {
            put(x, y, colors[((src[4 + y] >> (6 - x * 2)) & 3) as usize]);
        }
    }
}

/// Decodes a texture to RGBA8. `palette` maps an index to a color for the paletted formats.
pub fn decode(format: u32, w: usize, h: usize, src: &[u8], palette: &dyn Fn(usize) -> [u8; 4]) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    let Some((bw, bh, bytes)) = block_layout(format) else { return out };
    let mut set = |x: usize, y: usize, px: [u8; 4]| {
        if x < w && y < h {
            out[(y * w + x) * 4..][..4].copy_from_slice(&px);
        }
    };
    let mut p = 0;
    for by in (0..h).step_by(bh) {
        for bx in (0..w).step_by(bw) {
            let block = &src[p..p + bytes];
            p += bytes;
            match format {
                14 => {
                    for sub in 0..4 {
                        let (ox, oy) = (bx + (sub & 1) * 4, by + (sub >> 1) * 4);
                        dxt_block(&block[sub * 8..], |x, y, px| set(ox + x, oy + y, px));
                    }
                }
                6 => {
                    for i in 0..16 {
                        let px = [block[i * 2 + 1], block[32 + i * 2], block[33 + i * 2], block[i * 2]];
                        set(bx + i % 4, by + i / 4, px);
                    }
                }
                _ => {
                    for n in 0..bw * bh {
                        let nibble = || (block[n / 2] >> if n % 2 == 0 { 4 } else { 0 }) & 15;
                        let px = match format {
                            0 => [nibble() * 17; 4],
                            1 => [block[n]; 4],
                            2 => {
                                let (a, i) = ((block[n] >> 4) * 17, (block[n] & 15) * 17);
                                [i, i, i, a]
                            }
                            3 => ia8(be16(block, n * 2)),
                            4 => rgb565(be16(block, n * 2)),
                            5 => rgb5a3(be16(block, n * 2)),
                            8 => palette(nibble() as usize),
                            9 => palette(block[n] as usize),
                            _ => palette((be16(block, n * 2) & 0x3FFF) as usize),
                        };
                        set(bx + n % bw, by + n / bw, px);
                    }
                }
            }
        }
    }
    out
}

/// FNV-1a, for recognising texture data that has not changed.
pub fn hash(data: &[u8]) -> u64 {
    data.iter().fold(0xCBF2_9CE4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3))
}
