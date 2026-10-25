//! Controller input from the keyboard, the mouse and gamepads. The window thread records what
//! is held and how the mouse moved, a gamepad thread records the state of the controller last
//! touched, and the game thread reads all of it as GameCube controller 1 whenever it polls
//! the pad.
//!
//! Keyboard and mouse bindings come from config/controls.txt (relative to the working
//! directory), falling back to the copy built into the executable. The gamepad layout is fixed
//! (see `gamepad_state`).
//!
//! Whichever was used last decides how the camera behaves: after the keyboard or mouse it is
//! the mouse-look camera in camera.rs, after a gamepad it is the game's own.
//!
//! In menus the mouse points and clicks instead (menu.rs), and texts picture keys instead of
//! the controller's buttons while the keyboard and mouse are in use (glyph.rs).
//!
//! TSSM_INPUT scripts presses for unattended runs: "a:400,start:900" holds A for a few
//! frames starting at retrace 400, then Start at retrace 900. TSSM_MOUSE does the same for
//! the mouse in menus (see `script` in menu.rs).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use winit::event::MouseButton;
use winit::keyboard::KeyCode;

// PADStatus button bits.
pub(crate) const LEFT: u32 = 0x0001;
pub(crate) const RIGHT: u32 = 0x0002;
pub(crate) const DOWN: u32 = 0x0004;
pub(crate) const UP: u32 = 0x0008;
pub(crate) const Z: u32 = 0x0010;
pub(crate) const R: u32 = 0x0020;
pub(crate) const L: u32 = 0x0040;
pub(crate) const A: u32 = 0x0100;
pub(crate) const B: u32 = 0x0200;
pub(crate) const X: u32 = 0x0400;
pub(crate) const Y: u32 = 0x0800;
pub(crate) const START: u32 = 0x1000;
// Host-only bits above the 16 button bits: stick directions.
pub(crate) const STICK_LEFT: u32 = 1 << 16;
pub(crate) const STICK_RIGHT: u32 = 1 << 17;
pub(crate) const STICK_DOWN: u32 = 1 << 18;
pub(crate) const STICK_UP: u32 = 1 << 19;
const C_LEFT: u32 = 1 << 20;
const C_RIGHT: u32 = 1 << 21;
const C_DOWN: u32 = 1 << 22;
const C_UP: u32 = 1 << 23;
// Scripted mouse movement, for unattended tests of mouse look.
const LOOK_RIGHT: u32 = 1 << 24;
const LOOK_LEFT: u32 = 1 << 25;
const LOOK_DOWN: u32 = 1 << 26;
const LOOK_UP: u32 = 1 << 27;
/// Mouse counts a scripted look direction contributes per camera update.
const SCRIPTED_LOOK_COUNTS: f64 = 40.0;
/// The game's mode value while a level is running (as opposed to menus, pause, movies).
const MODE_IN_GAME: u32 = 12;

/// How far a held key pushes a stick.
const KEY_DEFLECTION: i32 = 80;
/// How long a scripted press is held, in retraces.
const SCRIPT_HOLD: u32 = 8;

/// Something the player can press.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    Key(KeyCode),
    Mouse(u8),
}

struct Controls {
    bindings: HashMap<Source, u32>,
    /// The bindings in the order the file gives them.
    order: Vec<(Source, u32)>,
    /// The same two while a car is being driven: the `driving` lines of the file stand in
    /// for the ordinary bindings of their keys.
    driving: HashMap<Source, u32>,
    driving_order: Vec<(Source, u32)>,
    sensitivity: f32,
    invert_y: bool,
    level_tilt: f32,
}

const DEFAULT_CONTROLS: &str = include_str!("../../../config/controls.txt");

static HELD: AtomicU32 = AtomicU32::new(0);
/// What the same keys and mouse buttons hold by the driving bindings.
static HELD_DRIVING: AtomicU32 = AtomicU32::new(0);
/// When the game last updated a car being driven (milliseconds on `clock`, 0 for never).
static DRIVING_SEEN: AtomicU64 = AtomicU64::new(0);
/// Whether the window currently owns the mouse; movement only counts while it does.
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Mouse movement the camera has not consumed yet, in device counts.
static LOOK: Mutex<[f64; 2]> = Mutex::new([0.0; 2]);
/// What the game last reported: its mode and state variables, and when its gameplay camera
/// last updated (milliseconds on `clock`, 0 for never).
static GAME_MODE: AtomicU32 = AtomicU32::new(u32::MAX);
static GAME_STATE: AtomicU32 = AtomicU32::new(u32::MAX);
static CAMERA_SEEN: AtomicU64 = AtomicU64::new(0);
/// Look directions currently held by the test script.
static SCRIPTED_LOOK: AtomicU32 = AtomicU32::new(0);
/// What the gamepad last touched is holding.
static GAMEPAD: Mutex<Gamepad> = Mutex::new(Gamepad { buttons: 0, sticks: [0; 4], triggers: [0; 2] });
/// What fingers on the touch screen are holding (touch.rs): button bits and the control stick.
static TOUCH: Mutex<(u32, [i8; 2])> = Mutex::new((0, [0; 2]));
/// Momentary presses waiting their turn (see `pulse`).
struct Pulses {
    queue: std::collections::VecDeque<u32>,
    /// The press in progress and how many pad polls of it are left, the gap after it included.
    current: u32,
    polls: u32,
}

static PULSES: Mutex<Pulses> = Mutex::new(Pulses { queue: std::collections::VecDeque::new(), current: 0, polls: 0 });
/// How many pad polls a momentary press is held, and how many pass before the next.
const PULSE_HOLD: u32 = 3;
const PULSE_GAP: u32 = 3;
/// Wheel movement not yet turned into presses, in notches.
static WHEEL: Mutex<f32> = Mutex::new(0.0);
/// Whether a gamepad was used more recently than the keyboard or mouse.
static GAMEPAD_LAST: AtomicBool = AtomicBool::new(false);

/// A gamepad's state in GameCube terms.
#[derive(Clone, Copy)]
struct Gamepad {
    /// PADStatus button bits.
    buttons: u32,
    /// Control stick x, y, then C-stick x, y.
    sticks: [i8; 4],
    /// L and R.
    triggers: [u8; 2],
}

/// How far a fully pushed gamepad stick reads, in GameCube stick units.
const STICK_RANGE: f32 = 100.0;
/// How far off an axis the camera stick may be pushed and still count as that axis alone, as
/// the ratio of the smaller deflection to the larger (0.7 is about 35 degrees).
const CAMERA_AXIS_SNAP: f32 = 0.7;
/// Mouse movement in one event, in counts, that takes the camera back from a gamepad. Smaller
/// movements are taken to be the desk being bumped.
const MOUSE_TAKEOVER: f64 = 3.0;

/// Whether the camera should follow the mouse (camera.rs) rather than behave as in the original
/// game: true unless a gamepad was the last thing used, or TSSM_ORIGINAL_CAMERA is set.
pub fn mouse_camera() -> bool {
    static FORCED_OFF: OnceLock<bool> = OnceLock::new();
    !GAMEPAD_LAST.load(Ordering::Relaxed) && !*FORCED_OFF.get_or_init(|| std::env::var_os("TSSM_ORIGINAL_CAMERA").is_some())
}

/// Reads a gamepad as a GameCube controller: the bottom face button is A, the right one
/// (B on Xbox, circle on PlayStation) B, the left one X, the top one Y. The triggers are L and R, both shoulder buttons
/// and the back/select button are Z.
fn gamepad_state(pad: &gilrs::Gamepad) -> Gamepad {
    use gilrs::{Axis, Button};
    const BUTTONS: [(Button, u32); 12] = [
        (Button::South, A),
        (Button::East, B),
        (Button::West, X),
        (Button::North, Y),
        (Button::LeftTrigger, Z),
        (Button::RightTrigger, Z),
        (Button::Select, Z),
        (Button::Start, START),
        (Button::DPadUp, UP),
        (Button::DPadDown, DOWN),
        (Button::DPadLeft, LEFT),
        (Button::DPadRight, RIGHT),
    ];
    let buttons = BUTTONS.iter().filter(|(button, _)| pad.is_pressed(*button)).fold(0, |bits, (_, bit)| bits | bit);
    let trigger = |button: Button| {
        let analog = pad.button_data(button).map_or(0.0, |data| data.value());
        let pressed = if pad.is_pressed(button) { 1.0 } else { 0.0 };
        analog.max(pressed)
    };
    gamepad_from(
        buttons,
        [pad.value(Axis::LeftStickX), pad.value(Axis::LeftStickY)],
        [pad.value(Axis::RightStickX), pad.value(Axis::RightStickY)],
        [trigger(Button::LeftTrigger2), trigger(Button::RightTrigger2)],
    )
}

/// A gamepad's state from its parts: button bits, the left and right sticks from -1 to 1
/// (right and up positive) and the L and R triggers from 0 to 1.
fn gamepad_from(mut buttons: u32, stick: [f32; 2], camera: [f32; 2], triggers: [f32; 2]) -> Gamepad {
    let triggers = triggers.map(|value| (value.clamp(0.0, 1.0) * 255.0) as u8);
    // The GameCube's triggers click at the end of their travel, which is what sets the bit.
    for (value, bit) in triggers.iter().zip([L, R]) {
        if *value > 160 {
            buttons |= bit;
        }
    }
    // The game turns the camera with one axis of the C-stick and moves it in and out with the
    // other, so a push that is mostly along one axis is read as that axis alone. Otherwise
    // turning with a thumb that is slightly off level also zooms.
    let [mut camera_x, mut camera_y] = camera;
    if camera_y.abs() < camera_x.abs() * CAMERA_AXIS_SNAP {
        camera_y = 0.0;
    } else if camera_x.abs() < camera_y.abs() * CAMERA_AXIS_SNAP {
        camera_x = 0.0;
    }
    let scaled = |value: f32| (value * STICK_RANGE).clamp(-STICK_RANGE, STICK_RANGE) as i8;
    Gamepad {
        buttons,
        sticks: [scaled(stick[0]), scaled(stick[1]), scaled(camera_x), scaled(camera_y)],
        triggers,
    }
}

/// Records what the gamepad holds. One that is being used (or was just connected) takes the
/// camera and puts the on-screen controls away.
fn gamepad_update(state: Gamepad, connected: bool) {
    let in_use = state.buttons != 0 || state.sticks.iter().any(|v| v.unsigned_abs() > 30);
    if in_use || connected {
        GAMEPAD_LAST.store(true, Ordering::Relaxed);
        crate::touch::hide();
    }
    *GAMEPAD.lock().unwrap() = state;
}

/// For platforms that read controllers themselves (Android): what the controller touched
/// last holds, in the terms of `gamepad_from`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_gamepad(buttons: u32, stick: [f32; 2], camera: [f32; 2], triggers: [f32; 2]) {
    gamepad_update(gamepad_from(buttons, stick, camera, triggers), false);
}

/// Whether the game has the first controller's rumble motor running. It only starts it with
/// rumble on in its settings.
static MOTOR: AtomicBool = AtomicBool::new(false);

/// Called when the game starts or stops the rumble motor.
pub fn set_motor(on: bool) {
    if MOTOR.swap(on, Ordering::Relaxed) != on {
        if !on {
            MOTOR_OFF_AT.store(clock(), Ordering::Relaxed);
        }
        if std::env::var_os("TSSM_TRACE_INPUT").is_some() {
            eprintln!("[input] rumble {}", if on { "on" } else { "off" });
        }
    }
}

/// When the motor was last stopped (milliseconds on `clock`).
static MOTOR_OFF_AT: AtomicU64 = AtomicU64::new(0);

/// Whether the rumble motor is running. The game makes a weak rumble by switching the motor
/// on and off every few frames, which a real motor's inertia smooths out: so do we, by
/// running on through gaps that short.
pub fn motor() -> bool {
    let stopped = MOTOR_OFF_AT.load(Ordering::Relaxed);
    MOTOR.load(Ordering::Relaxed) || (stopped != 0 && clock().saturating_sub(stopped) < 70)
}

/// Whether a gamepad, rather than the keyboard, the mouse or the touch screen, was used last.
pub fn gamepad_last() -> bool {
    GAMEPAD_LAST.load(Ordering::Relaxed)
}

/// A rumble that lasts until it is stopped, on every gamepad that can rumble.
fn rumble_effect(gilrs: &mut gilrs::Gilrs) -> Option<gilrs::ff::Effect> {
    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Repeat, Replay, Ticks};
    let pads: Vec<_> = gilrs.gamepads().filter(|(_, pad)| pad.is_ff_supported()).map(|(id, _)| id).collect();
    if pads.is_empty() {
        return None;
    }
    let part = |kind| BaseEffect { kind, scheduling: Replay { play_for: Ticks::from_ms(500), ..Default::default() }, ..Default::default() };
    EffectBuilder::new()
        .add_effect(part(BaseEffectType::Strong { magnitude: 48_000 }))
        .add_effect(part(BaseEffectType::Weak { magnitude: 32_000 }))
        .repeat(Repeat::Infinitely)
        .gamepads(&pads)
        .finish(gilrs)
        .map_err(|e| eprintln!("[input] rumble unavailable: {e}"))
        .ok()
}

/// Starts watching for gamepads on a thread of its own. They can be plugged in at any time.
pub fn start_gamepads() {
    use gilrs::EventType;
    let watch = || {
        let mut gilrs = match gilrs::Gilrs::new() {
            Ok(gilrs) => gilrs,
            Err(e) => return eprintln!("[input] gamepads unavailable: {e}"),
        };
        let mut effect: Option<gilrs::ff::Effect> = None;
        let mut rumbling = false;
        loop {
            // Waking a few times a frame keeps the rumble in step with the game.
            if let Some(gilrs::Event { id, event, .. }) = gilrs.next_event_blocking(Some(std::time::Duration::from_millis(8))) {
                let pad = gilrs.gamepad(id);
                match event {
                    EventType::Connected => eprintln!("[input] controller connected: {}", pad.name()),
                    EventType::Disconnected => eprintln!("[input] controller disconnected: {}", pad.name()),
                    _ => {}
                }
                if matches!(event, EventType::Connected | EventType::Disconnected) {
                    // The rumble is made anew for the gamepads there are now.
                    effect = None;
                    rumbling = false;
                }
                if event == EventType::Disconnected {
                    *GAMEPAD.lock().unwrap() = Gamepad { buttons: 0, sticks: [0; 4], triggers: [0; 2] };
                } else {
                    gamepad_update(gamepad_state(&pad), matches!(event, EventType::Connected));
                }
            }
            // Only the gamepad in the player's hands rumbles, not one lying beside the keyboard.
            let wanted = motor() && gamepad_last();
            if wanted != rumbling {
                rumbling = wanted;
                if wanted && effect.is_none() {
                    effect = rumble_effect(&mut gilrs);
                }
                if let Some(effect) = &effect {
                    if let Err(e) = if wanted { effect.play() } else { effect.stop() } {
                        eprintln!("[input] rumble failed: {e}");
                    }
                }
            }
        }
    };
    std::thread::Builder::new().name("gamepads".into()).spawn(watch).expect("cannot start the gamepad thread");
}

fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Runs before zCar::CarUpdate, once a frame while a car is being driven.
pub fn note_driving() {
    DRIVING_SEEN.store(clock(), Ordering::Relaxed);
}

/// Whether a car is being driven: the driving bindings apply.
fn driving() -> bool {
    let seen = DRIVING_SEEN.load(Ordering::Relaxed);
    seen != 0 && clock().saturating_sub(seen) < 250
}

/// Called by the camera hook each time the game updates its gameplay camera.
pub fn note_camera_update() {
    CAMERA_SEEN.store(clock(), Ordering::Relaxed);
}

/// Called once per pad poll with the game's mode and state variables.
pub fn note_game_state(mode: u32, state: u32) {
    let mode_changed = GAME_MODE.swap(mode, Ordering::Relaxed) != mode;
    let state_changed = GAME_STATE.swap(state, Ordering::Relaxed) != state;
    let changed = mode_changed || state_changed;
    if changed && std::env::var_os("TSSM_TRACE_INPUT").is_some() {
        eprintln!("[input] game mode {mode}, state {state}, gameplay camera active: {}", camera_active());
    }
}

/// Whether the gameplay camera has updated within the last quarter second.
fn camera_active() -> bool {
    let seen = CAMERA_SEEN.load(Ordering::Relaxed);
    seen != 0 && clock().saturating_sub(seen) < 250
}

/// Whether the game is in a state where the mouse should be hidden and held: a level is
/// running and no menu is on show. The title screen, the pause screen and movies all report
/// a different mode. Levels whose camera the game holds still (a boss fight) count too: the
/// cursor has nothing to do there, though the mouse steers nothing either.
pub fn wants_mouse() -> bool {
    GAME_MODE.load(Ordering::Relaxed) == MODE_IN_GAME && !crate::menu::active()
}

/// How much of the extra downward tilt that levels ask of the camera is applied, 0 to 1.
pub fn level_tilt() -> f32 {
    controls().level_tilt
}

/// The mouse's movement since the last call, scaled by the sensitivity setting, as
/// (rightward, downward) counts; downward is negated when vertical look is inverted.
pub fn take_look() -> (f32, f32) {
    let controls = controls();
    let mut moved = std::mem::take(&mut *LOOK.lock().unwrap());
    let scripted = SCRIPTED_LOOK.load(Ordering::Relaxed);
    let direction = |negative: u32, positive: u32| ((scripted & positive != 0) as i32 - (scripted & negative != 0) as i32) as f64;
    moved[0] += direction(LOOK_LEFT, LOOK_RIGHT) * SCRIPTED_LOOK_COUNTS;
    moved[1] += direction(LOOK_UP, LOOK_DOWN) * SCRIPTED_LOOK_COUNTS;
    let vertical = if controls.invert_y { -moved[1] } else { moved[1] };
    (moved[0] as f32 * controls.sensitivity, vertical as f32 * controls.sensitivity)
}

/// When the keyboard and mouse last took over from a gamepad (milliseconds on `clock`).
static KEYBOARD_SINCE: AtomicU64 = AtomicU64::new(0);

/// Whether the touch screen, rather than the keyboard and mouse, was used last. A phone
/// starts out that way.
static TOUCH_LAST: AtomicBool = AtomicBool::new(cfg!(target_os = "android"));

/// Notes that the keyboard or the mouse was just used. The touch controls are put away.
fn keyboard_used() {
    let from_gamepad = GAMEPAD_LAST.swap(false, Ordering::Relaxed);
    let from_touch = TOUCH_LAST.swap(false, Ordering::Relaxed);
    if from_gamepad || from_touch {
        KEYBOARD_SINCE.store(clock(), Ordering::Relaxed);
    }
    if from_touch {
        crate::touch::hide();
    }
}

/// Notes that the touch screen was just used. Every finger counts, also one that lands on
/// no control: it brings the touch controls back, and the next key or mouse movement has to
/// know to put them away again.
pub fn touch_used() {
    GAMEPAD_LAST.store(false, Ordering::Relaxed);
    TOUCH_LAST.store(true, Ordering::Relaxed);
}

/// For platforms that read the mouse themselves (Android): the mouse moved or was pressed.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn mouse_used() {
    keyboard_used();
}

/// Whether key pictures may stand in for the controller's buttons: the keyboard and mouse
/// are in use and have been for a moment. A frame is put together in steps, and one that
/// the change of device falls into would otherwise show the gamepad's hints with keys in
/// them.
pub fn key_pictures() -> bool {
    keyboard_in_use() && clock().saturating_sub(KEYBOARD_SINCE.load(Ordering::Relaxed)) > 150
}

/// Whether the keyboard and mouse are what the player last used, so that texts should
/// picture keys rather than the controller's buttons. Not with the touch controls, whose
/// buttons are drawn as the controller's.
pub fn keyboard_in_use() -> bool {
    !GAMEPAD_LAST.load(Ordering::Relaxed) && !TOUCH_LAST.load(Ordering::Relaxed)
}

/// What a key is called on its picture (glyph.rs has capital letters, digits and arrows).
fn key_name(key: KeyCode) -> Option<String> {
    use KeyCode::*;
    Some(
        match key {
            Space => "SPACE",
            Tab => "TAB",
            Escape => "ESC",
            Enter => "ENTER",
            ShiftLeft => "SHIFT",
            ControlLeft => "CTRL",
            AltLeft => "ALT",
            Backspace => "BKSP",
            ArrowUp => "^",
            ArrowDown => "v",
            ArrowLeft => "<",
            ArrowRight => ">",
            _ => {
                // The rest are named as the bindings file names them: KeyW, Digit4, F5.
                let name = format!("{key:?}");
                let short = name.strip_prefix("Key").or_else(|| name.strip_prefix("Digit")).unwrap_or(&name);
                return (short.len() <= 3).then(|| short.to_ascii_uppercase());
            }
        }
        .to_string(),
    )
}

/// What stands for one of the controller's buttons on the keyboard and mouse: the first key
/// bound to it in controls.txt, or failing that the mouse button bound to it. While a menu
/// is on show the mouse's buttons are not their bindings (the left one clicks) and Escape
/// goes back, so there B is Escape and anything else without a key of its own is the left
/// button: its hint can be clicked.
pub fn label(button: crate::glyph::Button, in_menu: bool) -> Option<crate::glyph::Label> {
    use crate::glyph::{Button, Label};
    let controls = controls();
    let order = if driving() { &controls.driving_order } else { &controls.order };
    let key_for = |bits: u32| {
        order.iter().find_map(|&(source, bound)| match source {
            Source::Key(key) if bound & bits != 0 => key_name(key),
            _ => None,
        })
    };
    let bits = match button {
        Button::A => A,
        Button::B => B,
        Button::X => X,
        Button::Y => Y,
        Button::L => L,
        Button::R => R,
        Button::Z => Z,
        Button::Start => START,
        // The camera follows the mouse.
        Button::CStick => return Some(Label::Mouse(3)),
        Button::Stick => {
            let keys: Option<Vec<String>> = [STICK_UP, STICK_LEFT, STICK_DOWN, STICK_RIGHT].into_iter().map(key_for).collect();
            let keys = keys?;
            let letters = keys.iter().all(|key| key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric());
            return Some(Label::Key(if letters { keys.concat() } else { "KEYS".to_string() }));
        }
    };
    if let Some(key) = key_for(bits) {
        return Some(Label::Key(key));
    }
    if in_menu {
        // B is Escape there (see `key_event`).
        return Some(if button == Button::B { Label::Key("ESC".to_string()) } else { Label::Mouse(0) });
    }
    order.iter().find_map(|&(source, bound)| match source {
        Source::Mouse(index) if bound & bits != 0 => Some(Label::Mouse(index)),
        _ => None,
    })
}

pub fn action(name: &str) -> Option<u32> {
    Some(match name {
        "a" => A,
        "b" => B,
        "x" => X,
        "y" => Y,
        "z" => Z,
        "l" => L,
        "r" => R,
        "start" => START,
        "stick_up" | "up" => STICK_UP,
        "stick_down" | "down" => STICK_DOWN,
        "stick_left" | "left" => STICK_LEFT,
        "stick_right" | "right" => STICK_RIGHT,
        "c_up" => C_UP,
        "c_down" => C_DOWN,
        "c_left" => C_LEFT,
        "c_right" => C_RIGHT,
        "dpad_up" => UP,
        "dpad_down" => DOWN,
        "dpad_left" => LEFT,
        "dpad_right" => RIGHT,
        "look_right" => LOOK_RIGHT,
        "look_left" => LOOK_LEFT,
        "look_down" => LOOK_DOWN,
        "look_up" => LOOK_UP,
        _ => return None,
    })
}

fn source(name: &str) -> Option<Source> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    const FUNCTION: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];

    let name = name.to_ascii_lowercase();
    if let [ch] = name.as_bytes() {
        return match ch {
            b'a'..=b'z' => Some(Source::Key(LETTERS[(ch - b'a') as usize])),
            b'0'..=b'9' => Some(Source::Key(DIGITS[(ch - b'0') as usize])),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
        return FUNCTION.get(n.wrapping_sub(1)).map(|&key| Source::Key(key));
    }
    Some(match name.as_str() {
        "space" => Source::Key(Space),
        "tab" => Source::Key(Tab),
        "escape" | "esc" => Source::Key(Escape),
        "enter" | "return" => Source::Key(Enter),
        "shift" => Source::Key(ShiftLeft),
        "ctrl" | "control" => Source::Key(ControlLeft),
        "alt" => Source::Key(AltLeft),
        "backspace" => Source::Key(Backspace),
        "arrowup" => Source::Key(ArrowUp),
        "arrowdown" => Source::Key(ArrowDown),
        "arrowleft" => Source::Key(ArrowLeft),
        "arrowright" => Source::Key(ArrowRight),
        "mouseleft" => Source::Mouse(0),
        "mouseright" => Source::Mouse(1),
        "mousemiddle" => Source::Mouse(2),
        _ => return None,
    })
}

fn parse(text: &str) -> Controls {
    let mut controls = Controls {
        bindings: HashMap::new(),
        order: Vec::new(),
        driving: HashMap::new(),
        driving_order: Vec::new(),
        sensitivity: 5.0,
        invert_y: false,
        level_tilt: 0.35,
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap().trim();
        let Some((name, value)) = line.split_once('=') else { continue };
        let (name, value) = (name.trim(), value.trim().to_ascii_lowercase());
        let understood = match name.to_ascii_lowercase().as_str() {
            "mouse_sensitivity" => value.parse().map(|v| controls.sensitivity = v).is_ok(),
            "camera_level_tilt" => value.parse().map(|v: f32| controls.level_tilt = v.clamp(0.0, 1.0)).is_ok(),
            "invert_y" => {
                controls.invert_y = matches!(value.as_str(), "yes" | "true" | "1" | "on");
                true
            }
            other if other.starts_with("driving ") => match (source(other["driving ".len()..].trim()), action(&value)) {
                (Some(source), Some(bits)) => {
                    *controls.driving.entry(source).or_insert(0) |= bits;
                    controls.driving_order.push((source, bits));
                    true
                }
                _ => false,
            },
            _ => match (source(name), action(&value)) {
                (Some(source), Some(bits)) => {
                    *controls.bindings.entry(source).or_insert(0) |= bits;
                    controls.order.push((source, bits));
                    true
                }
                _ => false,
            },
        };
        if !understood {
            eprintln!("[input] controls.txt: cannot understand `{line}`");
        }
    }
    // Every key without a driving line of its own does when driving what it always does.
    for &(source, bits) in &controls.order {
        if !controls.driving_order.iter().any(|&(driving, _)| driving == source) {
            *controls.driving.entry(source).or_insert(0) |= bits;
        }
    }
    let kept: Vec<_> = controls.order.iter().copied().filter(|(source, _)| !controls.driving_order.iter().any(|(driving, _)| driving == source)).collect();
    controls.driving_order.extend(kept);
    controls
}

fn controls() -> &'static Controls {
    static CONTROLS: OnceLock<Controls> = OnceLock::new();
    CONTROLS.get_or_init(|| match std::fs::read_to_string("config/controls.txt") {
        Ok(text) => parse(&text),
        Err(_) => parse(DEFAULT_CONTROLS),
    })
}

fn set_held(source: Source, pressed: bool) {
    let bits = controls().bindings.get(&source).copied().unwrap_or(0);
    let driving = controls().driving.get(&source).copied().unwrap_or(0);
    if pressed {
        if bits | driving != 0 {
            keyboard_used();
        }
        HELD.fetch_or(bits, Ordering::Relaxed);
        HELD_DRIVING.fetch_or(driving, Ordering::Relaxed);
    } else {
        HELD.fetch_and(!bits, Ordering::Relaxed);
        HELD_DRIVING.fetch_and(!driving, Ordering::Relaxed);
    }
}

/// Called from the window thread on every key press or release.
pub fn key_event(key: KeyCode, pressed: bool) {
    if pressed && controls().bindings.contains_key(&Source::Key(key)) {
        crate::menu::key_pressed();
    }
    // The title screen waits for any key, and any key skips a movie.
    if pressed && (crate::menu::on_title() || crate::menu::in_movie()) {
        pulse(START);
        return;
    }
    // In a menu Escape goes back a screen (B), and from the pause screen back to the game,
    // whatever it is bound to.
    if pressed && key == KeyCode::Escape && crate::menu::escape_goes_back() {
        pulse(B);
        return;
    }
    set_held(Source::Key(key), pressed);
}

/// Presses buttons for a moment on the player's behalf: mouse clicks and the wheel in menus.
/// Presses queue up, each held for a few pad polls with a gap before the next.
pub fn pulse(bits: u32) {
    keyboard_used();
    let mut pulses = PULSES.lock().unwrap();
    if pulses.queue.len() < 12 {
        pulses.queue.push_back(bits);
    }
}

/// For tests: makes the gamepad, or the keyboard and mouse, the last thing used.
pub fn set_gamepad_last(gamepad: bool) {
    if gamepad {
        GAMEPAD_LAST.store(true, Ordering::Relaxed);
    } else {
        keyboard_used();
    }
}

/// Whether every momentary press has been made and released.
pub fn pulses_idle() -> bool {
    let pulses = PULSES.lock().unwrap();
    pulses.queue.is_empty() && pulses.polls == 0
}

/// Called from the window thread when the mouse wheel turns, in notches (up positive).
/// Outside gameplay it presses Up or Down, a notch a press.
pub fn mouse_wheel(notches: f32) {
    if wants_mouse() {
        return;
    }
    let mut pending = WHEEL.lock().unwrap();
    *pending += notches;
    while pending.abs() >= 1.0 {
        pulse(if *pending > 0.0 { STICK_UP } else { STICK_DOWN });
        *pending -= pending.signum();
    }
}

/// Called from the window thread on every mouse button press or release. While a menu is on
/// show the left button clicks what is under the cursor and the right one does nothing,
/// whatever they are bound to.
pub fn mouse_button(button: MouseButton, pressed: bool) {
    let index = match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        _ => return,
    };
    if index == 0 {
        crate::menu::left_button(pressed);
    }
    if pressed && (crate::menu::on_title() || crate::menu::in_movie()) {
        pulse(START);
        return;
    }
    if pressed && index < 2 && crate::menu::active() {
        if index == 0 {
            crate::menu::click();
        }
        return;
    }
    set_held(Source::Mouse(index), pressed);
}

/// With TSSM_TRACE_INPUT, logs once a second how far the mouse has moved and in how many
/// reports, to compare one platform with another.
fn trace_mouse(dx: f64, dy: f64) {
    static TRACING: OnceLock<bool> = OnceLock::new();
    static MOVED: Mutex<(f64, f64, u32, u64)> = Mutex::new((0.0, 0.0, 0, 0));
    if !*TRACING.get_or_init(|| std::env::var_os("TSSM_TRACE_INPUT").is_some()) {
        return;
    }
    let mut moved = MOVED.lock().unwrap();
    let now = clock();
    if now.saturating_sub(moved.3) > 1000 {
        if moved.2 != 0 {
            eprintln!("[input] mouse moved {:.0} across, {:.0} down in {} reports", moved.0, moved.1, moved.2);
        }
        *moved = (0.0, 0.0, 0, now);
    }
    *moved = (moved.0 + dx.abs(), moved.1 + dy.abs(), moved.2 + 1, moved.3);
}

/// Called from the window thread with raw mouse movement in device units.
pub fn mouse_motion(dx: f64, dy: f64) {
    trace_mouse(dx, dy);
    if CAPTURED.load(Ordering::Relaxed) {
        if dx.abs() + dy.abs() >= MOUSE_TAKEOVER {
            keyboard_used();
        }
        // Movement while the gameplay camera is not running would all arrive at once when it
        // starts again.
        if camera_active() {
            let mut look = LOOK.lock().unwrap();
            look[0] += dx;
            look[1] += dy;
        }
    }
}

/// Called from the window thread with what the on-screen controls are holding: button bits
/// as `action` gives them, and the control stick from -1 to 1 (right and up positive).
pub fn set_touch(buttons: u32, stick: [f32; 2]) {
    if buttons != 0 || stick != [0.0; 2] {
        touch_used();
    }
    *TOUCH.lock().unwrap() = (buttons, stick.map(|v| (v * STICK_RANGE).clamp(-STICK_RANGE, STICK_RANGE) as i8));
}

/// Called from the window thread when a finger drags across the picture: turns the camera
/// as the mouse does, in mouse counts. There is no cursor to capture first.
pub fn touch_look(dx: f64, dy: f64) {
    touch_used();
    let mut look = LOOK.lock().unwrap();
    look[0] += dx;
    look[1] += dy;
}

/// Tells the input layer whether the window owns the mouse; its movement only steers the
/// camera while it does.
pub fn set_captured(captured: bool) {
    CAPTURED.store(captured, Ordering::Relaxed);
    if !captured {
        *LOOK.lock().unwrap() = [0.0; 2];
    }
}

/// Releases every held input, so nothing stays stuck down while the window is in the
/// background.
pub fn release_all() {
    HELD.store(0, Ordering::Relaxed);
    HELD_DRIVING.store(0, Ordering::Relaxed);
    // The game may be stopped before it stops the motor itself.
    set_motor(false);
    MOTOR_OFF_AT.store(0, Ordering::Relaxed);
}

/// Scripted presses as (retrace, bits).
fn script() -> &'static [(u32, u32)] {
    static SCRIPT: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let spec = std::env::var("TSSM_INPUT").unwrap_or_default();
        spec.split(',')
            .filter_map(|entry| {
                let (name, at) = entry.trim().split_once(':')?;
                Some((at.parse().ok()?, action(&name.to_ascii_lowercase())?))
            })
            .collect()
    })
}

/// Writes controller 1's state as a 12-byte PADStatus. Called once per pad poll.
pub fn fill_status(status: &mut [u8], retrace: u32) {
    crate::menu::script(retrace);
    let touch = *TOUCH.lock().unwrap();
    let driving = driving();
    static WAS_DRIVING: AtomicBool = AtomicBool::new(false);
    if WAS_DRIVING.swap(driving, Ordering::Relaxed) != driving && std::env::var_os("TSSM_TRACE_INPUT").is_some() {
        eprintln!("[input] driving bindings {}", if driving { "on" } else { "off" });
    }
    let keys = if driving { &HELD_DRIVING } else { &HELD };
    let mut held = keys.load(Ordering::Relaxed) | touch.0;
    for &(at, bit) in script() {
        if (at..at + SCRIPT_HOLD).contains(&retrace) {
            held |= bit;
        }
    }

    {
        let mut pulses = PULSES.lock().unwrap();
        if pulses.polls == 0 {
            if let Some(bits) = pulses.queue.pop_front() {
                (pulses.current, pulses.polls) = (bits, PULSE_HOLD + PULSE_GAP);
            }
        }
        if pulses.polls > 0 {
            if pulses.polls > PULSE_GAP {
                held |= pulses.current;
            }
            pulses.polls -= 1;
        }
    }

    SCRIPTED_LOOK.store(held & (LOOK_RIGHT | LOOK_LEFT | LOOK_DOWN | LOOK_UP), Ordering::Relaxed);
    // The mouse does not go through the C-stick: it steers the camera directly (camera.rs).
    let gamepad = *GAMEPAD.lock().unwrap();
    // A stick reads whichever of the keys, the gamepad and the touch screen pushes it further.
    let axis = |negative: u32, positive: u32, stick: usize| {
        let keys = ((held & positive != 0) as i32 - (held & negative != 0) as i32) * KEY_DEFLECTION;
        let finger = touch.1.get(stick).map_or(0, |&v| v as i32);
        let pad = gamepad.sticks[stick] as i32;
        [keys, finger, pad].into_iter().max_by_key(|v| v.abs()).unwrap() as i8 as u8
    };
    let trigger = |bit: u32, analog: u8| if held & bit != 0 { 255 } else { analog };
    status.fill(0);
    status[0..2].copy_from_slice(&((held | gamepad.buttons) as u16).to_be_bytes());
    status[2] = axis(STICK_LEFT, STICK_RIGHT, 0);
    status[3] = axis(STICK_DOWN, STICK_UP, 1);
    status[4] = axis(C_LEFT, C_RIGHT, 2);
    status[5] = axis(C_DOWN, C_UP, 3);
    status[6] = trigger(L, gamepad.triggers[0]);
    status[7] = trigger(R, gamepad.triggers[1]);
    crate::options::filter_pad(status);
}
