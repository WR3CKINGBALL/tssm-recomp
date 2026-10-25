//! Native replacements for Dolphin SDK functions.
//!
//! Every SDK function listed in config/hle.txt reaches `policy` once at startup. A function
//! with no policy panics by name when first called, which is the bring-up worklist.

use runtime::hle::Hle;
use runtime::Ctx;

use crate::host::{self, Alarm, SCRATCH_SIZE};
use crate::sched;

fn nop(_: &mut Ctx) {}

fn linker_sym(name: &str) -> u32 {
    game::LINKER_SYMS.iter().find(|s| s.0 == name).unwrap_or_else(|| panic!("missing linker symbol {name}")).1
}

/// Calls a recompiled guest function with integer arguments.
fn guest_call(c: &mut Ctx, name: &str, args: &[u32]) {
    let f = c.func_by_name(name).unwrap_or_else(|| panic!("no guest function {name}"));
    c.r[3..3 + args.len()].copy_from_slice(args);
    f(c);
}

/// Reading the clock is also where a loop that waits on time lets interrupts in: games poll
/// it while expecting timer alarms and other threads to make progress underneath them.
fn os_get_time(c: &mut Ctx) {
    host::pump(c);
    let t = host::clock_base() + c.timebase();
    c.r[3] = (t >> 32) as u32;
    c.r[4] = t as u32;
}

/// OSTicksToCalendarTime(OSTime ticks, OSCalendarTime* td): ticks since the start of 2000 as a
/// date and time. The structure is ten ints: sec, min, hour, mday, mon (0-11), year, wday
/// (0 is Sunday), yday, msec, usec.
fn ticks_to_calendar_time(c: &mut Ctx) {
    let (ticks, out) = (pair(c, 3) as i64, c.r[5]);
    let per_second = host::TICKS_PER_SECOND as i64;
    let (seconds, fraction) = (ticks.div_euclid(per_second), ticks.rem_euclid(per_second));
    let (days, time) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days since 1 March 2000, the first day of a 400-year leap cycle's year.
    let since_march = days - 60;
    let (cycle, day_of_cycle) = (since_march.div_euclid(146_097), since_march.rem_euclid(146_097));
    let year_of_cycle = (day_of_cycle - day_of_cycle / 1460 + day_of_cycle / 36_524 - day_of_cycle / 146_096) / 365;
    let day_of_year = day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = (month_from_march + 2) % 12;
    let year = 2000 + cycle * 400 + year_of_cycle + (month < 2) as i64;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let before_month = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][month as usize] + (leap && month > 1) as i64;
    let microseconds = fraction * 1_000_000 / per_second;
    let fields = [
        time % 60,
        time / 60 % 60,
        time / 3600,
        day,
        month,
        year,
        (days + 6).rem_euclid(7), // 1 January 2000 was a Saturday
        before_month + day - 1,
        microseconds / 1000,
        microseconds % 1000,
    ];
    for (i, value) in fields.into_iter().enumerate() {
        c.w32(out + 4 * i as u32, value as u32);
    }
}

/// Formats a printf-style message whose format string is in register `format_reg`, with the
/// arguments after it. They follow the PowerPC calling convention for variable arguments:
/// integers and pointers in the registers up to r10 and then on the caller's stack, floating
/// point in f1 to f8.
fn guest_format(c: &Ctx, format_reg: usize) -> String {
    let text = |c: &Ctx, mut addr: u32| {
        let mut bytes = Vec::new();
        while (0x8000_0000..0x8180_0000).contains(&addr) && c.r8(addr) != 0 && bytes.len() < 1024 {
            bytes.push(c.r8(addr));
            addr += 1;
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let format = text(c, c.r[format_reg]);
    let (mut next_int, mut next_float) = (format_reg + 1, 1usize);
    let mut int = |c: &Ctx| {
        next_int += 1;
        match next_int - 1 {
            reg @ 0..=10 => c.r[reg],
            n => c.r32(c.r[1] + 8 + 4 * (n as u32 - 11)),
        }
    };
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        // Flags, width and precision are skipped: this is a log, not a layout.
        while chars.peek().is_some_and(|c| "-+ #0123456789.lh".contains(*c)) {
            chars.next();
        }
        match chars.next() {
            Some('d' | 'i') => out += &(int(c) as i32).to_string(),
            Some('u') => out += &int(c).to_string(),
            Some('x' | 'p') => out += &format!("{:x}", int(c)),
            Some('X') => out += &format!("{:X}", int(c)),
            Some('c') => out.push(int(c) as u8 as char),
            Some('s') => {
                let addr = int(c);
                out += &text(c, addr)
            }
            Some('f' | 'g' | 'e') => {
                out += &c.f.get(next_float).map_or(String::from("?"), |v| v.to_string());
                next_float += 1;
            }
            Some('%') => out.push('%'),
            Some(other) => out += &format!("%{other}"),
            None => {}
        }
    }
    out.trim_end().to_string()
}

/// OSReport(format, ...): the game's debug console, sent to the log.
fn os_report(c: &mut Ctx) {
    eprintln!("[game] {}", guest_format(c, 3));
}

/// OSPanic(file, line, format, ...): the game has given up. On a console it halts here.
fn os_panic(c: &mut Ctx) {
    panic!("the game stopped itself: {}", guest_format(c, 5));
}

/// OSInit: publish the boot globals the rest of the SDK reads from low memory, then hand the
/// free memory between the end of the program and the top of RAM to the arena.
fn os_init(c: &mut Ctx) {
    c.w32(0x8000_0028, runtime::mem::RAM_SIZE); // physical memory size
    c.w32(0x8000_002C, 3); // console type: retail
    c.w32(0x8000_00CC, 0); // video mode: NTSC
    c.w32(0x8000_00F0, runtime::mem::RAM_SIZE); // simulated memory size
    c.w32(0x8000_00F8, 162_000_000); // bus clock
    c.w32(0x8000_00FC, 486_000_000); // CPU clock
    // The arena is what a retail console gives the game, and the larger levels need all of
    // it: from just above the stack (the debugger's stack beyond it is reclaimed) up to where
    // the boot loader puts the disc's file table, at the very top of memory. The linker's
    // __ArenaLo and __ArenaHi are fallbacks that leave a megabyte unused. The file table is
    // not needed in guest memory here, so its place holds the host's scratch block.
    let top = runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE;
    let scratch = top - host::with(|h| h.dvd.table_size()).next_multiple_of(32).max(SCRATCH_SIZE);
    host::with(|h| h.scratch = scratch);
    // The main thread's OSThread structure lives at the start of the scratch block.
    sched::init(scratch);
    guest_call(c, "OSSetArenaLo", &[linker_sym("_stack_addr").next_multiple_of(32)]);
    guest_call(c, "OSSetArenaHi", &[scratch]);
}

/// Sets the interrupt-enable state, returns the previous one in r3, and delivers anything
/// that was waiting if interrupts are now on.
fn set_interrupts(c: &mut Ctx, enabled: bool) {
    let old = host::with(|h| std::mem::replace(&mut h.interrupts_enabled, enabled));
    if enabled {
        host::pump(c);
    }
    c.r[3] = old as u32;
}

/// Arms an alarm. `start`/`period` are in time base ticks; the 64-bit arguments arrive in
/// register pairs.
fn set_alarm(c: &mut Ctx, delay: u64, period: u64, handler: u32) {
    let ptr = c.r[3];
    let due = c.timebase() + delay;
    c.w32(ptr, handler);
    host::with(|h| {
        h.alarms.retain(|a| a.ptr != ptr);
        h.alarms.push(Alarm { ptr, due, period, handler });
    });
}

fn pair(c: &Ctx, hi: usize) -> u64 {
    (c.r[hi] as u64) << 32 | c.r[hi + 1] as u64
}

/// Swaps a guest callback pointer held by the host and returns the previous one in r3.
fn swap_callback(c: &mut Ctx, slot: fn(&mut host::Host) -> &mut u32) {
    let new = c.r[3];
    c.r[3] = host::with(|h| std::mem::replace(slot(h), new));
}

/// Graphics and video. The GX library itself only encodes commands into the write-gather
/// pipe, so it runs as recompiled code; only the calls that wait on the GPU are native.
fn video_policy(name: &str) -> Option<Hle> {
    Some(match name {
        "GXDrawDone" | "GXWaitDrawDone" => Hle::Native(host::gx_process),
        "GXCallDisplayList" => Hle::Replace(call_display_list),
        "GXReadDrawSync" => Hle::Native(|c| {
            host::gx_process(c);
            c.r[3] = host::with(|h| h.gx.token) as u32;
        }),
        // The FIFO is a byte stream here, not guest memory, so positions and the breakpoint
        // are the host's to track. GXGetFifoPtrs(fifo, &read, &write).
        "GXGetFifoPtrs" => Hle::Native(|c| {
            let (read, write) = host::with(|h| h.gx.fifo_ptrs(c));
            let (read_out, write_out) = (c.r[4], c.r[5]);
            c.w32(read_out, read);
            c.w32(write_out, write);
        }),
        "GXEnableBreakPt" => Hle::Native(|c| {
            let addr = c.r[3];
            host::with(|h| h.gx.set_breakpoint(c, addr));
            host::gx_process(c);
        }),
        "GXDisableBreakPt" => Hle::Native(|c| {
            host::with(|h| h.gx.clear_breakpoint());
            host::gx_process(c);
        }),
        "GXSetDrawSyncCallback" => Hle::Native(|c| swap_callback(c, |h| &mut h.token_callback)),
        "GXSetBreakPtCallback" => Hle::Native(|c| swap_callback(c, |h| &mut h.break_callback)),
        "GXSetDrawDoneCallback" => Hle::Native(|c| swap_callback(c, |h| &mut h.finish_callback)),
        _ if name.starts_with("GX") || name.starts_with("__GX") => Hle::Original,

        "VIWaitForRetrace" => Hle::Native(|c| {
            host::gx_process(c);
            sched::sleep(c, sched::RETRACE_QUEUE);
        }),
        "VISetPreRetraceCallback" => Hle::Native(|c| swap_callback(c, |h| &mut h.pre_retrace)),
        "VISetPostRetraceCallback" => Hle::Native(|c| swap_callback(c, |h| &mut h.post_retrace)),
        "VIGetRetraceCount" => Hle::Native(|c| c.r[3] = host::with(|h| h.retraces)),
        // NTSC, interlaced, no progressive-scan cable.
        "VIGetTvFormat" | "VIGetDTVStatus" => Hle::Native(|c| c.r[3] = 0),
        "VIGetNextField" => Hle::Native(|c| c.r[3] = host::with(|h| h.retraces) & 1),
        "VISetNextFrameBuffer" => Hle::Native(|c| {
            let xfb = c.r[3] | 0x8000_0000;
            host::with(|h| h.xfb = xfb);
        }),
        // VIConfigure(GXRenderModeObj*): the display buffer is fbWidth (+4) by xfbHeight (+8).
        "VIConfigure" => Hle::Native(|c| {
            let mode = c.r[3];
            let size = (c.r16(mode + 4) as u32, c.r16(mode + 8) as u32);
            if size.0 > 0 && size.1 > 0 {
                host::with(|h| h.display = size);
            }
        }),
        "VIInit" | "VIFlush" | "VISetBlack" | "VIConfigurePan" => Hle::Native(nop),
        _ => return None,
    })
}

/// The game's gGameMode and gGameState variables (this build: GGVE78 revision 1).
const GAME_MODE: u32 = 0x804B_0D54;
const GAME_STATE: u32 = 0x804B_01EC;

/// PADRead(PADStatus status[4]): controller 1 is the keyboard, ports 2-4 are empty.
/// PADStatus is 12 bytes with the error code in byte 10.
fn pad_read(c: &mut Ctx) {
    const NO_CONTROLLER: u8 = -1i8 as u8;
    let status = c.r[3];
    let retrace = host::RETRACE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    crate::input::fill_status(c.slice_mut(status, 12), retrace);
    crate::input::note_game_state(c.r32(GAME_MODE), c.r32(GAME_STATE));
    for port in 1..4 {
        let s = c.slice_mut(status + port * 12, 12);
        s.fill(0);
        s[10] = NO_CONTROLLER;
    }
    warp(c, retrace);
    upgrades(c);
    trace_player(c);
    c.r[3] = 0;
}

/// TSSM_TRACE_PLAYER=1 logs the time in seconds and the player's position at every pad
/// poll (once a frame), to compare how the game moves at different frame rates.
fn trace_player(c: &mut Ctx) {
    /// Where the game keeps the pointer to the player's entity (this build: GGVE78 revision 1).
    const PLAYER: u32 = 0x804B_04A0;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("TSSM_TRACE_PLAYER").is_some()) || c.r32(GAME_MODE) != 12 {
        return;
    }
    let valid = |p: u32| (runtime::mem::RAM_BASE..runtime::mem::RAM_BASE + runtime::mem::RAM_SIZE - 0x1000).contains(&p);
    let holder = c.r32(PLAYER);
    let entity = if valid(holder) { c.r32(holder + 2036) } else { 0 };
    let matrix = if valid(entity) { c.r32(entity + 72) } else { 0 };
    if valid(matrix) {
        let at = |o: u32| f32::from_bits(c.r32(matrix + o));
        let seconds = c.timebase() as f64 / host::TICKS_PER_SECOND as f64;
        eprintln!("[player] {seconds:.4} {:.4} {:.4} {:.4}", at(48), at(52), at(56));
    }
    // The car of the driving levels (zEntPlayerCarGetCar): position from its matrix at 0x50,
    // then its velocity at 0xA8.
    const CAR: u32 = 0x8040_6860;
    let car = |o: u32| f32::from_bits(c.r32(CAR + o));
    if car(0x80) != 0.0 || car(0x88) != 0.0 {
        let seconds = c.timebase() as f64 / host::TICKS_PER_SECOND as f64;
        eprintln!("[car] {seconds:.4} {:.4} {:.4} {:.4} v {:.4} {:.4} {:.4}",
            car(0x80), car(0x84), car(0x88), car(0xA8), car(0xAC), car(0xB0));
    }
}

/// TSSM_UPGRADES=1 gives both characters every move at its upgraded level and all the
/// health slots, for testing parts of levels that need them. The game's own cheat
/// routines do the work, once a level has been running for a second, and again whenever
/// the levels are found lower (a level load or a loaded save puts them back).
fn upgrades(c: &mut Ctx) {
    /// The game's `globals`; the move levels are eight bytes at 0xAD9 (2 is upgraded) and
    /// the two characters' health levels follow (3 is the most).
    const GLOBALS: u32 = 0x804A_DA80;
    const IN_GAME: u32 = 12;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static SETTLED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if !*ON.get_or_init(|| std::env::var_os("TSSM_UPGRADES").is_some()) {
        return;
    }
    if c.r32(GAME_MODE) != IN_GAME {
        SETTLED.store(0, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    if SETTLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 60 {
        return;
    }
    let levels: [u8; 10] = c.slice_mut(GLOBALS + 0xAD9, 10).try_into().unwrap();
    if levels == [2, 2, 2, 2, 2, 2, 2, 2, 3, 3] {
        return;
    }
    SETTLED.store(0, std::sync::atomic::Ordering::Relaxed);
    for cheat in ["AllMovesCB__Fv", "AllUpgradedMovesCB__Fv", "MaxHealthSlotsCB__Fv"] {
        guest_call(c, cheat, &[]);
    }
    let now: [u8; 10] = c.slice_mut(GLOBALS + 0xAD9, 10).try_into().unwrap();
    eprintln!("[cheat] all upgrades given: levels {levels:?} -> {now:?}");
}

/// Runs before RenderWare's `_rwDlRasterShowRaster`, which submits a finished frame to its
/// queue of three. When the queue is full the function sleeps until the display has taken a
/// frame, and then carries on without looking again, which is only sound while one thread at
/// a time is submitting.
///
/// The game has two threads that submit: the main one, and the one that animates the loading
/// screen while the main one loads a level. The loading thread is told to stop when the level
/// is in memory, and stops after the frame it is on, which it is usually asleep in the middle
/// of submitting. On the console the main thread then still has seconds of level setup ahead
/// of its first frame. Here that setup can be over within one retrace, the main thread goes
/// to sleep on the same full queue, both are woken for the one free slot, and both take it:
/// the queue's bookkeeping is ruined and the game hangs on the loading screen, waiting for a
/// frame that the GPU is never let through to.
///
/// So a thread arriving while another is asleep in here waits a retrace at a time until the
/// other has gone, as it always has on the console.
fn before_show_raster(c: &mut Ctx) {
    /// RenderWare's thread queue for the frame queue (this build: GGVE78 revision 1).
    const FRAME_QUEUE_SLEEPERS: u32 = 0x804B_23C0;
    if !sched::has_sleepers(FRAME_QUEUE_SLEEPERS) {
        return;
    }
    // The wait lets interrupt handlers run on this thread: keep every register as it was.
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    while sched::has_sleepers(FRAME_QUEUE_SLEEPERS) {
        host::gx_process(c);
        sched::sleep(c, sched::RETRACE_QUEUE);
    }
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
}

/// Runs before RenderWare's `RwGameCubeGetXFBs`, which the game calls every frame for its
/// glow effect. The function waits for every frame submitted so far to have been shown by
/// going round a loop, which on the console costs nothing but here keeps a processor core
/// busy for most of every frame. The wait is done here instead, asleep until the retrace
/// that shows the frame; the function then finds nothing left to wait for.
fn before_get_xfbs(c: &mut Ctx) {
    /// RenderWare's frame queue (this build: GGVE78 revision 1): whether a frame is being
    /// handed to the display, and the positions frames are submitted at and shown from.
    const HANDING_OVER: u32 = 0x804B_23AC;
    const SUBMITTED: u32 = 0x804B_23A0;
    const SHOWN: u32 = 0x804B_23A4;
    let waiting = |c: &Ctx| c.r32(HANDING_OVER) == 1 || c.r32(SUBMITTED) != c.r32(SHOWN);
    if !waiting(c) || host::in_handler() {
        return;
    }
    // The wait lets interrupt handlers run on this thread: keep every register as it was.
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    while waiting(c) {
        host::gx_process(c);
        sched::sleep(c, sched::RETRACE_QUEUE);
    }
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
}

/// Runs in place of GXCallDisplayList(list, size): the game's own function is run, and then
/// the decoder takes down the list's vertices as they are at this moment (`Gx::look_ahead`).
fn call_display_list(c: &mut Ctx) -> bool {
    const GX_CALL_DISPLAY_LIST: u32 = 0x802E_F450;
    thread_local! {
        static INSIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if INSIDE.get() {
        return false;
    }
    INSIDE.set(true);
    c.call(GX_CALL_DISPLAY_LIST);
    INSIDE.set(false);
    host::with(|h| h.gx.look_ahead(c));
    true
}

/// TSSM_WARP=BB01@6000 sends the player through a portal to that level at that retrace, as
/// stepping into one of the game's own does: the current level is torn down and the other
/// loaded. For reproducing problems that only happen going from one level to another.
///
/// The portal is made up in the host's scratch block. A `_zPortal` points at its asset at
/// 0x10; the asset holds the destination's camera and marker at 8 and 0xC (none here), an
/// angle at 0x10 and the level's four-character name at 0x14.
fn warp(c: &mut Ctx, retrace: u32) {
    static PLAN: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("TSSM_WARP").ok()?;
        let (level, at) = spec.split_once('@')?;
        let name = <[u8; 4]>::try_from(level.to_ascii_uppercase().as_bytes()).ok()?;
        Some((u32::from_be_bytes(name), at.trim().parse().ok()?))
    });
    let Some((level, at)) = *plan else { return };
    if retrace < at || DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    const PORTAL_SCRATCH: u32 = 0x900;
    let portal = host::with(|h| h.scratch) + PORTAL_SCRATCH;
    let asset = portal + 0x40;
    c.slice_mut(portal, 0x80).fill(0);
    c.w32(portal + 0x10, asset);
    c.w32(asset + 0x14, level);
    eprintln!("[scene] warping to {}", String::from_utf8_lossy(&level.to_be_bytes()));
    guest_call(c, "zSceneSwitch__FP8_zPortali", &[portal, 0]);
}

/// Controllers. Memory cards are in card.rs.
fn peripheral_policy(name: &str) -> Option<Hle> {
    Some(match name {
        "PADRead" => Hle::Native(pad_read),
        "PADInit" | "PADReset" | "PADRecalibrate" => Hle::Native(|c| c.r[3] = 1),
        "PADClamp" | "PADSetSpec" => Hle::Native(nop),
        // PADControlMotor(chan, command): 1 starts the rumble motor, 0 and 2 stop it.
        "PADControlMotor" => Hle::Native(|c| {
            if c.r[3] == 0 {
                crate::input::set_motor(c.r[4] == 1);
            }
        }),
        "PADSetSamplingCallback" => Hle::Native(|c| c.r[3] = 0),

        _ => return None,
    })
}

pub fn policy(name: &str) -> Hle {
    let found = crate::dvd::policy(name)
        .or_else(|| video_policy(name))
        .or_else(|| peripheral_policy(name))
        .or_else(|| crate::card::policy(name))
        .or_else(|| crate::audio::policy(name));
    if let Some(hle) = found {
        return hle;
    }
    match name {
        "OSInit" => Hle::Native(os_init),
        "OSReport" => Hle::Native(os_report),
        "OSPanic" => Hle::Native(os_panic),
        "OSGetTime" | "__OSGetSystemTime" => Hle::Native(os_get_time),
        "OSGetTick" => Hle::Native(|c| {
            host::pump(c);
            c.r[3] = (host::clock_base() + c.timebase()) as u32;
        }),
        "OSTicksToCalendarTime" => Hle::Native(ticks_to_calendar_time),
        // A debugging check of the thread list; the game ignores the count.
        "OSCheckActiveThreads" => Hle::Native(|c| c.r[3] = 0),

        // Arena and heap bookkeeping is plain memory arithmetic: run the original.
        "OSGetArenaLo" | "OSGetArenaHi" | "OSSetArenaLo" | "OSSetArenaHi" | "OSInitAlloc" | "OSCreateHeap"
        | "OSDestroyHeap" | "OSSetCurrentHeap" | "OSAllocFromHeap" | "OSFreeToHeap" | "OSAllocFixed"
        | "OSAddToHeap" | "OSCheckHeap" | "OSReferentSize" | "OSDumpHeap" | "OSAllocFromArenaLo"
        | "OSAllocFromArenaHi" | "OSCreateAlarm" | "OSInitThreadQueue" | "OSGetConsoleType" => Hle::Original,
        "__OSSetInterruptHandler" | "__OSGetInterruptHandler" => Hle::Native(|c| c.r[3] = 0),
        // Mouse look runs ahead of the game's camera update.
        "zCameraUpdate__FP7xCameraf" => Hle::Before(crate::camera::before_update),
        "xCameraMove__FP7xCameraUiffffff" => Hle::Before(crate::camera::before_move),
        "xCameraLookYPR__FP7xCameraUiffffff" => Hle::Before(crate::camera::before_look),
        "zCameraTweakGlobal_Add__FUiffff" => Hle::Before(crate::camera::before_tweak),
        "zSceneInit__FUii" => Hle::Before(crate::dvd::before_scene_init),
        "_rwDlRasterShowRaster" => Hle::Before(before_show_raster),
        // The save and load menus go straight to the three save slots.
        "zSaveLoad_SaveLoop__Fv" | "zSaveLoad_LoadLoop__Fv" => Hle::Before(crate::card::before_menu),
        "zSaveLoad_Tick__Fb" => Hle::Replace(crate::card::pick_slot_a),
        "iSGTgtHaveRoomStartup__FP13st_ISGSESSIONiiPCcPCcPiPiPi" => Hle::Before(crate::card::before_room_check),
        // The mouse in the menus.
        "zUIMgrUpdate__Ff" => Hle::Before(crate::menu::before_update),
        "iFMVPlay__FPCcUifUibbi" => Hle::Before(crate::menu::before_movie),
        "BinkDoFrame" => Hle::Before(crate::menu::movie_frame),
        "RwGameCubeGetXFBs" => Hle::Before(before_get_xfbs),
        "CarUpdate__4zCarFffbbb" => Hle::Replace(crate::car::car_update),
        "DoRender__7zUITextCFv" => Hle::Replace(crate::menu::render_text),
        "DoRender__8zUIModelCFv" => Hle::Replace(crate::menu::render_model),
        "DoRender__8zUIImageCFv" => Hle::Replace(crate::menu::render_image),
        // A wide window widens the field of view.
        "RwCameraSetViewWindow" => Hle::Before(crate::aspect::before_set_view_window),
        "RwCameraBeginUpdate" => Hle::Before(crate::aspect::before_begin_update),
        // The heads-up display: each widget's drawing is marked in the GPU stream, so that it
        // can be placed at the edge of a wide picture (see `Flat` in render.rs). The game asks
        // for its mode right after drawing the display, which marks the end.
        "render__Q24xhud11text_widgetFv"
        | "render__Q24xhud12image_widgetFv"
        | "render__Q24xhud12model_widgetFv"
        | "render__Q24xhud17font_meter_widgetFv"
        | "render__Q24xhud17unit_meter_widgetFv" => Hle::Before(|c| host::with(|h| h.gx.mark_hud(c, true))),
        "zGameModeGet__Fv" => Hle::Before(|c| host::with(|h| h.gx.mark_hud(c, false))),
        // The game's own read-completion handler; it only looks like an SDK name.
        "DVDReadCallback" => Hle::Original,

        "OSDisableInterrupts" => Hle::Native(|c| set_interrupts(c, false)),
        "OSEnableInterrupts" => Hle::Native(|c| set_interrupts(c, true)),
        "OSRestoreInterrupts" => Hle::Native(|c| {
            let enabled = c.r[3] != 0;
            set_interrupts(c, enabled)
        }),
        "__OSUnmaskInterrupts" | "__OSMaskInterrupts" => Hle::Native(|c| c.r[3] = 0),
        "OSSetErrorHandler" | "OSSetResetCallback" => Hle::Native(|c| c.r[3] = 0),
        "OSGetResetButtonState" | "OSGetResetSwitchState" => Hle::Native(|c| c.r[3] = 0),

        // OSSetAlarm(alarm, s64 tick, handler); OSSetPeriodicAlarm(alarm, s64 start, s64 period, handler)
        "OSSetAlarm" => Hle::Native(|c| {
            let (delay, handler) = (pair(c, 5), c.r[7]);
            set_alarm(c, delay, 0, handler)
        }),
        "OSSetPeriodicAlarm" => Hle::Native(|c| {
            let (start, period, handler) = (pair(c, 5), pair(c, 7), c.r[9]);
            let delay = start.saturating_sub(host::clock_base() + c.timebase());
            set_alarm(c, delay, period.max(1), handler)
        }),
        "OSCancelAlarm" => Hle::Native(|c| {
            let ptr = c.r[3];
            c.w32(ptr, 0);
            host::with(|h| h.alarms.retain(|a| a.ptr != ptr));
        }),

        // Threads are scheduled by the host (see sched.rs).
        "OSCreateThread" => Hle::Native(sched::create),
        "OSGetCurrentThread" => Hle::Native(|c| c.r[3] = sched::current_guest()),
        "OSGetThreadPriority" => Hle::Native(|c| c.r[3] = sched::priority(c.r[3]) as u32),
        "OSResumeThread" => Hle::Native(|c| {
            let old = sched::resume(c.r[3]);
            sched::preempt(c);
            c.r[3] = old as u32;
        }),
        "OSSuspendThread" => Hle::Native(|c| {
            let thread = c.r[3];
            c.r[3] = sched::suspend(c, thread) as u32;
        }),
        "OSYieldThread" => Hle::Native(|c| sched::reschedule(c, true)),
        "OSSleepThread" => Hle::Native(|c| {
            let queue = c.r[3];
            sched::sleep(c, queue);
        }),
        "OSWakeupThread" => Hle::Native(|c| {
            sched::wake(c.r[3]);
            sched::preempt(c);
        }),
        "OSExitThread" => Hle::Native(|_| sched::exit_current()),
        "OSIsThreadSuspended" => Hle::Native(|c| c.r[3] = sched::is_suspended(c.r[3]) as u32),
        "OSIsThreadTerminated" => Hle::Native(|c| c.r[3] = sched::is_terminated(c.r[3]) as u32),
        // Semaphores are counters built on sleep and wakeup: run the original.
        "OSInitSemaphore" | "OSWaitSemaphore" | "OSTryWaitSemaphore" | "OSSignalSemaphore" | "OSGetSemaphoreCount" => {
            Hle::Original
        }
        // OSMutex: owner thread at +8, recursion count at +0xC. Waiters sleep on the mutex.
        "OSInitMutex" => Hle::Native(|c| {
            let mutex = c.r[3];
            c.slice_mut(mutex, 0x18).fill(0);
        }),
        "OSLockMutex" => Hle::Native(|c| {
            let (mutex, me) = (c.r[3], sched::current_guest());
            while c.r32(mutex + 8) != 0 && c.r32(mutex + 8) != me {
                sched::sleep(c, mutex);
            }
            c.w32(mutex + 8, me);
            c.w32(mutex + 0xC, c.r32(mutex + 0xC) + 1);
        }),
        "OSTryLockMutex" => Hle::Native(|c| {
            let (mutex, me) = (c.r[3], sched::current_guest());
            let free = c.r32(mutex + 8) == 0 || c.r32(mutex + 8) == me;
            if free {
                c.w32(mutex + 8, me);
                c.w32(mutex + 0xC, c.r32(mutex + 0xC) + 1);
            }
            c.r[3] = free as u32;
        }),
        "OSUnlockMutex" => Hle::Native(|c| {
            let mutex = c.r[3];
            let count = c.r32(mutex + 0xC).saturating_sub(1);
            c.w32(mutex + 0xC, count);
            if count == 0 {
                c.w32(mutex + 8, 0);
                sched::wake(mutex);
                sched::preempt(c);
            }
        }),

        // Console settings and registration hooks with nothing behind them here.
        "OSGetSoundMode" => Hle::Native(|c| c.r[3] = 1), // stereo
        "OSGetFontEncode" | "OSInitFont" | "PPCMfhid2" => Hle::Native(|c| c.r[3] = 0),
        "OSSetSoundMode" | "OSRegisterVersion" | "OSRegisterResetFunction" | "PPCMthid2" | "PPCMtwpar" => {
            Hle::Native(nop)
        }

        // There is no CPU cache to manage. DCZeroRange really does clear memory.
        "DCFlushRange" | "DCFlushRangeNoSync" | "DCStoreRange" | "DCStoreRangeNoSync" | "DCInvalidateRange"
        | "DCTouchRange" | "DCEnable" | "ICEnable" | "ICInvalidateRange" | "ICFlashInvalidate" | "ICSync" => {
            Hle::Native(nop)
        }
        "DCZeroRange" => Hle::Original,

        // Debugger, cache and CPU-sync plumbing.
        "DBInit" | "__DBExceptionDestination" | "DBPrintf" | "__flush_cache" | "PPCSync" | "PPCHalt" => {
            Hle::Native(nop)
        }
        _ => Hle::Unimplemented,
    }
}
