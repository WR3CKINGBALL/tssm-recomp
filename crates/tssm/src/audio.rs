//! Audio hardware beneath the SDK's AX library, which itself runs as recompiled code:
//! the audio interface (AI) that plays mixed output, the DSP task interface that AX drives
//! the mixer through, and audio RAM (ARAM) where sample data lives.
//!
//! AX works in 5 ms frames. Each frame the DSP signals that it finished the previous one
//! (the task's resume callback) and the AI raises its buffer-done interrupt (the DMA
//! callback), upon which AX sends the DSP a new command list. In between, `mix_frame` does the
//! DSP's job: it decodes and mixes every running voice from its parameter block.

use runtime::hle::Hle;
use runtime::Ctx;

use crate::host::{self, Event};

/// One AX frame, in time base ticks (40.5 MHz / 200).
pub const FRAME_TICKS: u64 = 202_500;
const ARAM_SIZE: usize = 16 << 20;
/// Where usable ARAM starts; the first 16 KB belong to the DSP.
const ARAM_BASE: u32 = 0x4000;

// DSPTaskInfo callback fields.
const TASK_INIT_CALLBACK: u32 = 0x28;
pub const TASK_RESUME_CALLBACK: u32 = 0x2C;

pub struct Audio {
    /// Guest callback for the AI buffer-done interrupt.
    pub dma_callback: u32,
    pub running: bool,
    /// Time base tick of the next audio frame.
    pub next_frame: u64,
    /// The DSP task AX registered (guest `DSPTaskInfo*`).
    pub dsp_task: u32,
    /// Output buffer AX last asked the AI to play: guest address and length in bytes.
    pub output: (u32, u32),
    aram: Vec<u8>,
    transfer_callback: u32,
    /// Guest address of AX's array of DSP parameter blocks, once looked up.
    blocks: u32,
    /// Frames mixed, and the loudest sample and most voices seen since the last trace line.
    frames: u64,
    peak: i32,
    busiest: u32,
}

impl Audio {
    pub fn new() -> Audio {
        Audio {
            dma_callback: 0,
            running: false,
            next_frame: 0,
            dsp_task: 0,
            output: (0, 0),
            aram: vec![0; ARAM_SIZE],
            transfer_callback: 0,
            blocks: 0,
            frames: 0,
            peak: 0,
            busiest: 0,
        }
    }
}

/// Samples per 5 ms frame at 32 kHz.
const FRAME_SAMPLES: usize = 160;
/// AX keeps 64 parameter blocks of 244 bytes, one per voice.
const BLOCK_COUNT: u32 = 64;
const BLOCK_SIZE: u32 = 244;

/// Offsets within a parameter block (AXPB). Fields are big-endian 16-bit unless noted.
mod pb {
    pub const SRC_TYPE: u32 = 0x08; // 0 polyphase, 1 linear, 2 none
    pub const MIXER_CONTROL: u32 = 0x0C;
    pub const RUNNING: u32 = 0x0E;
    pub const IS_STREAM: u32 = 0x10;
    pub const MIX_LEFT: u32 = 0x12; // then left delta, right, right delta
    pub const VOLUME: u32 = 0x64; // then volume delta
    pub const LOOPING: u32 = 0x6E;
    pub const FORMAT: u32 = 0x70; // 0 ADPCM, 0x0A PCM16, 0x19 PCM8
    pub const LOOP_ADDR: u32 = 0x72; // 32-bit
    pub const END_ADDR: u32 = 0x76; // 32-bit
    pub const CUR_ADDR: u32 = 0x7A; // 32-bit
    pub const COEFS: u32 = 0x7E; // 16 signed coefficients
    pub const PRED_SCALE: u32 = 0xA0; // then history yn1, yn2
    pub const RATIO: u32 = 0xA6; // 32-bit, 16.16 fixed point
    pub const FRACTION: u32 = 0xAA;
    pub const LAST_SAMPLES: u32 = 0xAC; // four, newest last
    pub const LOOP_PRED_SCALE: u32 = 0xB4; // then loop yn1, yn2
}

/// The sample source of one voice: reads and decodes from ARAM, following loops.
struct Source<'a> {
    aram: &'a [u8],
    format: u16,
    looping: bool,
    is_stream: bool,
    loop_addr: u32,
    end_addr: u32,
    cur: u32,
    coefs: [i32; 16],
    pred_scale: u16,
    history: [i32; 2],
    loop_state: (u16, i32, i32),
    running: bool,
}

impl Source<'_> {
    fn byte(&self, at: u32) -> u8 {
        self.aram.get(at as usize).copied().unwrap_or(0)
    }

    /// The next source sample. Addresses count nibbles for ADPCM, samples for PCM.
    fn next(&mut self) -> i32 {
        if !self.running {
            return 0;
        }
        let sample = match self.format {
            0x0A => {
                let at = self.cur * 2;
                i16::from_be_bytes([self.byte(at), self.byte(at + 1)]) as i32
            }
            0x19 => (self.byte(self.cur) as i8 as i32) << 8,
            _ => {
                // Each 8-byte ADPCM frame starts with a predictor/scale byte.
                if self.cur & 15 == 0 {
                    self.pred_scale = self.byte(self.cur >> 1) as u16;
                    self.cur += 2;
                }
                let byte = self.byte(self.cur >> 1);
                let nibble = if self.cur & 1 == 0 { byte >> 4 } else { byte & 15 } as i32;
                let nibble = if nibble >= 8 { nibble - 16 } else { nibble };
                let scale = 1 << (self.pred_scale & 15);
                let index = ((self.pred_scale >> 4) & 7) as usize * 2;
                let predicted = (0x400
                    + self.coefs[index] as i64 * self.history[0] as i64
                    + self.coefs[index + 1] as i64 * self.history[1] as i64)
                    >> 11;
                let value = ((scale * nibble) as i64 + predicted).clamp(-32768, 32767) as i32;
                self.history = [value, self.history[0]];
                value
            }
        };
        self.cur += 1;
        if self.cur > self.end_addr {
            if self.looping {
                self.cur = self.loop_addr;
                if self.format == 0 && !self.is_stream {
                    (self.pred_scale, self.history[0], self.history[1]) = self.loop_state;
                }
            } else {
                self.running = false;
            }
        }
        sample
    }
}

/// Mixes one voice into `out` (interleaved left, right) and writes its state back.
fn mix_voice(c: &mut Ctx, aram: &[u8], block: u32, out: &mut [i32; FRAME_SAMPLES * 2]) {
    let u = |c: &Ctx, at: u32| c.r16(block + at);
    let s = |c: &Ctx, at: u32| c.r16(block + at) as i16 as i32;
    let mut source = Source {
        aram,
        format: u(c, pb::FORMAT),
        looping: u(c, pb::LOOPING) != 0,
        is_stream: u(c, pb::IS_STREAM) != 0,
        loop_addr: c.r32(block + pb::LOOP_ADDR),
        end_addr: c.r32(block + pb::END_ADDR),
        cur: c.r32(block + pb::CUR_ADDR),
        coefs: std::array::from_fn(|k| s(c, pb::COEFS + k as u32 * 2)),
        pred_scale: u(c, pb::PRED_SCALE),
        history: [s(c, pb::PRED_SCALE + 2), s(c, pb::PRED_SCALE + 4)],
        loop_state: (u(c, pb::LOOP_PRED_SCALE), s(c, pb::LOOP_PRED_SCALE + 2), s(c, pb::LOOP_PRED_SCALE + 4)),
        running: true,
    };
    let resample = u(c, pb::SRC_TYPE) != 2;
    let ratio = c.r32(block + pb::RATIO);
    let mut fraction = u(c, pb::FRACTION) as u32;
    let (mut older, mut newer) = (s(c, pb::LAST_SAMPLES + 4), s(c, pb::LAST_SAMPLES + 6));
    // Volumes are fixed point with 0x8000 as unity; bit 2 of the mixer control ramps them.
    let (mut volume, volume_delta) = (u(c, pb::VOLUME) as i32, s(c, pb::VOLUME + 2));
    let (mut left, left_delta) = (u(c, pb::MIX_LEFT) as i32, s(c, pb::MIX_LEFT + 2));
    let (mut right, right_delta) = (u(c, pb::MIX_LEFT + 4) as i32, s(c, pb::MIX_LEFT + 6));
    let ramp = u(c, pb::MIXER_CONTROL) & 4 != 0;

    for frame in out.chunks_exact_mut(2) {
        let sample = if resample {
            // Linear interpolation between the two most recent source samples.
            // The products below can exceed 32 bits at full swing, so they are done in 64.
            let position = fraction as u64 + ratio as u64;
            for _ in 0..position >> 16 {
                older = newer;
                newer = source.next();
            }
            fraction = (position & 0xFFFF) as u32;
            older + (((newer - older) as i64 * fraction as i64) >> 16) as i32
        } else {
            source.next()
        };
        let scaled = (sample as i64 * volume as i64) >> 15;
        let mixed = |level: i32| ((scaled * level as i64) >> 15).clamp(i32::MIN as i64 / 2, i32::MAX as i64 / 2) as i32;
        frame[0] = frame[0].saturating_add(mixed(left));
        frame[1] = frame[1].saturating_add(mixed(right));
        volume = (volume + volume_delta).clamp(0, 0xFFFF);
        if ramp {
            left = (left + left_delta).clamp(0, 0xFFFF);
            right = (right + right_delta).clamp(0, 0xFFFF);
        }
    }

    c.w32(block + pb::CUR_ADDR, source.cur);
    c.w16(block + pb::PRED_SCALE, source.pred_scale);
    c.w16(block + pb::PRED_SCALE + 2, source.history[0] as u16);
    c.w16(block + pb::PRED_SCALE + 4, source.history[1] as u16);
    c.w16(block + pb::FRACTION, fraction as u16);
    c.w16(block + pb::LAST_SAMPLES + 4, older as u16);
    c.w16(block + pb::LAST_SAMPLES + 6, newer as u16);
    c.w16(block + pb::VOLUME, volume as u16);
    c.w16(block + pb::MIX_LEFT, left as u16);
    c.w16(block + pb::MIX_LEFT + 4, right as u16);
    if !source.running {
        c.w16(block + pb::RUNNING, 0);
    }
}

/// The DSP's work for one 5 ms frame: mix every running voice and queue the result.
pub fn mix_frame(c: &mut Ctx) {
    let mut blocks = host::with(|h| h.audio.blocks);
    if blocks == 0 {
        // AX exposes its parameter block array through a one-line accessor.
        let Some(get_blocks) = c.func_by_name("__AXGetPBs") else { return };
        let saved = c.r[3];
        get_blocks(c);
        blocks = std::mem::replace(&mut c.r[3], saved);
        host::with(|h| h.audio.blocks = blocks);
    }

    // The mixer needs guest memory and ARAM together, so borrow ARAM out of the host state.
    let aram = host::with(|h| std::mem::take(&mut h.audio.aram));
    let mut mixed = [0i32; FRAME_SAMPLES * 2];
    let mut voices = 0;
    for i in 0..BLOCK_COUNT {
        let block = blocks + i * BLOCK_SIZE;
        if c.r16(block + pb::RUNNING) == 1 {
            mix_voice(c, &aram, block, &mut mixed);
            voices += 1;
        }
    }
    let frames: Vec<[i16; 2]> =
        mixed.chunks_exact(2).map(|f| [f[0].clamp(-32768, 32767) as i16, f[1].clamp(-32768, 32767) as i16]).collect();
    crate::sound::push(&frames);

    let peak = mixed.iter().map(|v| v.abs()).max().unwrap_or(0);
    let trace = std::env::var_os("TSSM_TRACE_AUDIO").is_some();
    host::with(|h| {
        let a = &mut h.audio;
        a.aram = aram;
        a.frames += 1;
        a.peak = a.peak.max(peak);
        a.busiest = a.busiest.max(voices);
        if a.frames % 200 == 0 {
            if trace {
                eprintln!("[audio] {}s: up to {} voices, peak level {}", a.frames / 200, a.busiest, a.peak);
            }
            (a.peak, a.busiest) = (0, 0);
        }
    });
}

/// Copies between main memory and ARAM. `to_aram` selects the direction.
fn transfer(c: &mut Ctx, to_aram: bool, main: u32, aram: u32, len: u32) {
    let (aram, len) = (aram as usize, len as usize);
    let main_ok = main >= runtime::mem::RAM_BASE
        && main as u64 + len as u64 <= (runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE) as u64;
    if !main_ok || aram + len > ARAM_SIZE {
        eprintln!("[audio] ARAM transfer out of range: main {main:#010x}, aram {aram:#x}, {len:#x} bytes");
        return;
    }
    let memory = c.slice_mut(main, len);
    host::with(|h| {
        let stored = &mut h.audio.aram[aram..aram + len];
        if to_aram {
            stored.copy_from_slice(memory);
        } else {
            memory.copy_from_slice(stored);
        }
    });
}

/// ARStartDMA(type, mainmem_addr, aram_addr, length); type 0 writes to ARAM.
fn ar_start_dma(c: &mut Ctx) {
    let (kind, main, aram, len) = (c.r[3], c.r[4], c.r[5], c.r[6]);
    transfer(c, kind == 0, main, aram, len);
    host::with(|h| {
        if h.audio.transfer_callback != 0 {
            h.events.push_back(Event::Call(h.audio.transfer_callback, 0));
        }
    });
}

/// ARQPostRequest(request, owner, type, priority, source, dest, length, callback).
/// The transfer happens at once; the completion callback is delivered like an interrupt.
fn arq_post_request(c: &mut Ctx) {
    let (request, kind, source, dest, len, callback) = (c.r[3], c.r[5], c.r[7], c.r[8], c.r[9], c.r[10]);
    for (i, value) in [0, c.r[4], kind, c.r[6], source, dest, len, callback].into_iter().enumerate() {
        c.w32(request + i as u32 * 4, value);
    }
    if kind == 0 {
        transfer(c, true, source, dest, len);
    } else {
        transfer(c, false, dest, source, len);
    }
    if callback != 0 {
        host::with(|h| h.events.push_back(Event::Call(callback, request)));
    }
}

/// DSPAddTask(task): AX then spins until the task's init callback has run, so run it now.
fn dsp_add_task(c: &mut Ctx) {
    let task = c.r[3];
    host::with(|h| h.audio.dsp_task = task);
    let init = c.r32(task + TASK_INIT_CALLBACK);
    if init != 0 {
        c.call(init);
    }
    c.r[3] = task;
}

fn ai_start_dma(c: &mut Ctx) {
    let now = c.timebase();
    host::with(|h| {
        h.audio.running = true;
        h.audio.next_frame = now + FRAME_TICKS;
    });
}

/// Swaps a guest callback pointer and returns the previous one in r3.
fn swap(c: &mut Ctx, slot: fn(&mut Audio) -> &mut u32) {
    let new = c.r[3];
    c.r[3] = host::with(|h| std::mem::replace(slot(&mut h.audio), new));
}

pub fn policy(name: &str) -> Option<Hle> {
    Some(match name {
        // Audio interface.
        "AIInit" | "AISetDSPSampleRate" | "AISetStreamPlayState" | "AISetStreamVolLeft" | "AISetStreamVolRight" => {
            Hle::Native(|_| {})
        }
        "AICheckInit" => Hle::Native(|c| c.r[3] = 1),
        // 0 is the 32 kHz setting; streaming audio is off.
        "AIGetDSPSampleRate" | "AIGetStreamPlayState" | "AIGetStreamSampleRate" | "AIGetStreamVolLeft"
        | "AIGetStreamVolRight" => Hle::Native(|c| c.r[3] = 0),
        "AIRegisterDMACallback" => Hle::Native(|c| swap(c, |a| &mut a.dma_callback)),
        "AIInitDMA" => Hle::Native(|c| {
            let output = (c.r[3], c.r[4]);
            host::with(|h| h.audio.output = output);
        }),
        "AIStartDMA" => Hle::Native(ai_start_dma),
        "AIStopDMA" => Hle::Native(|_| host::with(|h| h.audio.running = false)),

        // DSP: always idle and ready for mail.
        "DSPInit" | "DSPCancelTask" => Hle::Native(|_| {}),
        "DSPCheckInit" => Hle::Native(|c| c.r[3] = 1),
        "DSPCheckMailToDSP" | "DSPCheckMailFromDSP" | "DSPReadMailFromDSP" => Hle::Native(|c| c.r[3] = 0),
        "DSPSendMailToDSP" | "DSPAssertTask" => Hle::Native(|_| {}),
        "DSPAddTask" => Hle::Native(dsp_add_task),

        // Audio RAM.
        "ARInit" | "ARGetBaseAddress" => Hle::Native(|c| c.r[3] = ARAM_BASE),
        "ARCheckInit" => Hle::Native(|c| c.r[3] = 1),
        "ARGetDMAStatus" => Hle::Native(|c| c.r[3] = 0),
        "ARRegisterDMACallback" => Hle::Native(|c| swap(c, |a| &mut a.transfer_callback)),
        "ARStartDMA" => Hle::Native(ar_start_dma),
        "ARQInit" => Hle::Native(|_| {}),
        "ARQPostRequest" => Hle::Native(arq_post_request),

        // The AX mixer library and its effects are plain software on top of the above.
        _ if ["AX", "__AX", "MIX", "__MIX"].iter().any(|p| name.starts_with(p)) => Hle::Original,
        _ => return None,
    })
}
