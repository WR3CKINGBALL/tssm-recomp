//! Runtime for the recompiled game: guest CPU state, guest memory and SDK replacements.

pub mod hle;
pub mod mem;

use hle::Hle;

/// A recompiled guest function.
pub type GuestFn = fn(&mut Ctx);

/// Loop iterations between chances for the host to deliver interrupts.
const POLL_INTERVAL: u32 = 1 << 16;

/// Id of the replaceable SDK function entered most recently on any thread; a breadcrumb for
/// diagnosing hangs.
pub static LAST_HLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Guest return address of that call.
pub static LAST_HLE_CALLER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Guest CPU state plus everything recompiled code needs to reach.
pub struct Ctx {
    pub r: [u32; 32],
    /// FPRs: `f` is the double / paired-single slot 0, `ps1` is paired-single slot 1.
    pub f: [f64; 32],
    pub ps1: [f64; 32],
    /// One entry per CR field: LT=8, GT=4, EQ=2, SO=1.
    pub cr: [u8; 8],
    pub lr: u32,
    pub ctr: u32,
    /// XER carry bit (0 or 1).
    pub ca: u32,
    pub xer_rest: u32,
    pub msr: u32,
    pub fpscr: u32,
    /// Every other special-purpose register, including the GQRs at 912..920.
    pub spr: [u32; 1024],
    /// Base of the 4 GB guest address space.
    pub mem: *mut u8,
    /// Bytes the guest has written to the GX write-gather pipe and the host has not consumed.
    pub fifo: Vec<u8>,
    /// Loop iterations left before `poll_hook` runs.
    budget: u32,
    /// Called every `POLL_INTERVAL` loop iterations so the host can deliver interrupts to
    /// code that loops without calling anything. It must leave the registers as it found them.
    pub poll_hook: Option<fn(&mut Ctx)>,

    table_base: u32,
    table: Vec<Option<GuestFn>>,
    hle: Vec<Hle>,
    hle_names: &'static [&'static str],
    hle_logged: Vec<bool>,
    /// Survey mode: unimplemented SDK calls are logged once and return 0 instead of panicking.
    pub survey: bool,
    funcs: &'static [(u32, GuestFn, &'static str)],
    start: std::time::Instant,
}

impl Ctx {
    pub fn new(
        funcs: &'static [(u32, GuestFn, &'static str)],
        hle_names: &'static [&'static str],
        policy: fn(&str) -> Hle,
    ) -> Box<Ctx> {
        let table_base = funcs.first().map_or(0, |f| f.0);
        let table_end = funcs.last().map_or(0, |f| f.0 + 4);
        let mut table = vec![None; ((table_end - table_base) / 4) as usize];
        for &(addr, f, _) in funcs {
            table[((addr - table_base) / 4) as usize] = Some(f);
        }
        Box::new(Ctx {
            r: [0; 32],
            f: [0.0; 32],
            ps1: [0.0; 32],
            cr: [0; 8],
            lr: 0,
            ctr: 0,
            ca: 0,
            xer_rest: 0,
            msr: 0x2000, // FP available
            fpscr: 0,
            spr: [0; 1024],
            mem: mem::alloc_guest_memory(),
            fifo: Vec::new(),
            budget: POLL_INTERVAL,
            poll_hook: None,
            table_base,
            table,
            hle: hle_names.iter().map(|n| policy(n)).collect(),
            hle_names,
            hle_logged: vec![false; hle_names.len()],
            survey: false,
            funcs,
            start: std::time::Instant::now(),
        })
    }

    /// A context for another guest thread: fresh registers, but the same memory, function
    /// tables, clock and special-purpose registers (the quantization registers among them).
    pub fn fork(&self) -> Box<Ctx> {
        Box::new(Ctx {
            r: [0; 32],
            f: [0.0; 32],
            ps1: [0.0; 32],
            cr: [0; 8],
            lr: 0,
            ctr: 0,
            ca: 0,
            xer_rest: 0,
            msr: self.msr,
            fpscr: 0,
            spr: self.spr,
            mem: self.mem,
            fifo: Vec::new(),
            budget: POLL_INTERVAL,
            poll_hook: self.poll_hook,
            table_base: self.table_base,
            table: self.table.clone(),
            hle: self.hle.clone(),
            hle_names: self.hle_names,
            hle_logged: self.hle_logged.clone(),
            survey: self.survey,
            funcs: self.funcs,
            start: self.start,
        })
    }

    /// Copies a DOL executable's sections into guest memory and returns its entry point.
    pub fn load_dol(&mut self, dol: &[u8]) -> u32 {
        let be = |o: usize| u32::from_be_bytes(dol[o..o + 4].try_into().unwrap());
        for i in 0..18 {
            let (off, addr, size) = (be(i * 4) as usize, be(0x48 + i * 4), be(0x90 + i * 4) as usize);
            if size != 0 {
                self.slice_mut(addr, size).copy_from_slice(&dol[off..off + size]);
            }
        }
        be(0xE0)
    }

    pub fn func_by_name(&self, name: &str) -> Option<GuestFn> {
        self.funcs.iter().find(|f| f.2 == name).map(|f| f.1)
    }

    /// Symbol name of the function containing `addr`, for diagnostics.
    pub fn func_name(&self, addr: u32) -> &'static str {
        let i = self.funcs.partition_point(|f| f.0 <= addr);
        if i == 0 {
            "?"
        } else {
            self.funcs[i - 1].2
        }
    }

    // ---- memory ------------------------------------------------------------------------

    /// Copies `len` bytes out of guest RAM.
    pub fn read_bytes(&mut self, addr: u32, len: usize) -> Vec<u8> {
        self.slice_mut(addr, len).to_vec()
    }

    pub fn slice_mut(&mut self, addr: u32, len: usize) -> &mut [u8] {
        let (lo, hi) = (addr as u64, addr as u64 + len as u64);
        let ram = mem::RAM_BASE as u64..=(mem::RAM_BASE + mem::RAM_SIZE) as u64;
        assert!(ram.contains(&lo) && ram.contains(&hi), "guest range {addr:#010x}+{len:#x} is outside RAM");
        unsafe { std::slice::from_raw_parts_mut(self.mem.add(addr as usize), len) }
    }

    #[inline(always)]
    pub fn r8(&self, a: u32) -> u8 {
        unsafe { self.mem.add(a as usize).read() }
    }
    #[inline(always)]
    pub fn r16(&self, a: u32) -> u16 {
        unsafe { u16::from_be((self.mem.add(a as usize) as *const u16).read_unaligned()) }
    }
    #[inline(always)]
    pub fn r32(&self, a: u32) -> u32 {
        unsafe { u32::from_be((self.mem.add(a as usize) as *const u32).read_unaligned()) }
    }
    #[inline(always)]
    pub fn r64(&self, a: u32) -> u64 {
        unsafe { u64::from_be((self.mem.add(a as usize) as *const u64).read_unaligned()) }
    }
    #[inline(always)]
    pub fn w8(&mut self, a: u32, v: u8) {
        unsafe { self.mem.add(a as usize).write(v) }
    }
    #[inline(always)]
    pub fn w16(&mut self, a: u32, v: u16) {
        unsafe { (self.mem.add(a as usize) as *mut u16).write_unaligned(v.to_be()) }
    }
    #[inline(always)]
    pub fn w32(&mut self, a: u32, v: u32) {
        unsafe { (self.mem.add(a as usize) as *mut u32).write_unaligned(v.to_be()) }
    }
    #[inline(always)]
    pub fn w64(&mut self, a: u32, v: u64) {
        unsafe { (self.mem.add(a as usize) as *mut u64).write_unaligned(v.to_be()) }
    }

    // Stores that may target the GX write-gather pipe.
    #[inline(always)]
    pub fn gw8(&mut self, a: u32, v: u8) {
        if a == mem::WGPIPE {
            self.fifo.push(v)
        } else {
            self.w8(a, v)
        }
    }
    #[inline(always)]
    pub fn gw16(&mut self, a: u32, v: u16) {
        if a == mem::WGPIPE {
            self.fifo.extend_from_slice(&v.to_be_bytes())
        } else {
            self.w16(a, v)
        }
    }
    #[inline(always)]
    pub fn gw32(&mut self, a: u32, v: u32) {
        if a == mem::WGPIPE {
            self.fifo.extend_from_slice(&v.to_be_bytes())
        } else {
            self.w32(a, v)
        }
    }
    #[inline(always)]
    pub fn gstfs(&mut self, s: usize, ea: u32) {
        self.gw32(ea, (self.f[s] as f32).to_bits());
    }
    #[inline(always)]
    pub fn gstfd(&mut self, s: usize, ea: u32) {
        if ea == mem::WGPIPE {
            self.fifo.extend_from_slice(&self.f[s].to_bits().to_be_bytes())
        } else {
            self.stfd(s, ea)
        }
    }

    #[inline(always)]
    pub fn lmw(&mut self, first: usize, ea: u32) {
        for (i, reg) in (first..32).enumerate() {
            self.r[reg] = self.r32(ea.wrapping_add(i as u32 * 4));
        }
    }
    #[inline(always)]
    pub fn stmw(&mut self, first: usize, ea: u32) {
        for (i, reg) in (first..32).enumerate() {
            self.w32(ea.wrapping_add(i as u32 * 4), self.r[reg]);
        }
    }
    pub fn dcbz(&mut self, _reg: usize, ea: u32) {
        let line = ea & !31;
        for i in 0..4 {
            self.w64(line + i * 8, 0);
        }
    }

    // ---- integer -----------------------------------------------------------------------

    /// a + b + carry-in, recording carry-out in XER[CA].
    #[inline(always)]
    pub fn add3(&mut self, a: u32, b: u32, carry: u32) -> u32 {
        let sum = a as u64 + b as u64 + carry as u64;
        self.ca = (sum >> 32) as u32;
        sum as u32
    }
    #[inline(always)]
    pub fn slw(&self, v: u32, n: u32) -> u32 {
        if n & 32 != 0 {
            0
        } else {
            v << (n & 31)
        }
    }
    #[inline(always)]
    pub fn srw(&self, v: u32, n: u32) -> u32 {
        if n & 32 != 0 {
            0
        } else {
            v >> (n & 31)
        }
    }
    #[inline(always)]
    pub fn sraw(&mut self, v: u32, n: u32) -> u32 {
        let s = v as i32;
        if n & 32 != 0 {
            self.ca = (s < 0) as u32;
            (s >> 31) as u32
        } else {
            let n = n & 31;
            self.ca = (s < 0 && v & ((1u32 << n) - 1) != 0) as u32;
            (s >> n) as u32
        }
    }

    // ---- condition register ------------------------------------------------------------

    #[inline(always)]
    pub fn cmps(&mut self, crf: usize, a: i32, b: i32) {
        self.cr[crf] = if a < b { 8 } else if a > b { 4 } else { 2 };
    }
    #[inline(always)]
    pub fn cmpu(&mut self, crf: usize, a: u32, b: u32) {
        self.cr[crf] = if a < b { 8 } else if a > b { 4 } else { 2 };
    }
    #[inline(always)]
    pub fn fcmp(&mut self, crf: usize, a: f64, b: f64) {
        self.cr[crf] = if a < b { 8 } else if a > b { 4 } else if a == b { 2 } else { 1 };
    }
    /// CR0 update for record-form (`.`) instructions.
    #[inline(always)]
    pub fn rc(&mut self, v: u32) {
        self.cmps(0, v as i32, 0);
    }
    #[inline(always)]
    pub fn crbit(&self, crf: usize, mask: u8) -> bool {
        self.cr[crf] & mask != 0
    }
    #[inline(always)]
    pub fn crset(&mut self, crf: usize, mask: u8, v: bool) {
        self.cr[crf] = if v { self.cr[crf] | mask } else { self.cr[crf] & !mask };
    }
    pub fn get_cr(&self) -> u32 {
        self.cr.iter().fold(0, |acc, &f| (acc << 4) | f as u32)
    }
    /// mtcrf: `mask` has one bit per CR field, most significant = CR0.
    pub fn set_cr(&mut self, mask: u32, v: u32) {
        for i in 0..8 {
            if mask & (0x80 >> i) != 0 {
                self.cr[i] = ((v >> (28 - 4 * i)) & 0xF) as u8;
            }
        }
    }
    pub fn get_xer(&self) -> u32 {
        self.xer_rest | (self.ca << 29)
    }
    pub fn set_xer(&mut self, v: u32) {
        self.ca = (v >> 29) & 1;
        self.xer_rest = v & !(1 << 29);
    }

    // ---- floating point ----------------------------------------------------------------

    /// Result of a single-precision operation: rounded to f32 and mirrored into both slots.
    #[inline(always)]
    pub fn set_fs(&mut self, d: usize, v: f64) {
        let v = v as f32 as f64;
        self.f[d] = v;
        self.ps1[d] = v;
    }
    #[inline(always)]
    pub fn set_ps(&mut self, d: usize, v0: f64, v1: f64) {
        self.f[d] = v0 as f32 as f64;
        self.ps1[d] = v1 as f32 as f64;
    }
    #[inline(always)]
    pub fn lfs(&mut self, d: usize, ea: u32) {
        let v = f32::from_bits(self.r32(ea)) as f64;
        self.f[d] = v;
        self.ps1[d] = v;
    }
    #[inline(always)]
    pub fn lfd(&mut self, d: usize, ea: u32) {
        self.f[d] = f64::from_bits(self.r64(ea));
    }
    #[inline(always)]
    pub fn stfs(&mut self, s: usize, ea: u32) {
        self.w32(ea, (self.f[s] as f32).to_bits());
    }
    #[inline(always)]
    pub fn stfd(&mut self, s: usize, ea: u32) {
        self.w64(ea, self.f[s].to_bits());
    }
    #[inline(always)]
    pub fn stfiw(&mut self, s: usize, ea: u32) {
        self.w32(ea, self.f[s].to_bits() as u32);
    }
    /// Float to integer, truncating; the integer lands in the low word of the register.
    #[inline(always)]
    pub fn fctiwz(&self, v: f64) -> f64 {
        f64::from_bits(v as i32 as u32 as u64)
    }
    #[inline(always)]
    pub fn fctiw(&self, v: f64) -> f64 {
        f64::from_bits(v.round_ties_even() as i32 as u32 as u64)
    }

    /// Quantized paired-single load: element type and scale come from GQR[i].
    pub fn psq_l(&mut self, d: usize, ea: u32, single: bool, i: usize) {
        let gqr = self.spr[912 + i];
        let (ty, scale) = ((gqr >> 16) & 7, (gqr >> 24) & 0x3F);
        let (v0, size) = self.dequantize(ea, ty, scale);
        let v1 = if single { 1.0 } else { self.dequantize(ea.wrapping_add(size), ty, scale).0 };
        self.f[d] = v0;
        self.ps1[d] = v1;
    }
    pub fn psq_st(&mut self, s: usize, ea: u32, single: bool, i: usize) {
        let gqr = self.spr[912 + i];
        let (ty, scale) = (gqr & 7, (gqr >> 8) & 0x3F);
        let (b0, size) = Self::quantize(ty, scale, self.f[s]);
        let (b1, _) = Self::quantize(ty, scale, self.ps1[s]);
        let count = if single { 1 } else { 2 };
        for (i, bytes) in [b0, b1].iter().take(count).enumerate() {
            // The SDK's matrix loaders stream straight into the write-gather pipe this way.
            if ea == mem::WGPIPE {
                self.fifo.extend_from_slice(&bytes[..size]);
            } else {
                let at = ea.wrapping_add((i * size) as u32);
                self.slice_unchecked(at, size).copy_from_slice(&bytes[..size]);
            }
        }
    }
    fn dequantize(&self, ea: u32, ty: u32, scale: u32) -> (f64, u32) {
        // scale is a signed 6-bit exponent
        let k = (2.0f64).powi(-(((scale ^ 32) as i32) - 32));
        match ty {
            4 => (self.r8(ea) as f64 * k, 1),
            5 => (self.r16(ea) as f64 * k, 2),
            6 => (self.r8(ea) as i8 as f64 * k, 1),
            7 => (self.r16(ea) as i16 as f64 * k, 2),
            _ => (f32::from_bits(self.r32(ea)) as f64, 4),
        }
    }
    /// Big-endian encoding of `v` in quantization type `ty`, and its length in bytes.
    fn quantize(ty: u32, scale: u32, v: f64) -> ([u8; 4], usize) {
        let k = (2.0f64).powi(((scale ^ 32) as i32) - 32);
        let mut out = [0u8; 4];
        let size = match ty {
            4 => {
                out[0] = (v * k) as u8;
                1
            }
            5 => {
                out[..2].copy_from_slice(&((v * k) as u16).to_be_bytes());
                2
            }
            6 => {
                out[0] = (v * k) as i8 as u8;
                1
            }
            7 => {
                out[..2].copy_from_slice(&((v * k) as i16).to_be_bytes());
                2
            }
            _ => {
                out = (v as f32).to_bits().to_be_bytes();
                4
            }
        };
        (out, size)
    }

    /// Raw view of guest memory with no range check; faults like any other guest access.
    #[inline(always)]
    fn slice_unchecked(&mut self, addr: u32, len: usize) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.mem.add(addr as usize), len) }
    }

    /// Time base: ticks at 1/4 of the 162 MHz bus clock.
    pub fn timebase(&self) -> u64 {
        (self.start.elapsed().as_nanos() * 40_500_000 / 1_000_000_000) as u64
    }

    // ---- control flow ------------------------------------------------------------------

    /// Counts one loop iteration (a backward branch) in recompiled code.
    #[inline(always)]
    pub fn back_edge(&mut self) {
        self.budget -= 1;
        if self.budget == 0 {
            self.poll();
        }
    }

    #[cold]
    fn poll(&mut self) {
        self.budget = POLL_INTERVAL;
        if let Some(hook) = self.poll_hook {
            hook(self);
        }
    }

    /// Indirect call through a guest address (function pointers, virtual calls).
    #[inline]
    pub fn call(&mut self, addr: u32) {
        let slot = self.table.get((addr.wrapping_sub(self.table_base) / 4) as usize).copied().flatten();
        match slot {
            Some(f) if addr & 3 == 0 => f(self),
            _ => panic!("indirect call to {addr:#010x}, which is not a function (lr = {:#010x})", self.lr),
        }
    }

    /// Entry check for a replaceable SDK function; true means a native version handled the call.
    #[inline]
    pub fn hle(&mut self, id: usize) -> bool {
        LAST_HLE.store(id, std::sync::atomic::Ordering::Relaxed);
        LAST_HLE_CALLER.store(self.lr, std::sync::atomic::Ordering::Relaxed);
        match self.hle[id] {
            Hle::Native(f) => {
                f(self);
                true
            }
            Hle::Original => false,
            Hle::Before(f) => {
                f(self);
                false
            }
            Hle::Replace(f) => f(self),
            Hle::Unimplemented if self.survey => {
                if !std::mem::replace(&mut self.hle_logged[id], true) {
                    eprintln!("[survey] {} (first called from {})", self.hle_names[id], self.func_name(self.lr));
                }
                self.r[3] = 0;
                true
            }
            Hle::Unimplemented => panic!(
                "SDK function {} has no native implementation yet (called from {}, lr = {:#010x})",
                self.hle_names[id],
                self.func_name(self.lr),
                self.lr
            ),
        }
    }

    #[cold]
    pub fn bad_branch(&mut self, pc: u32, target: u32) {
        panic!("{pc:#010x} ({}) branches into the middle of {} at {target:#010x}", self.func_name(pc), self.func_name(target))
    }
    #[cold]
    pub fn bad_jump(&mut self, func: u32, pc: u32) {
        panic!("computed jump in {} ({func:#010x}) to {pc:#010x}, which is not a known block", self.func_name(func))
    }
    #[cold]
    pub fn invalid(&mut self, pc: u32, word: u32) {
        panic!("untranslated instruction {word:#010x} at {pc:#010x} ({})", self.func_name(pc))
    }
    #[cold]
    pub fn trap(&mut self, what: &str) {
        panic!("privileged instruction `{what}` executed; its SDK function needs a native replacement")
    }
}
