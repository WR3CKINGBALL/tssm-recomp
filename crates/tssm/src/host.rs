//! Host-side state behind the native SDK replacements, and the event pump that stands in for
//! hardware interrupts.
//!
//! The SDK delivers disc completions, timer alarms, audio frames and the vertical retrace from
//! interrupt handlers. Here they are delivered at safe points: whenever the running guest
//! thread re-enables interrupts or polls a device, and from the scheduler's idle loop when
//! every thread is blocked. Handlers run on whichever guest thread is current, as on hardware.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use runtime::Ctx;

use crate::audio::{self, Audio};
use crate::dvd::Dvd;
use crate::gx::{Gx, Raised};
use crate::render::{Renderer, WindowSurface};
use crate::sched;

/// Time base ticks per second.
pub const TICKS_PER_SECOND: u64 = 40_500_000;

/// What the console's clock read when the game started, in ticks since the start of 2000.
/// The guest's time is this plus the time base, which counts from the start of the game;
/// everything timed on the host side (alarms, retraces, audio) uses the time base alone.
pub fn clock_base() -> u64 {
    static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| crate::card::now() * TICKS_PER_SECOND)
}
/// One NTSC field (59.94 Hz), in time base ticks: the console's retrace.
const FIELD_TICKS: u64 = 675_675;

/// The display's refresh rate in millihertz, once the window knows it; 0 before.
static DISPLAY_MILLIHERTZ: AtomicU32 = AtomicU32::new(0);

pub fn set_display_rate(millihertz: u32) {
    DISPLAY_MILLIHERTZ.store(millihertz, Ordering::Relaxed);
}

/// How often the retrace comes, which is what the game shows its frames by.
#[derive(Clone, Copy, PartialEq)]
enum FrameRate {
    /// The console's 59.94 a second.
    Console,
    /// The display's refresh rate.
    Display,
    /// This many a second.
    Fixed(f64),
}

/// The frame rate in use: read from TSSM_FPS or config/fps.txt at first, then whatever the
/// settings screen sets (options.rs).
static RATE: std::sync::Mutex<Option<FrameRate>> = std::sync::Mutex::new(None);

fn frame_rate() -> FrameRate {
    *RATE.lock().unwrap().get_or_insert_with(read_frame_rate)
}

/// TSSM_FPS, or else config/fps.txt: `display`, `60` (the console's rate) or a number from
/// 20 to 1000. Without either: the display's rate, or the console's without a window.
fn read_frame_rate() -> FrameRate {
    let default = FrameRate::Display;
    let Some(text) = std::env::var("TSSM_FPS").ok().or_else(|| std::fs::read_to_string("config/fps.txt").ok()) else {
        return default;
    };
    let word = text.lines().map(|line| line.split('#').next().unwrap().trim()).find(|line| !line.is_empty()).unwrap_or("");
    match word.parse::<f64>() {
        _ if word.eq_ignore_ascii_case("display") => FrameRate::Display,
        Ok(rate) if rate == 60.0 => FrameRate::Console,
        Ok(rate) if (20.0..=1000.0).contains(&rate) => FrameRate::Fixed(rate),
        _ => {
            eprintln!("[vi] cannot understand the frame rate `{word}`");
            default
        }
    }
}

/// Sets the frame rate from now on (60 is the console's rate).
pub fn set_frame_rate(rate: f64) {
    *RATE.lock().unwrap() = Some(if rate == 60.0 { FrameRate::Console } else { FrameRate::Fixed(rate.clamp(20.0, 1000.0)) });
}

/// The frame rate in use, in frames a second (the display's, when that is what is set).
pub fn frame_rate_now() -> f64 {
    match frame_rate() {
        FrameRate::Console => 60.0,
        FrameRate::Display => match DISPLAY_MILLIHERTZ.load(Ordering::Relaxed) {
            0 => 60.0,
            millihertz => millihertz as f64 / 1000.0,
        },
        FrameRate::Fixed(rate) => rate,
    }
}

/// Whether the game may show frames at another rate than the console's.
pub fn frame_rate_unlocked() -> bool {
    frame_rate() != FrameRate::Console
}

/// The time from one retrace to the next, in time base ticks. The game steps by the time
/// that has passed, so it runs at the same speed whatever this is. Movies keep the
/// console's rate: their player counts retraces.
fn retrace_ticks() -> u64 {
    let rate = match frame_rate() {
        _ if crate::menu::in_movie() => return FIELD_TICKS,
        FrameRate::Console => return FIELD_TICKS,
        FrameRate::Display => match DISPLAY_MILLIHERTZ.load(Ordering::Relaxed) {
            0 => return FIELD_TICKS,
            millihertz => millihertz as f64 / 1000.0,
        },
        FrameRate::Fixed(rate) => rate,
    };
    (TICKS_PER_SECOND as f64 / rate) as u64
}

/// The time from one retrace to the next.
pub fn retrace_period() -> Duration {
    Duration::from_nanos(retrace_ticks() * 1_000_000_000 / TICKS_PER_SECOND)
}

pub enum Event {
    /// An asynchronous disc command finished: `callback(result, block)`.
    DvdDone { block: u32, callback: u32, result: i32 },
    /// The GPU reached a draw-sync token: `token_callback(token)`.
    GxToken(u16),
    /// The GPU stopped at the FIFO breakpoint: `break_callback()`.
    GxBreak,
    /// The GPU finished everything up to a draw-done command: `finish_callback()`.
    GxFinish,
    /// Call a guest completion callback with one argument.
    Call(u32, u32),
    /// A memory card operation finished: `callback(chan, result)`, if there is a callback.
    CardDone { chan: u32, callback: u32, result: i32 },
}

/// The driving levels ask RenderWare to show each frame for at least two retraces
/// (`RwGameCubeSetMinRetraceCount`), which holds them to 30 frames a second: they were too
/// much for the console. Here that minimum (`_RwDlRetraceMinCount`) is put back to one
/// every retrace, so they run at 60 like the other levels; the game steps by the time that
/// has passed, so its speed does not change. TSSM_ORIGINAL_FPS=1 leaves the game's cap alone.
fn uncap_frame_rate(c: &mut Ctx) {
    const RETRACE_MIN_COUNT: u32 = 0x804B_0889;
    static KEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*KEEP.get_or_init(|| std::env::var_os("TSSM_ORIGINAL_FPS").is_some()) && c.r8(RETRACE_MIN_COUNT) > 1 {
        c.w8(RETRACE_MIN_COUNT, 1);
    }
}

/// Runs the GX consumer and queues whatever interrupts the GPU would have raised.
pub fn gx_process(c: &mut Ctx) {
    with(|h| {
        h.gx.process(c);
        for raised in h.gx.raised.drain(..) {
            h.events.push_back(match raised {
                Raised::Token(token) => Event::GxToken(token),
                Raised::Break => Event::GxBreak,
                Raised::Finish => Event::GxFinish,
            });
        }
    });
}

pub struct Alarm {
    /// Guest `OSAlarm*`.
    pub ptr: u32,
    /// Time base tick at which it fires.
    pub due: u64,
    /// Re-arm interval in ticks; 0 for one-shot.
    pub period: u64,
    pub handler: u32,
}

pub struct Host {
    pub dvd: Dvd,
    pub gx: Gx,
    pub audio: Audio,
    /// Video interface: retrace counter, guest retrace callbacks, and when the next one is due.
    pub retraces: u32,
    pub pre_retrace: u32,
    pub post_retrace: u32,
    next_retrace: u64,
    /// The time the retraces so far stand for, in ticks (see `RETRACE_COUNT`).
    retrace_time: u64,
    /// The display buffer (XFB) the game last selected, and its size in pixels.
    pub xfb: u32,
    pub display: (u32, u32),
    /// True while the picture comes from pixels the game wrote into the display buffer itself
    /// (its movie player does) rather than from a GPU frame.
    xfb_direct: bool,
    /// Sampled hash of each display buffer at the last retrace, to notice such writes.
    xfb_hashes: HashMap<u32, u64>,
    /// Guest callbacks for GPU interrupts.
    pub token_callback: u32,
    pub break_callback: u32,
    pub finish_callback: u32,
    pub events: VecDeque<Event>,
    pub alarms: Vec<Alarm>,
    /// Interrupt enable of the running guest thread; the scheduler swaps it on a switch.
    pub interrupts_enabled: bool,
    in_handler: bool,
    /// Guest memory set aside for structures the host owns (main thread, context).
    pub scratch: u32,
}

/// Least size of the block at the top of memory for host-owned guest structures: the main
/// thread at 0, an interrupt context at 0x400, view windows at 0x800 (aspect.rs).
pub const SCRATCH_SIZE: u32 = 0x1000;

/// Retraces delivered so far, counted at the console's rate whatever rate they come at (as
/// many as there are, at 60 a second); readable from outside the guest threads. Test scripts
/// and logs count in these. The game itself is given the true count (`Host::retraces`).
pub static RETRACE_COUNT: AtomicU32 = AtomicU32::new(0);
/// GPU frames presented so far, as of the last retrace.
pub static FRAME_COUNT: AtomicU32 = AtomicU32::new(0);

/// Nanoseconds slept waiting for a timed event, and spent mixing sound, since the last
/// report (TSSM_TRACE_PERF=1).
static PERF_SLEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PERF_SOUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Nanoseconds slept since the last frame was shown, for the report of slow frames (gx.rs).
pub static FRAME_SLEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Shared by every guest thread; the scheduler lets only one of them run at a time.
static HOST: Mutex<Option<Host>> = Mutex::new(None);

/// `window` is where frames are shown. Without one the game runs unseen, rendering offscreen
/// only if screenshots were requested.
pub fn init(disc: &Path, c: &mut Ctx, window: Option<WindowSurface>) {
    let mut gx = Gx::new();
    if window.is_some() || std::env::var_os("TSSM_SCREENSHOT").is_some() {
        gx.renderer = Some(Renderer::new(window));
    }
    let dvd = Dvd::open(disc);
    // The first 0x20 bytes of the disc header (game ID and friends) live at the bottom of RAM.
    let boot = std::fs::read(disc.join("sys/boot.bin")).expect("cannot read sys/boot.bin");
    c.slice_mut(0x8000_0000, 0x20).copy_from_slice(&boot[..0x20]);
    *HOST.lock().unwrap() = Some(Host {
        dvd,
        gx,
        audio: Audio::new(),
        retraces: 0,
        pre_retrace: 0,
        post_retrace: 0,
        next_retrace: c.timebase() + FIELD_TICKS,
        retrace_time: 0,
        xfb: 0,
        display: (640, 480),
        xfb_direct: false,
        xfb_hashes: HashMap::new(),
        token_callback: 0,
        break_callback: 0,
        finish_callback: 0,
        events: VecDeque::new(),
        alarms: Vec::new(),
        interrupts_enabled: true,
        in_handler: false,
        scratch: 0,
    });
    sched::init(0);
}

/// Runs `f` with the host state. Must not be held across a call into guest code, and must
/// not be nested: only the running guest thread ever touches the state.
pub fn with<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    thread_local! {
        static INSIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    // Nesting on one thread would deadlock on the mutex; fail loudly instead.
    assert!(!INSIDE.replace(true), "host state re-entered");
    // The lock is only ever contended by the watchdog's brief diagnostic reads.
    let mut guard = HOST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = f(guard.as_mut().expect("host state used before init"));
    INSIDE.set(false);
    result
}

/// Interrupt state for diagnostics, readable from outside the guest threads. `None` if a
/// guest thread is using the host state at this instant.
pub fn interrupt_flags() -> Option<(bool, bool, usize)> {
    let guard = HOST.try_lock().ok()?;
    guard.as_ref().map(|h| (h.interrupts_enabled, h.in_handler, h.events.len()))
}

/// GPU consumer state and registered GPU callbacks for diagnostics, or `None` if a guest
/// thread is using the host state at this instant.
pub fn gpu_state() -> Option<String> {
    let guard = HOST.try_lock().ok()?;
    guard.as_ref().map(|h| {
        format!(
            "{}; callbacks: done {:#010x}, break {:#010x}, token {:#010x}",
            h.gx.describe(),
            h.finish_callback,
            h.break_callback,
            h.token_callback
        )
    })
}

/// Whether an interrupt handler is running; handlers must not block or switch threads.
pub fn in_handler() -> bool {
    with(|h| h.in_handler)
}

enum Work {
    Event(Event),
    /// `field` is the count at the console's rate, and `new_field` whether it has moved on.
    Retrace { pre: u32, post: u32, count: u32, field: u32, new_field: bool },
    /// A 5 ms audio frame is due: the DSP task to resume and the AI callback to raise.
    AudioFrame { task: u32, callback: u32 },
    Alarm { ptr: u32, handler: u32 },
}

/// The next timed event's due tick after catching up: one period on, but never more than a
/// few periods in the past, so a long stall does not replay every missed event.
fn advance(due: u64, period: u64, now: u64) -> u64 {
    (due + period).max(now.saturating_sub(period * 4))
}

/// Delivers every pending completion and due timed event on the calling thread, without
/// reconsidering which thread should run. Clobbers volatile guest registers.
fn deliver(c: &mut Ctx) {
    let entered = with(|h| {
        let ok = h.interrupts_enabled && !h.in_handler;
        h.in_handler |= ok;
        ok
    });
    if !entered {
        return;
    }
    let now = c.timebase();
    loop {
        let work = with(|h| {
            if let Some(e) = h.events.pop_front() {
                return Some(Work::Event(e));
            }
            if now >= h.next_retrace {
                let period = retrace_ticks();
                h.next_retrace = advance(h.next_retrace, period, now);
                h.retraces += 1;
                h.retrace_time += period;
                let field = (h.retrace_time / FIELD_TICKS) as u32;
                let new_field = RETRACE_COUNT.swap(field, Ordering::Relaxed) != field;
                FRAME_COUNT.store(h.gx.frames as u32, Ordering::Relaxed);
                return Some(Work::Retrace { pre: h.pre_retrace, post: h.post_retrace, count: h.retraces, field, new_field });
            }
            if h.audio.running && h.audio.dma_callback != 0 && now >= h.audio.next_frame {
                h.audio.next_frame = advance(h.audio.next_frame, audio::FRAME_TICKS, now);
                return Some(Work::AudioFrame { task: h.audio.dsp_task, callback: h.audio.dma_callback });
            }
            let i = h.alarms.iter().position(|a| a.due <= now)?;
            let (ptr, handler) = (h.alarms[i].ptr, h.alarms[i].handler);
            if h.alarms[i].period != 0 {
                // The next tick still in the future, as the console's own alarm code works it
                // out: ticks missed while the game could not take them are dropped, not
                // replayed in a burst.
                let alarm = &mut h.alarms[i];
                alarm.due += alarm.period * ((now - alarm.due) / alarm.period + 1);
            } else {
                h.alarms.swap_remove(i);
            }
            Some(Work::Alarm { ptr, handler })
        });
        match work {
            Some(Work::Event(Event::DvdDone { block, callback, result })) => {
                c.w32(block + 0x0C, 0); // DVD_STATE_END
                if callback != 0 {
                    c.r[3] = result as u32;
                    c.r[4] = block;
                    c.call(callback);
                }
            }
            Some(Work::Event(Event::GxToken(token))) => {
                let thread = sched::current_guest();
                let callback = with(|h| {
                    h.gx.note(format!("token {token:#06x} interrupt delivered on thread {thread:#010x}"));
                    h.token_callback
                });
                if callback != 0 {
                    c.r[3] = token as u32;
                    c.call(callback);
                }
            }
            Some(Work::Event(Event::GxBreak)) => {
                let thread = sched::current_guest();
                let callback = with(|h| {
                    h.gx.note(format!("break interrupt delivered on thread {thread:#010x}"));
                    h.break_callback
                });
                if std::env::var_os("TSSM_TRACE_GX").is_some() {
                    eprintln!("[gx] break interrupt -> callback {callback:#010x}");
                }
                if callback != 0 {
                    c.call(callback);
                }
            }
            Some(Work::Event(Event::GxFinish)) => {
                let callback = with(|h| h.finish_callback);
                if callback != 0 {
                    c.call(callback);
                }
            }
            Some(Work::Event(Event::Call(function, argument))) => {
                c.r[3] = argument;
                c.call(function);
            }
            Some(Work::Event(Event::CardDone { chan, callback, result })) => {
                crate::card::completed(result);
                if callback != 0 {
                    c.r[3] = chan;
                    c.r[4] = result as u32;
                    c.call(callback);
                }
            }
            Some(Work::Retrace { pre, post, count, field, new_field }) => {
                uncap_frame_rate(c);
                // The vertical interrupt: let the GPU catch up, run the game's retrace
                // callbacks, and release anyone in VIWaitForRetrace.
                gx_process(c);
                if new_field && field % 120 == 0 {
                    let frames = with(|h| h.gx.frames);
                    let seconds = c.timebase() as f64 / TICKS_PER_SECOND as f64;
                    eprintln!("[vi] retrace {field} at {seconds:.1}s, {frames} frames presented");
                    if std::env::var_os("TSSM_TRACE_PERF").is_some() {
                        let (decode, render) =
                            with(|h| (std::mem::take(&mut h.gx.decode_time), std::mem::take(&mut h.gx.render_time)));
                        eprintln!(
                            "[perf] last 2 s: {:.0} ms decoding graphics commands, of which {:.0} ms in the renderer",
                            decode.as_secs_f64() * 1000.0,
                            render.as_secs_f64() * 1000.0
                        );
                        // The whole of the time since the last report: what was slept away
                        // waiting for a timed event, the sound mixer, the graphics decoding
                        // above, and the rest, which is the game's own code.
                        static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
                        let now = (std::time::Instant::now(), frames);
                        if let Some((then, frames_then)) = LAST.lock().unwrap().replace(now) {
                            let wall = (now.0 - then).as_secs_f64() * 1000.0;
                            let shown = (frames - frames_then).max(1) as f64;
                            let slept = PERF_SLEPT.swap(0, Ordering::Relaxed) as f64 / 1e6;
                            let sound = PERF_SOUND.swap(0, Ordering::Relaxed) as f64 / 1e6;
                            let decode = decode.as_secs_f64() * 1000.0;
                            let game = wall - slept - sound - decode;
                            eprintln!(
                                "[perf]   {:.1} frames a second; a frame: {:.2} ms game code, {:.2} ms decoding ({:.2} ms of it the renderer), {:.2} ms sound, {:.2} ms asleep",
                                shown * 1000.0 / wall,
                                game / shown,
                                decode / shown,
                                render.as_secs_f64() * 1000.0 / shown,
                                sound / shown,
                                slept / shown
                            );
                        }
                        let prof: Vec<u64> = crate::gx::PROF.iter().map(|p| p.swap(0, Ordering::Relaxed)).collect();
                        eprintln!(
                            "[perf]   of the decoding: {:.0} ms draw materials, {:.0} ms draw setup, {:.0} ms vertices; {} draws, {} vertices",
                            prof[0] as f64 / 1e6,
                            prof[1] as f64 / 1e6,
                            prof[2] as f64 / 1e6,
                            prof[3],
                            prof[4]
                        );
                        eprintln!(
                            "[perf]   of the vertices: {} from the display list cache, {} to the vertex shader afresh, {} on the CPU; {} put into the cache, which holds {}; {} draws for the GPU",
                            prof[5],
                            prof[6],
                            prof[4].saturating_sub(prof[5] + prof[6]),
                            prof[7],
                            with(|h| h.gx.cache_room_used()),
                            prof[8]
                        );
                    }
                }
                for callback in [pre, post] {
                    if callback != 0 {
                        c.r[3] = count;
                        c.call(callback);
                    }
                }
                show_display_buffer(c, field, new_field);
                sched::wake(sched::RETRACE_QUEUE);
            }
            Some(Work::AudioFrame { task, callback }) => {
                // The DSP mixes the frame it was given, reports it finished, then the AI asks
                // for the next.
                let started = std::time::Instant::now();
                audio::mix_frame(c);
                PERF_SOUND.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let resume = if task != 0 { c.r32(task + audio::TASK_RESUME_CALLBACK) } else { 0 };
                if resume != 0 {
                    c.r[3] = task;
                    c.call(resume);
                }
                c.call(callback);
            }
            Some(Work::Alarm { ptr, handler }) => {
                let context = with(|h| h.scratch) + 0x400;
                c.r[3] = ptr;
                c.r[4] = context;
                c.call(handler);
            }
            None => break,
        }
    }
    with(|h| h.in_handler = false);
}

/// Retrace numbers to save the display buffer at (TSSM_SCREENSHOT_VI=900,1200).
fn display_screenshots() -> &'static [u32] {
    static LIST: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let spec = std::env::var("TSSM_SCREENSHOT_VI").unwrap_or_default();
        spec.split(',').filter_map(|n| n.trim().parse().ok()).collect()
    })
}

/// Shows the display buffer straight from guest memory when the game has drawn into it
/// without the GPU. The buffer holds YUV 4:2:2 pixel pairs (Y0 U Y1 V).
fn show_display_buffer(c: &mut Ctx, retrace: u32, new_retrace: bool) {
    let (addr, (w, h)) = with(|h| (h.xfb, h.display));
    let len = (w * h * 2) as usize;
    let end = addr as u64 + len as u64;
    if addr < runtime::mem::RAM_BASE || end > (runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE) as u64 {
        return;
    }
    let sample_of = |c: &Ctx, addr: u32| (0..len as u32).step_by(64).fold(0u64, |acc, o| acc.rotate_left(5) ^ c.r32(addr + (o & !3)) as u64);
    // A frame the GPU copied to a display buffer is shown from the GPU and never written to
    // guest memory, so the buffer there would keep what a movie last drew into it. A movie
    // that is less tall than the screen draws only its own rows, with the game's cleared
    // frame above and below them on the console; here those rows would show the movie
    // before. So the buffer is made black wherever the GPU has copied to it.
    for copied in with(|host| std::mem::take(&mut host.gx.display_copies)) {
        if copied >= runtime::mem::RAM_BASE && copied as u64 + len as u64 <= (runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE) as u64 {
            for pair in c.slice_mut(copied, len).chunks_exact_mut(4) {
                pair.copy_from_slice(&[16, 128, 16, 128]);
            }
            let black = sample_of(c, copied);
            with(|host| host.xfb_hashes.insert(copied, black));
        }
    }
    let sample = sample_of(c, addr);
    let direct = with(|host| {
        if std::mem::take(&mut host.gx.copied_to_display) {
            host.xfb_direct = false;
        }
        if host.xfb_hashes.insert(addr, sample).is_some_and(|old| old != sample) {
            host.xfb_direct = true;
        }
        host.xfb_direct
    });
    let screenshot = new_retrace && display_screenshots().contains(&retrace);
    if !screenshot && !(direct && with(|host| host.gx.renderer.is_some())) {
        return;
    }

    let yuv = c.read_bytes(addr, len);
    let mut rgba = Vec::with_capacity(len * 2);
    for pair in yuv.chunks_exact(4) {
        let (u, v) = (pair[1] as f32 - 128.0, pair[3] as f32 - 128.0);
        for y in [pair[0], pair[2]] {
            let y = 1.164 * (y as f32 - 16.0);
            rgba.extend_from_slice(&[(y + 1.596 * v) as u8, (y - 0.813 * v - 0.391 * u) as u8, (y + 2.018 * u) as u8, 255]);
        }
    }
    if direct {
        with(|host| {
            if let Some(renderer) = &mut host.gx.renderer {
                renderer.present_image(w, h, &rgba);
            }
        });
    }
    if screenshot {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let _ = std::fs::create_dir_all("screenshots");
        let path = format!("screenshots/vi_{retrace}.png");
        match std::fs::write(&path, crate::png::encode_rgb(w, h, &rgb)) {
            Ok(()) => eprintln!("[vi] wrote {path} ({w}x{h}, direct display: {direct})"),
            Err(e) => eprintln!("[vi] cannot write {path}: {e}"),
        }
    }
}

/// Delivers pending interrupts, then lets a higher-priority thread run if a handler readied
/// one. Callers set their own return values afterwards.
pub fn pump(c: &mut Ctx) {
    deliver(c);
    sched::preempt(c);
}

/// Interrupt delivery in the middle of recompiled code (the `Ctx` poll hook). Unlike `pump`,
/// this can land anywhere, so the whole register file is saved and restored around it, as
/// an interrupt's context save would.
pub fn poll(c: &mut Ctx) {
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    pump(c);
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
}

/// Called by the scheduler when no guest thread can run: waits for the next timed event and
/// delivers it. The caller has interrupts enabled.
pub fn idle(c: &mut Ctx) {
    gx_process(c);
    let now = c.timebase();
    let next = with(|h| {
        if !h.events.is_empty() {
            return now;
        }
        let audio = if h.audio.running && h.audio.dma_callback != 0 { h.audio.next_frame } else { u64::MAX };
        let alarm = h.alarms.iter().map(|a| a.due).min().unwrap_or(u64::MAX);
        h.next_retrace.min(audio).min(alarm)
    });
    if next > now {
        let wait = Duration::from_nanos((next - now) * 1_000_000_000 / TICKS_PER_SECOND);
        let started = std::time::Instant::now();
        std::thread::sleep(wait);
        let slept = started.elapsed();
        PERF_SLEPT.fetch_add(slept.as_nanos() as u64, Ordering::Relaxed);
        FRAME_SLEPT.fetch_add(slept.as_nanos() as u64, Ordering::Relaxed);
        crate::hint::slept(slept);
    }
    deliver(c);
}
