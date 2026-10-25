//! On-screen controls for a touch screen (Android; a touch screen on the desktop works too).
//!
//! The left part of the screen is the control stick: it is centred wherever the thumb lands.
//! The buttons sit on the right in the same arrangement the gamepad layout uses (A at the
//! bottom, B left, X right, Y top), with R and Z above them, L at the top left and Start at
//! the top centre. Dragging anywhere else turns the camera as the mouse does (camera.rs).
//!
//! The small eye button beside Start hides the outlines: the controls still work where they
//! were, only the eye stays, faintly, to bring them back. Using a physical controller puts
//! everything away, eye included, until the screen is touched again.
//!
//! The window thread feeds fingers in; the renderer asks for the controls' outlines every
//! frame and draws them over the picture.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::input;
use crate::render::OverlayVertex;

/// How far the thumb travels from where it landed to push the stick fully, as a fraction of
/// the screen's height.
const STICK_TRAVEL: f32 = 0.16;
/// Where the stick is drawn while nothing holds it (fractions of the height, from the left
/// and from the top).
const STICK_REST: (f32, f32) = (0.32, 0.66);
/// The stick takes fingers landing left of this fraction of the width and below this
/// fraction of the height.
const STICK_AREA: (f32, f32) = (0.42, 0.28);
/// Mouse counts per pixel dragged on a screen 1080 pixels high; scaled so the same physical
/// swipe turns the camera equally on any screen.
const LOOK_COUNTS: f32 = 1.3;
/// A button answers to touches this much beyond its outline.
const REACH: f32 = 1.25;

const LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.5];
const FILL: [f32; 4] = [1.0, 1.0, 1.0, 0.3];
/// How much fainter the eye is drawn while it is all that is left.
const FAINT: f32 = 0.45;
/// The eye button: right of the screen's centre and from the top, and its radius (fractions
/// of the height).
const EYE: (f32, f32, f32) = (0.16, 0.1, 0.042);

type Stroke = &'static [(f32, f32)];

/// A button: controller action, position (from the right edge if `from_right`, else from the
/// left, and from the top; fractions of the height), radius, and its label as pen strokes in
/// a unit square with y up.
struct Button {
    action: &'static str,
    from_right: bool,
    x: f32,
    y: f32,
    radius: f32,
    label: &'static [Stroke],
}

/// The eye's label, in a button's unit square: the two lids and the pupil.
const EYE_LABEL: [Stroke; 2] = [
    &[(-0.1, 0.5), (0.2, 0.75), (0.5, 0.83), (0.8, 0.75), (1.1, 0.5), (0.8, 0.25), (0.5, 0.17), (0.2, 0.25), (-0.1, 0.5)],
    &[(0.4, 0.42), (0.4, 0.58), (0.6, 0.58), (0.6, 0.42), (0.4, 0.42)],
];
const BOWL: Stroke = &[(0.1, 0.0), (0.1, 1.0), (0.65, 1.0), (0.9, 0.87), (0.9, 0.63), (0.65, 0.5), (0.1, 0.5)];
const BUTTONS: [Button; 8] = [
    Button {
        action: "a",
        from_right: true,
        x: 0.36,
        y: 0.82,
        radius: 0.095,
        label: &[&[(0.05, 0.0), (0.5, 1.0), (0.95, 0.0)], &[(0.23, 0.38), (0.77, 0.38)]],
    },
    Button {
        action: "b",
        from_right: true,
        x: 0.18,
        y: 0.65,
        radius: 0.085,
        label: &[BOWL, &[(0.65, 0.5), (0.93, 0.37), (0.93, 0.13), (0.65, 0.0), (0.1, 0.0)]],
    },
    Button {
        action: "x",
        from_right: true,
        x: 0.54,
        y: 0.65,
        radius: 0.085,
        label: &[&[(0.05, 0.0), (0.95, 1.0)], &[(0.05, 1.0), (0.95, 0.0)]],
    },
    Button {
        action: "y",
        from_right: true,
        x: 0.36,
        y: 0.48,
        radius: 0.085,
        label: &[&[(0.05, 1.0), (0.5, 0.5), (0.95, 1.0)], &[(0.5, 0.5), (0.5, 0.0)]],
    },
    Button { action: "r", from_right: true, x: 0.15, y: 0.34, radius: 0.07, label: &[BOWL, &[(0.5, 0.5), (0.92, 0.0)]] },
    Button {
        action: "z",
        from_right: true,
        x: 0.15,
        y: 0.14,
        radius: 0.065,
        label: &[&[(0.05, 1.0), (0.95, 1.0), (0.05, 0.0), (0.95, 0.0)]],
    },
    Button { action: "l", from_right: false, x: 0.15, y: 0.14, radius: 0.065, label: &[&[(0.15, 1.0), (0.15, 0.0), (0.9, 0.0)]] },
    // Start pauses the game: two bars.
    Button {
        action: "start",
        from_right: false,
        x: f32::NAN, // centred
        y: 0.1,
        radius: 0.055,
        label: &[&[(0.25, 0.0), (0.25, 1.0)], &[(0.75, 0.0), (0.75, 1.0)]],
    },
];

impl Button {
    /// Centre and radius in pixels on a screen of this size.
    fn place(&self, (width, height): (f32, f32)) -> ((f32, f32), f32) {
        let x = if self.x.is_nan() {
            width / 2.0
        } else if self.from_right {
            width - self.x * height
        } else {
            self.x * height
        };
        ((x, self.y * height), self.radius * height)
    }
}

/// What a finger on the screen is doing.
enum Finger {
    Stick { origin: (f32, f32), at: (f32, f32) },
    Button(usize),
    Look { last: (f32, f32) },
    /// On the eye button, which acted when the finger landed.
    Eye,
}

/// Centre and radius of the eye button in pixels.
fn eye((width, height): (f32, f32)) -> ((f32, f32), f32) {
    ((width / 2.0 + EYE.0 * height, EYE.1 * height), EYE.2 * height)
}

struct Touch {
    /// Whether the controls are shown: always on a phone, elsewhere once the screen is touched.
    enabled: bool,
    /// Whether the eye button has hidden the outlines. The controls work all the same.
    outlines_hidden: bool,
    fingers: Option<HashMap<u64, Finger>>,
}

static TOUCH: Mutex<Touch> = Mutex::new(Touch { enabled: false, outlines_hidden: false, fingers: None });

/// Shows the controls from the start.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn enable() {
    TOUCH.lock().unwrap().enabled = true;
}

/// Puts the controls away until the screen is next touched: a physical controller is in use.
/// Whether the touch controls are in use (no controller has taken over).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn enabled() -> bool {
    TOUCH.lock().unwrap().enabled
}

pub fn hide() {
    TOUCH.lock().unwrap().enabled = false;
}

/// Lifts every finger, for when the app leaves the screen.
pub fn release_all() {
    TOUCH.lock().unwrap().fingers = None;
    input::set_touch(0, [0.0; 2]);
}

/// The stick's deflection, -1 to 1 each way with up positive, for a thumb at `at` that
/// landed at `origin`.
fn deflection(origin: (f32, f32), at: (f32, f32), height: f32) -> [f32; 2] {
    let travel = STICK_TRAVEL * height;
    let (x, y) = ((at.0 - origin.0) / travel, (origin.1 - at.1) / travel);
    let length = x.hypot(y).max(1.0);
    [x / length, y / length]
}

/// Called from the window thread for every finger that lands, moves (`down`) or lifts.
/// `at` and `size` are in pixels.
pub fn finger(id: u64, down: bool, at: (f32, f32), size: (f32, f32)) {
    input::touch_used();
    let mut touch = TOUCH.lock().unwrap();
    touch.enabled = true;
    let touch = &mut *touch;
    let fingers = touch.fingers.get_or_insert_with(HashMap::new);
    if !down {
        fingers.remove(&id);
    } else if let Some(finger) = fingers.get_mut(&id) {
        match finger {
            Finger::Stick { at: stick_at, .. } => *stick_at = at,
            Finger::Button(_) | Finger::Eye => {}
            Finger::Look { last } => {
                let counts = (LOOK_COUNTS * 1080.0 / size.1) as f64;
                input::touch_look((at.0 - last.0) as f64 * counts, (at.1 - last.1) as f64 * counts);
                *last = at;
            }
        }
    } else {
        let button = BUTTONS.iter().position(|button| {
            let (centre, radius) = button.place(size);
            (at.0 - centre.0).hypot(at.1 - centre.1) <= radius * REACH
        });
        let stick_free = !fingers.values().any(|finger| matches!(finger, Finger::Stick { .. }));
        let (eye, eye_radius) = eye(size);
        let finger = match button {
            Some(button) => Finger::Button(button),
            None if (at.0 - eye.0).hypot(at.1 - eye.1) <= eye_radius * REACH => {
                touch.outlines_hidden = !touch.outlines_hidden;
                Finger::Eye
            }
            None if stick_free && at.0 < STICK_AREA.0 * size.0 && at.1 > STICK_AREA.1 * size.1 => {
                Finger::Stick { origin: at, at }
            }
            None => Finger::Look { last: at },
        };
        fingers.insert(id, finger);
    }

    let (mut buttons, mut stick) = (0, [0.0; 2]);
    for finger in fingers.values() {
        match finger {
            Finger::Button(button) => buttons |= input::action(BUTTONS[*button].action).unwrap_or(0),
            Finger::Stick { origin, at } => stick = deflection(*origin, *at, size.1),
            Finger::Look { .. } | Finger::Eye => {}
        }
    }
    input::set_touch(buttons, stick);
}

/// Collects triangles in pixel coordinates and hands them over in the renderer's terms.
struct Pen<'a> {
    out: &'a mut Vec<OverlayVertex>,
    size: (f32, f32),
    /// Multiplies every color's opacity.
    opacity: f32,
}

impl Pen<'_> {
    fn triangle(&mut self, corners: [(f32, f32); 3], color: [f32; 4]) {
        let color = [color[0], color[1], color[2], color[3] * self.opacity];
        for (x, y) in corners {
            self.out.push(OverlayVertex { pos: [x / self.size.0 * 2.0 - 1.0, 1.0 - y / self.size.1 * 2.0], color });
        }
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), width: f32) {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length = dx.hypot(dy).max(1e-3);
        // Half the width sideways, and as much again past each end so that joints close.
        let (nx, ny) = (-dy / length * width / 2.0, dx / length * width / 2.0);
        let (ex, ey) = (dx / length * width / 2.0, dy / length * width / 2.0);
        let (from, to) = ((from.0 - ex, from.1 - ey), (to.0 + ex, to.1 + ey));
        let corners = [(from.0 + nx, from.1 + ny), (from.0 - nx, from.1 - ny), (to.0 - nx, to.1 - ny), (to.0 + nx, to.1 + ny)];
        self.triangle([corners[0], corners[1], corners[2]], LINE);
        self.triangle([corners[0], corners[2], corners[3]], LINE);
    }

    fn circle(&mut self, centre: (f32, f32), radius: f32, width: f32, filled: bool) {
        const STEPS: usize = 40;
        let point = |i: usize| {
            let angle = i as f32 / STEPS as f32 * std::f32::consts::TAU;
            (centre.0 + radius * angle.cos(), centre.1 + radius * angle.sin())
        };
        for i in 0..STEPS {
            if filled {
                self.triangle([centre, point(i), point(i + 1)], FILL);
            }
            // A ring: the band between two radii, so that segments do not overlap.
            let (inner, outer) = (1.0 - width / 2.0 / radius, 1.0 + width / 2.0 / radius);
            let scaled = |p: (f32, f32), k: f32| (centre.0 + (p.0 - centre.0) * k, centre.1 + (p.1 - centre.1) * k);
            let (a, b) = (point(i), point(i + 1));
            self.triangle([scaled(a, inner), scaled(a, outer), scaled(b, outer)], LINE);
            self.triangle([scaled(a, inner), scaled(b, outer), scaled(b, inner)], LINE);
        }
    }
}

/// The game's own mouse pointer, in window pixels: where a platform hands over the mouse's
/// raw movement and shows no pointer itself (Android). None until a mouse has moved.
static CURSOR: Mutex<Option<(f32, f32)>> = Mutex::new(None);

/// The screen height on which one count of the mouse moves the pointer one pixel, as
/// Windows does at its default pointer speed without acceleration.
const CURSOR_SCREEN: f32 = 1080.0;

/// Moves the pointer by the mouse's raw movement in counts, in a window of `size` pixels.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn move_cursor(dx: f32, dy: f32, size: (f32, f32)) {
    let scale = size.1 / CURSOR_SCREEN;
    let mut cursor = CURSOR.lock().unwrap();
    let at = cursor.unwrap_or((size.0 / 2.0, size.1 / 2.0));
    let at = ((at.0 + dx * scale).clamp(0.0, (size.0 - 1.0).max(0.0)), (at.1 + dy * scale).clamp(0.0, (size.1 - 1.0).max(0.0)));
    *cursor = Some(at);
    drop(cursor);
    crate::menu::cursor_moved(at, size);
    crate::render::show_soon();
}

/// Puts the pointer where the system's own was last reported.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn place_cursor(at: (f32, f32), size: (f32, f32)) {
    *CURSOR.lock().unwrap() = Some(at);
    crate::menu::cursor_moved(at, size);
    crate::render::show_soon();
}

/// The pointer, an arrow with its tip at the origin, in units of its height: the head's
/// three corners, then the tail's four.
const ARROW: [(f32, f32); 7] = [(0.0, 0.0), (0.0, 0.8), (0.56, 0.56), (0.15, 0.6), (0.33, 1.0), (0.46, 0.94), (0.28, 0.54)];

/// Draws the pointer while the keyboard and mouse are in use and the mouse is not turning
/// the camera.
fn draw_cursor(size: (f32, f32), out: &mut Vec<OverlayVertex>) {
    let Some(tip) = *CURSOR.lock().unwrap() else { return };
    if !input::keyboard_in_use() || input::wants_mouse() {
        return;
    }
    let tall = size.1 * 0.03;
    let mut pen = Pen { out, size, opacity: 1.0 };
    let mut arrow = |by: (f32, f32), color: [f32; 4]| {
        let at = |i: usize| (tip.0 + ARROW[i].0 * tall + by.0, tip.1 + ARROW[i].1 * tall + by.1);
        pen.triangle([at(0), at(1), at(2)], color);
        pen.triangle([at(3), at(4), at(5)], color);
        pen.triangle([at(3), at(5), at(6)], color);
    };
    // A black edge: the arrow in black, nudged every way, under the white one.
    let edge = (tall * 0.06).max(1.5);
    for step in 0..16 {
        let angle = step as f32 / 16.0 * std::f32::consts::TAU;
        arrow((edge * angle.cos(), edge * angle.sin()), [0.0, 0.0, 0.0, 1.0]);
    }
    arrow((0.0, 0.0), [1.0, 1.0, 1.0, 1.0]);
}

/// Appends the controls' triangles for a window of `size` pixels; nothing unless enabled.
pub fn overlay(size: (f32, f32), out: &mut Vec<OverlayVertex>) {
    draw_cursor(size, out);
    let touch = TOUCH.lock().unwrap();
    if !touch.enabled {
        return;
    }
    let height = size.1;
    let width = (height * 0.007).max(2.0);
    let mut pen = Pen { out, size, opacity: if touch.outlines_hidden { FAINT } else { 1.0 } };
    let fingers = touch.fingers.as_ref();

    let (centre, radius) = eye(size);
    pen.circle(centre, radius, width, false);
    let at = |(x, y): (f32, f32)| (centre.0 + (x - 0.5) * radius * 0.8, centre.1 - (y - 0.5) * radius);
    for stroke in EYE_LABEL {
        for pair in stroke.windows(2) {
            pen.line(at(pair[0]), at(pair[1]), width * 0.7);
        }
    }
    if touch.outlines_hidden {
        return;
    }
    let held = |index: usize| fingers.is_some_and(|f| f.values().any(|f| matches!(f, Finger::Button(i) if *i == index)));

    for (index, button) in BUTTONS.iter().enumerate() {
        let (centre, radius) = button.place(size);
        pen.circle(centre, radius, width, held(index));
        // The label fills a box 0.8 radii wide and one radius high.
        let at = |(x, y): (f32, f32)| (centre.0 + (x - 0.5) * radius * 0.8, centre.1 - (y - 0.5) * radius);
        for stroke in button.label {
            for pair in stroke.windows(2) {
                pen.line(at(pair[0]), at(pair[1]), width);
            }
        }
    }

    let stick = fingers.and_then(|f| {
        f.values().find_map(|f| match f {
            Finger::Stick { origin, at } => Some((*origin, *at)),
            _ => None,
        })
    });
    let rest = (STICK_REST.0 * height, STICK_REST.1 * height);
    let (origin, push) = stick.map_or((rest, [0.0; 2]), |(origin, at)| (origin, deflection(origin, at, height)));
    let travel = STICK_TRAVEL * height;
    pen.circle(origin, travel, width, false);
    pen.circle((origin.0 + push[0] * travel, origin.1 - push[1] * travel), travel * 0.45, width, true);
}
