//! The driving levels' car, stepped at a fixed 60 times a second whatever the frame rate.
//!
//! `zEntPlayerCarUpdate` calls `zCar::CarUpdate` once a frame, or splits a long frame into
//! steps of at most 1/60 s. Inside, the car's position moves by velocity × time, but some
//! forces are added to the velocity once per step without being scaled by the time, so the
//! car only behaves as designed with steps of 1/60 s. At 240 frames a second each step is
//! 1/240 s and those forces act four times as often.
//!
//! The hook below stands in for `CarUpdate`: it adds up the time it is given and runs the
//! original in whole steps of 1/60 s (none on most frames at a high frame rate, two at 30).
//! What is shown is blended between the last two steps, so the car still moves smoothly at
//! any rate. The blended matrix is what the rest of the frame sees (camera, model, effects);
//! the next call puts the car's real matrix back before it steps again.
//!
//! zCar keeps its matrix at 0x50 and the one before the last step at 0x10. Before each call,
//! `zCar::SetMatrix` copies 0x50 to 0x10 and the car entity's matrix to 0x50; after it, the
//! caller copies 0x50 back to the entity.
//!
//! Addresses and offsets are for this build of the game (GGVE78, revision 1).

use std::sync::Mutex;

use runtime::Ctx;

/// zCar::CarUpdate(float dt, float, bool, bool, bool).
const CAR_UPDATE: u32 = 0x800C_8724;
const STEP: f32 = 1.0 / 60.0;
/// More time than this in one frame is dropped (the game itself limits a frame to 0.1 s).
const MOST_STEPS: u32 = 6;
const MATRIX: u32 = 0x50;
const PREVIOUS: u32 = 0x10;

type Matrix = [u32; 16];

struct State {
    /// The matrix after the last step and the one before it.
    now: Matrix,
    before: Matrix,
    /// The blended matrix last handed to the game.
    shown: Matrix,
    /// Time not yet stepped.
    left: f32,
    /// Timebase when the hook last ran, to notice a car that has not been driven for a while.
    last_seen: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static INSIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn read(c: &Ctx, at: u32) -> Matrix {
    std::array::from_fn(|i| c.r32(at + 4 * i as u32))
}

fn write(c: &mut Ctx, at: u32, m: &Matrix) {
    for (i, &w) in m.iter().enumerate() {
        c.w32(at + 4 * i as u32, w);
    }
}

/// `TSSM_CAR_FRAME_STEP=1` leaves the car stepping once a frame, as the game does, for
/// comparisons.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSSM_CAR_FRAME_STEP").is_none())
}

/// Replaces zCar::CarUpdate. Returns false (run the original) for its own calls.
pub fn car_update(c: &mut Ctx) -> bool {
    crate::input::note_driving();
    if INSIDE.load(std::sync::atomic::Ordering::Relaxed) || !enabled() {
        return false;
    }
    let car = c.r[3];
    let args = (c.r[4], c.r[5], c.r[6], c.f[2], c.lr);
    let dt = c.f[1] as f32;
    let now_tb = c.timebase();
    let current = read(c, car + MATRIX);

    let mut guard = STATE.lock().unwrap();
    let fresh = match guard.as_ref() {
        None => true,
        // Moved by something else (a reset after a fall, a new level), or not driven for a
        // while: start again from where the car is.
        Some(s) => {
            current != s.shown
                || now_tb.saturating_sub(s.last_seen) > crate::host::TICKS_PER_SECOND / 4
        }
    };
    if fresh {
        *guard = Some(State { now: current, before: current, shown: current, left: STEP, last_seen: now_tb });
    }
    let s = guard.as_mut().unwrap();
    s.last_seen = now_tb;
    s.left = (s.left + dt).min(STEP * MOST_STEPS as f32);

    let shown_before = s.shown;
    write(c, car + MATRIX, &s.now);
    while s.left >= STEP {
        s.left -= STEP;
        s.before = s.now;
        // As SetMatrix does before each step the game makes.
        write(c, car + PREVIOUS, &s.now);
        c.r[3] = car;
        (c.r[4], c.r[5], c.r[6], c.f[2]) = (args.0, args.1, args.2, args.3);
        c.f[1] = STEP as f64;
        INSIDE.store(true, std::sync::atomic::Ordering::Relaxed);
        c.call(CAR_UPDATE);
        INSIDE.store(false, std::sync::atomic::Ordering::Relaxed);
        s.now = read(c, car + MATRIX);
    }
    c.lr = args.4;

    s.shown = blend(&s.before, &s.now, s.left / STEP);
    write(c, car + PREVIOUS, &shown_before);
    write(c, car + MATRIX, &s.shown);
    true
}

/// The matrix `t` of the way from `a` to `b`: rows blended and made square again, position
/// blended, the fourth word of each row kept from `b`.
fn blend(a: &Matrix, b: &Matrix, t: f32) -> Matrix {
    let f = |m: &Matrix, i: usize| f32::from_bits(m[i]);
    let row = |r: usize| -> [f32; 3] {
        std::array::from_fn(|k| f(a, r * 4 + k) + (f(b, r * 4 + k) - f(a, r * 4 + k)) * t)
    };
    let dot = |x: [f32; 3], y: [f32; 3]| x[0] * y[0] + x[1] * y[1] + x[2] * y[2];
    let sub = |x: [f32; 3], y: [f32; 3], s: f32| [x[0] - y[0] * s, x[1] - y[1] * s, x[2] - y[2] * s];
    let scale = |x: [f32; 3], s: f32| [x[0] * s, x[1] * s, x[2] * s];
    let length = |x: [f32; 3]| dot(x, x).sqrt();
    let target = |r: usize| length(std::array::from_fn(|k| f(b, r * 4 + k)));

    // Gram-Schmidt keeps each row's direction close to the blend and its length as in `b`.
    let x = row(0);
    let x = scale(x, 1.0 / length(x).max(1e-6));
    let y = row(1);
    let y = sub(y, x, dot(y, x));
    let y = scale(y, 1.0 / length(y).max(1e-6));
    let z = row(2);
    let z = sub(sub(z, x, dot(z, x)), y, dot(z, y));
    let z = scale(z, 1.0 / length(z).max(1e-6));
    let rows = [scale(x, target(0)), scale(y, target(1)), scale(z, target(2)), row(3)];

    let mut out = *b;
    for (r, v) in rows.iter().enumerate() {
        for k in 0..3 {
            out[r * 4 + k] = v[k].to_bits();
        }
    }
    out
}
