//! Consumer for the GX command stream.
//!
//! The recompiled GX library encodes all graphics state and geometry as commands written to
//! the write-gather pipe; `Ctx` captures those bytes. This module decodes the stream: register
//! loads for the three GPU blocks (BP, CP, XF), display-list calls and draw commands. For now
//! it tracks register state and frame statistics; the renderer will hang off the same decoder.

use runtime::Ctx;

use crate::render::{
    Draw, DrawState, Flat, Frame, Geometry, RawVertex, Renderer, SamplerKey, Vertex, PARAMS_FLOATS, TEXTURE_SLOTS,
    VPARAMS_WORDS, WHITE_TEXTURE,
};
use std::sync::Arc;
use crate::tev;
use std::sync::atomic::Ordering::Relaxed;
use crate::texture;

fn in_ram(addr: u32, len: usize) -> bool {
    let end = addr as u64 + len as u64;
    addr >= runtime::mem::RAM_BASE && end <= (runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE) as u64
}

/// Byte widths of position/normal/texcoord component formats: u8 s8 u16 s16 f32.
const COMP: [usize; 8] = [1, 1, 2, 2, 4, 0, 0, 0];
/// Byte widths of vertex color formats: 565, 888, 888x, 4444, 6666, 8888.
const COLOR: [usize; 8] = [2, 3, 4, 2, 3, 4, 0, 0];

/// Copies one vertex attribute into `buf`: from the command stream when direct, or from the
/// attribute's array in guest memory when indexed. `kind` is the descriptor's 2-bit code.
/// Returns the index used, or `u32::MAX` when there was none.
#[inline(always)]
fn fetch(c: Mem, data: &[u8], o: &mut usize, kind: u32, size: usize, array: (u32, u32), buf: &mut [u8; 16]) -> u32 {
    let index = match kind {
        1 => {
            buf[..size].copy_from_slice(&data[*o..*o + size]);
            *o += size;
            return u32::MAX;
        }
        2 => {
            *o += 1;
            data[*o - 1] as u32
        }
        3 => {
            *o += 2;
            be16(data, *o - 2) as u32
        }
        _ => return u32::MAX,
    };
    let addr = array.0.wrapping_add(index * array.1);
    if in_ram(addr, 16) {
        // No attribute is longer than the buffer; copying all of it beats a loop over `size`.
        buf[..8].copy_from_slice(&c.r64(addr).to_be_bytes());
        buf[8..].copy_from_slice(&c.r64(addr + 8).to_be_bytes());
        return index;
    }
    for (k, byte) in buf[..size].iter_mut().enumerate() {
        *byte = c.r8(addr.wrapping_add(k as u32));
    }
    index
}

/// How a draw's vertices are laid out: the vertex descriptor, the part of its vertex
/// format's attribute table that covers the first four texture coordinates, and (base
/// address, stride) of the twelve attribute arrays.
#[derive(Clone, Copy)]
struct Format {
    vcd_lo: u32,
    vcd_hi: u32,
    a: u32,
    b: u32,
    arrays: [(u32, u32); 12],
}

/// Adds a draw's vertices to `out` as the game sent them, for the vertex shader to transform
/// (`vs_gx` in render.rs). Only for vertices without matrix indices and with at most the
/// first four texture coordinate attributes. `ranges` takes in the lowest and highest index
/// used into each attribute's array.
fn decode_raw(mem: Mem, data: &[u8], count: usize, format: &Format, out: &mut Vec<RawVertex>, ranges: &mut [(u32, u32); 12]) {
    let Format { vcd_lo, vcd_hi, a, b, ref arrays } = *format;
    let kind = |shift: u32| (vcd_lo >> shift) & 3;
    let indexed = |k: u32| match k {
        2 => 1,
        3 => 2,
        _ => 0,
    };
    let (pos_count, pos_format) = (2 + (a & 1) as usize, (a >> 1) & 7);
    let pos_scale = 1.0 / (1u64 << ((a >> 4) & 31)) as f32;
    let pos_size = pos_count * COMP[pos_format as usize];
    let nbt = (a >> 9) & 1 != 0;
    let normal_format = (a >> 10) & 7;
    let normal_size = match kind(11) {
        1 => (if nbt { 9 } else { 3 }) * COMP[normal_format as usize],
        k => indexed(k) * (if nbt && a >> 31 != 0 { 3 } else { 1 }),
    };
    let normal_bytes = 3 * COMP[normal_format as usize];
    let color_format = [(a >> 14) & 7, (a >> 18) & 7];
    let color_kinds = [kind(13), kind(15)];
    let tex = [(a >> 21, a >> 22), (b, b >> 1), (b >> 9, b >> 10), (b >> 18, b >> 19)];
    let tex_kind = |i: usize| (vcd_hi >> (i * 2)) & 3;
    let tex_scale = [a >> 25, b >> 4, b >> 13, b >> 22].map(|shift| 1.0 / (1u64 << (shift & 31)) as f32);
    let mut note = |n: usize, index: u32| {
        if index != u32::MAX {
            ranges[n] = (ranges[n].0.min(index), ranges[n].1.max(index));
        }
    };
    out.reserve(count);
    let mut buf = [0u8; 16];
    let mut o = 0;
    for _ in 0..count {
        note(0, fetch(mem, data, &mut o, kind(9), pos_size, arrays[0], &mut buf));
        let comp = |i: usize| match pos_format {
            0 => buf[i] as f32 * pos_scale,
            1 => buf[i] as i8 as f32 * pos_scale,
            2 => u16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 * pos_scale,
            3 => i16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 * pos_scale,
            _ => f32::from_be_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()),
        };
        let pos = [comp(0), comp(1), if pos_count == 3 { comp(2) } else { 0.0 }];
        let mut normal = [0.0f32; 3];
        if kind(11) != 0 {
            let mut at = o;
            note(1, fetch(mem, data, &mut at, kind(11), normal_bytes, arrays[1], &mut buf));
            normal = std::array::from_fn(|i| match normal_format {
                0 | 1 => buf[i] as i8 as f32 / 64.0,
                2 | 3 => i16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 / 16384.0,
                _ => f32::from_be_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()),
            });
        }
        o += normal_size;
        let mut colors = [[0.0f32; 4]; 2];
        for n in 0..2 {
            if color_kinds[n] != 0 {
                let format = color_format[n];
                note(2 + n, fetch(mem, data, &mut o, color_kinds[n], COLOR[format as usize], arrays[2 + n], &mut buf));
                colors[n] = decode_color(format, &buf);
            }
        }
        let mut coords = [[0.0f32; 2]; 4];
        for i in 0..4 {
            let k = tex_kind(i);
            if k == 0 {
                continue;
            }
            let (components, component) = (1 + (tex[i].0 & 1) as usize, tex[i].1 & 7);
            note(4 + i, fetch(mem, data, &mut o, k, components * COMP[component as usize], arrays[4 + i], &mut buf));
            let scale = tex_scale[i];
            for (n, out) in coords[i].iter_mut().enumerate().take(components) {
                *out = match component {
                    0 => buf[n] as f32 * scale,
                    1 => buf[n] as i8 as f32 * scale,
                    2 => u16::from_be_bytes([buf[n * 2], buf[n * 2 + 1]]) as f32 * scale,
                    3 => i16::from_be_bytes([buf[n * 2], buf[n * 2 + 1]]) as f32 * scale,
                    _ => f32::from_be_bytes(buf[n * 4..n * 4 + 4].try_into().unwrap()),
                };
            }
        }
        out.push(RawVertex { pos, pad0: 0.0, normal, pad1: 0.0, colors, tex: coords });
    }
}

/// Adds the triangles of a primitive of `count` vertices starting at vertex `base`.
fn emit_indices(op: u8, base: u32, count: u32, out: &mut Vec<u32>) {
    match op & 0xF8 {
        0x80 | 0x88 => {
            for q in (0..count / 4).map(|q| base + q * 4) {
                out.extend_from_slice(&[q, q + 1, q + 2, q, q + 2, q + 3]);
            }
        }
        0x90 => out.extend(base..base + count - count % 3),
        0x98 => {
            for i in 0..count.saturating_sub(2) {
                let v = base + i;
                out.extend_from_slice(&if i % 2 == 0 { [v, v + 1, v + 2] } else { [v + 1, v, v + 2] });
            }
        }
        0xA0 => {
            for i in 1..count.saturating_sub(1) {
                out.extend_from_slice(&[base, base + i, base + i + 1]);
            }
        }
        _ => {} // lines and points: not drawn yet
    }
}

/// A quick hash of a stretch of memory, to notice when it has changed.
fn hash_bytes(data: &[u8]) -> u64 {
    const K: [u64; 4] = [0x9E37_79B9_7F4A_7C15, 0xC2B2_AE3D_27D4_EB4F, 0x1656_67B1_9E37_79F9, 0x27D4_EB2F_1656_67C5];
    let mut lanes = K;
    let mut chunks = data.chunks_exact(32);
    for chunk in &mut chunks {
        for k in 0..4 {
            let word = u64::from_le_bytes(chunk[k * 8..k * 8 + 8].try_into().unwrap());
            lanes[k] = (lanes[k] ^ word).wrapping_mul(K[k]).rotate_left(29);
        }
    }
    let mut hash = lanes[0] ^ lanes[1].rotate_left(13) ^ lanes[2].rotate_left(27) ^ lanes[3].rotate_left(41) ^ data.len() as u64;
    for &byte in chunks.remainder() {
        hash = (hash ^ byte as u64).wrapping_mul(K[0]).rotate_left(11);
    }
    hash ^ (hash >> 31)
}

/// Guest memory is hashed in blocks of this many bytes (a power of two).
const BLOCK_SHIFT: u32 = 8;

/// TSSM_CPU_VERTICES=1 brings back the path that transforms and lights every vertex on the
/// CPU each frame, to compare with the vertex shader and the display list cache.
/// TSSM_CPU_VERTICES=even does so for the even-numbered frames only, so that two screenshots
/// one frame apart show the same scene drawn both ways.
fn cpu_vertices(frame: u64) -> bool {
    static MODE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    match *MODE.get_or_init(|| std::env::var("TSSM_CPU_VERTICES").ok().map(|value| value == "even")) {
        None => false,
        Some(false) => true,
        Some(true) => frame % 2 == 0,
    }
}

fn late_traced() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSSM_TRACE_LATE").is_some())
}

fn perf_traced() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSSM_TRACE_PERF").is_some())
}

#[inline(always)]
fn decode_color(format: u32, b: &[u8; 16]) -> [f32; 4] {
    // By the reciprocal: a division for every component of every vertex is slow, and the last
    // bit of the result does not matter.
    let wide = |v: u32, bits: u32| v as f32 * (1.0 / ((1u32 << bits) - 1) as f32);
    match format {
        0 => {
            let v = (b[0] as u32) << 8 | b[1] as u32;
            [wide(v >> 11, 5), wide((v >> 5) & 63, 6), wide(v & 31, 5), 1.0]
        }
        1 | 2 => [wide(b[0] as u32, 8), wide(b[1] as u32, 8), wide(b[2] as u32, 8), 1.0],
        3 => {
            let v = (b[0] as u32) << 8 | b[1] as u32;
            [wide(v >> 12, 4), wide((v >> 8) & 15, 4), wide((v >> 4) & 15, 4), wide(v & 15, 4)]
        }
        4 => {
            let v = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            [wide(v >> 18, 6), wide((v >> 12) & 63, 6), wide((v >> 6) & 63, 6), wide(v & 63, 6)]
        }
        _ => [wide(b[0] as u32, 8), wide(b[1] as u32, 8), wide(b[2] as u32, 8), wide(b[3] as u32, 8)],
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Unit vector along `v`, or zero for a zero vector. Scaled first so that the very distant
/// positions used for directional lights do not overflow when squared.
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let squared = dot(v, v);
    if squared.is_finite() && squared > 1e-30 {
        let scale = 1.0 / squared.sqrt();
        return v.map(|x| x * scale);
    }
    let largest = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if largest == 0.0 || !largest.is_finite() {
        return [0.0; 3];
    }
    let v = v.map(|x| x / largest);
    let length = dot(v, v).sqrt();
    v.map(|x| x / length)
}

/// One of the eight lights (XF 0x600 + 16n). Position and direction are in view space.
#[derive(Clone, Copy, Default)]
struct Light {
    color: [f32; 4],
    /// Angle attenuation coefficients: a0 + a1 cos + a2 cos^2.
    cos_att: [f32; 3],
    /// Distance attenuation coefficients: k0 + k1 d + k2 d^2.
    dist_att: [f32; 3],
    pos: [f32; 3],
    dir: [f32; 3],
}

impl Light {
    /// How strongly this light reaches a vertex at `pos` with unit normal `normal` (both in
    /// view space) under a channel control register: its attenuation function (bits 9-10:
    /// none, specular, none, spotlight) times its diffuse function (bits 7-8: none, signed,
    /// clamped).
    fn amount(&self, ctrl: u32, pos: [f32; 3], normal: [f32; 3]) -> f32 {
        let to_light = [self.pos[0] - pos[0], self.pos[1] - pos[1], self.pos[2] - pos[2]];
        let mut direction = normalize(to_light);
        let diffuse = (ctrl >> 7) & 3;
        let ratio = |above: f32, below: f32| if below == 0.0 { 0.0 } else { above.max(0.0) / below };
        let attenuation = match (ctrl >> 9) & 3 {
            1 => {
                let cos = if dot(direction, normal) >= 0.0 { dot(self.dir, normal).max(0.0) } else { 0.0 };
                let powers = [1.0, cos, cos * cos];
                let dist_att = if diffuse == 0 { self.dist_att } else { normalize(self.dist_att) };
                ratio(dot(powers, self.cos_att), dot(powers, dist_att))
            }
            3 => {
                let distance = dot(to_light, to_light).sqrt();
                let cos = dot(direction, self.dir).max(0.0);
                ratio(dot([1.0, cos, cos * cos], self.cos_att), dot([1.0, distance, distance * distance], self.dist_att))
            }
            _ => {
                if direction == [0.0; 3] {
                    direction = normal;
                }
                1.0
            }
        };
        match diffuse {
            0 => attenuation,
            1 => attenuation * dot(direction, normal),
            _ => attenuation * dot(direction, normal).max(0.0),
        }
    }
}

/// A color channel's lighting setup: material and ambient colors (XF 0x100C and 0x100A + n) and
/// the control registers for its color (0x100E + n) and its alpha (0x1010 + n).
#[derive(Clone, Copy)]
struct Channel {
    material: [f32; 4],
    ambient: [f32; 4],
    ctrl: [u32; 2],
}

/// What a draw's pixels are made from: the shader for the combiner state, the textures it
/// samples and how, which texture coordinates it reads (with the correction `map_texture`
/// gives for each), and whether one of the textures is a copy of the framebuffer.
#[derive(Clone, Copy)]
struct Material {
    shader: u64,
    textures: [usize; TEXTURE_SLOTS],
    samplers: [SamplerKey; TEXTURE_SLOTS],
    coord_used: [bool; 8],
    coord_bias: [[f32; 2]; 8],
    samples_copy: bool,
}

/// Where a texture coordinate's input comes from.
#[derive(Clone, Copy)]
enum Source {
    Position,
    Tex(usize),
    Nothing,
}

/// The position matrices (3x4, row-major, four words per index) and normal matrices (3x3,
/// three words per index) a draw's vertices may use.
enum Matrices {
    /// The draw's one pair.
    One([f32; 12], [f32; 9]),
    /// For vertices that each choose their own: XF 0x000-0x0FF, then 0x400-0x45F.
    All(Box<[u32; 0x160]>),
}

impl Matrices {
    fn rows(&self, matrix: usize) -> ([f32; 12], [f32; 9]) {
        match self {
            Matrices::One(position, normal) => (*position, *normal),
            Matrices::All(all) => (
                std::array::from_fn(|k| f32::from_bits(all[(matrix * 4 + k) & 0xFF])),
                std::array::from_fn(|k| f32::from_bits(all[0x100 + (matrix & 31) * 3 + k])),
            ),
        }
    }
}

/// Guest memory, for reading vertex arrays from several threads while the game is stopped.
#[derive(Clone, Copy)]
struct Mem(*const u8);

unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

impl Mem {
    fn r8(self, addr: u32) -> u8 {
        unsafe { self.0.add(addr as usize).read() }
    }

    fn r64(self, addr: u32) -> u64 {
        unsafe { u64::from_be((self.0.add(addr as usize) as *const u64).read_unaligned()) }
    }
}

/// The helpers that transform vertices alongside the game's thread: three threads in all by
/// default (fewer on a processor with fewer than five cores). More do little: three are
/// nearly as fast as six on the desktop. On a phone the threads only pay off because the
/// system is told how long frames take (hint.rs); without that it ran them all slowly.
/// TSSM_GX_THREADS sets the number, the game's thread included; 1 keeps all the work there.
fn vertex_threads() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let wanted = std::env::var("TSSM_GX_THREADS").ok().and_then(|v| v.parse().ok());
        let threads: usize = wanted.unwrap_or(cores.saturating_sub(2).clamp(1, 3));
        if threads < 2 {
            return None;
        }
        // The game's thread is one of them.
        let name = |i: usize| format!("vertices {i}");
        let started = |_| crate::hint::helper_started();
        rayon::ThreadPoolBuilder::new().num_threads(threads - 1).thread_name(name).start_handler(started).build().ok()
    })
    .as_ref()
}

/// A list whose elements are handed out in batches, each batch to one thread only.
struct Claimed<T>(*mut T, usize);

unsafe impl<T: Send> Sync for Claimed<T> {}

impl<T> Claimed<T> {
    /// The elements from `start` to `end`. The caller must be the only one to ask for them.
    #[allow(clippy::mut_from_ref)]
    fn batch(&self, start: usize, end: usize) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.0.add(start), end - start) }
    }
}

/// How many tasks a thread takes at a time.
const TASK_BATCH: usize = 32;
/// The most vertices one task transforms; larger draws are split between threads.
const TASK_VERTICES: usize = 512;
/// Fewer vertices than this in a batch are not worth handing to other threads.
const PARALLEL_FROM: usize = 2048;

/// One draw's vertices still to be decoded and transformed.
struct VertexJob {
    /// The draw's vertex data, within `Gx::job_data`.
    data: std::ops::Range<usize>,
    count: usize,
    /// Index of its first vertex in the frame.
    base: u32,
    /// The attribute table of its vertex format.
    a: u32,
    b: u32,
    cc: u32,
    /// Whether to list the vertices (TSSM_DUMP_FRAME).
    dump: bool,
    /// The heads-up display widget the draw belongs to, or 0.
    hud: u32,
    state: Arc<VState>,
}

/// Everything a draw's vertices depend on in the state of the registers when the draw was
/// issued, apart from its vertex format. Shared by the draws that follow one another with
/// no register changed between them.
struct VState {
    vcd_lo: u32,
    vcd_hi: u32,
    arrays: [(u32, u32); 12],
    default_matrix: usize,
    matrices: Matrices,
    gens: [(Source, usize, bool); 8],
    gen_rows: [[f32; 12]; 8],
    coord_used: [bool; 8],
    coord_bias: [[f32; 2]; 8],
    channels: [Channel; 2],
    lit_channels: usize,
    lighting: bool,
    lights: [Light; 8],
    proj: [f32; 6],
    orthographic: bool,
    /// Viewport half-extents and centre in console pixels, for snapping 2D geometry.
    snap: [f32; 4],
    samples_copy: bool,
    /// The model's origin in clip coordinates, when drawn in perspective in front of the camera.
    origin: Option<[f32; 4]>,
    /// Whether the vertex shader can do these vertices: drawn in perspective, without matrix
    /// indices, and with nothing beyond the first four texture coordinate attributes.
    raw_ok: bool,
}

/// The vertex shader's parameters for draws in this state (`VParams` in render.rs). With
/// `over_flat` the block is for a model drawn over flat drawing, which may be placed again
/// when the frame is shown (`Frame::place_again`); it is then like no other block.
fn vparams_block(state: &VState, over_flat: Option<[f32; 4]>) -> [u32; VPARAMS_WORDS] {
    let mut words = [0u32; VPARAMS_WORDS];
    if let Some(origin) = over_flat {
        for k in 0..4 {
            words[VPARAMS_WORDS - 8 + k] = origin[k].to_bits();
        }
        words[VPARAMS_WORDS - 4] = 1.0f32.to_bits();
        words[VPARAMS_WORDS - 1] = 1.0f32.to_bits();
    }
    let mut put = |at: usize, values: &[f32]| {
        for (k, value) in values.iter().enumerate() {
            words[at + k] = value.to_bits();
        }
    };
    put(0, &state.proj[..4]);
    put(4, &[state.proj[4], state.proj[5], state.lighting as u32 as f32]);
    let (position, normal) = state.matrices.rows(state.default_matrix);
    put(8, &position);
    for row in 0..3 {
        put(20 + row * 4, &normal[row * 3..row * 3 + 3]);
    }
    for n in 0..8 {
        let at = 32 + n * 16;
        put(at, &state.gen_rows[n]);
        let source = match state.gens[n].0 {
            _ if !state.coord_used[n] => -1.0,
            Source::Position => 0.0,
            Source::Tex(k) => 1.0 + k as f32,
            Source::Nothing => 5.0,
        };
        put(at + 12, &[source, state.gens[n].2 as u32 as f32, state.coord_bias[n][0], state.coord_bias[n][1]]);
    }
    for n in 0..2 {
        let at = 160 + n * 12;
        put(at, &state.channels[n].material);
        put(at + 4, &state.channels[n].ambient);
    }
    for n in 0..8 {
        let (at, light) = (184 + n * 20, &state.lights[n]);
        put(at, &light.color);
        put(at + 4, &light.cos_att);
        put(at + 8, &light.dist_att);
        put(at + 12, &light.pos);
        put(at + 16, &light.dir);
    }
    for n in 0..2 {
        let at = 160 + n * 12 + 8;
        let has_color = (state.vcd_lo >> (13 + 2 * n)) & 3 != 0;
        words[at] = state.channels[n].ctrl[0];
        words[at + 1] = state.channels[n].ctrl[1];
        words[at + 2] = has_color as u32 | ((n < state.lit_channels) as u32) << 1;
    }
    words
}

/// What draws share while no register changes between them: the material, the state of the
/// vertices, and where in the frame their parameter blocks are.
struct Template {
    /// Told apart from every other template of the run.
    serial: u64,
    material: Material,
    vstate: Arc<VState>,
    state: DrawState,
    viewport: [f32; 4],
    scissor: [u32; 4],
    params: u32,
    /// The vertex shader's parameter block, once a draw has needed it, and the one for
    /// draws over flat drawing.
    vparams: Option<u32>,
    vparams_flat: Option<u32>,
}

impl Template {
    /// The offset of the vertex shader's parameter block for a draw from this template.
    fn vparams(&mut self, frame: &mut Frame, flat_started: bool) -> u32 {
        // A model drawn over the frame's flat drawing is placed by its origin (see
        // `finish_vertices`); one behind the camera has none and is left alone.
        match self.vstate.origin.filter(|_| flat_started) {
            Some(origin) => *self.vparams_flat.get_or_insert_with(|| {
                let offset = frame.push_vparams(&vparams_block(&self.vstate, Some(origin)));
                frame.raw_flat.push((offset, origin));
                offset
            }),
            None => *self.vparams.get_or_insert_with(|| frame.push_vparams(&vparams_block(&self.vstate, None))),
        }
    }
}

/// What is known of a display list from its bytes and the vertex layout alone.
#[derive(Clone, Copy)]
struct ListShape {
    /// Whether it holds nothing but draws; otherwise it is decoded command by command.
    plain: bool,
    /// (offset from the array's start, length) of what it reads of each attribute's array;
    /// length 0 for none.
    spans: [(u32, u32); 12],
    index_count: u32,
    draws: u32,
    vertices: u32,
}

/// A display list's vertices as they were when the game called it (`Gx::look_ahead`).
enum Ahead {
    /// In the cache's buffers: first vertex and first index.
    Cached(ListShape, (i32, u32)),
    /// These, with indices counted from the first of them.
    Owned(ListShape, Vec<RawVertex>, Vec<u32>),
    /// Not taken down: the list is decoded from memory when the decoder comes to it.
    Late,
}

/// What a display list read of the attribute arrays the last time they were where they are:
/// the hash of the blocks of memory around it, and of exactly it.
#[derive(Clone, Copy)]
struct ListCheck {
    blocks: u64,
    exact: u64,
}

/// How often a display list has had to be put in the cache anew: the frame it last was, how
/// many frames running, and the frame until which it is left out of the cache.
#[derive(Clone, Copy, Default)]
struct Churn {
    /// How many times it was put in the cache, and how many vertices that came to.
    builds: u32,
    vertices: u64,
    built: u64,
    run: u8,
    out_until: u64,
    /// How many times it has been left out; each time is twice as long as the last.
    outs: u8,
}

impl VertexJob {
    /// Writes the draw's vertices from number `first` on to `out`, as many as fit there, and
    /// returns how far left and right they reach (orthographic draws only).
    fn run(&self, mem: Mem, data: &[u8], first: usize, out: &mut [std::mem::MaybeUninit<Vertex>]) -> (f32, f32) {
        let VertexJob { a, b, cc, .. } = *self;
        let VState {
            vcd_lo, vcd_hi, ref gens, ref gen_rows, coord_used, coord_bias, ref channels, lit_channels, lighting,
            ref lights, proj, orthographic, ..
        } = *self.state;
        let data = &data[self.data.clone()];
        let [snap_w, snap_h, snap_cx, snap_cy] = self.state.snap;
        let dump_vertices = self.dump;
        let kind = |shift: u32| (vcd_lo >> shift) & 3;
        let array = |n: usize| self.state.arrays[n];
        let indexed = |k: u32| match k {
            2 => 1,
            3 => 2,
            _ => 0,
        };

        let (pos_count, pos_format) = (2 + (a & 1) as usize, (a >> 1) & 7);
        let pos_scale = 1.0 / (1u64 << ((a >> 4) & 31)) as f32;
        let pos_size = pos_count * COMP[pos_format as usize];
        let nbt = (a >> 9) & 1 != 0;
        let normal_size = match kind(11) {
            1 => (if nbt { 9 } else { 3 }) * COMP[((a >> 10) & 7) as usize],
            k => indexed(k) * (if nbt && a >> 31 != 0 { 3 } else { 1 }),
        };
        let color_format = [(a >> 14) & 7, (a >> 18) & 7];
        let tex = [
            (a >> 21, a >> 22),
            (b, b >> 1),
            (b >> 9, b >> 10),
            (b >> 18, b >> 19),
            (b >> 27, b >> 28),
            (cc >> 5, cc >> 6),
            (cc >> 14, cc >> 15),
            (cc >> 23, cc >> 24),
        ];
        let tex_kind = |i: usize| (vcd_hi >> (i * 2)) & 3;
        let tex_direct = |i: usize| (1 + (tex[i].0 & 1) as usize) * COMP[(tex[i].1 & 7) as usize];
        let tex_shift = [a >> 25, b >> 4, b >> 13, b >> 22, cc, cc >> 9, cc >> 18, cc >> 27].map(|v| v & 31);
        let normal_format = (a >> 10) & 7;

        // The position matrix in use (3x4, row-major, four words per index) with its normal
        // matrix (3x3, three words per index); a vertex can select another.
        let mut loaded = usize::MAX;
        let (mut position_rows, mut normal_rows) = ([0.0f32; 12], [0.0f32; 9]);

        // What varies from draw to draw but not from vertex to vertex is settled here: which
        // texture attributes and coordinates there are, and for the color and the alpha of
        // each channel where the material and ambient colors come from and which lights count.
        let (mut tex_present, mut tex_count) = ([0usize; 8], 0);
        for i in (0..8).filter(|&i| tex_kind(i) != 0) {
            tex_present[tex_count] = i;
            tex_count += 1;
        }
        let tex_scale = tex_shift.map(|shift| 1.0 / (1u64 << shift) as f32);
        let (mut gens_used, mut gens_count) = ([0usize; 8], 0);
        for n in (0..8).filter(|&n| coord_used[n]) {
            gens_used[gens_count] = n;
            gens_count += 1;
        }
        #[derive(Clone, Copy)]
        struct Part {
            ctrl: u32,
            material_from_vertex: bool,
            ambient_from_vertex: bool,
            lit: bool,
            lights: u32,
        }
        let color_kinds = [kind(13), kind(15)];
        let parts: [[Part; 2]; 2] = std::array::from_fn(|n| {
            std::array::from_fn(|part| {
                let ctrl = channels[n].ctrl[part];
                Part {
                    ctrl,
                    material_from_vertex: color_kinds[n] != 0 && ctrl & 1 != 0,
                    ambient_from_vertex: color_kinds[n] != 0 && ctrl & 0x40 != 0,
                    lit: ctrl & 2 != 0 && n < lit_channels,
                    lights: ((ctrl >> 2) & 0xF) | ((ctrl >> 11) & 0xF) << 4,
                }
            })
        });
        let has_normal = lighting && kind(11) != 0;
        let normal_bytes = 3 * COMP[normal_format as usize];
        let matrix_indices = ((vcd_lo >> 1) & 0xFF).count_ones() as usize;

        let mut buf = [0u8; 16];
        // Every vertex of a draw takes the same number of bytes.
        let mut o = first * (data.len() / self.count.max(1));
        let mut extent = (f32::MAX, f32::MIN);
        for slot in out.iter_mut() {
            let mut matrix = self.state.default_matrix;
            if vcd_lo & 1 != 0 {
                matrix = (data[o] & 0x3F) as usize;
                o += 1;
            }
            o += matrix_indices; // texture matrix indices

            fetch(mem, data, &mut o, kind(9), pos_size, array(0), &mut buf);
            let comp = |i: usize| match pos_format {
                0 => buf[i] as f32 * pos_scale,
                1 => buf[i] as i8 as f32 * pos_scale,
                2 => u16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 * pos_scale,
                3 => i16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 * pos_scale,
                _ => f32::from_be_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()),
            };
            let p = [comp(0), comp(1), if pos_count == 3 { comp(2) } else { 0.0 }];
            // Only the normal itself is needed; a tangent and binormal after it are skipped.
            let mut normal = [0.0f32; 3];
            if has_normal {
                let mut at = o;
                fetch(mem, data, &mut at, kind(11), normal_bytes, array(1), &mut buf);
                normal = std::array::from_fn(|i| match normal_format {
                    0 | 1 => buf[i] as i8 as f32 / 64.0,
                    2 | 3 => i16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]) as f32 / 16384.0,
                    _ => f32::from_be_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()),
                });
            }
            o += normal_size;

            let mut vertex_colors = [[0.0f32; 4]; 2];
            for n in 0..2 {
                let k = color_kinds[n];
                if k == 0 {
                    continue;
                }
                let format = color_format[n];
                fetch(mem, data, &mut o, k, COLOR[format as usize], array(2 + n), &mut buf);
                vertex_colors[n] = decode_color(format, &buf);
            }

            let mut tex_in = [[0.0f32; 2]; 8];
            for &i in &tex_present[..tex_count] {
                fetch(mem, data, &mut o, tex_kind(i), tex_direct(i), array(4 + i), &mut buf);
                let scale = tex_scale[i];
                for (n, out) in tex_in[i].iter_mut().enumerate().take(1 + (tex[i].0 & 1) as usize) {
                    *out = match tex[i].1 & 7 {
                        0 => buf[n] as f32 * scale,
                        1 => buf[n] as i8 as f32 * scale,
                        2 => u16::from_be_bytes([buf[n * 2], buf[n * 2 + 1]]) as f32 * scale,
                        3 => i16::from_be_bytes([buf[n * 2], buf[n * 2 + 1]]) as f32 * scale,
                        _ => f32::from_be_bytes(buf[n * 4..n * 4 + 4].try_into().unwrap()),
                    };
                }
            }
            let mut uvs = [[0.0f32; 4]; 4];
            for &n in &gens_used[..gens_count] {
                let (source, _, projective) = gens[n];
                let input = match source {
                    Source::Tex(k) => [tex_in[k][0], tex_in[k][1], 1.0, 1.0],
                    Source::Position => [p[0], p[1], p[2], 1.0],
                    Source::Nothing => [0.0; 4],
                };
                let rows = &gen_rows[n];
                let row = |r: usize| {
                    let m = &rows[r * 4..r * 4 + 4];
                    m[0] * input[0] + m[1] * input[1] + m[2] * input[2] + m[3] * input[3]
                };
                let (mut s, mut t) = (row(0), row(1));
                if projective {
                    let q = row(2);
                    if q != 0.0 {
                        s /= q;
                        t /= q;
                    }
                }
                uvs[n / 2][n % 2 * 2] = s - coord_bias[n][0];
                uvs[n / 2][n % 2 * 2 + 1] = t - coord_bias[n][1];
            }

            if matrix != loaded {
                loaded = matrix;
                (position_rows, normal_rows) = self.state.matrices.rows(matrix);
            }
            let row = |r: usize| {
                let m = &position_rows[r * 4..r * 4 + 4];
                m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3]
            };
            let (x, y, z) = (row(0), row(1), row(2));

            let mut colors = [[0.0f32; 4]; 2];
            if lighting {
                normal = normalize([0, 1, 2].map(|r| dot([0, 1, 2].map(|k| normal_rows[r * 3 + k]), normal)));
            }
            // All four components are worked out for the color and again for the alpha, and
            // three taken from the one and one from the other: fixed-size arithmetic is far
            // quicker than loops over a part of a color.
            for (n, channel) in channels.iter().enumerate() {
                let vertex = vertex_colors[n];
                let shade = |part: &Part| {
                    let material = if part.material_from_vertex { vertex } else { channel.material };
                    if !part.lit {
                        return material;
                    }
                    let mut sum = if part.ambient_from_vertex { vertex } else { channel.ambient };
                    let mut remaining = part.lights;
                    while remaining != 0 {
                        let light = &lights[remaining.trailing_zeros() as usize];
                        remaining &= remaining - 1;
                        let amount = light.amount(part.ctrl, [x, y, z], normal);
                        sum = std::array::from_fn(|i| sum[i] + light.color[i] * amount);
                    }
                    std::array::from_fn(|i| material[i] * sum[i].clamp(0.0, 1.0))
                };
                let color = shade(&parts[n][0]);
                let alpha = if parts[n][1].ctrl == parts[n][0].ctrl { color } else { shade(&parts[n][1]) };
                colors[n] = [color[0], color[1], color[2], alpha[3]];
            }

            // GX clip space has z in [-w, 0]; wgpu wants [0, w].
            let pos = if orthographic {
                let (mut clip_x, mut clip_y) = (proj[0] * x + proj[1], proj[2] * y + proj[3]);
                if snap_w > 0.0 && snap_h > 0.0 {
                    // Screen-space geometry is authored for the console's pixel grid, where a
                    // pixel is sampled 7/12 of the way across it. A quad edge at a half-pixel
                    // offset still covers its first pixel there, but not the first of the
                    // several samples per pixel at a higher internal resolution, which leaves
                    // uncleared borders. Snap edges to the pixel boundary the console would
                    // effectively use.
                    let snap = |pixel: f32| (pixel + 5.0 / 12.0).floor();
                    clip_x = (snap(snap_cx + clip_x * snap_w) - snap_cx) / snap_w;
                    clip_y = -(snap(snap_cy - clip_y * snap_h) - snap_cy) / snap_h;
                }
                [clip_x, clip_y, proj[4] * z + proj[5] + 1.0, 1.0]
            } else {
                [proj[0] * x + proj[1] * z, proj[2] * y + proj[3] * z, proj[4] * z + proj[5] - z, -z]
            };
            if dump_vertices {
                eprintln!(
                    "[gx]   vertex: position {p:?} (matrix {matrix}) -> clip {pos:?}, color {:?}, tex inputs {:?}, uv0 {:?}",
                    colors[0],
                    &tex_in[..2],
                    &uvs[0][..2]
                );
            }
            if orthographic {
                extent = (extent.0.min(pos[0]), extent.1.max(pos[0]));
            }
            slot.write(Vertex { pos, colors, uvs });
        }
        extent
    }
}

#[derive(Default, Clone, Copy)]
pub struct FrameStats {
    pub draws: u32,
    pub vertices: u32,
    pub display_lists: u32,
    pub bp_loads: u32,
    pub cp_loads: u32,
    pub xf_loads: u32,
}

/// Something the GPU would signal with an interrupt; the host turns these into guest callbacks.
pub enum Raised {
    /// A draw-sync token with interrupt was reached.
    Token(u16),
    /// The read position reached the FIFO breakpoint.
    Break,
    /// A "draw done" command was reached: everything sent before it has been drawn.
    Finish,
}

pub struct Gx {
    /// Stream bytes received but not yet decoded: either an incomplete command or commands
    /// held back by the breakpoint.
    pending: Vec<u8>,
    /// Stream position (bytes since boot) up to which commands have been decoded.
    consumed: u64,
    /// FIFO breakpoint as a stream position: the GPU does not read past it. The game queues
    /// several frames ahead and releases them one per retrace by moving this.
    limit: Option<u64>,
    break_signalled: bool,
    /// The last few things that happened to the breakpoint and the GPU's interrupts, kept
    /// for the stall report (`describe`): a stall here depends on timing and does not happen
    /// while every event is being printed.
    history: std::collections::VecDeque<String>,
    /// Recent (guest address, stream position) pairs returned as FIFO write pointers.
    marks: Vec<(u32, u64)>,
    pub raised: Vec<Raised>,
    pub bp: [u32; 256],
    pub cp: [u32; 256],
    pub xf: Vec<u32>,
    pub stats: FrameStats,
    pub frames: u64,
    /// Last draw-sync token the game sent (read back through GXReadDrawSync).
    pub token: u16,
    errors: u32,
    /// Texture memory: holds the palettes the game loads for paletted textures.
    tmem: Vec<u8>,
    /// The combiner's four color registers and four constants, as RGBA. Both are written
    /// through the same BP registers, told apart by a type bit, so they are kept separately.
    tev_reg: [[f32; 4]; 4],
    tev_konst: [[f32; 4]; 4],
    /// Geometry decoded since the last presented frame.
    frame: Frame,
    pub renderer: Option<Renderer>,
    /// Color to clear the framebuffer to before the next frame, if the last copy cleared it.
    clear: Option<[f64; 4]>,
    /// Set whenever the GPU copies a frame to the display; the video interface clears it.
    pub copied_to_display: bool,
    /// Where in guest memory the display copies since the host last looked were sent.
    pub display_copies: Vec<u32>,
    /// Draw-done commands decoded so far.
    finish_requests: u32,
    /// Copy descriptions already logged by the copy trace.
    traced: std::collections::HashSet<String>,
    /// Draws still to describe in the trace, and whether that has been triggered already.
    trace_draws: u32,
    traced_after_copy: bool,
    /// Texture lookups already done this frame, keyed by address, image and palette
    /// registers. Emptied whenever texture contents may have changed.
    texture_cache: std::collections::HashMap<(u32, u32, u32), Option<usize>>,
    /// Index of the next draw within the current frame.
    draw_index: u32,
    /// Debugging aids: a frame whose draws are all described (TSSM_DUMP_FRAME=n), and draw
    /// indices to leave out of every frame (TSSM_SKIP_DRAWS=3,17,40-52).
    dump_frame: Option<u64>,
    skip_draws: Vec<(u32, u32)>,
    /// Time spent decoding the stream (including drawing) and, within that, in the renderer,
    /// since the last report (TSSM_TRACE_PERF=1).
    pub decode_time: std::time::Duration,
    pub render_time: std::time::Duration,
    /// Whether flat drawing that gets squeezed on a wide picture has begun in this frame
    /// (since the last capture into a texture).
    flat_started: bool,
    /// Draws whose vertices are still to be worked out (`finish_vertices`), their vertex data
    /// end to end, and how many vertices they will add to the frame.
    jobs: Vec<VertexJob>,
    job_data: Vec<u8>,
    job_vertices: usize,
    /// Guest memory, set whenever the stream is decoded.
    mem: Mem,
    /// The material of the last draw, while nothing it depends on has changed.
    material: Option<Material>,
    /// What the last draw was made from, while no register has changed since.
    template: Option<Template>,
    template_serial: u64,
    /// The template of the frame's last draw, if `draw` queued it; 0 otherwise.
    last_serial: u64,
    /// Whether a flat draw is among `jobs`: `flat_started` is not up to date.
    pending_ortho: bool,
    /// Whether the vertex shader transforms what it can (not with TSSM_CPU_VERTICES=1).
    raw_enabled: bool,
    /// The display list cache (`cached_list`), not used while a debugging aid looks at
    /// draws one by one. Lists are known by the hash of their bytes and the vertex layout
    /// (`cp_signature`), not by where they are: the game writes many of its lists and
    /// arrays afresh every frame, each time somewhere else, and most the same as before.
    /// `shapes`: what follows from a list's bytes. `checks`: by list and where the arrays
    /// are, the last look at the vertices there. `places`: by list and the hash of exactly
    /// the vertices it reads, where those are in the cache's buffers (first vertex, first
    /// index); one list has a place for every set of vertices it is drawn with.
    shapes: std::collections::HashMap<(u64, u64), ListShape>,
    checks: std::collections::HashMap<(u64, u64, u64), ListCheck>,
    places: std::collections::HashMap<(u64, u64, u64), (i32, u32)>,
    /// By what the list holds and the vertex layout (`cp_signature`): lists whose vertices
    /// keep changing (animated models).
    churn: std::collections::HashMap<(u64, u64), Churn>,
    /// Where in the cache's buffers each distinct set of vertices and indices is, by their
    /// hashes and the number of vertices: lists that hold the same (the game keeps a copy
    /// of a model's list for every one of the model in a level) share one place.
    contents: std::collections::HashMap<(u64, u64, u32), (i32, u32)>,
    /// By the same: the frame a list not in the cache was last met in. A list is only
    /// cached once it has come again in a later frame: the game also writes lists that are
    /// drawn once and never again.
    seen: std::collections::HashMap<(u64, u64), u64>,
    cache_enabled: bool,
    /// Set when the cache's buffers are full; everything is dropped when the frame ends.
    cache_full: bool,
    cp_sig: Option<u64>,
    /// Hashes of blocks of guest memory, each good while its stamp is `epoch`, which moves
    /// on whenever the game has run.
    block_hash: Vec<u64>,
    block_stamp: Vec<u32>,
    epoch: u32,
    /// For the report of slow frames: when the last frame was shown, the usual time from
    /// one frame to the next, and what this frame has cost so far (decoding, of that the
    /// renderer, vertices put into the display list cache, whether the cache was emptied).
    shown_at: Option<std::time::Instant>,
    decode_started: Option<std::time::Instant>,
    usual: f64,
    frame_decode: std::time::Duration,
    frame_render: std::time::Duration,
    frame_cached: u32,
    frame_emptied: bool,
    /// Reading ahead of the decoder (`look_ahead`): the stream position read up to, the
    /// layout registers (CP) as they are there with their signature, the display list calls
    /// met and not yet decoded (stream position and vertices), whether the reader has given
    /// up on the stream, and buffers kept for reuse.
    ahead_pos: u64,
    ahead_cp: [u32; 256],
    ahead_sig: Option<u64>,
    ahead: std::collections::VecDeque<(u64, Ahead)>,
    ahead_bad: bool,
    ahead_data: Vec<u8>,
    spare: Vec<(Vec<RawVertex>, Vec<u32>)>,
    /// Buffers for building a list's vertices and indices.
    list_vertices: Vec<RawVertex>,
    list_indices: Vec<u32>,
    /// The heads-up display widget being drawn at this point of the stream (0 for none), and
    /// the points still ahead where that changes: stream position and widget. The game's
    /// thread leaves these as it draws (`mark_hud`); `hud_marked` is the last one it left.
    hud: u32,
    hud_marks: std::collections::VecDeque<(u64, u32)>,
    hud_marked: u32,
    hud_widgets: u32,
}

const BP_PE_DONE: usize = 0x45;
const BP_PE_TOKEN: usize = 0x47;
const BP_PE_TOKEN_INT: usize = 0x48;
const BP_EFB_COPY: usize = 0x52;

/// TSSM_NO_CULL=1 draws both faces of every triangle, to compare.
/// Where decoding time went since the last report (TSSM_TRACE_PERF=1): nanoseconds in a
/// draw's material, in the rest of its setup, and in transforming vertices; then the draws
/// and vertices counted.
/// After those: vertices drawn from the display list cache, vertices sent to the vertex
/// shader afresh, vertices put into the cache, and draws handed to the GPU.
pub static PROF: [std::sync::atomic::AtomicU64; 9] = [const { std::sync::atomic::AtomicU64::new(0) }; 9];

/// Whether the heads-up display goes to the edges of a wide picture; TSSM_HUD_CENTRE=1 keeps
/// it within the middle 4:3 part.
fn anchor_hud() -> bool {
    static CENTRE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !*CENTRE.get_or_init(|| std::env::var_os("TSSM_HUD_CENTRE").is_some())
}

fn no_cull() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("TSSM_NO_CULL").is_some())
}

fn be16(d: &[u8], o: usize) -> usize {
    u16::from_be_bytes([d[o], d[o + 1]]) as usize
}

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(d[o..o + 4].try_into().unwrap())
}

impl Gx {
    pub fn new() -> Gx {
        Gx {
            pending: Vec::new(),
            consumed: 0,
            limit: None,
            break_signalled: false,
            history: Default::default(),
            marks: Vec::new(),
            raised: Vec::new(),
            bp: std::array::from_fn(|reg| if reg == 0xFE { 0xFF_FFFF } else { 0 }),
            cp: [0; 256],
            xf: vec![0; 0x1100],
            stats: FrameStats::default(),
            frames: 0,
            token: 0,
            errors: 0,
            tmem: vec![0; 0x10_0000],
            tev_reg: [[0.0; 4]; 4],
            tev_konst: [[0.0; 4]; 4],
            frame: Frame::default(),
            renderer: None,
            clear: Some([0.0, 0.0, 0.0, 1.0]),
            copied_to_display: false,
            display_copies: Vec::new(),
            finish_requests: 0,
            traced: std::collections::HashSet::new(),
            trace_draws: 0,
            traced_after_copy: false,
            texture_cache: std::collections::HashMap::new(),
            draw_index: 0,
            dump_frame: std::env::var("TSSM_DUMP_FRAME").ok().and_then(|v| v.trim().parse().ok()),
            skip_draws: std::env::var("TSSM_SKIP_DRAWS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|range| {
                    let (first, last) = range.trim().split_once('-').unwrap_or((range.trim(), range.trim()));
                    Some((first.parse().ok()?, last.parse().ok()?))
                })
                .collect(),
            decode_time: Default::default(),
            render_time: Default::default(),
            flat_started: false,
            jobs: Vec::new(),
            job_data: Vec::new(),
            job_vertices: 0,
            mem: Mem(std::ptr::null()),
            material: None,
            template: None,
            template_serial: 0,
            last_serial: 0,
            pending_ortho: false,
            raw_enabled: !cpu_vertices(1),
            shapes: std::collections::HashMap::new(),
            checks: std::collections::HashMap::new(),
            places: std::collections::HashMap::new(),
            churn: std::collections::HashMap::new(),
            contents: std::collections::HashMap::new(),
            seen: std::collections::HashMap::new(),
            cache_enabled: ["TSSM_DUMP_FRAME", "TSSM_SKIP_DRAWS", "TSSM_SURVEY_GX", "TSSM_TRACE_GX"]
                .iter()
                .all(|name| std::env::var_os(name).is_none()),
            cache_full: false,
            cp_sig: None,
            block_hash: vec![0; (runtime::mem::RAM_SIZE >> BLOCK_SHIFT) as usize],
            block_stamp: vec![0; (runtime::mem::RAM_SIZE >> BLOCK_SHIFT) as usize],
            epoch: 1,
            shown_at: None,
            decode_started: None,
            usual: 0.0,
            frame_decode: Default::default(),
            frame_render: Default::default(),
            frame_cached: 0,
            frame_emptied: false,
            ahead_pos: 0,
            ahead_cp: [0; 256],
            ahead_sig: None,
            ahead: Default::default(),
            ahead_bad: false,
            ahead_data: Vec::new(),
            spare: Vec::new(),
            list_vertices: Vec::new(),
            list_indices: Vec::new(),
            hud: 0,
            hud_marks: std::collections::VecDeque::new(),
            hud_marked: 0,
            hud_widgets: 0,
        }
    }

    fn xf_float(&self, index: usize) -> f32 {
        f32::from_bits(self.xf[index])
    }

    /// The texture bound to texture map `map`: its renderer id, its sampling mode, and an
    /// amount to subtract from coordinates that sample it. Decodes and uploads the texture on
    /// first use.
    fn map_texture(&mut self, c: &mut Ctx, map: usize) -> Option<(usize, SamplerKey, [f32; 2])> {
        // Texture maps 0-3 and 4-7 sit in two separate register banks.
        let reg = |low: usize, high: usize| self.bp[if map < 4 { low + map } else { high + map - 4 }];
        let (mode, image, address, tlut) = (reg(0x80, 0xA0), reg(0x88, 0xA8), reg(0x94, 0xB4), reg(0x98, 0xB8));
        let (w, h, format) = ((image & 0x3FF) + 1, ((image >> 10) & 0x3FF) + 1, (image >> 20) & 0xF);
        let address = ((address & 0xFF_FFFF) << 5) | 0x8000_0000;
        let sampler = ((mode & 3) as u8, ((mode >> 2) & 3) as u8, mode & 0x10 != 0);
        // A texture the game filled by copying the framebuffer lives on the GPU, not in RAM.
        if let Some(id) = self.renderer.as_ref().and_then(|r| r.efb_copy(address)) {
            // The game draws such a copy back pixel for pixel with unfiltered sampling and
            // coordinates half a texel late, which the console rounds down to the same pixel.
            // The copy here has several texels per console texel, so that half texel would be
            // a visible shift, with the far edge wrapping around. Take it out.
            let bias = if sampler.2 { [0.0; 2] } else { [0.5 / w as f32, 0.5 / h as f32] };
            return Some((id, sampler, bias));
        }
        // Finding a texture means hashing all of its data to notice changes. Do that once per
        // frame per texture, not once per draw; a failed lookup is remembered too.
        let cache_key = (address, image, tlut);
        if let Some(&cached) = self.texture_cache.get(&cache_key) {
            return cached.map(|id| (id, sampler, [0.0; 2]));
        }
        self.texture_cache.insert(cache_key, None);
        // A controller button's picture, while the keyboard and mouse are in use: show the key
        // bound to it instead (glyph.rs).
        if let Some((label, picture)) = crate::glyph::for_texture(address) {
            let key = (address, crate::glyph::SIZE, crate::glyph::SIZE, u32::MAX, picture);
            let renderer = self.renderer.as_mut()?;
            let id = renderer.find_texture(&key).unwrap_or_else(|| {
                renderer.add_texture(key, crate::glyph::SIZE, crate::glyph::SIZE, &crate::glyph::draw(&label))
            });
            self.texture_cache.insert(cache_key, Some(id));
            return Some((id, (sampler.0, sampler.1, true), [0.0; 2]));
        }
        let size = texture::data_size(format, w as usize, h as usize)?;
        if !in_ram(address, size) {
            return None;
        }
        let data = c.read_bytes(address, size);

        let (palette_base, palette_format) = (((tlut & 0x3FF) << 9) as usize, (tlut >> 10) & 3);
        let palette_end = (palette_base + texture::palette_entries(format) * 2).min(self.tmem.len());
        let palette = &self.tmem[palette_base..palette_end];
        let contents = texture::hash(&data) ^ texture::hash(palette).rotate_left(1);
        let key = (address, w, h, format | palette_format << 8, contents);

        let renderer = self.renderer.as_mut()?;
        let id = renderer.find_texture(&key).unwrap_or_else(|| {
            let lookup = |i: usize| {
                let entry = palette.get(i * 2..i * 2 + 2);
                entry.map_or([0; 4], |e| texture::palette_color(palette_format, u16::from_be_bytes([e[0], e[1]])))
            };
            let rgba = texture::decode(format, w as usize, h as usize, &data, &lookup);
            renderer.add_texture(key, w, h, &rgba)
        });
        self.texture_cache.insert(cache_key, Some(id));
        Some((id, sampler, [0.0; 2]))
    }

    /// With the draw trace armed, describes the material state of the draw about to happen.
    fn trace_draw(&mut self, index: u32, op: u8, count: usize) {
        if self.trace_draws == 0 {
            return;
        }
        self.trace_draws -= 1;
        let stages = ((self.bp[0] >> 10) & 15) as usize + 1;
        let material = self.xf[0x100C];
        eprintln!(
            "[gx] draw {index}: primitive {:#04x} x{count}, {stages} TEV stages, {} texgens, blend {:#08x}, zmode {:#04x}, alpha test {:#08x}, material {material:#010x} chan ctrl {:#x}/{:#x}, cull {}, viewport {:?}",
            op & 0xF8,
            self.bp[0] & 15,
            self.bp[0x41],
            self.bp[0x40],
            self.bp[0xF3],
            self.xf[0x100E],
            self.xf[0x1010],
            (self.bp[0] >> 14) & 3,
            [0x101A, 0x101B, 0x101D, 0x101E].map(|i| self.xf_float(i))
        );
        eprintln!(
            "[gx]   projection {} {:?}",
            if self.xf[0x1026] & 1 != 0 { "orthographic" } else { "perspective" },
            [0x1020, 0x1021, 0x1022, 0x1023, 0x1024, 0x1025].map(|i| self.xf_float(i))
        );
        {
            // Where the first color's array is, and what its first bytes hold.
            let base = (self.cp[0xA2] & 0x03FF_FFFF) | 0x8000_0000;
            let bytes: Vec<u8> = (0..16).map(|k| if in_ram(base, 16) && !self.mem.0.is_null() { self.mem.r8(base + k) } else { 0 }).collect();
            if in_ram(base, 8192) && !self.mem.0.is_null() {
                // SAFETY: within guest RAM.
                let more = unsafe { std::slice::from_raw_parts(self.mem.0.add(base as usize), 8192) };
                eprintln!("[gx]   color array {base:#010x} hash of 8 KB {:016x}, tev registers {:?}", hash_bytes(more), self.tev_reg);
            }
            eprintln!(
                "[gx]   color array {base:#010x} stride {} (vertex descriptor {:#x}), position array {:#010x}: first bytes {bytes:02x?}",
                self.cp[0xB2] & 0xFF,
                self.cp[0x50],
                (self.cp[0xA0] & 0x03FF_FFFF) | 0x8000_0000
            );
        }
        let texgens = (self.bp[0] & 15) as usize;
        eprintln!(
            "[gx]   texgens {:x?}, post-transform {:x?}, dual-tex {:#x}, matrix index A {:#x} B {:#x}",
            &self.xf[0x1040..0x1040 + texgens],
            &self.xf[0x1050..0x1050 + texgens],
            self.xf[0x1012],
            self.cp[0x30],
            self.cp[0x40]
        );
        // Lighting: channel count, ambient colors, and the lights the color controls select.
        let mask = |ctrl: u32| if ctrl & 2 != 0 { ((ctrl >> 2) & 0xF) | ((ctrl >> 11) & 0xF) << 4 } else { 0 };
        let used = mask(self.xf[0x100E]) | mask(self.xf[0x1010]) | mask(self.xf[0x100F]) | mask(self.xf[0x1011]);
        eprintln!(
            "[gx]   {} color channels, ambient {:#010x}/{:#010x}, chan 1 ctrl {:#x}/{:#x}, lights used {used:#04x}",
            self.xf[0x1009] & 3,
            self.xf[0x100A],
            self.xf[0x100B],
            self.xf[0x100F],
            self.xf[0x1011]
        );
        for n in (0..8).filter(|n| used >> n & 1 != 0) {
            let base = 0x600 + n * 16;
            let floats: Vec<f32> = (4..16).map(|k| self.xf_float(base + k)).collect();
            eprintln!("[gx]   light {n}: color {:#010x}, cos att / dist att / position / direction {floats:?}", self.xf[base + 3]);
        }
        for s in 0..stages {
            let order = (self.bp[0x28 + s / 2] >> (12 * (s % 2))) & 0x3FF;
            let map = (order & 7) as usize;
            let (image, address) = if map < 4 { (0x88 + map, 0x94 + map) } else { (0xA8 + map - 4, 0xB4 + map - 4) };
            eprintln!(
                "[gx]   stage {s}: texmap {map} coord {} enabled {} chan {}, color env {:#08x}, alpha env {:#08x}, texture {:#010x} {}x{} fmt {}",
                (order >> 3) & 7,
                order & 0x40 != 0,
                (order >> 7) & 7,
                self.bp[0xC0 + s * 2],
                self.bp[0xC1 + s * 2],
                ((self.bp[address] & 0xFF_FFFF) << 5) | 0x8000_0000,
                (self.bp[image] & 0x3FF) + 1,
                ((self.bp[image] >> 10) & 0x3FF) + 1,
                (self.bp[image] >> 20) & 15
            );
        }
    }

    /// The parameter block for the current combiner state: the four color registers, the four
    /// constants, then the two alpha-test references (0-255).
    fn tev_params(&self) -> [f32; PARAMS_FLOATS] {
        let mut block = [0.0f32; PARAMS_FLOATS];
        for r in 0..4 {
            block[r * 4..r * 4 + 4].copy_from_slice(&self.tev_reg[r]);
            block[16 + r * 4..20 + r * 4].copy_from_slice(&self.tev_konst[r]);
        }
        block[32] = (self.bp[0xF3] & 0xFF) as f32;
        block[33] = ((self.bp[0xF3] >> 8) & 0xFF) as f32;
        // Fog (BP 0xEE-0xF2): the color, then the terms of the depth-to-distance formula the
        // shader evaluates (tev.rs). A and C are floats packed as sign, 8-bit exponent and
        // 11-bit mantissa; B is a 24-bit magnitude and a shift.
        let packed = |v: u32| f32::from_bits(((v >> 19) & 1) << 31 | ((v >> 11) & 0xFF) << 23 | (v & 0x7FF) << 12);
        let color = self.bp[0xF2];
        block[36..39].copy_from_slice(&[16, 8, 0].map(|shift| ((color >> shift) & 0xFF) as f32 / 255.0));
        block[40] = packed(self.bp[0xEE]);
        block[41] = (self.bp[0xEF] & 0xFF_FFFF) as f32;
        block[42] = (1u32 << (self.bp[0xF0] & 31)) as f32;
        block[43] = packed(self.bp[0xF1]);
        block
    }

    /// TSSM_SURVEY_GX=1 reports, once each, the GPU features a draw uses that the renderer
    /// does not model yet (or only approximately), to show what a level needs.
    fn survey(&mut self, op: u8) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("TSSM_SURVEY_GX").is_some()) {
            return;
        }
        static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut found = Vec::new();
        let bp = &self.bp;
        let fog = (bp[0xEE + 3] >> 21) & 7;
        if fog != 0 {
            let p = self.tev_params();
            found.push(format!(
                "fog type {fog}{}: A {} B {} shift {} C {} color {:?}",
                if bp[0xEE + 3] & (1 << 20) != 0 { " (orthographic)" } else { "" },
                p[40], p[41], p[42], p[43], &p[36..39]
            ));
        }
        let stages = ((bp[0] >> 10) & 15) as usize + 1;
        let indirect_stages = (bp[0] >> 16) & 7;
        for i in 0..stages {
            // Indirect: matrix selection in bits 9-12 of the stage's indirect register.
            if indirect_stages > 0 && (bp[0x10 + i] >> 9) & 15 != 0 {
                found.push(format!("indirect texture (matrix {}, format {})", (bp[0x10 + i] >> 9) & 15, (bp[0x10 + i] >> 2) & 3));
            }
            let (ras_swap, tex_swap) = (bp[0xC1 + i * 2] & 3, (bp[0xC1 + i * 2] >> 2) & 3);
            for (what, table) in [("rasterized color", ras_swap), ("texture", tex_swap)] {
                // Table n: red and green selection in ksel register 2n, blue and alpha in 2n + 1.
                let (rg, ba) = (bp[0xF6 + table as usize * 2] & 15, bp[0xF7 + table as usize * 2] & 15);
                if (rg, ba) != (0b0100, 0b1110) {
                    found.push(format!("{what} channel swap {:?}", [rg & 3, rg >> 2, ba & 3, ba >> 2]));
                }
            }
        }
        match op & 0xF8 {
            0xA8 | 0xB0 => found.push("lines".into()),
            0xB8 => found.push("points".into()),
            _ => {}
        }
        if (bp[0xF5] >> 2) & 3 != 0 {
            found.push("depth texture".into());
        }
        if bp[0x41] & 2 != 0 {
            found.push(format!("logic operation {}", (bp[0x41] >> 12) & 15));
        }
        if bp[0x42] & 0x100 != 0 {
            found.push("constant destination alpha".into());
        }
        match (bp[0] >> 14) & 3 {
            0 => {}
            cull => found.push(format!("culling mode {cull}")),
        }
        let mut seen = SEEN.lock().unwrap();
        for feature in found {
            if !seen.contains(&feature) {
                eprintln!("[gx-survey] {feature} (first in frame {})", self.frames + 1);
                seen.push(feature);
            }
        }
    }

    /// Works out the vertices of the draws queued since the last call, on several threads
    /// when there are enough of them. The game is stopped meanwhile, so the arrays the
    /// vertices index are as they would have been had each draw been done on the spot.
    fn finish_vertices(&mut self) {
        self.pending_ortho = false;
        if self.jobs.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let jobs = std::mem::take(&mut self.jobs);
        let total = std::mem::take(&mut self.job_vertices);
        let (mem, data) = (self.mem, &self.job_data);
        let mut vertices = std::mem::take(&mut self.frame.vertices);
        vertices.reserve(total);
        let mut spare = &mut vertices.spare_capacity_mut()[..total];
        // A task is a draw, or a piece of a large one: (draw, first vertex, output, extent).
        let mut tasks = Vec::with_capacity(jobs.len());
        for (index, job) in jobs.iter().enumerate() {
            let (out, rest) = std::mem::take(&mut spare).split_at_mut(job.count);
            spare = rest;
            for (piece, out) in out.chunks_mut(TASK_VERTICES).enumerate() {
                tasks.push((index, piece * TASK_VERTICES, out, (f32::MAX, f32::MIN)));
            }
        }
        let run = |(index, first, out, extent): &mut (usize, usize, &mut [std::mem::MaybeUninit<Vertex>], (f32, f32))| {
            *extent = jobs[*index].run(mem, data, *first, out)
        };
        match vertex_threads().filter(|_| total >= PARALLEL_FROM) {
            // This thread and the helpers each take the next batch of tasks until none are
            // left. This thread never waits for a helper to start: if the helpers are slow to
            // wake, or run slowly, it has simply done more of the work itself by then.
            Some(threads) => {
                let shared = Claimed(tasks.as_mut_ptr(), tasks.len());
                let next = std::sync::atomic::AtomicUsize::new(0);
                let work = || loop {
                    let start = next.fetch_add(TASK_BATCH, Relaxed);
                    if start >= shared.1 {
                        break;
                    }
                    shared.batch(start, (start + TASK_BATCH).min(shared.1)).iter_mut().for_each(run);
                };
                threads.in_place_scope(|scope| {
                    for _ in 0..threads.current_num_threads() {
                        scope.spawn(|_| work());
                    }
                    work();
                });
            }
            None => tasks.iter_mut().for_each(run),
        }
        let mut extents = vec![(f32::MAX, f32::MIN); jobs.len()];
        for (index, _, _, extent) in tasks {
            extents[index] = (extents[index].0.min(extent.0), extents[index].1.max(extent.1));
        }
        // Every job has written all of its vertices.
        unsafe { vertices.set_len(vertices.len() + total) };
        self.frame.vertices = vertices;

        for (job, (left, right)) in jobs.iter().zip(extents) {
            let range = (job.base, job.base + job.count as u32);
            // Flat drawing is made for a 4:3 screen and is squeezed to keep its shape on a
            // wider one (see aspect.rs), with two exceptions: whatever spans the full width
            // (backdrops, fades, bars) keeps doing so, and a framebuffer copy drawn back stays
            // where it was. The squeeze itself waits until the frame is shown, since drawing
            // that is captured into a texture instead must not be touched.
            let state = &job.state;
            if state.orthographic && !state.samples_copy && (left > -0.98 || right < 0.98) {
                self.frame.flat.push(Flat { from: range.0, to: range.1, origin: None, hud: job.hud, extent: (left, right) });
                self.flat_started = true;
            }
            // Models drawn over the flat drawing (tokens and chests in the menus, the health
            // burgers) use the world's camera, so they already have the right shape, but the
            // game places them at a fixed spot on the screen whatever the field of view, and
            // sizes them by the width of the view, so that on a wider view they come out
            // larger in both directions. They move with the flat drawing they sit on, as a
            // whole: anything in perspective that follows flat drawing within a frame is
            // brought back to its 4:3 size about its origin and shifted to where that origin
            // lands after the squeeze.
            // The models of the heads-up display (the health burgers) are such models too,
            // whether or not flat drawing came first.
            let over_flat = self.flat_started || job.hud != 0;
            if let (false, false, true, Some(origin)) = (state.orthographic, state.samples_copy, over_flat, state.origin) {
                let at = origin[0] / origin[3];
                self.frame.flat.push(Flat { from: range.0, to: range.1, origin: Some(origin), hud: job.hud, extent: (at, at) });
            }
        }
        self.jobs = jobs;
        self.jobs.clear();
        self.job_data.clear();
        PROF[2].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
    }

    /// Forgets the material of the last draw: something it depends on has changed.
    fn forget_material(&mut self) {
        self.material = None;
        self.template = None;
    }

    /// Works out what the draws from here on share until a register changes: the material
    /// (a shader generated from the combiner state, and the textures it samples), the state
    /// their vertices are transformed in, and the fixed-function state.
    fn build_template(&mut self, c: &mut Ctx) {
        // The material only depends on the BP registers and on what the textures hold, so it
        // is worked out again only after one of those has changed.
        let material = match self.material {
            Some(material) => material,
            None => {
                let config = tev::Config::from_registers(&self.bp);
                let shader = config.key();
                let mut textures = [WHITE_TEXTURE; TEXTURE_SLOTS];
                let mut samplers: [SamplerKey; TEXTURE_SLOTS] = [(0, 0, false); TEXTURE_SLOTS];
                let mut coord_used = [false; 8];
                let mut coord_bias = [[0.0f32; 2]; 8];
                for (map, coord) in config.texture_uses() {
                    coord_used[coord] = true;
                    if let Some((id, sampler, bias)) = self.map_texture(c, map) {
                        textures[map] = id;
                        samplers[map] = sampler;
                        coord_bias[coord] = bias;
                    }
                }
                let samples_copy = self.renderer.as_ref().is_some_and(|r| textures.iter().any(|&id| r.is_efb_copy(id)));
                if let Some(renderer) = &mut self.renderer {
                    renderer.ensure_shader(shader, || config.fragment());
                }
                let material = Material { shader, textures, samplers, coord_used, coord_bias, samples_copy };
                self.material = Some(material);
                material
            }
        };
        let Material { coord_used, coord_bias, samples_copy, .. } = material;

        let (vcd_lo, vcd_hi) = (self.cp[0x50], self.cp[0x60]);
        // (base address, stride) of attribute array n: 0 position, 1 normal, 2 color0, ...
        let arrays: [(u32, u32); 12] =
            std::array::from_fn(|n| ((self.cp[0xA0 + n] & 0x03FF_FFFF) | 0x8000_0000, self.cp[0xB0 + n] & 0xFF));

        // Texture coordinate generation, per coordinate: where its input comes from (source
        // row 0 is the position, rows 5-12 the eight texture attributes), which matrix
        // transforms it, and whether the result is divided by a third row (projective).
        let gens: [(Source, usize, bool); 8] = std::array::from_fn(|n| {
            let reg = self.xf[0x1040 + n];
            let source = match (reg >> 7) & 0x1F {
                0 => Source::Position,
                row @ 5..=12 => Source::Tex(row as usize - 5),
                _ => Source::Nothing,
            };
            let matrix = if n < 4 { self.cp[0x30] >> (6 + 6 * n) } else { self.cp[0x40] >> (6 * (n - 4)) };
            (source, (matrix & 0x3F) as usize, reg & 2 != 0)
        });

        // The two color channels. Color and alpha are controlled separately: the vertex color
        // can replace the material (bit 0) and the ambient color (bit 6), and with lighting on
        // (bit 1) the result is material x (ambient + the lights in the mask, bits 2-5 and
        // 11-14). Lighting is per vertex, as on the console.
        let channels: [Channel; 2] = std::array::from_fn(|n| {
            let color = |reg: u32| [24, 16, 8, 0].map(|s| ((reg >> s) & 0xFF) as f32 / 255.0);
            Channel {
                material: color(self.xf[0x100C + n]),
                ambient: color(self.xf[0x100A + n]),
                ctrl: [self.xf[0x100E + n], self.xf[0x1010 + n]],
            }
        });
        let lit_channels = (self.xf[0x1009] & 3).min(2) as usize;
        // Normals and lights only matter to a channel that has lighting on and lights in its
        // mask. The level's geometry has lighting on with no lights at all: its vertex colors
        // go in as the ambient color.
        let lit = |ctrl: u32| ctrl & 2 != 0 && ctrl & 0x783C != 0;
        let lighting = channels[..lit_channels].iter().any(|ch| lit(ch.ctrl[0]) || lit(ch.ctrl[1]));
        let lights: [Light; 8] = std::array::from_fn(|n| {
            if !lighting {
                return Light::default();
            }
            let base = 0x600 + n * 16;
            let vector = |at: usize| [0, 1, 2].map(|k| self.xf_float(base + at + k));
            Light {
                color: [24, 16, 8, 0].map(|s| ((self.xf[base + 3] >> s) & 0xFF) as f32 / 255.0),
                cos_att: vector(4),
                dist_att: vector(7),
                pos: vector(10),
                dir: vector(13),
            }
        });
        let proj: [f32; 6] = std::array::from_fn(|i| self.xf_float(0x1020 + i));
        let orthographic = self.xf[0x1026] & 1 != 0;
        if lighting && std::env::var_os("TSSM_TRACE_LIGHTS").is_some() {
            // Each light in use, taken back out of view space by the draw's position matrix
            // (for the level's own geometry that is the world's space).
            let matrix = (self.cp[0x30] & 0x3F) as usize;
            let m: [f32; 12] = std::array::from_fn(|k| self.xf_float((matrix * 4 + k) & 0xFF));
            let back = |v: [f32; 3], point: bool| {
                let d = if point { [v[0] - m[3], v[1] - m[7], v[2] - m[11]] } else { v };
                [0, 1, 2].map(|c| m[c] * d[0] + m[4 + c] * d[1] + m[8 + c] * d[2])
            };
            let mask = channels.iter().flat_map(|ch| ch.ctrl).fold(0, |all, ctrl| all | ((ctrl >> 2) & 0xF) | ((ctrl >> 11) & 0xF) << 4);
            for n in (0..8).filter(|n| mask >> n & 1 != 0) {
                eprintln!(
                    "[lights] frame {} matrix row0 {:?}: light {n} color {:?} at {:?} towards {:?} cos {:?} dist {:?} ctrl {:x?}",
                    self.frames + 1, &m[..3], lights[n].color, back(lights[n].pos, true), back(lights[n].dir, false),
                    lights[n].cos_att, lights[n].dist_att, channels[0].ctrl
                );
            }
        }

        // Viewport half-extents and centre in console pixels, for snapping 2D geometry.
        let (snap_w, snap_h) = (self.xf_float(0x101A), -self.xf_float(0x101B));
        let (snap_cx, snap_cy) = (self.xf_float(0x101D) - 342.0, self.xf_float(0x101E) - 342.0);

        // Matrices are read out of the register file once per state, not once per vertex.
        let gen_rows: [[f32; 12]; 8] = std::array::from_fn(|n| {
            std::array::from_fn(|k| if coord_used[n] { self.xf_float((gens[n].1 * 4 + k) & 0xFF) } else { 0.0 })
        });
        let default_matrix = (self.cp[0x30] & 0x3F) as usize;
        let matrices = if vcd_lo & 1 != 0 {
            // Vertices choose their own matrices: keep all of them as they are now.
            let mut all = Box::new([0u32; 0x160]);
            all[..0x100].copy_from_slice(&self.xf[..0x100]);
            all[0x100..].copy_from_slice(&self.xf[0x400..0x460]);
            Matrices::All(all)
        } else {
            Matrices::One(
                std::array::from_fn(|k| self.xf_float((default_matrix * 4 + k) & 0xFF)),
                std::array::from_fn(|k| self.xf_float(0x400 + (default_matrix & 31) * 3 + k)),
            )
        };
        // Models drawn over the flat drawing (see `finish_vertices`) are placed by their origin.
        let origin = [3, 7, 11].map(|k| self.xf_float((default_matrix * 4 + k) & 0xFF));
        let origin = (!orthographic && origin[2] < 0.0).then(|| {
            [
                proj[0] * origin[0] + proj[1] * origin[2],
                proj[2] * origin[1] + proj[3] * origin[2],
                proj[4] * origin[2] + proj[5],
                -origin[2],
            ]
        });
        let raw_ok = !orthographic
            && vcd_lo & 0x1FF == 0
            && vcd_hi >> 8 == 0
            && (0..8).all(|n| !coord_used[n] || !matches!(gens[n].0, Source::Tex(k) if k >= 4));
        let vstate = Arc::new(VState {
            vcd_lo,
            vcd_hi,
            arrays,
            default_matrix,
            matrices,
            gens,
            gen_rows,
            coord_used,
            coord_bias,
            channels,
            lit_channels,
            lighting,
            lights,
            proj,
            orthographic,
            snap: [snap_w, snap_h, snap_cx, snap_cy],
            samples_copy,
            origin,
            raw_ok,
        });

        let (zmode, cmode) = (self.bp[0x40], self.bp[0x41]);
        let state = DrawState {
            depth_test: zmode & 1 != 0,
            depth_func: ((zmode >> 1) & 7) as u8,
            depth_write: zmode & 0x10 != 0,
            blend: cmode & 1 != 0 || cmode & 0x800 != 0,
            subtract: cmode & 0x800 != 0,
            color_write: cmode & 8 != 0,
            alpha_write: cmode & 0x10 != 0,
            dst_factor: ((cmode >> 5) & 7) as u8,
            src_factor: ((cmode >> 8) & 7) as u8,
            cull: if no_cull() { 0 } else { ((self.bp[0x00] >> 14) & 3) as u8 },
        };
        // Viewport registers hold half-extents and a centre, both offset by 342.
        let (half_w, half_h) = (self.xf_float(0x101A), -self.xf_float(0x101B));
        let viewport = if half_w > 0.0 && half_h > 0.0 {
            let (cx, cy) = (self.xf_float(0x101D) - 342.0, self.xf_float(0x101E) - 342.0);
            [cx - half_w, cy - half_h, half_w * 2.0, half_h * 2.0]
        } else {
            [0.0, 0.0, 640.0, 480.0]
        };
        let (tl, br) = (self.bp[0x20], self.bp[0x21]);
        let edge = |v: u32, extra: u32| ((v & 0x7FF) + extra).saturating_sub(342);
        let scissor = [edge(tl >> 12, 0), edge(tl, 0), edge(br >> 12, 1), edge(br, 1)];
        let params = self.frame.push_params(&self.tev_params());
        self.template_serial += 1;
        self.template =
            Some(Template { serial: self.template_serial, material, vstate, state, viewport, scissor, params, vparams: None, vparams_flat: None });
    }

    /// Queues one draw command as a triangle list with its material. Its vertices go to the
    /// vertex shader as they are when it can do them; otherwise they are decoded and
    /// transformed to clip space with the game's matrices in `finish_vertices`.
    fn draw(&mut self, op: u8, count: usize, data: &[u8], c: &mut Ctx) {
        let index = self.draw_index;
        self.draw_index += 1;
        if self.dump_frame == Some(self.frames + 1) {
            self.trace_draws = self.trace_draws.max(1);
        }
        self.trace_draw(index, op, count);
        if self.skip_draws.iter().any(|&(first, last)| (first..=last).contains(&index)) {
            return;
        }

        let started = perf_traced().then(std::time::Instant::now);
        self.survey(op);
        if self.template.is_none() {
            self.build_template(c);
        }
        let material_done = perf_traced().then(std::time::Instant::now);
        let vertices_before = PROF[2].load(Relaxed);
        let mut template = self.template.take().expect("a template was just built");
        self.draw_with(&mut template, op, count, data);
        self.template = Some(template);
        if let (Some(started), Some(material_done)) = (started, material_done) {
            // Time in the vertices is counted on its own.
            let vertices = PROF[2].load(Relaxed) - vertices_before;
            PROF[0].fetch_add((material_done - started).as_nanos() as u64, Relaxed);
            PROF[1].fetch_add((material_done.elapsed().as_nanos() as u64).saturating_sub(vertices), Relaxed);
        }
        PROF[3].fetch_add(1, Relaxed);
        PROF[4].fetch_add(count as u64, Relaxed);
    }

    fn draw_with(&mut self, template: &mut Template, op: u8, count: usize, data: &[u8]) {
        let vat = (op & 7) as usize;
        let (a, b, cc) = (self.cp[0x70 + vat], self.cp[0x80 + vat], self.cp[0x90 + vat]);
        let vstate = &template.vstate;

        // In a dumped frame, small draws also get their coordinate generation and vertices listed.
        let dumping = self.dump_frame == Some(self.frames + 1);
        let dump_vertices = dumping && count <= 16;
        if dumping && !vstate.orthographic {
            let matrix = (self.cp[0x30] & 0x3F) as usize;
            let rows: Vec<f32> = (0..12).map(|k| self.xf_float((matrix * 4 + k) & 0xFF)).collect();
            let normal: Vec<f32> = (0..9).map(|k| self.xf_float(0x400 + (matrix & 31) * 3 + k)).collect();
            eprintln!("[gx]   position matrix {matrix} = {rows:?}; normal matrix = {normal:?}");
        }
        if dump_vertices {
            for n in (0..8).filter(|&n| vstate.coord_used[n]) {
                let matrix = vstate.gens[n].1;
                let rows: Vec<f32> = (0..12).map(|k| self.xf_float((matrix * 4 + k) & 0xFF)).collect();
                eprintln!(
                    "[gx]   texgen {n}: register {:#x}, post-transform {:#x}, dual-tex enable {:#x}, matrix {matrix} = {rows:?}",
                    self.xf[0x1040 + n],
                    self.xf[0x1050 + n],
                    self.xf[0x1012]
                );
            }
            eprintln!(
                "[gx]   vertex descriptor {:#x}/{:#x}, matrix index A {:#x} B {:#x}, samplers {:?}",
                vstate.vcd_lo,
                vstate.vcd_hi,
                self.cp[0x30],
                self.cp[0x40],
                &template.material.samplers[..2]
            );
        }

        // The vertex shader takes what is drawn in perspective, except the models of the
        // heads-up display, which are placed for a wide picture on the CPU together with the
        // flat drawing of their widgets (see `finish_vertices`).
        let raw = self.raw_enabled && vstate.raw_ok && self.hud == 0 && !dumping;
        if raw && self.pending_ortho {
            // Whether flat drawing has begun has to be known.
            self.finish_vertices();
        }
        let base = if raw {
            // The primitive only decides the indices; without triangles nothing is drawn.
            if !matches!(op & 0xF8, 0x80 | 0x88 | 0x90 | 0x98 | 0xA0) || template.state.cull == 3 {
                return;
            }
            let started = perf_traced().then(std::time::Instant::now);
            let base = self.frame.raw.len() as u32;
            let format = Format { vcd_lo: vstate.vcd_lo, vcd_hi: vstate.vcd_hi, a, b, arrays: vstate.arrays };
            decode_raw(self.mem, data, count, &format, &mut self.frame.raw, &mut [(u32::MAX, 0); 12]);
            PROF[6].fetch_add(count as u64, Relaxed);
            if let Some(started) = started {
                PROF[2].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
            }
            base
        } else {
            // The vertices themselves are worked out later, together with those of the draws
            // around this one and on several threads (`finish_vertices`): everything they
            // depend on is kept as it is now.
            let base = (self.frame.vertices.len() + self.job_vertices) as u32;
            let bytes = self.job_data.len();
            self.job_data.extend_from_slice(data);
            self.job_vertices += count;
            self.jobs.push(VertexJob {
                data: bytes..self.job_data.len(),
                count,
                base,
                a,
                b,
                cc,
                dump: dump_vertices,
                hud: self.hud,
                state: vstate.clone(),
            });
            self.pending_ortho |= vstate.orthographic;
            if dump_vertices {
                // So that the vertices are listed under their draw.
                self.finish_vertices();
            }
            base
        };

        let first = self.frame.indices.len() as u32;
        emit_indices(op, base, count as u32, &mut self.frame.indices);
        let emitted = self.frame.indices.len() as u32 - first;
        if emitted == 0 {
            return;
        }
        if template.state.cull == 3 {
            // Both faces culled: nothing of a triangle is drawn.
            self.frame.indices.truncate(first as usize);
            return;
        }
        let geometry = if raw { Geometry::Raw } else { Geometry::Transformed };
        let vparams = if raw { template.vparams(&mut self.frame, self.flat_started) } else { 0 };
        // The game sends its geometry in thousands of small pieces per frame; one that
        // continues the last draw from the same template only makes that one longer.
        if self.last_serial == template.serial {
            if let Some(last) = self.frame.draws.last_mut() {
                if last.geometry == geometry && last.vparams == vparams && last.first + last.count == first {
                    last.count += emitted;
                    return;
                }
            }
        }
        let Material { shader, textures, samplers, .. } = template.material;
        self.frame.push_draw(Draw {
            first,
            count: emitted,
            state: template.state,
            shader,
            textures,
            samplers,
            params: template.params,
            geometry,
            vparams,
            viewport: template.viewport,
            scissor: template.scissor,
        });
        self.last_serial = template.serial;
    }

    /// A number that stands for how vertices are laid out now: the vertex descriptor, every
    /// vertex format's attribute table, and the attribute arrays' strides.
    fn cp_signature(&mut self) -> u64 {
        if let Some(signature) = self.cp_sig {
            return signature;
        }
        let mut hash = 0u64;
        // Not the arrays' addresses: the same vertices somewhere else are the same vertices.
        let registers = [0x50..0x51, 0x60..0x61, 0x70..0x78, 0x80..0x88, 0x90..0x98, 0xB0..0xBC];
        for register in registers.into_iter().flatten() {
            hash = (hash.rotate_left(7) ^ self.cp[register] as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        self.cp_sig = Some(hash);
        hash
    }

    /// Where `spans` (offsets into the attribute arrays) are in memory now, as (address,
    /// length), or `None` if any of it, with the 16 bytes a vertex fetch may read beyond it,
    /// lies outside memory.
    fn spans_at(&self, spans: &[(u32, u32); 12]) -> Option<[(u32, u32); 12]> {
        let mut at = [(0u32, 0u32); 12];
        for n in (0..12).filter(|&n| spans[n].1 != 0) {
            let base = (self.cp[0xA0 + n] & 0x03FF_FFFF) | 0x8000_0000;
            let addr = base as u64 + spans[n].0 as u64;
            if addr + spans[n].1 as u64 + 16 > (runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE) as u64 {
                return None;
            }
            at[n] = (addr as u32, spans[n].1);
        }
        Some(at)
    }

    /// The addresses of the twelve attribute arrays.
    fn array_bases(&self) -> [u32; 12] {
        std::array::from_fn(|n| (self.cp[0xA0 + n] & 0x03FF_FFFF) | 0x8000_0000)
    }

    /// A hash of exactly the bytes of `spans` (address, length).
    fn exact_hash(&self, spans: &[(u32, u32); 12]) -> u64 {
        let mut hash = 0u64;
        for &(addr, len) in spans.iter().filter(|span| span.1 != 0) {
            // SAFETY: `spans_at` gave these spans: they lie within guest RAM.
            let bytes = unsafe { std::slice::from_raw_parts(self.mem.0.add(addr as usize), len as usize) };
            hash = (hash.rotate_left(5) ^ hash_bytes(bytes)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        hash
    }

    /// A hash of the blocks of guest memory that `spans` lie in. Each block is hashed once
    /// for as long as the game stands still.
    fn spans_hash(&mut self, spans: &[(u32, u32); 12]) -> u64 {
        let mut hash = 0u64;
        for &(addr, len) in spans.iter().filter(|span| span.1 != 0) {
            let from = addr - runtime::mem::RAM_BASE;
            for block in (from >> BLOCK_SHIFT) as usize..=((from + len - 1) >> BLOCK_SHIFT) as usize {
                if self.block_stamp[block] != self.epoch {
                    let at = runtime::mem::RAM_BASE as usize + (block << BLOCK_SHIFT);
                    // SAFETY: the block lies within guest RAM, which stays mapped.
                    let bytes = unsafe { std::slice::from_raw_parts(self.mem.0.add(at), 1 << BLOCK_SHIFT) };
                    self.block_hash[block] = hash_bytes(bytes);
                    self.block_stamp[block] = self.epoch;
                }
                hash = (hash.rotate_left(5) ^ self.block_hash[block]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            }
        }
        hash
    }

    /// Decodes a display list that holds nothing but draws into `list_vertices` and
    /// `list_indices`, and returns what it reads of each attribute array, its draws and its
    /// vertices. `None` if it holds anything else or reads outside memory.
    fn build_list(&mut self, list: &[u8]) -> Option<([(u32, u32); 12], u32, u32)> {
        let mut vertices = std::mem::take(&mut self.list_vertices);
        let mut indices = std::mem::take(&mut self.list_indices);
        vertices.clear();
        indices.clear();
        let (vcd_lo, vcd_hi) = (self.cp[0x50], self.cp[0x60]);
        let arrays: [(u32, u32); 12] =
            std::array::from_fn(|n| ((self.cp[0xA0 + n] & 0x03FF_FFFF) | 0x8000_0000, self.cp[0xB0 + n] & 0xFF));
        let mut ranges = [(u32::MAX, 0u32); 12];
        let (mut at, mut draws, mut total) = (0, 0u32, 0u32);
        let mut plain = true;
        while at < list.len() {
            let op = list[at];
            if op == 0 {
                at += 1;
                continue;
            }
            if !(0x80..=0xBF).contains(&op) || at + 3 > list.len() {
                plain = false;
                break;
            }
            let (count, vat) = (be16(list, at + 1), (op & 7) as usize);
            let need = 3 + count * self.vertex_size(vat);
            if at + need > list.len() {
                plain = false;
                break;
            }
            let format =
                Format { vcd_lo, vcd_hi, a: self.cp[0x70 + vat], b: self.cp[0x80 + vat], arrays };
            let base = vertices.len() as u32;
            decode_raw(self.mem, &list[at + 3..at + need], count, &format, &mut vertices, &mut ranges);
            emit_indices(op, base, count as u32, &mut indices);
            draws += 1;
            total += count as u32;
            at += need;
        }
        let mut spans = [(0u32, 0u32); 12];
        for n in (0..12).filter(|&n| ranges[n].0 != u32::MAX) {
            let stride = arrays[n].1 as u64;
            // From the element of the lowest index to the end of the one of the highest. (An
            // array without a stride has one element; 16 bytes is the most read of one.)
            let (offset, len) = (ranges[n].0 as u64 * stride, (ranges[n].1 - ranges[n].0) as u64 * stride + stride.clamp(1, 16));
            if offset + len > runtime::mem::RAM_SIZE as u64 {
                plain = false;
                break;
            }
            spans[n] = (offset as u32, len as u32);
        }
        plain &= self.spans_at(&spans).is_some();
        self.list_vertices = vertices;
        self.list_indices = indices;
        plain.then_some((spans, draws, total))
    }

    /// Works out a display list's vertices from memory as it is now: where they are in the
    /// cache of lists kept on the GPU (putting them there first if need be), or, for a list
    /// whose vertices keep changing, the vertices themselves.
    fn resolve_list(&mut self, addr: u32, size: usize) -> Ahead {
        if !self.raw_enabled || !self.cache_enabled || self.renderer.is_none() || size == 0 || !in_ram(addr, size) {
            return Ahead::Late;
        }
        // Only what the vertex shader can do (see `VState::raw_ok`).
        if self.cp[0x50] & 0x1FF != 0 || self.cp[0x60] >> 8 != 0 {
            return Ahead::Late;
        }
        let started = perf_traced().then(std::time::Instant::now);
        // SAFETY: the list lies within guest RAM, which stays mapped; the game is stopped.
        let list = unsafe { std::slice::from_raw_parts(self.mem.0.add(addr as usize), size) };
        let list_hash = hash_bytes(list);
        let key = (list_hash, self.cp_signature());
        let mut built = false;
        let shape = match self.shapes.get(&key).copied() {
            Some(shape) => shape,
            None => {
                let shape = match self.build_list(list) {
                    Some((spans, draws, vertices)) => {
                        built = true;
                        ListShape { plain: true, spans, index_count: self.list_indices.len() as u32, draws, vertices }
                    }
                    None => ListShape { plain: false, spans: [(0, 0); 12], index_count: 0, draws: 0, vertices: 0 },
                };
                self.shapes.insert(key, shape);
                shape
            }
        };
        if !shape.plain {
            return Ahead::Late;
        }
        // A list is kept only once it has come again in a later frame (the game also writes
        // lists that are drawn once and never again), and not while its vertices keep
        // changing.
        if self.seen.len() > 1 << 16 {
            self.seen.clear();
        }
        let came_before = match self.seen.get(&key) {
            Some(&frame) => frame != self.frames,
            None => {
                self.seen.insert(key, self.frames);
                false
            }
        };
        let keep = came_before && !self.cache_full && !self.churn.get(&key).is_some_and(|churn| self.frames < churn.out_until);
        let mut place = None;
        if let (true, Some(at)) = (keep, self.spans_at(&shape.spans)) {
            // Which vertices these are: by the last look, where the arrays are where they
            // were then and nothing in the blocks around the vertices has changed; else by
            // hashing the vertices themselves.
            let bases = self.array_bases().iter().fold(0u64, |hash, &base| (hash.rotate_left(9) ^ base as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let blocks = self.spans_hash(&at);
            let exact = match self.checks.get(&(key.0, key.1, bases)) {
                Some(check) if check.blocks == blocks => check.exact,
                _ => {
                    let exact = self.exact_hash(&at);
                    if self.checks.len() > 1 << 16 {
                        self.checks.clear();
                    }
                    self.checks.insert((key.0, key.1, bases), ListCheck { blocks, exact });
                    exact
                }
            };
            place = self.places.get(&(key.0, key.1, exact)).copied();
            if place.is_none() && (built || self.build_list(list).is_some()) {
                built = true;
                // The same vertices and indices as another list's are drawn from where those are.
                let content = (
                    hash_bytes(bytemuck::cast_slice(&self.list_vertices)),
                    hash_bytes(bytemuck::cast_slice(&self.list_indices)),
                    self.list_vertices.len() as u32,
                );
                let shared = self.contents.get(&content).copied();
                place = shared
                    .or_else(|| self.renderer.as_mut().and_then(|r| r.cache_alloc(&self.list_vertices, &self.list_indices)));
                PROF[7].fetch_add(shape.vertices as u64, Relaxed);
                self.frame_cached += shape.vertices;
                match place {
                    None => self.cache_full = true,
                    Some(place) => {
                        self.places.insert((key.0, key.1, exact), place);
                        if shared.is_none() {
                            self.contents.insert(content, place);
                            // A list that takes new room three times within 30 frames is an
                            // animated model: its vertices are not kept for a while, twice
                            // as long each time.
                            let frame = self.frames;
                            let churn = self.churn.entry(key).or_default();
                            churn.builds += 1;
                            churn.vertices += shape.vertices as u64;
                            churn.run = if frame - churn.built <= 30 { churn.run + 1 } else { 1 };
                            churn.built = frame;
                            if churn.run >= 3 {
                                churn.outs = (churn.outs + 1).min(6);
                                churn.run = 1;
                                churn.out_until = frame + (150 << churn.outs);
                            }
                        }
                    }
                }
            }
        }
        let ahead = match place {
            Some(place) => Ahead::Cached(shape, place),
            None if built || self.build_list(list).is_some() => {
                let (vertices, indices) = self.spare.pop().unwrap_or_default();
                Ahead::Owned(
                    shape,
                    std::mem::replace(&mut self.list_vertices, vertices),
                    std::mem::replace(&mut self.list_indices, indices),
                )
            }
            None => Ahead::Late,
        };
        if let Some(started) = started {
            PROF[2].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        }
        ahead
    }

    /// Draws the display list at `addr`, which the stream calls at position `at`, without
    /// decoding it command by command: from the cache of lists kept on the GPU, or from the
    /// vertices `look_ahead` took down when the game called it. Returns false if the list
    /// has to be decoded the long way.
    fn cached_list(&mut self, addr: u32, size: usize, at: u64, c: &mut Ctx) -> bool {
        while self.ahead.front().is_some_and(|ahead| ahead.0 < at) {
            if let Some((_, Ahead::Owned(_, vertices, indices))) = self.ahead.pop_front() {
                self.spare.push((vertices, indices));
            }
        }
        let noted = match self.ahead.front() {
            Some(ahead) if ahead.0 == at => self.ahead.pop_front().map(|ahead| ahead.1),
            _ => None,
        };
        let give_back = |gx: &mut Gx, ahead: Option<Ahead>| {
            if let Some(Ahead::Owned(_, vertices, indices)) = ahead {
                gx.spare.push((vertices, indices));
            }
            false
        };
        if !self.raw_enabled || !self.cache_enabled || self.hud != 0 || self.renderer.is_none() {
            return give_back(self, noted);
        }
        if self.template.is_none() {
            self.build_template(c);
        }
        if !self.template.as_ref().is_some_and(|template| template.vstate.raw_ok) {
            return give_back(self, noted);
        }
        if self.pending_ortho {
            self.finish_vertices();
        }
        if let (Some(Ahead::Owned(_, vertices, _)), true) = (&noted, late_traced()) {
            // TSSM_TRACE_LATE=1: are the vertices in memory still those taken down when the
            // game called the list?
            static COUNT: [std::sync::atomic::AtomicU32; 2] = [const { std::sync::atomic::AtomicU32::new(0) }; 2];
            // SAFETY: `resolve_list` found the list within guest RAM.
            let list = unsafe { std::slice::from_raw_parts(self.mem.0.add(addr as usize), size) };
            let then = hash_bytes(bytemuck::cast_slice(vertices));
            let same = self.build_list(list).is_some() && hash_bytes(bytemuck::cast_slice(&self.list_vertices)) == then;
            let lists = COUNT[0].fetch_add(1, Relaxed) + 1;
            let changed = COUNT[1].fetch_add(!same as u32, Relaxed) + !same as u32;
            if lists % 20000 == 0 {
                eprintln!("[late] of the last 20000 lists with changing vertices, {changed} held other vertices by the time they were decoded");
                COUNT[1].store(0, Relaxed);
            }
        }
        let ahead = match noted {
            Some(ahead) => ahead,
            None => self.resolve_list(addr, size),
        };
        let shape = match &ahead {
            Ahead::Cached(shape, _) | Ahead::Owned(shape, ..) => *shape,
            Ahead::Late => return false,
        };
        self.stats.draws += shape.draws;
        self.stats.vertices += shape.vertices;
        self.draw_index += shape.draws;
        PROF[3].fetch_add(shape.draws as u64, Relaxed);
        PROF[4].fetch_add(shape.vertices as u64, Relaxed);
        let mut template = self.template.take().expect("a template was just built");
        let drawn = shape.index_count != 0 && template.state.cull != 3;
        let (first, geometry) = match ahead {
            Ahead::Cached(_, (base_vertex, first_index)) => {
                PROF[5].fetch_add(shape.vertices as u64, Relaxed);
                (first_index, Geometry::Cached(base_vertex))
            }
            Ahead::Owned(_, vertices, indices) => {
                PROF[6].fetch_add(shape.vertices as u64, Relaxed);
                let (base, first) = (self.frame.raw.len() as u32, self.frame.indices.len() as u32);
                if drawn {
                    self.frame.raw.extend_from_slice(&vertices);
                    self.frame.indices.extend(indices.iter().map(|index| index + base));
                }
                self.spare.push((vertices, indices));
                (first, Geometry::Raw)
            }
            Ahead::Late => unreachable!(),
        };
        if drawn {
            let vparams = template.vparams(&mut self.frame, self.flat_started);
            let Material { shader, textures, samplers, .. } = template.material;
            self.frame.draws.push(Draw {
                first,
                count: shape.index_count,
                state: template.state,
                shader,
                textures,
                samplers,
                params: template.params,
                geometry,
                vparams,
                viewport: template.viewport,
                scissor: template.scissor,
            });
            self.last_serial = 0;
        }
        self.template = Some(template);
        true
    }

    /// Called right after the game has called a display list: reads on through what it has
    /// written to the pipe so far, only far enough to know how vertices are laid out at
    /// each display list call, and takes down each list's vertices as they are now
    /// (`resolve_list`). The decoder itself may come to a call much later: the game runs up
    /// to two frames ahead of what is shown, and by then the arrays a list indexes can hold
    /// something else. The game works out some of its lighting (the police car's light at
    /// the Krusty Krab) into buffers it uses again every frame.
    pub fn look_ahead(&mut self, c: &Ctx) {
        if !self.raw_enabled || !self.cache_enabled || self.renderer.is_none() {
            return;
        }
        self.mem = Mem(c.mem);
        // The game has run since the last look: memory may hold something else now.
        self.epoch = self.epoch.wrapping_add(1).max(1);
        if self.ahead_pos < self.consumed || self.ahead_bad {
            // Nothing read ahead of the decoder: start from where it is and what it knows.
            self.ahead_pos = self.consumed;
            self.ahead_cp.copy_from_slice(&self.cp);
            self.ahead_sig = self.cp_sig;
            self.ahead_bad = false;
        }
        let start = (self.ahead_pos - self.consumed) as usize;
        let mut data = std::mem::take(&mut self.ahead_data);
        data.clear();
        if start < self.pending.len() {
            data.extend_from_slice(&self.pending[start..]);
            data.extend_from_slice(&c.fifo);
        } else {
            data.extend_from_slice(c.fifo.get(start - self.pending.len()..).unwrap_or(&[]));
        }
        // The layout registers are the reader's own for the while.
        std::mem::swap(&mut self.cp, &mut self.ahead_cp);
        std::mem::swap(&mut self.cp_sig, &mut self.ahead_sig);
        let mut i = 0;
        while i < data.len() {
            let rest = &data[i..];
            let op = rest[0];
            let need = match op {
                0x00 | 0x48 => 1,
                0x08 => 6,
                0x10 if rest.len() >= 5 => 5 + 4 * ((be16(rest, 1) & 0xF) + 1),
                0x20 | 0x28 | 0x30 | 0x38 | 0x61 => 5,
                0x40 => 9,
                0x80..=0xBF if rest.len() >= 3 => 3 + be16(rest, 1) * self.vertex_size((op & 7) as usize),
                0x10 | 0x80..=0xBF => break,
                _ => {
                    // Not a stream this reader understands: leave it to the decoder.
                    self.ahead_bad = true;
                    break;
                }
            };
            if rest.len() < need {
                break;
            }
            match op {
                0x08 => {
                    let (register, value) = (rest[1] as usize, be32(rest, 2));
                    if self.cp[register] != value {
                        self.cp[register] = value;
                        self.cp_sig = None;
                    }
                }
                0x40 => {
                    let (addr, size) = (be32(rest, 1) | 0x8000_0000, be32(rest, 5));
                    let ahead = self.resolve_list(addr, size as usize);
                    self.ahead.push_back((self.ahead_pos + i as u64, ahead));
                }
                _ => {}
            }
            i += need;
        }
        std::mem::swap(&mut self.cp, &mut self.ahead_cp);
        std::mem::swap(&mut self.cp_sig, &mut self.ahead_sig);
        self.ahead_pos += i as u64;
        self.ahead_data = data;
    }

    /// How many vertices the display list cache holds on the GPU.
    pub fn cache_room_used(&self) -> u64 {
        self.renderer.as_ref().map_or(0, |renderer| renderer.cache_used())
    }

    /// Size in bytes of one vertex in format `vat`, from the vertex descriptor (CP 0x50/0x60)
    /// and the attribute table (CP 0x70/0x80/0x90 + vat).
    fn vertex_size(&self, vat: usize) -> usize {
        let (vcd_lo, vcd_hi) = (self.cp[0x50], self.cp[0x60]);
        let (a, b, c) = (self.cp[0x70 + vat], self.cp[0x80 + vat], self.cp[0x90 + vat]);
        let attr = |kind: u32, direct: usize| match kind {
            1 => direct,
            2 => 1,
            3 => 2,
            _ => 0,
        };

        // matrix indices: position/normal plus eight texture matrices, one byte each
        let mut size = (vcd_lo & 0x1FF).count_ones() as usize;
        size += attr((vcd_lo >> 9) & 3, (2 + (a & 1) as usize) * COMP[((a >> 1) & 7) as usize]);
        let nbt = (a >> 9) & 1 != 0;
        let nrm_direct = if nbt { 9 } else { 3 } * COMP[((a >> 10) & 7) as usize];
        let nrm_indices = if nbt && a >> 31 != 0 { 3 } else { 1 };
        size += match (vcd_lo >> 11) & 3 {
            1 => nrm_direct,
            k => attr(k, 0) * nrm_indices,
        };
        size += attr((vcd_lo >> 13) & 3, COLOR[((a >> 14) & 7) as usize]);
        size += attr((vcd_lo >> 15) & 3, COLOR[((a >> 18) & 7) as usize]);
        // (count bit, format field) for the eight texture coordinates, spread over VAT A/B/C
        let tex = [
            (a >> 21, a >> 22),
            (b, b >> 1),
            (b >> 9, b >> 10),
            (b >> 18, b >> 19),
            (b >> 27, b >> 28),
            (c >> 5, c >> 6),
            (c >> 14, c >> 15),
            (c >> 23, c >> 24),
        ];
        for (i, (cnt, fmt)) in tex.into_iter().enumerate() {
            size += attr((vcd_hi >> (i * 2)) & 3, (1 + (cnt & 1) as usize) * COMP[(fmt & 7) as usize]);
        }
        size
    }

    /// Decodes as many whole commands as `data` holds and returns the bytes consumed.
    fn parse(&mut self, data: &[u8], c: &mut Ctx, depth: u32) -> usize {
        let mut i = 0;
        while i < data.len() {
            if depth == 0 {
                while let Some(&(at, widget)) = self.hud_marks.front().filter(|mark| mark.0 <= self.consumed + i as u64) {
                    let _ = at;
                    self.hud = widget;
                    self.hud_marks.pop_front();
                }
            }
            let rest = &data[i..];
            let op = rest[0];
            let need = match op {
                0x00 | 0x48 => 1,
                0x08 => 6,
                0x10 if rest.len() >= 5 => 5 + 4 * ((be16(rest, 1) & 0xF) + 1),
                0x20 | 0x28 | 0x30 | 0x38 | 0x61 => 5,
                0x40 => 9,
                0x80..=0xBF if rest.len() >= 3 => 3 + be16(rest, 1) * self.vertex_size((op & 7) as usize),
                0x10 | 0x80..=0xBF => break,
                _ => {
                    self.errors += 1;
                    if self.errors <= 10 {
                        let ctx: Vec<String> = rest.iter().take(16).map(|b| format!("{b:02x}")).collect();
                        eprintln!("[gx] bad opcode {op:#04x} at stream offset {i} (depth {depth}): {}", ctx.join(" "));
                    }
                    return data.len(); // resynchronise by dropping the rest
                }
            };
            if rest.len() < need {
                break;
            }
            match op {
                0x08 => {
                    let (register, value) = (rest[1] as usize, be32(rest, 2));
                    if self.cp[register] != value {
                        self.cp[register] = value;
                        self.template = None;
                        self.cp_sig = None;
                    }
                    self.stats.cp_loads += 1;
                }
                0x10 => {
                    let base = be16(rest, 3);
                    let mut changed = false;
                    for (k, word) in rest[5..need].chunks_exact(4).enumerate() {
                        if let Some(slot) = self.xf.get_mut(base + k) {
                            let value = be32(word, 0);
                            changed |= *slot != value;
                            *slot = value;
                        }
                    }
                    if changed {
                        self.template = None;
                    }
                    self.stats.xf_loads += 1;
                }
                0x40 if depth < 4 => {
                    let (addr, size) = (be32(rest, 1) | 0x8000_0000, be32(rest, 5));
                    self.stats.display_lists += 1;

                    let at = self.consumed + i as u64;
                    if depth != 0 || !self.cached_list(addr, size as usize, at, c) {
                        let list = c.read_bytes(addr, size as usize);
                        self.parse(&list, c, depth + 1);
                    }
                }
                0x61 => self.bp_load(be32(rest, 1), c),
                0x80..=0xBF => {
                    self.stats.draws += 1;
                    self.stats.vertices += be16(rest, 1) as u32;
                    if self.renderer.is_some() {
                        self.draw(op, be16(rest, 1), &rest[3..need], c);
                    }
                }
                _ => {}
            }
            i += need;
        }
        i
    }

    fn bp_load(&mut self, word: u32, c: &mut Ctx) {
        let reg = (word >> 24) as usize;
        // Register 0xFE masks the next write: only the bits set in it change. It applies once.
        let mask = if reg == 0xFE { 0xFF_FFFF } else { std::mem::replace(&mut self.bp[0xFE], 0xFF_FFFF) };
        let value = (self.bp[reg] & !mask) | (word & mask & 0xFF_FFFF);
        if self.bp[reg] != value {
            self.forget_material();
        }
        self.bp[reg] = value;
        self.stats.bp_loads += 1;
        match reg {
            // Combiner color registers and constants: red/alpha in the even register, blue/green
            // in the odd one, as 11-bit signed values; bit 23 selects the constant.
            0xE0..=0xE7 => {
                let part = |v: u32| (((v & 0x7FF) << 21) as i32 >> 21) as f32 / 255.0;
                let (low, high) = (part(value), part(value >> 12));
                let index = (reg - 0xE0) / 2;
                let target = if value & 0x80_0000 != 0 { &mut self.tev_konst[index] } else { &mut self.tev_reg[index] };
                if reg & 1 == 0 {
                    (target[0], target[3]) = (low, high);
                } else {
                    (target[2], target[1]) = (low, high);
                }
            }
            // Palette load: copy from RAM (address in BP 0x64) into texture memory. The
            // destination is in 512-byte units and the length in 32-byte lines.
            0x65 => {
                let source = ((self.bp[0x64] & 0xFF_FFFF) << 5) | 0x8000_0000;
                let (dest, len) = (((value & 0x3FF) << 9) as usize, (((value >> 10) & 0x3FF) << 5) as usize);
                if in_ram(source, len) && dest + len <= self.tmem.len() {
                    self.tmem[dest..dest + len].copy_from_slice(&c.read_bytes(source, len));
                }
                self.texture_cache.clear(); // paletted textures may look different now
                self.forget_material();
            }
            BP_PE_TOKEN => self.token = value as u16,
            BP_PE_TOKEN_INT => {
                self.token = value as u16;
                self.note(format!("token {value:#06x} with interrupt reached"));
                self.raised.push(Raised::Token(value as u16));
            }
            // Bit 1 asks for the "draw done" interrupt once everything before it is drawn.
            BP_PE_DONE if value & 2 != 0 => {
                self.finish_requests += 1;
                self.raised.push(Raised::Finish);
            }
            // bit 14 selects a copy to the display buffer rather than to a texture
            BP_EFB_COPY if value & (1 << 14) != 0 => self.end_frame(value),
            BP_EFB_COPY => self.copy_to_texture(value),
            _ => {}
        }
    }

    /// Source rectangle of a framebuffer copy (x, y, width, height): position in BP 0x49,
    /// size minus one in BP 0x4A.
    fn copy_source(&self) -> [u32; 4] {
        let (xy, wh) = (self.bp[0x49], self.bp[0x4A]);
        [xy & 0x3FF, (xy >> 10) & 0x3FF, (wh & 0x3FF) + 1, ((wh >> 10) & 0x3FF) + 1]
    }

    /// The copy-clear color from BP 0x4F (alpha, red) and 0x50 (green, blue).
    fn clear_color(&self) -> [f64; 4] {
        let (ar, gb) = (self.bp[0x4F], self.bp[0x50]);
        [ar & 0xFF, (gb >> 8) & 0xFF, gb & 0xFF, (ar >> 8) & 0xFF].map(|v| v as f64 / 255.0)
    }

    /// A copy of the framebuffer into a texture: draw what is queued so far, capture the
    /// region, and clear that region afterwards if the copy asks for it.
    fn copy_to_texture(&mut self, copy: u32) {
        self.finish_vertices();
        let mut frame = std::mem::take(&mut self.frame);
        let draws_before = frame.draws.len();
        // Flat drawing that ends up in a texture is the game's own image processing.
        frame.flat.clear();
        frame.raw_flat.clear();
        self.flat_started = false;
        let rect = self.copy_source();
        let dest = ((self.bp[0x4B] & 0xFF_FFFF) << 5) | 0x8000_0000;
        if let Some(renderer) = &mut self.renderer {
            let start = std::time::Instant::now();
            renderer.flush(&frame, self.clear.take());
            renderer.copy_efb(dest, rect);
            self.render_time += start.elapsed();
            self.frame_render += start.elapsed();
            // In a dumped frame, record where each capture falls and what it captured.
            if self.dump_frame == Some(self.frames + 1) {
                eprintln!("[gx] capture before draw {}: rect {rect:?} -> {dest:#010x}", self.draw_index);
                renderer.save_copy(dest, &format!("screenshots/copy_{dest:08x}_before_draw_{}.png", self.draw_index));
            }
        }
        frame.clear();
        self.frame = frame;
        // Captured textures have new contents; lookups cached this frame may be stale.
        self.texture_cache.clear();
        self.forget_material();
        self.trace_copy("texture copy", rect, dest, copy, draws_before);
        // Once, describe the draws that follow the smallest capture of the glow chain.
        if rect[2] == 32 && !self.traced_after_copy && std::env::var_os("TSSM_TRACE_GX").is_some() {
            self.traced_after_copy = true;
            self.trace_draws = 10;
        }
        if copy & (1 << 11) != 0 {
            self.queue_clear(rect);
        }
    }

    /// The color, alpha and depth write enables. The clear after a copy only touches the
    /// channels that are enabled, so a game can copy and then reset just the depth buffer.
    fn write_masks(&self) -> (bool, bool, bool) {
        (self.bp[0x41] & 8 != 0, self.bp[0x41] & 0x10 != 0, self.bp[0x40] & 0x10 != 0)
    }

    /// Queues the clear that follows a copy: a quad at the far plane over `rect` (x, y,
    /// width, height) in the clear color, limited to the enabled channels.
    fn queue_clear(&mut self, rect: [u32; 4]) {
        let (color_write, alpha_write, depth_write) = self.write_masks();
        if !(color_write || alpha_write || depth_write) {
            return;
        }
        let color = self.clear_color().map(|v| v as f32);
        let corner = |x: f32, y: f32| Vertex { pos: [x, y, 1.0, 1.0], colors: [color; 2], uvs: [[0.0; 4]; 4] };
        // The clear is drawn with a material that outputs the color untouched.
        let material = tev::Config::passthrough();
        let shader = material.key();
        if let Some(renderer) = &mut self.renderer {
            renderer.ensure_shader(shader, || material.fragment());
        }
        let params = self.frame.push_params(&[0.0; PARAMS_FLOATS]);
        self.finish_vertices();
        let base = self.frame.vertices.len() as u32;
        let first = self.frame.indices.len() as u32;
        self.frame.vertices.extend_from_slice(&[corner(-1.0, 1.0), corner(1.0, 1.0), corner(1.0, -1.0), corner(-1.0, -1.0)]);
        self.frame.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        let state = DrawState {
            depth_test: depth_write,
            depth_func: 7, // always
            depth_write,
            blend: false,
            subtract: false,
            src_factor: 1,
            cull: 0,
            dst_factor: 0,
            color_write,
            alpha_write,
        };
        self.frame.push_draw(Draw {
            first,
            count: 6,
            state,
            shader,
            textures: [WHITE_TEXTURE; TEXTURE_SLOTS],
            samplers: [(0, 0, false); TEXTURE_SLOTS],
            params,
            geometry: Geometry::Transformed,
            vparams: 0,
            viewport: [0.0, 0.0, crate::render::EFB_WIDTH as f32, crate::render::EFB_HEIGHT as f32],
            scissor: [rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]],
        });
        self.last_serial = 0;
    }

    /// With TSSM_TRACE_GX, logs each distinct kind of framebuffer copy once.
    fn trace_copy(&mut self, kind: &str, rect: [u32; 4], dest: u32, copy: u32, draws_before: usize) {
        if std::env::var_os("TSSM_TRACE_GX").is_none() {
            return;
        }
        let (color, alpha, depth) = self.write_masks();
        // Copy format: bits 4-6 with bit 3 as the fourth bit; bit 15 converts to intensity.
        let format = ((copy >> 4) & 7) | ((copy >> 3) & 1) << 3;
        let line = format!(
            "{kind}: rect {rect:?} -> {dest:#010x}, format {format} intensity {} half-size {}, framebuffer format {}, clear {}, write masks color {color} alpha {alpha} depth {depth}, {} before it",
            copy & (1 << 15) != 0,
            copy & (1 << 9) != 0,
            self.bp[0x43] & 7,
            copy & (1 << 11) != 0,
            if draws_before == 0 { "nothing drawn" } else { "draws queued" }
        );
        if self.traced.insert(line.clone()) {
            eprintln!("[gx] frame {}: {line}", self.frames);
        }
    }

    /// A copy of the framebuffer to the display: present what was drawn.
    fn end_frame(&mut self, copy: u32) {
        self.finish_vertices();
        self.frames += 1;
        self.raw_enabled = !cpu_vertices(self.frames + 1);
        self.draw_index = 0;
        self.texture_cache.clear();
        self.forget_material();
        self.copied_to_display = true;
        // The copy's destination is in BP 0x4B, in units of 32 bytes.
        let to = ((self.bp[0x4B] & 0xFF_FFFF) << 5) | 0x8000_0000;
        if !self.display_copies.contains(&to) {
            self.display_copies.push(to);
        }
        let mut frame = std::mem::take(&mut self.frame);
        let draws_before = frame.draws.len();
        self.flat_started = false;
        let widen = crate::aspect::widen();
        if widen != 1.0 {
            // A widget of the heads-up display that lies wholly in one half of the 4:3 picture
            // belongs to that edge of the screen: the health at the top left, the counters at
            // the top right. It is decided for the widget as a whole, from everything drawn
            // for it, so that its parts stay together; one that straddles the middle (a boss's
            // health bar) stays in the middle.
            let mut widgets: std::collections::HashMap<u32, (f32, f32)> = std::collections::HashMap::new();
            for flat in frame.flat.iter().filter(|flat| flat.hud != 0 && anchor_hud()) {
                let extent = widgets.entry(flat.hud).or_insert((f32::MAX, f32::MIN));
                *extent = (extent.0.min(flat.extent.0), extent.1.max(flat.extent.1));
            }
            let margin = 1.0 - 1.0 / widen;
            for flat in &frame.flat {
                let side = match widgets.get(&flat.hud) {
                    Some(&(_, right)) if right < 0.0 => -margin,
                    Some(&(left, _)) if left > 0.0 => margin,
                    _ => 0.0,
                };
                for vertex in &mut frame.vertices[flat.from as usize..flat.to as usize] {
                    match flat.origin {
                        None => vertex.pos[0] /= widen,
                        Some(centre) => {
                            for (at, origin) in vertex.pos.iter_mut().zip(centre) {
                                *at = origin + (*at - origin) / widen;
                            }
                            vertex.pos[0] -= centre[0] / centre[3] * margin * vertex.pos[3];
                        }
                    }
                    vertex.pos[0] += side * vertex.pos[3];
                }
            }
            // The same for the models whose vertices the vertex shader transforms.
            for (offset, centre) in std::mem::take(&mut frame.raw_flat) {
                frame.place_again(offset, 1.0 / widen, -centre[0] / centre[3] * margin);
            }
        }
        if let Some(renderer) = &mut self.renderer {
            // Source rectangle of the copy: position in BP 0x49, size minus one in BP 0x4A.
            let (xy, wh) = (self.bp[0x49], self.bp[0x4A]);
            let shown = [xy & 0x3FF, (xy >> 10) & 0x3FF, (wh & 0x3FF) + 1, ((wh >> 10) & 0x3FF) + 1];
            let start = std::time::Instant::now();
            renderer.present(&frame, self.clear, shown, self.frames);
            self.render_time += start.elapsed();
            self.frame_render += start.elapsed();
            crate::hint::frame_shown();
            // A frame that took far longer than those before it is a stutter: say what was
            // in it. (The first frames of a level and of a movie are reported too.)
            let now = std::time::Instant::now();
            let made = std::mem::take(&mut renderer.made);
            let slept = crate::host::FRAME_SLEPT.swap(0, Relaxed) as f64 / 1e6;
            if let Some(last) = self.shown_at.replace(now) {
                let took = (now - last).as_secs_f64() * 1000.0;
                if self.usual > 0.0 && took > self.usual * 2.5 && took > 12.0 {
                    eprintln!(
                        "[stutter] frame {} took {took:.1} ms (usually {:.1}): {slept:.1} ms asleep, {:.1} ms decoding so far, {:.1} ms of it the renderer; made {} shaders, {} pipelines ({:.1} ms), {} textures; {} vertices put into the display list cache{}; {} draws for the GPU; last seen in: {}",
                        self.frames,
                        self.usual,
                        (self.frame_decode + self.decode_started.map_or(Default::default(), |started| started.elapsed())).as_secs_f64() * 1000.0,
                        self.frame_render.as_secs_f64() * 1000.0,
                        made.0,
                        made.1,
                        made.3.as_secs_f64() * 1000.0,
                        made.2,
                        self.frame_cached,
                        if self.frame_emptied { ", cache emptied the frame before" } else { "" },
                        frame.draws.len(),
                        crate::calls_since(last)
                    );
                }
                self.usual = if self.usual == 0.0 { took } else { self.usual * 0.95 + took.min(self.usual * 3.0) * 0.05 };
            }
            (self.frame_decode, self.frame_render, self.frame_cached, self.frame_emptied) = Default::default();
        }
        frame.clear();
        self.frame = frame;
        if std::mem::take(&mut self.cache_full) {
            // Out of room, after many levels or much that kept changing: start afresh.
            self.shapes.clear();
            self.checks.clear();
            self.places.clear();
            // What was read ahead names places in the buffers that are gone.
            for (_, ahead) in &mut self.ahead {
                if matches!(ahead, Ahead::Cached(..)) {
                    *ahead = Ahead::Late;
                }
            }
            self.contents.clear();
            self.seen.clear();
            if let Some(renderer) = &mut self.renderer {
                renderer.cache_reset();
            }
            self.frame_emptied = true;
            eprintln!("[gx] the display list cache was full and has been emptied (frame {})", self.frames);
            let mut most: Vec<_> = self.churn.iter().map(|(list, churn)| (*list, *churn)).collect();
            most.sort_by_key(|(_, churn)| std::cmp::Reverse(churn.vertices));
            let total: u64 = most.iter().map(|(_, churn)| churn.vertices).sum();
            eprintln!("[gx]   {} lists took {total} vertices of room; the most:", most.len());
            for ((list, _), churn) in most.into_iter().take(8) {
                eprintln!(
                    "[gx]   list {list:016x}: {} vertices of room in {} builds, left out {} times",
                    churn.vertices, churn.builds, churn.outs
                );
            }
            self.churn.clear();
        }
        let rect = self.copy_source();
        self.trace_copy("display copy", rect, 0, copy, draws_before);
        // Bit 11 clears the framebuffer after the copy. With every channel enabled that is a
        // plain clear before the next frame; otherwise only the enabled channels are reset.
        self.clear = None;
        if copy & (1 << 11) != 0 {
            if self.write_masks() == (true, true, true) {
                self.clear = Some(self.clear_color());
            } else {
                self.queue_clear(rect);
            }
        }
        let s = std::mem::take(&mut self.stats);
        if self.frames <= 5 || self.frames % 600 == 0 {
            eprintln!(
                "[gx] frame {}: {} draws, {} vertices, {} display lists, {} BP / {} CP / {} XF loads",
                self.frames, s.draws, s.vertices, s.display_lists, s.bp_loads, s.cp_loads, s.xf_loads
            );
        }
    }

    /// Decodes what the guest has written to the pipe, up to the breakpoint if one is set.
    pub fn process(&mut self, c: &mut Ctx) {
        let mut data = std::mem::take(&mut self.pending);
        data.append(&mut c.fifo);
        let allowed = match self.limit {
            Some(limit) => (limit.saturating_sub(self.consumed) as usize).min(data.len()),
            None => data.len(),
        };
        let start = std::time::Instant::now();
        self.decode_started = Some(start);
        self.mem = Mem(c.mem);
        // The game has run since the last call: memory may hold something else now.
        self.epoch = self.epoch.wrapping_add(1).max(1);
        let used = self.parse(&data[..allowed], c, 0);
        self.finish_vertices();
        self.decode_time += start.elapsed();
        self.frame_decode += start.elapsed();
        self.decode_started = None;
        data.drain(..used);
        self.pending = data;
        self.consumed += used as u64;
        if self.limit == Some(self.consumed) && !self.break_signalled {
            self.break_signalled = true;
            self.raised.push(Raised::Break);
            self.note(format!("reached the breakpoint at {}, {} bytes waiting behind it", self.consumed, self.pending.len()));
        }
    }

    /// Moves a thread's pipe writes into the shared stream without decoding them. Each guest
    /// thread buffers its own writes in its `Ctx`; a thread must hand them over before another
    /// thread runs, or they would be stranded while it sleeps.
    pub fn absorb(&mut self, c: &mut Ctx) {
        self.pending.append(&mut c.fifo);
    }

    /// Records an event for the stall report, with the retrace it happened in.
    pub fn note(&mut self, what: String) {
        if self.history.len() == 96 {
            self.history.pop_front();
        }
        let retrace = crate::host::RETRACE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        self.history.push_back(format!("retrace {retrace}: {what}"));
    }

    /// Where the consumer stands in the command stream and how it got there, for diagnosing
    /// stalls.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "consumed {} bytes, {} held back, breakpoint {:?} (signalled: {}), last token {:#06x}, done requests seen {}",
            self.consumed,
            self.pending.len(),
            self.limit,
            self.break_signalled,
            self.token,
            self.finish_requests
        );
        for event in &self.history {
            text += "
    ";
            text += event;
        }
        text
    }

    /// Called as the game starts to draw a widget of its heads-up display (`widget`), or has
    /// finished drawing the display: what it writes to the pipe from here on is marked so.
    pub fn mark_hud(&mut self, c: &Ctx, widget: bool) {
        if !widget && self.hud_marked == 0 {
            return;
        }
        if widget {
            self.hud_widgets = self.hud_widgets % 0xFFFF + 1;
        }
        self.hud_marked = if widget { self.hud_widgets } else { 0 };
        self.hud_marks.push_back((self.written(c), self.hud_marked));
    }

    /// Total bytes the guest has ever written to the pipe.
    fn written(&self, c: &Ctx) -> u64 {
        self.consumed + (self.pending.len() + c.fifo.len()) as u64
    }

    /// The FIFO's guest address range (base, size), as the GX library programmed it into the
    /// processor-interface registers.
    fn fifo_range(c: &Ctx) -> (u32, u64) {
        let (base, top) = (c.r32(0xCC00_300C) & 0x03FF_FFE0, c.r32(0xCC00_3010) & 0x03FF_FFE0);
        if top > base {
            (base | 0x8000_0000, (top - base + 4) as u64)
        } else {
            (0x8000_0000, 0x4_0000)
        }
    }

    /// GXGetFifoPtrs: guest addresses of the GPU read position and the CPU write position.
    ///
    /// The stream is not bounded by the guest FIFO's size the way the hardware FIFO is (there
    /// the GPU drains while the CPU writes), so a frame can be larger than the FIFO and its
    /// addresses wrap. Each write address handed out is remembered with its exact stream
    /// position so a later breakpoint on it is unambiguous.
    pub fn fifo_ptrs(&mut self, c: &Ctx) -> (u32, u32) {
        let (base, size) = Self::fifo_range(c);
        let at = |pos: u64| base + (pos % size) as u32;
        let written = self.written(c);
        if self.marks.len() == 64 {
            self.marks.remove(0);
        }
        self.marks.push((at(written), written));
        (at(self.consumed), at(written))
    }

    /// GXEnableBreakPt: `addr` is a write position the game got from GXGetFifoPtrs. It names
    /// the earliest such position the GPU has not passed yet.
    pub fn set_breakpoint(&mut self, c: &Ctx, addr: u32) {
        let known = self.marks.iter().filter(|m| m.0 == addr && m.1 >= self.consumed).map(|m| m.1).min();
        let limit = known.unwrap_or_else(|| {
            // Not an address we handed out: assume it lies within one FIFO length behind.
            let (base, size) = Self::fifo_range(c);
            let written = self.written(c);
            let behind = (written + size - (addr.wrapping_sub(base) as u64 % size)) % size;
            written - behind.min(written)
        });
        self.limit = Some(limit);
        self.break_signalled = false;
        let written = self.written(c);
        self.note(format!(
            "breakpoint set at {addr:#010x} = position {limit} (known: {}); consumed {}, written {written}",
            known.is_some(),
            self.consumed
        ));
        if std::env::var_os("TSSM_TRACE_GX").is_some() {
            eprintln!("[gx] breakpoint {addr:#010x} -> position {limit} (consumed {}, known {})", self.consumed, known.is_some());
        }
    }

    pub fn clear_breakpoint(&mut self) {
        self.limit = None;
        self.note(format!("breakpoint cleared; consumed {}", self.consumed));
        if std::env::var_os("TSSM_TRACE_GX").is_some() {
            eprintln!("[gx] breakpoint cleared at consumed {}", self.consumed);
        }
    }
}
