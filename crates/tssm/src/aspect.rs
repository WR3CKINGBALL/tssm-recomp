//! Wide and custom aspect ratios.
//!
//! The game is made for a 4:3 picture. In a wider window it is shown wider rather than
//! stretched or boxed in, in three cooperating parts:
//!
//! - A hook before RenderWare's `RwCameraSetViewWindow` widens the horizontal field of view of
//!   every camera that fills the screen. The game then draws, and culls, a wider view into its
//!   usual framebuffer; nothing else in the game knows. The game only sets a camera's view
//!   window when its field of view changes, so a second hook, before `RwCameraBeginUpdate`
//!   (run whenever a camera starts drawing), sets it again once the window's shape has
//!   changed since.
//! - The renderer gives the framebuffer more columns and shows it across the wider picture
//!   (render.rs).
//! - Flat screen-space drawing (menus, the heads-up display, text) would come out stretched,
//!   so it is squeezed back toward the centre of the screen (gx.rs). So are the models the
//!   game draws on top of it (tokens, chests, the health burgers): they go through the same
//!   camera as the world, but the game places them to cover a fixed part of the screen.
//!
//! Windows narrower than 4:3 keep the 4:3 picture with bars above and below.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use runtime::Ctx;

use crate::host;

const NATIVE: f32 = 4.0 / 3.0;
/// The widest picture drawn, as a multiple of 4:3 (this is 32:9). Wider windows get side bars.
const MAX_WIDEN: f32 = 2.0;
/// Where in the host's scratch block the widened view window is kept.
const VIEW_WINDOW_SCRATCH: u32 = 0x800;

/// Width over height of the window, as `f32` bits.
static WINDOW: AtomicU32 = AtomicU32::new(0);

/// The screen-filling cameras, by guest address: the view window the game last asked for,
/// and the widening it was given.
static CAMERAS: Mutex<Option<HashMap<u32, (f32, f32, f32, f32)>>> = Mutex::new(None);

/// The game's own field of view in play, in degrees across a 4:3 picture (80 while driving).
pub const ORIGINAL_FOV: f32 = 75.0;
/// Views at least this wide are play; narrower ones are a cutscene's shots, left alone.
const PLAY_FOV_LEAST: f32 = 70.0;
/// The field of view chosen in the settings (options.rs), as `f32` bits; 0 until read.
static FOV: AtomicU32 = AtomicU32::new(0);

/// The field of view chosen for play, in degrees across a 4:3 picture: TSSM_FOV, or else
/// config/fov.txt, or else the game's own.
pub fn fov() -> f32 {
    let bits = FOV.load(Ordering::Relaxed);
    if bits != 0 {
        return f32::from_bits(bits);
    }
    let text = std::env::var("TSSM_FOV").ok().or_else(|| std::fs::read_to_string("config/fov.txt").ok()).unwrap_or_default();
    let word = text.lines().map(|line| line.split('#').next().unwrap().trim()).find(|line| !line.is_empty()).unwrap_or("");
    let fov = word.parse::<f32>().ok().filter(|f| (30.0..=150.0).contains(f)).unwrap_or(ORIGINAL_FOV);
    FOV.store(fov.to_bits(), Ordering::Relaxed);
    fov
}

/// Sets the field of view for play. Cameras take it the next time they draw.
pub fn set_fov(degrees: f32) {
    FOV.store(degrees.clamp(30.0, 150.0).to_bits(), Ordering::Relaxed);
}

/// A view window (tangents of half the fields of view) with the chosen field of view: the
/// game's view in play is widened or narrowed by the difference between the chosen field of
/// view and the game's own, keeping its shape, so that driving stays 5 degrees wider than
/// walking. Narrower views (a cutscene's) are left as the game made them.
fn chosen_fov(x: f32, y: f32) -> (f32, f32) {
    let across = 2.0 * x.atan().to_degrees();
    let change = fov() - ORIGINAL_FOV;
    if across < PLAY_FOV_LEAST || change == 0.0 {
        return (x, y);
    }
    let wanted = ((across + change).clamp(10.0, 170.0) / 2.0).to_radians().tan();
    (wanted, y * wanted / x)
}

/// TSSM_ASPECT=16:9 (or 2.37) fixes the picture's shape regardless of the window; without a
/// window that is the only way to get a wide picture. TSSM_ASPECT=16:9@7400 does so only
/// from that retrace on, with 4:3 before it, to test a change of shape in mid-game.
fn forced() -> Option<f32> {
    static FORCED: OnceLock<Option<(f32, u32)>> = OnceLock::new();
    let (ratio, from) = (*FORCED.get_or_init(|| {
        let spec = std::env::var("TSSM_ASPECT").ok()?;
        let (shape, from) = spec.split_once('@').unwrap_or((&spec, "0"));
        let ratio = match shape.split_once(':') {
            Some((w, h)) => w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?,
            None => shape.trim().parse().ok()?,
        };
        (ratio.is_finite() && ratio > 0.0).then_some((ratio, from.trim().parse().ok()?))
    }))?;
    Some(if host::RETRACE_COUNT.load(Ordering::Relaxed) >= from { ratio } else { NATIVE })
}

/// Called from the window thread whenever the window's size changes.
pub fn set_window(width: u32, height: u32) {
    if width > 0 && height > 0 {
        WINDOW.store((width as f32 / height as f32).to_bits(), Ordering::Relaxed);
    }
}

/// How many times wider than 4:3 the picture is: 1 for 4:3 and anything narrower.
pub fn widen() -> f32 {
    let window = f32::from_bits(WINDOW.load(Ordering::Relaxed));
    let ratio = forced().unwrap_or(if window > 0.0 { window } else { NATIVE });
    (ratio / NATIVE).clamp(1.0, MAX_WIDEN)
}

/// Width over height of the picture.
pub fn picture() -> f32 {
    NATIVE * widen()
}

/// Names of the functions up the guest call stack from the one being entered.
fn callers(c: &Ctx) -> String {
    let name = |addr: u32| {
        let i = game::FUNCS.partition_point(|f| f.0 <= addr);
        if i == 0 { "?" } else { game::FUNCS[i - 1].2 }
    };
    let mut names = vec![name(c.lr)];
    // Each frame starts with a link to the caller's frame, which holds the return address
    // of the function that made it one word further on.
    let mut frame = c.r32(c.r[1]);
    for _ in 0..4 {
        if frame < 0x8000_0000 || frame >= 0x8180_0000 - 8 {
            break;
        }
        names.push(name(c.r32(frame + 4)));
        frame = c.r32(frame);
    }
    names.join(" < ")
}

/// Runs before `RwCameraSetViewWindow(camera, &window)`. The view window holds the tangents of
/// half the horizontal and vertical fields of view. Cameras that fill the screen have them in
/// the ratio 4:3 and get the horizontal one widened; others (the square camera that renders
/// shadows into a texture, for one) are left alone.
pub fn before_set_view_window(c: &mut Ctx) {
    let (x, y) = (f32::from_bits(c.r32(c.r[4])), f32::from_bits(c.r32(c.r[4] + 4)));
    let widen = widen();
    if std::env::var_os("TSSM_TRACE_GX").is_some() {
        static SEEN: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap();
        if !seen.contains(&(x.to_bits(), y.to_bits())) && seen.len() < 64 {
            seen.push((x.to_bits(), y.to_bits()));
            eprintln!(
                "[aspect] view window {x} x {y} (ratio {:.3}) on camera {:#010x} from {}, widening {widen:.3}",
                x / y,
                c.r[3],
                callers(c)
            );
        }
    }
    let fills_screen = y > 0.0 && (x / y - NATIVE).abs() <= 0.02;
    {
        let mut cameras = CAMERAS.lock().unwrap();
        let cameras = cameras.get_or_insert_with(HashMap::new);
        if fills_screen {
            cameras.insert(c.r[3], (x, y, widen, fov()));
        } else {
            cameras.remove(&c.r[3]);
        }
    }
    if !fills_screen {
        return;
    }
    let (x_chosen, y_chosen) = chosen_fov(x, y);
    if widen == 1.0 && x_chosen == x {
        return;
    }
    // The caller's structure is left as it was: it may be reused, and must not widen twice.
    let copy = host::with(|h| h.scratch) + VIEW_WINDOW_SCRATCH;
    c.w32(copy, (x_chosen * widen).to_bits());
    c.w32(copy + 4, y_chosen.to_bits());
    c.r[4] = copy;
}

/// Runs before `RwCameraBeginUpdate(camera)`. If the picture has changed shape since this
/// camera's view window was set, sets it again from what the game asked for, so the field
/// of view always matches the picture the frame will be shown in.
pub fn before_begin_update(c: &mut Ctx) {
    let camera = c.r[3];
    let widen = widen();
    let stale = CAMERAS.lock().unwrap().as_ref().and_then(|cameras| cameras.get(&camera).copied()).filter(|entry| entry.2 != widen || entry.3 != fov());
    let Some((x, y, ..)) = stale else { return };
    // This lands in the middle of the caller's code: keep every register as it was.
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    let window = host::with(|h| h.scratch) + VIEW_WINDOW_SCRATCH + 8;
    c.w32(window, x.to_bits());
    c.w32(window + 4, y.to_bits());
    c.r[4] = window;
    let set_view_window = c.func_by_name("RwCameraSetViewWindow").expect("game has no RwCameraSetViewWindow");
    set_view_window(c);
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
}
