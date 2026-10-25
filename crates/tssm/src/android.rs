//! The window and the input on Android, straight from the activity glue rather than through
//! winit: winit passes on a controller's buttons but not its sticks, triggers or d-pad (it
//! reports a stick's movement as a finger at the top left of the screen).
//!
//! The system's main thread calls in here through `run`, which never returns. The window's
//! surface comes and goes as the app enters and leaves the screen (see `Handover` in
//! render.rs); fingers go to touch.rs, controllers, keyboards and mice to input.rs.
//!
//! Controllers are read as on the desktop: the bottom face button is A, the right one B,
//! the left one X, the top one Y, the triggers are L and R, both shoulder buttons and
//! Select are Z, the left stick moves and the right stick turns the camera. The controller
//! touched last is the one that counts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use wgpu::rwh;
use winit::event::MouseButton;
use winit::keyboard::KeyCode;
use winit::platform::android::activity::input::{
    Axis, InputEvent, KeyAction, KeyEvent, Keycode, MotionAction, MotionEvent, Source,
};
use winit::platform::android::activity::ndk::native_window::NativeWindow;
use winit::platform::android::activity::{AndroidApp, InputStatus, MainEvent, PollEvent};

use crate::{aspect, input, render, touch};

/// The window's size in pixels, for the renderer, which asks from the game's thread.
static SIZE: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// What wgpu needs to draw into the activity's window. Holding the window keeps it alive for
/// as long as the surface made from it.
struct Handle(NativeWindow);

impl rwh::HasWindowHandle for Handle {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        self.0.window_handle()
    }
}

impl rwh::HasDisplayHandle for Handle {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(rwh::DisplayHandle::android())
    }
}

fn note_size(window: &NativeWindow) {
    let (width, height) = (window.width().max(0) as u32, window.height().max(0) as u32);
    SIZE[0].store(width, Ordering::Relaxed);
    SIZE[1].store(height, Ordering::Relaxed);
    aspect::set_window(width, height);
}

fn size() -> (u32, u32) {
    (SIZE[0].load(Ordering::Relaxed), SIZE[1].load(Ordering::Relaxed))
}

/// The controller touched last, as its events have left it.
#[derive(Default)]
struct Pad {
    device: i32,
    /// Button bits held, from key events.
    keys: u32,
    stick: [f32; 2],
    camera: [f32; 2],
    /// L and R as far as they are pulled, and whether their buttons report them held: a
    /// controller may send either or both.
    triggers: [f32; 2],
    trigger_keys: [bool; 2],
    /// The d-pad of controllers that report it as a pair of axes.
    hat: [f32; 2],
    /// Devices that have shown themselves to be controllers.
    known: Vec<i32>,
}

impl Pad {
    /// Makes `device` the controller that counts, starting from nothing held if it was not.
    fn select(&mut self, device: i32) {
        if !self.known.contains(&device) {
            eprintln!("[input] controller in use (device {device})");
            self.known.push(device);
        }
        PAD_DEVICE.store(device, Ordering::Relaxed);
        if self.device != device {
            *self = Pad { device, known: std::mem::take(&mut self.known), ..Pad::default() };
        }
    }

    fn release(&mut self) {
        *self = Pad { device: self.device, known: std::mem::take(&mut self.known), ..Pad::default() };
        self.send();
    }

    fn send(&self) {
        let mut buttons = self.keys;
        for (value, negative, positive) in [(self.hat[0], input::LEFT, input::RIGHT), (self.hat[1], input::UP, input::DOWN)] {
            if value < -0.5 {
                buttons |= negative;
            } else if value > 0.5 {
                buttons |= positive;
            }
        }
        let trigger = |i: usize| if self.trigger_keys[i] { 1.0 } else { self.triggers[i] };
        input::set_gamepad(buttons, self.stick, self.camera, [trigger(0), trigger(1)]);
    }

    fn motion(&mut self, motion: &MotionEvent) {
        self.select(motion.device_id());
        let pointer = motion.pointer_at_index(0);
        let axis = |axis: Axis| pointer.axis_value(axis);
        // Android's sticks read positive downward.
        self.stick = [axis(Axis::X), -axis(Axis::Y)];
        self.camera = [axis(Axis::Z), -axis(Axis::Rz)];
        // Controllers report their triggers on one pair of axes or the other.
        self.triggers = [axis(Axis::Ltrigger).max(axis(Axis::Brake)), axis(Axis::Rtrigger).max(axis(Axis::Gas))];
        self.hat = [axis(Axis::HatX), axis(Axis::HatY)];
        self.send();
    }

    /// Takes a key event if it is a controller's. D-pad keys also come from keyboards (the
    /// arrow keys), so those count only from a device known to be a controller.
    fn key(&mut self, key: &KeyEvent, pressed: bool) -> bool {
        let from_pad = matches!(key.source(), Source::Gamepad | Source::Joystick | Source::Dpad)
            || self.known.contains(&key.device_id());
        let bit = match key.key_code() {
            Keycode::ButtonA => input::A,
            Keycode::ButtonB => input::B,
            Keycode::ButtonX => input::X,
            Keycode::ButtonY => input::Y,
            Keycode::ButtonL1 | Keycode::ButtonR1 | Keycode::ButtonSelect => input::Z,
            Keycode::ButtonStart => input::START,
            Keycode::ButtonL2 | Keycode::ButtonR2 => 0,
            Keycode::DpadUp if from_pad => input::UP,
            Keycode::DpadDown if from_pad => input::DOWN,
            Keycode::DpadLeft if from_pad => input::LEFT,
            Keycode::DpadRight if from_pad => input::RIGHT,
            _ => return false,
        };
        self.select(key.device_id());
        match key.key_code() {
            Keycode::ButtonL2 => self.trigger_keys[0] = pressed,
            Keycode::ButtonR2 => self.trigger_keys[1] = pressed,
            _ if pressed => self.keys |= bit,
            _ => self.keys &= !bit,
        }
        self.send();
        true
    }
}

/// A keyboard's key in the terms config/controls.txt is read in.
fn keyboard(code: Keycode) -> Option<KeyCode> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    const FUNCTION: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    let number: u32 = code.into();
    let (a, zero, f1): (u32, u32, u32) = (Keycode::A.into(), Keycode::Keycode0.into(), Keycode::F1.into());
    Some(match code {
        Keycode::Space => Space,
        Keycode::Tab => Tab,
        Keycode::Escape => Escape,
        Keycode::Enter => Enter,
        Keycode::ShiftLeft => ShiftLeft,
        Keycode::CtrlLeft => ControlLeft,
        Keycode::AltLeft => AltLeft,
        Keycode::Del => Backspace,
        Keycode::DpadUp => ArrowUp,
        Keycode::DpadDown => ArrowDown,
        Keycode::DpadLeft => ArrowLeft,
        Keycode::DpadRight => ArrowRight,
        _ if (a..a + 26).contains(&number) => LETTERS[(number - a) as usize],
        _ if (zero..zero + 10).contains(&number) => DIGITS[(number - zero) as usize],
        _ if (f1..f1 + 12).contains(&number) => FUNCTION[(number - f1) as usize],
        _ => return None,
    })
}

/// Passes fingers on to the on-screen controls.
fn fingers(motion: &MotionEvent) {
    let (width, height) = size();
    let size = (width as f32, height as f32);
    let (down, all) = match motion.action() {
        MotionAction::Down | MotionAction::PointerDown => (true, false),
        MotionAction::Up | MotionAction::PointerUp => (false, false),
        MotionAction::Move => (true, true),
        MotionAction::Cancel => (false, true),
        _ => return,
    };
    // Landing and lifting concern one finger; moving and cancelling, all of them.
    let one = motion.pointer_index();
    for pointer in motion.pointers().filter(|pointer| all || pointer.pointer_index() == one) {
        touch::finger(pointer.pointer_id() as u64, down, (pointer.x(), pointer.y()), size);
    }
}

/// The controller that counts (Android's device id), for its rumble.
static PAD_DEVICE: AtomicI32 = AtomicI32::new(-1);

/// What rumbles: the phone itself under the touch controls, or a controller.
#[derive(Clone, Copy, PartialEq)]
enum Rumble {
    Phone,
    Pad(i32),
}

/// Starts or stops a vibrator. A start lasts `RUMBLE_MS` unless stopped or started again.
fn vibrate(env: &mut JNIEnv, activity: &JObject, what: Rumble, on: bool) -> jni::errors::Result<()> {
    env.with_local_frame(8, |env| {
        let vibrator = match what {
            Rumble::Phone => {
                let name = env.new_string("vibrator")?;
                env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?.l()?
            }
            Rumble::Pad(device) => {
                let device = env.call_static_method("android/view/InputDevice", "getDevice", "(I)Landroid/view/InputDevice;", &[JValue::Int(device)])?.l()?;
                if device.is_null() {
                    return Ok(());
                }
                env.call_method(&device, "getVibrator", "()Landroid/os/Vibrator;", &[])?.l()?
            }
        };
        if vibrator.is_null() {
            return Ok(());
        }
        if on {
            env.call_method(&vibrator, "vibrate", "(J)V", &[JValue::Long(RUMBLE_MS)])?;
        } else {
            env.call_method(&vibrator, "cancel", "()V", &[])?;
        }
        Ok(())
    })
}

const RUMBLE_MS: i64 = 5000;

/// Does what needs Java, on a thread of its own: holds the mouse during play, and follows
/// the game's rumble motor: with a controller last used the
/// controller rumbles, with the touch controls the phone does.
fn start_rumble(app: &AndroidApp) {
    let (vm, activity) = (app.vm_as_ptr() as usize, app.activity_as_ptr() as usize);
    let follow = move || {
        // SAFETY: the activity's virtual machine and object outlive the process's threads.
        let Ok(vm) = (unsafe { jni::JavaVM::from_raw(vm as *mut _) }) else { return };
        let Ok(mut env) = vm.attach_current_thread_permanently() else { return eprintln!("[input] rumble unavailable") };
        let activity = unsafe { JObject::from_raw(activity as *mut _) };
        let mut running: Option<(Rumble, std::time::Instant)> = None;
        let (mut held, mut looking) = (false, false);
        loop {
            std::thread::sleep(Duration::from_millis(8));
            // The mouse is held whenever the app has the focus; the system takes it back when
            // the focus goes. Its movement is the camera's only during play.
            let hold = FOCUSED.load(Ordering::Relaxed);
            if hold != held || (hold && RECAPTURE.swap(false, Ordering::Relaxed)) {
                held = hold;
                if let Err(e) = capture_pointer(&mut env, &activity, hold) {
                    let _ = env.exception_clear();
                    eprintln!("[input] cannot hold the mouse: {e}");
                }
            }
            let look = hold && input::wants_mouse();
            if look != looking {
                looking = look;
                input::set_captured(look);
            }
            let wanted = if !input::motor() {
                None
            } else if input::gamepad_last() {
                Some(Rumble::Pad(PAD_DEVICE.load(Ordering::Relaxed)))
            } else {
                touch::enabled().then_some(Rumble::Phone)
            };
            let now = std::time::Instant::now();
            let fresh = running.is_some_and(|(_, since)| now.duration_since(since).as_millis() < RUMBLE_MS as u128 - 1000);
            if wanted == running.map(|(what, _)| what) && (fresh || wanted.is_none()) {
                continue;
            }
            let mut result = Ok(());
            if let Some((what, _)) = running.filter(|(what, _)| Some(*what) != wanted) {
                result = vibrate(&mut env, &activity, what, false);
            }
            if let Some(what) = wanted {
                result = result.and(vibrate(&mut env, &activity, what, true));
            }
            if let Err(e) = result {
                let _ = env.exception_clear();
                eprintln!("[input] rumble failed: {e}");
            }
            running = wanted.map(|what| (what, now));
        }
    };
    std::thread::Builder::new().name("rumble".into()).spawn(follow).expect("cannot start the rumble thread");
}

/// Whether the app has the focus: only then can it hold the mouse.
static FOCUSED: AtomicBool = AtomicBool::new(false);
/// The mouse buttons held (Android's bits: 1 left, 2 right, 4 middle), as last reported.
static MOUSE_BUTTONS: AtomicU32 = AtomicU32::new(0);

/// Takes the mouse for the camera, or gives it back: while it is held the system shows no
/// pointer and reports the mouse's raw movement (`Source::MouseRelative`).
fn capture_pointer(env: &mut JNIEnv, activity: &JObject, capture: bool) -> jni::errors::Result<()> {
    env.with_local_frame(8, |env| {
        let window = env.call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
        let view = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
        env.call_method(&view, if capture { "requestPointerCapture" } else { "releasePointerCapture" }, "()V", &[])?;
        Ok(())
    })
}

/// Set when the mouse reports as the system's pointer although it should be held.
static RECAPTURE: AtomicBool = AtomicBool::new(false);

/// A mouse's event. The mouse is held whenever the app has the focus: the system shows no
/// pointer and hands over raw movement, which turns the camera during play and otherwise
/// moves the game's own pointer (touch.rs), so that both are as fast as on the desktop.
fn mouse(motion: &MotionEvent, held: bool) {
    input::mouse_used();
    let pointer = motion.pointer_at_index(0);
    let (width, height) = size();
    let size = (width as f32, height as f32);
    match motion.action() {
        MotionAction::Scroll => input::mouse_wheel(pointer.axis_value(Axis::Vscroll)),
        // Held, the place reported is how far the mouse has moved. The system hands over
        // several of a fast mouse's reports as one event, the earlier ones as its history.
        _ if held => {
            let moves = pointer.history().map(|past| (past.x(), past.y())).chain([(pointer.x(), pointer.y())]);
            if input::wants_mouse() {
                moves.for_each(|(dx, dy)| input::mouse_motion(dx as f64, dy as f64));
            } else {
                let (dx, dy) = moves.fold((0.0, 0.0), |sum, by| (sum.0 + by.0, sum.1 + by.1));
                touch::move_cursor(dx, dy, size);
            }
        }
        // Not held yet (the app has just come to the front): follow the system's pointer
        // and ask for the mouse again.
        _ => {
            touch::place_cursor((pointer.x(), pointer.y()), size);
            RECAPTURE.store(true, Ordering::Relaxed);
        }
    }
    let buttons = motion.button_state().0;
    let before = MOUSE_BUTTONS.swap(buttons, Ordering::Relaxed);
    for (bit, button) in [(1, MouseButton::Left), (2, MouseButton::Right), (4, MouseButton::Middle)] {
        if (buttons ^ before) & bit != 0 {
            input::mouse_button(button, buttons & bit != 0);
        }
    }
}

fn handle_input(pad: &mut Pad, event: &InputEvent) -> InputStatus {
    match event {
        InputEvent::MotionEvent(motion) => match motion.source() {
            Source::Joystick | Source::Gamepad => pad.motion(motion),
            Source::Mouse => mouse(motion, false),
            Source::MouseRelative => mouse(motion, true),
            Source::Touchscreen | Source::Stylus | Source::BluetoothStylus => fingers(motion),
            _ => return InputStatus::Unhandled,
        },
        InputEvent::KeyEvent(key) => {
            let pressed = match key.action() {
                KeyAction::Down => true,
                KeyAction::Up => false,
                _ => return InputStatus::Unhandled,
            };
            if pad.key(key, pressed) {
            } else if let Some(code) = keyboard(key.key_code()) {
                input::key_event(code, pressed);
            } else if key.key_code() != Keycode::Back {
                // Volume and the like are the system's. Back is swallowed: the system would
                // answer it by closing the game.
                return InputStatus::Unhandled;
            }
        }
        _ => return InputStatus::Unhandled,
    }
    InputStatus::Handled
}

/// The highest refresh rate the display has a mode for.
fn highest_refresh_rate(app: &AndroidApp) -> Option<f32> {
    // SAFETY: the activity's virtual machine and object outlive the process's threads.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr() as *mut _) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as *mut _) };
    let rate: jni::errors::Result<f32> = env.with_local_frame(64, |env| {
        let manager = env.call_method(&activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?.l()?;
        let display = env.call_method(&manager, "getDefaultDisplay", "()Landroid/view/Display;", &[])?.l()?;
        let modes = env.call_method(&display, "getSupportedModes", "()[Landroid/view/Display$Mode;", &[])?.l()?;
        let modes = jni::objects::JObjectArray::from(modes);
        let mut best = 0.0f32;
        for index in 0..env.get_array_length(&modes)? {
            let mode = env.get_object_array_element(&modes, index)?;
            best = best.max(env.call_method(&mode, "getRefreshRate", "()F", &[])?.f()?);
        }
        Ok(best)
    });
    match rate {
        Ok(rate) if rate > 0.0 => Some(rate),
        Ok(_) => None,
        Err(e) => {
            let _ = env.exception_clear();
            eprintln!("[window] cannot ask for the display's modes: {e}");
            None
        }
    }
}

/// Asks the system to run the display at `rate` while this window is on it. Left alone it
/// settles on 60 Hz for a game that draws 60 frames a second, and the pointer, which is
/// drawn whenever the mouse moves, could move no more smoothly than that.
fn ask_frame_rate(window: &NativeWindow, rate: f32) {
    use std::ffi::{c_char, c_int, c_void};
    extern "C" {
        fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(library: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    // ANativeWindow_setFrameRate came with Android 11; the library is built for 9, so it
    // is looked up when the game runs.
    // SAFETY: the function, if there, has this signature, and the window is alive.
    unsafe {
        let library = dlopen(c"libnativewindow.so".as_ptr(), 2);
        let found = if library.is_null() { library } else { dlsym(library, c"ANativeWindow_setFrameRate".as_ptr()) };
        if found.is_null() {
            return eprintln!("[window] this Android cannot be asked for a refresh rate");
        }
        let set: unsafe extern "C" fn(*mut c_void, f32, i8) -> i32 = std::mem::transmute(found);
        let result = set(window.ptr().as_ptr().cast(), rate, 0);
        eprintln!("[window] asked for {rate} Hz (result {result})");
    }
}

/// Runs the activity's event loop; the game starts when the window first appears.
pub fn run(app: AndroidApp, disc: PathBuf) -> ! {
    let instance = render::new_instance();
    let mut started = false;
    let mut pad = Pad::default();
    start_rumble(&app);
    let refresh = highest_refresh_rate(&app);
    if let Some(rate) = refresh {
        crate::host::set_display_rate((rate * 1000.0) as u32);
    }
    loop {
        app.poll_events(Some(Duration::from_millis(50)), |event| {
            let PollEvent::Main(event) = event else { return };
            match event {
                MainEvent::InitWindow { .. } => {
                    let Some(window) = app.native_window() else { return };
                    note_size(&window);
                    if let Some(rate) = refresh {
                        ask_frame_rate(&window, rate);
                    }
                    let surface = match instance.create_surface(Handle(window)) {
                        Ok(surface) => surface,
                        Err(e) => return eprintln!("[window] cannot create the surface: {e}"),
                    };
                    if started {
                        // Back on screen: the window has a new native surface.
                        render::surface_replaced(surface);
                    } else {
                        started = true;
                        let window = render::WindowSurface::from_parts(instance.clone(), surface, size);
                        crate::spawn_guest(disc.clone(), Some(window));
                    }
                }
                // The system destroys the surface once this returns. The game stops at its
                // next frame until there is a new one.
                MainEvent::TerminateWindow { .. } => {
                    render::surface_lost();
                    input::release_all();
                    touch::release_all();
                    pad.release();
                }
                MainEvent::WindowResized { .. } | MainEvent::ConfigChanged { .. } | MainEvent::RedrawNeeded { .. } => {
                    if let Some(window) = app.native_window() {
                        note_size(&window);
                    }
                }
                MainEvent::GainedFocus => FOCUSED.store(true, Ordering::Relaxed),
                MainEvent::LostFocus => {
                    FOCUSED.store(false, Ordering::Relaxed);
                    input::release_all();
                    touch::release_all();
                    pad.release();
                }
                MainEvent::Destroy => std::process::exit(0),
                _ => {}
            }
        });
        match app.input_events_iter() {
            Ok(mut events) => while events.next(|event| handle_input(&mut pad, event)) {},
            Err(e) => eprintln!("[input] cannot read input events: {e}"),
        }
    }
}
