//! The GameCube's texture environment (TEV): up to 16 stages, each combining a texture sample,
//! the rasterized color, constants and four color registers into one of those registers.
//! This turns the game's TEV register state into a WGSL fragment function that does the same.
//!
//! Fog is applied after the last stage. Not modelled yet: indirect textures, color-channel
//! swap tables, fog range adjustment.

use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

/// The registers that define one stage, with don't-care bits masked off.
#[derive(Clone, Copy, Hash)]
pub struct Stage {
    /// Color combiner (BP 0xC0 + 2n): inputs d, c, b, a, then bias, op, clamp, scale, dest.
    color: u32,
    /// Alpha combiner (BP 0xC1 + 2n), same layout from bit 4.
    alpha: u32,
    /// Texture map (bits 0-2), texture coordinate (3-5), texture enable (6), color channel (7-9).
    order: u32,
    /// Constant selection: color in bits 0-4, alpha in bits 5-9.
    ksel: u32,
}

#[derive(Hash)]
pub struct Config {
    stages: Vec<Stage>,
    /// Alpha test: comparison 0 in bits 0-2, comparison 1 in bits 3-5, logic in bits 6-7.
    alpha_test: u32,
    /// Fog: the curve in bits 0-2 (0 off, 2 linear, 4 exponential, 5 exponential squared, 6
    /// and 7 those two reversed), bit 3 set for an orthographic projection.
    fog: u32,
}

const UV: [&str; 8] = ["i.uv01.xy", "i.uv01.zw", "i.uv23.xy", "i.uv23.zw", "i.uv45.xy", "i.uv45.zw", "i.uv67.xy", "i.uv67.zw"];
const REGISTERS: [&str; 4] = ["prev", "r0", "r1", "r2"];
/// Fixed constants selectable in place of a constant register: 1, 7/8, ... 1/8.
const FRACTIONS: [&str; 8] = ["1.0", "0.875", "0.75", "0.625", "0.5", "0.375", "0.25", "0.125"];

/// A color combiner input (4-bit code) as a vec3 expression.
fn color_input(code: u32) -> &'static str {
    [
        "prev.rgb", "vec3<f32>(prev.a)", "r0.rgb", "vec3<f32>(r0.a)", "r1.rgb", "vec3<f32>(r1.a)", "r2.rgb",
        "vec3<f32>(r2.a)", "tex.rgb", "vec3<f32>(tex.a)", "ras.rgb", "vec3<f32>(ras.a)", "vec3<f32>(1.0)",
        "vec3<f32>(0.5)", "kc", "vec3<f32>(0.0)",
    ][code as usize & 15]
}

/// An alpha combiner input (3-bit code) as a scalar expression.
fn alpha_input(code: u32) -> &'static str {
    ["prev.a", "r0.a", "r1.a", "r2.a", "tex.a", "ras.a", "ka", "0.0"][code as usize & 7]
}

fn konst_color(sel: u32) -> String {
    match sel {
        0..=7 => format!("vec3<f32>({})", FRACTIONS[sel as usize]),
        12..=15 => format!("p.konst[{}].rgb", sel - 12),
        16..=31 => format!("vec3<f32>(p.konst[{}].{})", sel & 3, ["r", "g", "b", "a"][(sel as usize - 16) / 4]),
        _ => "vec3<f32>(0.0)".into(),
    }
}

fn konst_alpha(sel: u32) -> String {
    match sel {
        0..=7 => FRACTIONS[sel as usize].into(),
        16..=31 => format!("p.konst[{}].{}", sel & 3, ["r", "g", "b", "a"][(sel as usize - 16) / 4]),
        _ => "0.0".into(),
    }
}

/// One alpha-test comparison of the 8-bit alpha `a8` against a reference.
fn alpha_compare(code: u32, reference: &str) -> String {
    match code & 7 {
        0 => "false".into(),
        7 => "true".into(),
        c => format!("(a8 {} {reference})", ["", "<", "==", "<=", ">", "!=", ">="][c as usize]),
    }
}

impl Config {
    /// Reads the TEV state out of the BP register file.
    pub fn from_registers(bp: &[u32; 256]) -> Config {
        let count = ((bp[0x00] >> 10) & 15) as usize + 1;
        let stages = (0..count)
            .map(|i| {
                // Stages share order and constant-select registers in pairs.
                let shift = 12 * (i % 2) as u32;
                Stage {
                    color: bp[0xC0 + i * 2] & 0xFF_FFFF,
                    alpha: bp[0xC1 + i * 2] & 0xFF_FFF0,
                    order: (bp[0x28 + i / 2] >> shift) & 0x3FF,
                    ksel: (bp[0xF6 + i / 2] >> if i % 2 == 0 { 4 } else { 14 }) & 0x3FF,
                }
            })
            .collect();
        // TSSM_NO_FOG=1 draws without fog, to compare.
        static NO_FOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let fog = match NO_FOG.get_or_init(|| std::env::var_os("TSSM_NO_FOG").is_some()) {
            true => 0,
            false => ((bp[0xF1] >> 21) & 7) | ((bp[0xF1] >> 20) & 1) << 3,
        };
        Config { stages, alpha_test: (bp[0xF3] >> 16) & 0xFF, fog }
    }

    /// A single stage that outputs the rasterized color untouched, with no alpha test.
    pub fn passthrough() -> Config {
        Config { stages: vec![Stage { color: 0x08_FFFA, alpha: 0x08_FFD0, order: 0, ksel: 0 }], alpha_test: 0x3F, fog: 0 }
    }

    /// Identifies the generated shader.
    pub fn key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    /// The (texture map, texture coordinate) pairs the stages sample.
    pub fn texture_uses(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.stages.iter().filter(|s| s.order & 0x40 != 0).map(|s| ((s.order & 7) as usize, ((s.order >> 3) & 7) as usize))
    }

    /// The fragment function. It relies on the declarations in the renderer's shader header:
    /// the `p` parameter block, textures `t0..t7` with samplers `s0..s7`, and `VOut`.
    pub fn fragment(&self) -> String {
        let mut s = String::from(
            "@fragment fn fs(i: VOut) -> @location(0) vec4<f32> {\n    var prev = p.reg[0];\n    var r0 = p.reg[1];\n    var r1 = p.reg[2];\n    var r2 = p.reg[3];\n",
        );
        for stage in &self.stages {
            let (map, coord, channel) = (stage.order & 7, (stage.order >> 3) & 7, (stage.order >> 7) & 7);
            let tex = match stage.order & 0x40 {
                0 => "vec4<f32>(1.0)".to_string(),
                _ => format!("textureSample(t{map}, s{map}, {})", UV[coord as usize]),
            };
            let ras = ["i.c0", "i.c1"].get(channel as usize).copied().unwrap_or("vec4<f32>(0.0)");
            writeln!(s, "    {{\n        let tex = {tex};\n        let ras = {ras};").unwrap();
            writeln!(s, "        let kc = {};\n        let ka = {};", konst_color(stage.ksel & 31), konst_alpha((stage.ksel >> 5) & 31)).unwrap();

            let (c, a) = (stage.color, stage.alpha);
            let ci = |shift: u32| color_input(c >> shift);
            let ai = |shift: u32| alpha_input(a >> shift);
            writeln!(s, "        let ca = {}; let cb = {}; let cc = {}; let cd = {};", ci(12), ci(8), ci(4), ci(0)).unwrap();
            writeln!(s, "        let aa = {}; let ab = {}; let ac = {}; let ad = {};", ai(13), ai(10), ai(7), ai(4)).unwrap();
            writeln!(s, "        let col = {};", combine(c, true)).unwrap();
            writeln!(s, "        let alp = {};", combine(a, false)).unwrap();

            let limits = |env: u32| if env & (1 << 19) != 0 { ("0.0", "1.0") } else { ("-4.0", "4.0") };
            let ((clo, chi), (alo, ahi)) = (limits(c), limits(a));
            let (cdest, adest) = (REGISTERS[((c >> 22) & 3) as usize], REGISTERS[((a >> 22) & 3) as usize]);
            writeln!(s, "        {cdest} = vec4<f32>(clamp(col, vec3<f32>({clo}), vec3<f32>({chi})), {cdest}.a);").unwrap();
            writeln!(s, "        {adest}.a = clamp(alp, {alo}, {ahi});\n    }}").unwrap();
        }

        let (comp0, comp1, logic) = (self.alpha_test & 7, (self.alpha_test >> 3) & 7, (self.alpha_test >> 6) & 3);
        if (comp0, comp1) != (7, 7) || logic >= 2 {
            let (first, second) = (alpha_compare(comp0, "p.alpha_ref.x"), alpha_compare(comp1, "p.alpha_ref.y"));
            let op = ["&&", "||", "!=", "=="][logic as usize];
            writeln!(s, "    let a8 = floor(clamp(prev.a, 0.0, 1.0) * 255.0 + 0.5);").unwrap();
            writeln!(s, "    if (!({first} {op} {second})) {{ discard; }}").unwrap();
        }
        s.push_str("    prev = clamp(prev, vec4<f32>(0.0), vec4<f32>(1.0));\n");
        if self.fog & 7 != 0 {
            // The console works out the eye-space distance from the 24-bit depth it has just
            // computed for the pixel: A / (B - (z >> shift)) under perspective, A * z in an
            // orthographic view. Past C the color fades to the fog color along the curve.
            s.push_str("    let z24 = floor(clamp(i.pos.z, 0.0, 1.0) * 16777215.0);\n");
            s.push_str(if self.fog & 8 == 0 {
                "    let eye = p.fog.x * 16777216.0 / (p.fog.y - floor(z24 / p.fog.z));\n"
            } else {
                "    let eye = p.fog.x * z24 / 16777216.0;\n"
            });
            s.push_str("    let linear = clamp(eye - p.fog.w, 0.0, 1.0);\n");
            let amount = match self.fog & 7 {
                4 => "1.0 - exp2(-8.0 * linear)",
                5 => "1.0 - exp2(-8.0 * linear * linear)",
                6 => "exp2(-8.0 * (1.0 - linear))",
                7 => "exp2(-8.0 * (1.0 - linear) * (1.0 - linear))",
                _ => "linear",
            };
            writeln!(s, "    prev = vec4<f32>(mix(prev.rgb, p.fog_color.rgb, {amount}), prev.a);").unwrap();
        }
        s.push_str("    return prev;\n}\n");
        s
    }
}

/// The combine expression for one stage's color (`ca`..`cd`) or alpha (`aa`..`ad`) inputs.
/// Bits 16-17 are the bias, 18 the operation, 20-21 the scale; bias 3 selects compare mode.
fn combine(env: u32, color: bool) -> String {
    let (bias, subtract, scale) = ((env >> 16) & 3, env & (1 << 18) != 0, (env >> 20) & 3);
    let (a, b, c, d) = if color { ("ca", "cb", "cc", "cd") } else { ("aa", "ab", "ac", "ad") };
    if bias != 3 {
        // d +/- lerp(a, b, c), plus bias, times scale
        let op = if subtract { "-" } else { "+" };
        let bias = ["", " + 0.5", " - 0.5"][bias as usize];
        let scale = ["1.0", "2.0", "4.0", "0.5"][scale as usize];
        return format!("({d} {op} mix({a}, {b}, {c}){bias}) * {scale}");
    }
    // Compare mode: d + (a OP b ? c : 0). `scale` picks which channels are compared (as an
    // 8, 16 or 24-bit number, or per channel) and `subtract` picks equality over greater-than.
    // The multi-channel comparisons always use the color inputs.
    let zero = if color { "vec3<f32>(0.0)" } else { "0.0" };
    let condition = match (scale, color) {
        (3, true) if subtract => "abs(ca - cb) < vec3<f32>(0.002)".to_string(),
        (3, true) => "ca > cb".to_string(),
        (3, false) if subtract => "abs(aa - ab) < 0.002".to_string(),
        (3, false) => "aa > ab".to_string(),
        _ => {
            let weights = ["vec3<f32>(1.0, 0.0, 0.0)", "vec3<f32>(1.0, 256.0, 0.0)", "vec3<f32>(1.0, 256.0, 65536.0)"][scale as usize];
            let value = |v: &str| format!("dot(floor({v} * 255.0 + 0.5), {weights})");
            format!("{} {} {}", value("ca"), if subtract { "==" } else { ">" }, value("cb"))
        }
    };
    format!("{d} + select({zero}, {c}, {condition})")
}
