//! Translates one PowerPC (Gekko) function into a Rust function operating on `runtime::Ctx`.
//!
//! Functions with internal control flow become a `loop { match pc { .. } }` state machine with
//! one arm per basic block; straight-line functions are emitted as plain statements.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

pub struct Func {
    pub addr: u32,
    pub size: u32,
    pub name: String,
    pub hle: Option<usize>,
}

#[derive(Default)]
pub struct Stats {
    pub insns: usize,
    pub looped: usize,
    pub bad_branches: Vec<(u32, u32)>,
    pub invalid: Vec<(u32, u32)>,
    pub fallthrough: usize,
}

fn mask(mb: u32, me: u32) -> u32 {
    let x = 0xFFFF_FFFFu32 >> mb;
    let y = 0xFFFF_FFFFu32 << (31 - me);
    if mb <= me {
        x & y
    } else {
        x | y
    }
}

fn ea_d(a: u32, d: i32) -> String {
    match (a, d) {
        (0, _) => format!("0x{:X}u32", d as u32),
        (_, 0) => format!("c.r[{a}]"),
        _ => format!("c.r[{a}].wrapping_add(0x{:X})", d as u32),
    }
}

fn ea_x(a: u32, b: u32) -> String {
    if a == 0 {
        format!("c.r[{b}]")
    } else {
        format!("c.r[{a}].wrapping_add(c.r[{b}])")
    }
}

/// A load or store at `ea`; `update` writes the effective address back to rA afterwards.
fn mem(kind: &str, reg: u32, ea: String, update: Option<u32>) -> String {
    let body = match kind {
        "lwz" => format!("c.r[{reg}] = c.r32(ea);"),
        "lbz" => format!("c.r[{reg}] = c.r8(ea) as u32;"),
        "lhz" => format!("c.r[{reg}] = c.r16(ea) as u32;"),
        "lha" => format!("c.r[{reg}] = c.r16(ea) as i16 as u32;"),
        "lwbr" => format!("c.r[{reg}] = c.r32(ea).swap_bytes();"),
        "lhbr" => format!("c.r[{reg}] = c.r16(ea).swap_bytes() as u32;"),
        "stw" => format!("c.w32(ea, c.r[{reg}]);"),
        "stb" => format!("c.w8(ea, c.r[{reg}] as u8);"),
        "sth" => format!("c.w16(ea, c.r[{reg}] as u16);"),
        "stwbr" => format!("c.w32(ea, c.r[{reg}].swap_bytes());"),
        "sthbr" => format!("c.w16(ea, (c.r[{reg}] as u16).swap_bytes());"),
        "gstw" => format!("c.gw32(ea, c.r[{reg}]);"),
        "gstb" => format!("c.gw8(ea, c.r[{reg}] as u8);"),
        "gsth" => format!("c.gw16(ea, c.r[{reg}] as u16);"),
        "lfs" | "lfd" | "stfs" | "stfd" | "gstfs" | "gstfd" | "stfiw" | "lmw" | "stmw" | "dcbz" => {
            format!("c.{kind}({reg}, ea);")
        }
        _ => unreachable!("{kind}"),
    };
    match update {
        Some(a) => format!("{{ let ea = {ea}; {body} c.r[{a}] = ea; }}"),
        None => format!("{{ let ea = {ea}; {body} }}"),
    }
}

/// Splits a conditional-branch BO/BI pair into a statement to run first and a condition
/// (`None` = branch always).
fn branch_cond(bo: u32, bi: u32) -> (&'static str, Option<String>) {
    let mut pre = "";
    let mut parts = Vec::new();
    if bo & 4 == 0 {
        pre = "c.ctr = c.ctr.wrapping_sub(1); ";
        parts.push(if bo & 2 != 0 { "c.ctr == 0".to_string() } else { "c.ctr != 0".to_string() });
    }
    if bo & 16 == 0 {
        let op = if bo & 8 != 0 { "!=" } else { "==" };
        parts.push(format!("c.cr[{}] & {} {op} 0", bi / 4, 8 >> (bi % 4)));
    }
    (pre, if parts.is_empty() { None } else { Some(parts.join(" && ")) })
}

fn crbit(b: u32) -> String {
    format!("c.crbit({}, {})", b / 4, 8 >> (b % 4))
}

struct Emitter<'a> {
    f: &'a Func,
    funcs: &'a BTreeMap<u32, Func>,
    looped: bool,
    /// Treat every store as a possible write to the GX pipe (set for GX library functions).
    pipe_all: bool,
    stats: &'a mut Stats,
}

impl Emitter<'_> {
    fn in_func(&self, t: u32) -> bool {
        t >= self.f.addr && t < self.f.addr + self.f.size
    }

    fn call(&mut self, pc: u32, t: u32) -> String {
        if self.funcs.contains_key(&t) {
            format!("c.lr = 0x{:08X}; fn_{t:08X}(c);", pc + 4)
        } else {
            self.stats.bad_branches.push((pc, t));
            format!("c.lr = 0x{:08X}; c.bad_branch(0x{pc:08X}, 0x{t:08X});", pc + 4)
        }
    }

    fn jump(&mut self, pc: u32, t: u32) -> String {
        if self.in_func(t) && t <= pc {
            // A loop iteration: count it, so that a loop which calls nothing still lets
            // interrupts in now and then.
            format!("{{ c.back_edge(); pc = 0x{t:08X}; continue; }}")
        } else if self.in_func(t) {
            format!("{{ pc = 0x{t:08X}; continue; }}")
        } else if self.funcs.contains_key(&t) {
            format!("return fn_{t:08X}(c);")
        } else {
            self.stats.bad_branches.push((pc, t));
            format!("return c.bad_branch(0x{pc:08X}, 0x{t:08X});")
        }
    }

    /// Emits one instruction. Returns the statement and whether control never continues past it.
    fn insn(&mut self, pc: u32, w: u32) -> Option<(String, bool)> {
        let op = w >> 26;
        let (d, a, b, cc) = ((w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31, (w >> 6) & 31);
        let s = d;
        let simm = (w & 0xFFFF) as i16 as i32;
        let uimm = w & 0xFFFF;
        let rc = w & 1 != 0;
        let crf = d >> 2;
        let rec = |stmt: String, reg: u32| if rc { format!("{stmt} c.rc(c.r[{reg}]);") } else { stmt };
        let ok = |s: String| Some((s, false));

        match op {
            7 => ok(format!("c.r[{d}] = (c.r[{a}] as i32).wrapping_mul({simm}) as u32;")),
            8 => ok(format!("c.r[{d}] = c.add3(!c.r[{a}], 0x{:X}, 1);", simm as u32)),
            10 => ok(format!("c.cmpu({crf}, c.r[{a}], 0x{uimm:X});")),
            11 => ok(format!("c.cmps({crf}, c.r[{a}] as i32, {simm});")),
            12 => ok(format!("c.r[{d}] = c.add3(c.r[{a}], 0x{:X}, 0);", simm as u32)),
            13 => ok(format!("c.r[{d}] = c.add3(c.r[{a}], 0x{:X}, 0); c.rc(c.r[{d}]);", simm as u32)),
            14 => ok(format!("c.r[{d}] = {};", ea_d(a, simm))),
            15 => ok(format!("c.r[{d}] = {};", ea_d(a, simm << 16))),
            16 => {
                let t = if w & 2 != 0 { (w & 0xFFFC) as i16 as i32 as u32 } else { pc.wrapping_add((w & 0xFFFC) as i16 as i32 as u32) };
                let (pre, cond) = branch_cond(d, a);
                let act = if w & 1 != 0 { self.call(pc, t) } else { self.jump(pc, t) };
                match cond {
                    Some(cond) => ok(format!("{pre}if {cond} {{ {act} }}")),
                    None => Some((format!("{pre}{act}"), w & 1 == 0)),
                }
            }
            17 => ok("c.trap(\"sc\");".into()),
            18 => {
                let li = ((w & 0x03FF_FFFC) << 6) as i32 >> 6;
                let t = if w & 2 != 0 { li as u32 } else { pc.wrapping_add(li as u32) };
                if w & 1 != 0 {
                    ok(self.call(pc, t))
                } else {
                    Some((self.jump(pc, t), true))
                }
            }
            19 => self.op19(pc, w),
            20 => ok(rec(
                format!("c.r[{a}] = (c.r[{s}].rotate_left({b}) & 0x{m:X}) | (c.r[{a}] & 0x{n:X});", m = mask(cc, (w >> 1) & 31), n = !mask(cc, (w >> 1) & 31)),
                a,
            )),
            21 => ok(rec(format!("c.r[{a}] = c.r[{s}].rotate_left({b}) & 0x{:X};", mask(cc, (w >> 1) & 31)), a)),
            23 => ok(rec(format!("c.r[{a}] = c.r[{s}].rotate_left(c.r[{b}] & 31) & 0x{:X};", mask(cc, (w >> 1) & 31)), a)),
            24 if w == 0x6000_0000 => ok(String::new()),
            24 => ok(format!("c.r[{a}] = c.r[{s}] | 0x{uimm:X};")),
            25 => ok(format!("c.r[{a}] = c.r[{s}] | 0x{:X};", uimm << 16)),
            26 => ok(format!("c.r[{a}] = c.r[{s}] ^ 0x{uimm:X};")),
            27 => ok(format!("c.r[{a}] = c.r[{s}] ^ 0x{:X};", uimm << 16)),
            28 => ok(format!("c.r[{a}] = c.r[{s}] & 0x{uimm:X}; c.rc(c.r[{a}]);")),
            29 => ok(format!("c.r[{a}] = c.r[{s}] & 0x{:X}; c.rc(c.r[{a}]);", uimm << 16)),
            31 => self.op31(w).map(|s| (s, false)),
            32..=55 => {
                const KINDS: [&str; 12] = ["lwz", "lbz", "stw", "stb", "lhz", "lha", "sth", "", "lfs", "lfd", "stfs", "stfd"];
                let kind = match op {
                    46 => "lmw",
                    47 => "stmw",
                    _ => KINDS[((op - 32) / 2) as usize],
                };
                let update = (op & 1 != 0 && op < 46 || op >= 48 && op & 1 != 0).then_some(a);
                // The compiler addresses the GX write-gather pipe (0xCC008000) as -0x8000 off a
                // 0xCC01 base, so only stores with that displacement need the pipe check. The GX
                // library's hand-written assembly holds the pipe address in a register instead.
                let piped = format!("g{kind}");
                let may_pipe = simm == -0x8000 && a != 0 || self.pipe_all;
                let kind = if may_pipe && kind.starts_with("st") && kind != "stmw" { &piped } else { kind };
                ok(mem(kind, d, ea_d(a, simm), update))
            }
            56 | 57 | 60 | 61 => {
                let d12 = ((w & 0xFFF) << 20) as i32 >> 20;
                let name = if op < 60 { "psq_l" } else { "psq_st" };
                let call = format!("c.{name}({d}, ea, {}, {});", (w >> 15) & 1 != 0, (w >> 12) & 7);
                let upd = if op & 1 != 0 { format!(" c.r[{a}] = ea;") } else { String::new() };
                ok(format!("{{ let ea = {}; {call}{upd} }}", ea_d(a, d12)))
            }
            59 => {
                let (fa, fb, fc) = (format!("c.f[{a}]"), format!("c.f[{b}]"), format!("c.f[{cc}]"));
                let e = match (w >> 1) & 31 {
                    18 => format!("{fa} / {fb}"),
                    20 => format!("{fa} - {fb}"),
                    21 => format!("{fa} + {fb}"),
                    22 => format!("{fb}.sqrt()"),
                    24 => format!("1.0 / {fb}"),
                    25 => format!("{fa} * {fc}"),
                    28 => format!("{fa} * {fc} - {fb}"),
                    29 => format!("{fa} * {fc} + {fb}"),
                    30 => format!("-({fa} * {fc} - {fb})"),
                    31 => format!("-({fa} * {fc} + {fb})"),
                    _ => return None,
                };
                ok(format!("c.set_fs({d}, {e});"))
            }
            63 => self.op63(w).map(|s| (s, false)),
            4 => self.op4(w).map(|s| (s, false)),
            _ => None,
        }
    }

    fn op19(&mut self, pc: u32, w: u32) -> Option<(String, bool)> {
        let (d, a, b) = ((w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31);
        let lk = w & 1 != 0;
        let ok = |s: String| Some((s, false));
        let crop = |e: String| ok(format!("{{ let v = {e}; c.crset({}, {}, v); }}", d / 4, 8 >> (d % 4)));
        match (w >> 1) & 0x3FF {
            0 => ok(format!("c.cr[{}] = c.cr[{}];", d >> 2, a >> 2)),
            16 => {
                let (pre, cond) = branch_cond(d, a);
                let act = if lk {
                    format!("{{ let t = c.lr; c.lr = 0x{:08X}; c.call(t); }}", pc + 4)
                } else {
                    "return;".to_string()
                };
                match cond {
                    Some(cond) => ok(format!("{pre}if {cond} {{ {act} }}")),
                    None => Some((format!("{pre}{act}"), !lk)),
                }
            }
            528 => {
                let (_, cond) = branch_cond(d | 4, a);
                let act = if lk {
                    format!("c.lr = 0x{:08X}; c.call(c.ctr);", pc + 4)
                } else if self.looped {
                    // jump table (target inside this function) or indirect tail call
                    format!(
                        "{{ let t = c.ctr; if t.wrapping_sub(0x{:08X}) < 0x{:X} {{ pc = t; continue; }} return c.call(t); }}",
                        self.f.addr, self.f.size
                    )
                } else {
                    "return c.call(c.ctr);".to_string()
                };
                match cond {
                    Some(cond) => ok(format!("if {cond} {{ {act} }}")),
                    None => Some((act, !lk)),
                }
            }
            33 => crop(format!("!({} | {})", crbit(a), crbit(b))),
            129 => crop(format!("{} & !{}", crbit(a), crbit(b))),
            193 => crop(format!("{} ^ {}", crbit(a), crbit(b))),
            225 => crop(format!("!({} & {})", crbit(a), crbit(b))),
            257 => crop(format!("{} & {}", crbit(a), crbit(b))),
            289 => crop(format!("!({} ^ {})", crbit(a), crbit(b))),
            417 => crop(format!("{} | !{}", crbit(a), crbit(b))),
            449 => crop(format!("{} | {}", crbit(a), crbit(b))),
            50 => ok("c.trap(\"rfi\");".into()),
            150 => ok(String::new()),
            _ => None,
        }
    }

    fn op31(&mut self, w: u32) -> Option<String> {
        let (d, a, b) = ((w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31);
        let s = d;
        let rc = w & 1 != 0;
        let crf = d >> 2;
        let mut xo = (w >> 1) & 0x3FF;
        // arithmetic ops carry an OE bit in the top of the extended opcode; overflow is not tracked
        if [8, 10, 40, 104, 136, 138, 200, 202, 232, 234, 235, 266, 459, 491].contains(&(xo & 0x1FF)) {
            xo &= 0x1FF;
        }
        let (ra, rb, rs) = (format!("c.r[{a}]"), format!("c.r[{b}]"), format!("c.r[{s}]"));
        let arith = |e: String| Some(if rc { format!("c.r[{d}] = {e}; c.rc(c.r[{d}]);") } else { format!("c.r[{d}] = {e};") });
        let logic = |e: String| Some(if rc { format!("c.r[{a}] = {e}; c.rc(c.r[{a}]);") } else { format!("c.r[{a}] = {e};") });
        let pipe_all = self.pipe_all;
        let ld = |kind: &str, upd: bool| {
            // Indexed stores in pipe-aware functions get the same pipe check as the others.
            let piped = format!("g{kind}");
            let kind = if pipe_all && matches!(kind, "stw" | "stb" | "sth" | "stfs" | "stfd") { &piped } else { kind };
            Some(mem(kind, d, ea_x(a, b), upd.then_some(a)))
        };
        let spr = ((w >> 16) & 0x1F) | ((w >> 6) & 0x3E0);
        match xo {
            0 => Some(format!("c.cmps({crf}, {ra} as i32, {rb} as i32);")),
            32 => Some(format!("c.cmpu({crf}, {ra}, {rb});")),
            8 => arith(format!("c.add3(!{ra}, {rb}, 1)")),
            10 => arith(format!("c.add3({ra}, {rb}, 0)")),
            11 => arith(format!("(({ra} as u64 * {rb} as u64) >> 32) as u32")),
            75 => arith(format!("(({ra} as i32 as i64 * {rb} as i32 as i64) >> 32) as u32")),
            40 => arith(format!("{rb}.wrapping_sub({ra})")),
            104 => arith(format!("{ra}.wrapping_neg()")),
            136 => arith(format!("c.add3(!{ra}, {rb}, c.ca)")),
            138 => arith(format!("c.add3({ra}, {rb}, c.ca)")),
            200 => arith(format!("c.add3(!{ra}, 0, c.ca)")),
            202 => arith(format!("c.add3({ra}, 0, c.ca)")),
            232 => arith(format!("c.add3(!{ra}, 0xFFFFFFFF, c.ca)")),
            234 => arith(format!("c.add3({ra}, 0xFFFFFFFF, c.ca)")),
            235 => arith(format!("{ra}.wrapping_mul({rb})")),
            266 => arith(format!("{ra}.wrapping_add({rb})")),
            459 => arith(format!("{ra}.checked_div({rb}).unwrap_or(0)")),
            491 => arith(format!("({ra} as i32).checked_div({rb} as i32).unwrap_or(0) as u32")),
            19 => Some(format!("c.r[{d}] = c.get_cr();")),
            144 => Some(format!("c.set_cr(0x{:X}, {rs});", (w >> 12) & 0xFF)),
            83 => Some(format!("c.r[{d}] = c.msr;")),
            146 => Some(format!("c.msr = {rs};")),
            595 => Some(format!("c.r[{d}] = 0;")),
            339 => Some(match spr {
                1 => format!("c.r[{d}] = c.get_xer();"),
                8 => format!("c.r[{d}] = c.lr;"),
                9 => format!("c.r[{d}] = c.ctr;"),
                _ => format!("c.r[{d}] = c.spr[{spr}];"),
            }),
            467 => Some(match spr {
                1 => format!("c.set_xer({rs});"),
                8 => format!("c.lr = {rs};"),
                9 => format!("c.ctr = {rs};"),
                _ => format!("c.spr[{spr}] = {rs};"),
            }),
            371 => Some(format!("c.r[{d}] = (c.timebase() >> {}) as u32;", if spr == 269 { 32 } else { 0 })),
            24 => logic(format!("c.slw({rs}, {rb})")),
            536 => logic(format!("c.srw({rs}, {rb})")),
            792 => logic(format!("c.sraw({rs}, {rb})")),
            824 => logic(format!("c.sraw({rs}, {b})")),
            26 => logic(format!("{rs}.leading_zeros()")),
            28 => logic(format!("{rs} & {rb}")),
            60 => logic(format!("{rs} & !{rb}")),
            124 => logic(format!("!({rs} | {rb})")),
            284 => logic(format!("!({rs} ^ {rb})")),
            316 => logic(format!("{rs} ^ {rb}")),
            412 => logic(format!("{rs} | !{rb}")),
            444 => logic(format!("{rs} | {rb}")),
            476 => logic(format!("!({rs} & {rb})")),
            922 => logic(format!("{rs} as i16 as u32")),
            954 => logic(format!("{rs} as i8 as u32")),
            20 | 23 => ld("lwz", false),
            55 => ld("lwz", true),
            87 => ld("lbz", false),
            119 => ld("lbz", true),
            279 => ld("lhz", false),
            311 => ld("lhz", true),
            343 => ld("lha", false),
            375 => ld("lha", true),
            534 => ld("lwbr", false),
            790 => ld("lhbr", false),
            535 => ld("lfs", false),
            567 => ld("lfs", true),
            599 => ld("lfd", false),
            631 => ld("lfd", true),
            151 => ld("stw", false),
            183 => ld("stw", true),
            215 => ld("stb", false),
            247 => ld("stb", true),
            407 => ld("sth", false),
            439 => ld("sth", true),
            662 => ld("stwbr", false),
            918 => ld("sthbr", false),
            663 => ld("stfs", false),
            695 => ld("stfs", true),
            727 => ld("stfd", false),
            759 => ld("stfd", true),
            983 => ld("stfiw", false),
            // stwcx. always succeeds: there is no preemption inside recompiled code
            150 => Some(format!("{} c.cr[0] = 2;", mem("stw", s, ea_x(a, b), None))),
            1014 => Some(mem("dcbz", 0, ea_x(a, b), None)),
            // cache/TLB/segment management has no meaning natively
            54 | 86 | 246 | 278 | 470 | 598 | 854 | 982 | 210 | 306 => Some(String::new()),
            _ => None,
        }
    }

    fn op63(&mut self, w: u32) -> Option<String> {
        let (d, a, b, cc) = ((w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31, (w >> 6) & 31);
        let (fa, fb, fc) = (format!("c.f[{a}]"), format!("c.f[{b}]"), format!("c.f[{cc}]"));
        let set = |e: String| Some(format!("c.f[{d}] = {e};"));
        match (w >> 1) & 0x3FF {
            0 | 32 => return Some(format!("c.fcmp({}, {fa}, {fb});", d >> 2)),
            12 => return Some(format!("c.set_fs({d}, {fb});")),
            14 => return set(format!("c.fctiw({fb})")),
            15 => return set(format!("c.fctiwz({fb})")),
            40 => return set(format!("-{fb}")),
            72 => return set(fb),
            136 => return set(format!("-{fb}.abs()")),
            264 => return set(format!("{fb}.abs()")),
            583 => return set("f64::from_bits(c.fpscr as u64)".into()),
            711 => return Some(format!("c.fpscr = {fb}.to_bits() as u32;")),
            // FPSCR bit twiddling: rounding mode stays round-to-nearest
            38 | 64 | 70 | 134 => return Some(String::new()),
            _ => {}
        }
        match (w >> 1) & 31 {
            18 => set(format!("{fa} / {fb}")),
            20 => set(format!("{fa} - {fb}")),
            21 => set(format!("{fa} + {fb}")),
            22 => set(format!("{fb}.sqrt()")),
            23 => set(format!("if {fa} >= 0.0 {{ {fc} }} else {{ {fb} }}")),
            25 => set(format!("{fa} * {fc}")),
            26 => set(format!("1.0 / {fb}.sqrt()")),
            28 => set(format!("{fa} * {fc} - {fb}")),
            29 => set(format!("{fa} * {fc} + {fb}")),
            30 => set(format!("-({fa} * {fc} - {fb})")),
            31 => set(format!("-({fa} * {fc} + {fb})")),
            _ => None,
        }
    }

    /// Gekko paired-single instructions.
    fn op4(&mut self, w: u32) -> Option<String> {
        let (d, a, b, cc) = ((w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31, (w >> 6) & 31);
        let load = format!(
            "let (a0, a1, b0, b1, c0, c1) = (c.f[{a}], c.ps1[{a}], c.f[{b}], c.ps1[{b}], c.f[{cc}], c.ps1[{cc}]);"
        );
        let ps = |e0: &str, e1: &str| Some(format!("{{ {load} c.set_ps({d}, {e0}, {e1}); }}"));
        match (w >> 1) & 31 {
            10 => return ps("a0 + b1", "c1"),
            11 => return ps("c0", "a0 + b1"),
            12 => return ps("a0 * c0", "a1 * c0"),
            13 => return ps("a0 * c1", "a1 * c1"),
            14 => return ps("a0 * c0 + b0", "a1 * c0 + b1"),
            15 => return ps("a0 * c1 + b0", "a1 * c1 + b1"),
            18 => return ps("a0 / b0", "a1 / b1"),
            20 => return ps("a0 - b0", "a1 - b1"),
            21 => return ps("a0 + b0", "a1 + b1"),
            23 => return ps("if a0 >= 0.0 { c0 } else { b0 }", "if a1 >= 0.0 { c1 } else { b1 }"),
            24 => return ps("1.0 / b0", "1.0 / b1"),
            25 => return ps("a0 * c0", "a1 * c1"),
            26 => return ps("1.0 / b0.sqrt()", "1.0 / b1.sqrt()"),
            28 => return ps("a0 * c0 - b0", "a1 * c1 - b1"),
            29 => return ps("a0 * c0 + b0", "a1 * c1 + b1"),
            30 => return ps("-(a0 * c0 - b0)", "-(a1 * c1 - b1)"),
            31 => return ps("-(a0 * c0 + b0)", "-(a1 * c1 + b1)"),
            _ => {}
        }
        let xo6 = (w >> 1) & 0x3F;
        if matches!(xo6, 6 | 7 | 38 | 39) {
            let name = if xo6 & 1 == 0 { "psq_l" } else { "psq_st" };
            let upd = if xo6 >= 32 { format!(" c.r[{a}] = ea;") } else { String::new() };
            return Some(format!(
                "{{ let ea = {}; c.{name}({d}, ea, {}, {});{upd} }}",
                ea_x(a, b),
                (w >> 10) & 1 != 0,
                (w >> 7) & 7
            ));
        }
        match (w >> 1) & 0x3FF {
            0 | 32 => Some(format!("c.fcmp({}, c.f[{a}], c.f[{b}]);", d >> 2)),
            64 | 96 => Some(format!("c.fcmp({}, c.ps1[{a}], c.ps1[{b}]);", d >> 2)),
            40 => ps("-b0", "-b1"),
            72 => ps("b0", "b1"),
            136 => ps("-b0.abs()", "-b1.abs()"),
            264 => ps("b0.abs()", "b1.abs()"),
            528 => ps("a0", "b0"),
            560 => ps("a0", "b1"),
            592 => ps("a1", "b0"),
            624 => ps("a1", "b1"),
            1014 => Some(mem("dcbz", 0, ea_x(a, b), None)),
            _ => None,
        }
    }
}

/// Destination of a direct branch instruction (`b`, `bc` and their link forms).
pub fn branch_target(pc: u32, w: u32) -> Option<u32> {
    let base = if w & 2 != 0 { 0 } else { pc };
    match w >> 26 {
        18 => Some(base.wrapping_add((((w & 0x03FF_FFFC) << 6) as i32 >> 6) as u32)),
        16 => Some(base.wrapping_add((w & 0xFFFC) as i16 as i32 as u32)),
        _ => None,
    }
}

/// Addresses inside the function that begin a basic block, and whether it needs a state machine.
fn leaders(f: &Func, words: &[u32], data_targets: &BTreeSet<u32>) -> (BTreeSet<u32>, bool) {
    let end = f.addr + f.size;
    let mut set = BTreeSet::from([f.addr]);
    let mut looped = false;
    for (i, &w) in words.iter().enumerate() {
        let pc = f.addr + i as u32 * 4;
        let (op, lk) = (w >> 26, w & 1 != 0);
        if let Some(t) = branch_target(pc, w) {
            if !lk && t >= f.addr && t < end {
                set.insert(t);
                looped = true;
            }
        }
        let xo = (w >> 1) & 0x3FF;
        let indirect = op == 19 && xo == 528 && !lk;
        looped |= indirect;
        if (op == 18 && !lk || op == 19 && (xo == 16 || indirect)) && pc + 4 < end {
            set.insert(pc + 4);
        }
    }
    set.extend(data_targets.range(f.addr + 4..end));
    (set, looped)
}

pub fn emit_function(
    f: &Func,
    words: &[u32],
    funcs: &BTreeMap<u32, Func>,
    data_targets: &BTreeSet<u32>,
    stats: &mut Stats,
    out: &mut String,
) {
    let (leaders, looped) = leaders(f, words, data_targets);
    let end = f.addr + f.size;
    stats.insns += words.len();
    stats.looped += looped as usize;

    writeln!(out, "// {}", f.name).unwrap();
    writeln!(out, "pub fn fn_{:08X}(c: &mut Ctx) {{", f.addr).unwrap();
    if let Some(i) = f.hle {
        writeln!(out, "    if c.hle({i}) {{ return; }}").unwrap();
    }
    if looped {
        writeln!(out, "    let mut pc: u32 = 0x{:08X};", f.addr).unwrap();
        writeln!(out, "    loop {{ match pc {{").unwrap();
    }

    // Hand-written assembly keeps the pipe address in a register: the GX library's matrix
    // loaders, and RenderWare's GameCube vertex submitters.
    let pipe_all = f.name.trim_start_matches('_').starts_with("GX") || f.name.contains("GameCubeAsm");
    let mut e = Emitter { f, funcs, looped, pipe_all, stats };
    let mut terminated = false;
    let indent = if looped { "            " } else { "    " };
    for (i, &w) in words.iter().enumerate() {
        let pc = f.addr + i as u32 * 4;
        if looped && (leaders.contains(&pc) || terminated) {
            if i > 0 {
                if !terminated {
                    writeln!(out, "{indent}pc = 0x{pc:08X};").unwrap();
                }
                writeln!(out, "        }}").unwrap();
            }
            writeln!(out, "        0x{pc:08X} => {{").unwrap();
        } else if terminated {
            // straight-line function: nothing can reach code after an unconditional exit
            break;
        }
        let (stmt, term) = e.insn(pc, w).unwrap_or_else(|| {
            e.stats.invalid.push((pc, w));
            (format!("return c.invalid(0x{pc:08X}, 0x{w:08X});"), true)
        });
        if !stmt.is_empty() {
            writeln!(out, "{indent}{stmt}").unwrap();
        }
        terminated = term;
    }
    if !terminated {
        // the function runs off its end into whatever the linker placed next
        e.stats.fallthrough += 1;
        let stmt = e.jump(end - 4, end);
        writeln!(out, "{indent}{stmt}").unwrap();
    }
    if looped {
        writeln!(out, "        }}").unwrap();
        writeln!(out, "        _ => return c.bad_jump(0x{:08X}, pc),", f.addr).unwrap();
        writeln!(out, "    }} }}").unwrap();
    }
    writeln!(out, "}}\n").unwrap();
}
