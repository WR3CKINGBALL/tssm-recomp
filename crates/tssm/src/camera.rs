//! Mouse look: a rigid orbit camera around the character.
//!
//! The original camera is built for a stick, and it is left exactly as it was while a
//! controller is in use. Turning is a rate, looking up or down blends toward preset positions
//! close to the character, and the camera eases toward where it wants to be, so it trails
//! behind a moving character and swings around on its own.
//!
//! None of that suits a mouse. While the keyboard and mouse are in use, the camera instead
//! sits at a fixed distance from the character, at the angles the mouse has set, and nothing
//! else moves it. This is done from hooks that run just before the game's own camera functions:
//!
//! - before `zCameraUpdate`, mouse movement is added to the two orbit angles.
//! - before `xCameraMove`, the distance and height the game wants are swung up or down an arc
//!   of constant radius, its orbit angle is replaced, and its easing time is set to zero so the
//!   camera is placed there at once.
//! - before `xCameraLookYPR`, the view is aimed to match, also at once.
//!
//! The engine's collision pass still runs afterwards, so the camera moves in toward the
//! character when something is in the way.
//!
//! Addresses and offsets below are for this build of the game (GGVE78, revision 1).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use runtime::Ctx;

use crate::input;

// xCamera fields.
/// The camera's world position (x, y, z), the last row of its matrix at 0x20.
const CAMERA_POS: u32 = 0x50;
/// Pointer to the matrix of whatever the camera follows.
const TARGET_MATRIX: u32 = 0x11C;
/// Flag word. Bit 0x01 is set while a timed move is easing the position, bit 0x40 while a timed
/// look is easing the view.
const FLAGS: u32 = 0x148;
const EASING_MOVE: u32 = 0x01;
const EASING_LOOK: u32 = 0x40;
/// Angle of the camera around its target: atan2 of the horizontal offset.
const ORBIT_CURRENT: u32 = 0x18C;
/// The direction the view faces, as a goal; an untimed move sets it to face the target.
const YAW_GOAL: u32 = 0x1E4;
/// Downward tilt of the view, in radians.
const TILT_CURRENT: u32 = 0x1E8;
/// Offset of the position row within a 4x3 matrix.
const MATRIX_POS: u32 = 0x30;

/// Small-data byte (offset from r13): nonzero while the game lets the player control the camera.
const INPUT_ENABLED: u32 = 0xFFFF_80DC;

/// Address range of zCameraUpdate; the goal hooks only act on calls made from inside it.
const GAMEPLAY_UPDATE: std::ops::Range<u32> = 0x800C_C5B4..0x800C_DCC4;

/// Radians per mouse count at a sensitivity of 1, for both axes.
const RADIANS_PER_COUNT: f32 = 0.0015;
/// How far the mouse may tilt the view up or down from where the game would aim it, in radians.
const TILT_RANGE: (f32, f32) = (-1.0, 0.85);
/// Limits on the camera's elevation above the horizontal, in radians. The lower one keeps the
/// camera off the ground; looking further up than that tilts the view without moving the camera.
const ELEVATION_RANGE: (f32, f32) = (0.06, 1.45);

/// What the mouse has set.
struct Orbit {
    /// Whether the mouse owns the camera right now. Cleared whenever it does not, so the
    /// angles are taken afresh from wherever the game left the camera.
    active: bool,
    /// Angle of the camera around the character.
    yaw: f32,
    /// Vertical angle added to the game's own; positive looks down from higher up.
    tilt: f32,
    /// Whether this update has placed the camera yet, which the look hook relies on.
    placed: bool,
}

static ORBIT: Mutex<Orbit> = Mutex::new(Orbit { active: false, yaw: 0.0, tilt: 0.0, placed: false });

fn read_f32(c: &Ctx, addr: u32) -> f32 {
    f32::from_bits(c.r32(addr))
}

/// Swings a (distance, height) pair up by `tilt` at constant radius, within the elevation limits.
fn swing(distance: f32, height: f32, tilt: f32) -> (f32, f32) {
    let radius = distance.hypot(height);
    let base = height.atan2(distance);
    // Never push past a limit, but leave alone a camera the game itself placed beyond one.
    let elevation = (base + tilt).clamp(ELEVATION_RANGE.0.min(base), ELEVATION_RANGE.1.max(base));
    (radius * elevation.cos(), radius * elevation.sin())
}

/// Runs before zCameraUpdate(xCamera* cam, float dt): takes in the mouse's movement.
pub fn before_update(c: &mut Ctx) {
    input::note_camera_update();
    let (dx, dy) = input::take_look();
    let camera = c.r[3];
    let target = c.r32(camera + TARGET_MATRIX);
    let mut orbit = ORBIT.lock().unwrap();
    orbit.placed = false;
    trace_view(c, camera, target, &orbit);

    // Cutscenes and the like own the camera, and so does the game itself for a controller.
    let player_controls = c.r8(c.r[13].wrapping_add(INPUT_ENABLED)) != 0;
    if !input::mouse_camera() || !player_controls || !(0x8000_0000..0x8180_0000).contains(&target) {
        orbit.active = false;
        if (0x8000_0000..0x8180_0000).contains(&target) {
            trace(c, camera, target, dx, dy);
        }
        return;
    }
    if !orbit.active {
        *orbit = Orbit { active: true, yaw: read_f32(c, camera + ORBIT_CURRENT), tilt: 0.0, placed: false };
    }
    // Moving the mouse right turns the view right, which is a decreasing orbit angle; moving it
    // down tilts the view down, which raises the camera over the character.
    orbit.yaw = (orbit.yaw - dx * RADIANS_PER_COUNT).rem_euclid(std::f32::consts::TAU);
    orbit.tilt = (orbit.tilt + dy * RADIANS_PER_COUNT).clamp(TILT_RANGE.0, TILT_RANGE.1);
    drop(orbit);
    trace(c, camera, target, dx, dy);
}

/// With TSSM_TRACE_INPUT, logs where the camera sits relative to its target: every update
/// around mouse movement, and otherwise now and then.
fn trace(c: &Ctx, camera: u32, target: u32, dx: f32, dy: f32) {
    static TRACE_LEFT: AtomicU32 = AtomicU32::new(0);
    static UPDATES: AtomicU32 = AtomicU32::new(0);
    if std::env::var_os("TSSM_TRACE_INPUT").is_some() {
        if dx != 0.0 || dy != 0.0 {
            TRACE_LEFT.store(4, Ordering::Relaxed);
        }
        let periodic = UPDATES.fetch_add(1, Ordering::Relaxed) % 20 == 0;
        if periodic || TRACE_LEFT.load(Ordering::Relaxed) > 0 {
            TRACE_LEFT.store(TRACE_LEFT.load(Ordering::Relaxed).saturating_sub(1), Ordering::Relaxed);
            let at = |base: u32, i: u32| read_f32(c, base + i * 4);
            let (x, y, z) = (
                at(camera + CAMERA_POS, 0) - at(target + MATRIX_POS, 0),
                at(camera + CAMERA_POS, 1) - at(target + MATRIX_POS, 1),
                at(camera + CAMERA_POS, 2) - at(target + MATRIX_POS, 2),
            );
            eprintln!(
                "[camera] target {:.1},{:.1},{:.1}; orbit {:.3}, distance {:.2} height {:.2} (radius {:.2}), tilt {:.3}, mouse {dx:.0},{dy:.0}",
                at(target + MATRIX_POS, 0),
                at(target + MATRIX_POS, 1),
                at(target + MATRIX_POS, 2),
                x.atan2(z).rem_euclid(std::f32::consts::TAU),
                x.hypot(z),
                y,
                x.hypot(z).hypot(y),
                read_f32(c, camera + TILT_CURRENT),
            );
        }
    }
}

/// Runs before xCameraMove(cam, flags, float distance, float height, float orbit, float time,
/// ...): puts the camera where the mouse has it, at once.
pub fn before_move(c: &mut Ctx) {
    trace_goal(c, "move");
    let mut orbit = ORBIT.lock().unwrap();
    if !orbit.active || !GAMEPLAY_UPDATE.contains(&c.lr) {
        return;
    }
    let (distance, height) = swing(c.f[1] as f32, c.f[2] as f32, orbit.tilt);
    c.f[1] = distance.max(0.05) as f64;
    c.f[2] = height as f64;
    c.f[3] = orbit.yaw as f64;
    c.f[4] = 0.0;
    // Abandon any eased move still in progress from before the mouse took over.
    let flags = c.r32(c.r[3] + FLAGS);
    c.w32(c.r[3] + FLAGS, flags & !EASING_MOVE);
    orbit.placed = true;
}

/// Runs before xCameraLookYPR(cam, flags, float yaw, float pitch, float roll, float time, ...):
/// aims the view to match, at once.
pub fn before_look(c: &mut Ctx) {
    trace_goal(c, "look");
    let orbit = ORBIT.lock().unwrap();
    if !orbit.active || !GAMEPLAY_UPDATE.contains(&c.lr) {
        return;
    }
    if orbit.placed {
        // The game worked its direction out before the camera was moved; the move itself left
        // the direction that faces the character from the new position.
        c.f[1] = read_f32(c, c.r[3] + YAW_GOAL) as f64;
    }
    c.f[2] += orbit.tilt as f64;
    c.f[4] = 0.0;
    let flags = c.r32(c.r[3] + FLAGS);
    c.w32(c.r[3] + FLAGS, flags & !EASING_LOOK);
}

/// Runs before zCameraTweakGlobal_Add(owner, float priority, float time, float pitch, float
/// distance): a level's camera tweak coming into force, as the character enters its trigger.
/// The pitch is added to the camera's downward angle, in degrees, for as long as the tweak
/// lasts; levels use it near drops and on steep slides, the sliding levels by up to 60
/// degrees with several stacked. Only the share set in controls.txt is passed on.
pub fn before_tweak(c: &mut Ctx) {
    let asked = c.f[3];
    c.f[3] *= input::level_tilt() as f64;
    if tracing() {
        eprintln!("[camera] level tilt of {asked:.0} degrees taken as {:.0} (distance x{:.2})", c.f[3], c.f[4]);
    }
}

/// TSSM_TRACE_CAMERA=1: for finding out what turned the view. Each gameplay camera update
/// in which the view's downward angle has changed by 3 degrees since the last line, logs
/// that angle with everything known to steer it: the goals the game last asked for, the
/// state of its own camera modes, and what the mouse has set.
fn trace_view(c: &Ctx, camera: u32, target: u32, orbit: &Orbit) {
    static LAST: AtomicU32 = AtomicU32::new(0);
    if !tracing() {
        return;
    }
    // The view direction is the third row of the camera's matrix.
    let down = -read_f32(c, camera + 0x44).clamp(-1.0, 1.0).asin();
    if (down - f32::from_bits(LAST.load(Ordering::Relaxed))).abs() < 0.05 {
        return;
    }
    LAST.store(down.to_bits(), Ordering::Relaxed);
    let small = |offset: u32| c.r32(c.r[13].wrapping_add(offset));
    let small_f = |offset: u32| f32::from_bits(small(offset));
    let height = if (0x8000_0000..0x8180_0000).contains(&target) {
        read_f32(c, camera + CAMERA_POS + 4) - read_f32(c, target + MATRIX_POS + 4)
    } else {
        f32::NAN
    };
    let goals = GOALS.lock().unwrap();
    eprintln!(
        "[camera] view {:.0} deg down, camera {height:.2} above target; game asked move {:x?} look {:x?}; tilt {:.3}, \
         stick {:.2}, bounce {}/{}/{}, tracking off {:#x} ({:.2}), tweaks {} ({:.2}), mode {}, input {}, mouse {} tilt {:.2}",
        down.to_degrees(),
        goals.0,
        goals.1,
        read_f32(c, camera + TILT_CURRENT),
        small_f(0xFFFF_8B04),
        small(0xFFFF_8A70),
        small(0xFFFF_8A84),
        small(0xFFFF_8A80),
        small(0xFFFF_8ACC),
        small_f(0xFFFF_8AD4),
        small(0xFFFF_8B10),
        small_f(0xFFFF_8B3C),
        small(0xFFFF_8AE0),
        c.r8(c.r[13].wrapping_add(INPUT_ENABLED)),
        orbit.active,
        orbit.tilt,
    );
}

/// What the game last asked of `xCameraMove` (distance, height) and `xCameraLookYPR` (pitch),
/// with the caller's address, for `trace_view`.
static GOALS: Mutex<((u32, f32, f32), (u32, f32))> = Mutex::new(((0, 0.0, 0.0), (0, 0.0)));

fn trace_goal(c: &Ctx, which: &str) {
    if !tracing() {
        return;
    }
    let mut goals = GOALS.lock().unwrap();
    if which == "move" {
        goals.0 = (c.lr, c.f[1] as f32, c.f[2] as f32);
    } else {
        goals.1 = (c.lr, c.f[2] as f32);
    }
}

fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSSM_TRACE_CAMERA").is_some())
}
