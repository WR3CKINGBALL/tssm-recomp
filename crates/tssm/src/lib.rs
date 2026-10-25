//! The host: loads the game image from an extracted disc and runs the recompiled code. On the
//! desktop this is the executable behind main.rs (which lists the debug environment
//! variables); on Android it is the library crates/android wraps, entered at `android_main`.

#[cfg(target_os = "android")]
mod android;
mod aspect;
mod audio;
mod camera;
mod car;
mod card;
mod dvd;
mod glyph;
mod gx;
mod hint;
mod hle;
mod host;
mod input;
mod menu;
mod options;
mod pack;
mod png;
mod render;
mod sched;
mod sharpen;
mod sound;
mod tev;
mod texture;
mod touch;

use std::path::PathBuf;
use std::sync::Arc;

use render::WindowSurface;
use runtime::Ctx;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

/// Guest calls nest as native calls, and unoptimized recompiled frames are large.
const GUEST_STACK: usize = 256 << 20;

fn run(disc: PathBuf, window: Option<WindowSurface>) {
    let dol_path = disc.join("sys/main.dol");
    let dol = std::fs::read(&dol_path).unwrap_or_else(|e| panic!("cannot read {}: {e}", dol_path.display()));

    let trace = std::env::var_os("TSSM_TRACE_BOOT").is_some();
    let step = |what: &str| {
        if trace {
            eprintln!("[boot] {what}");
        }
    };
    step("read main.dol");
    let mut c = Ctx::new(game::FUNCS, game::HLE_NAMES, hle::policy);
    step("created guest context");
    c.survey = std::env::var_os("TSSM_SURVEY").is_some();
    c.poll_hook = Some(host::poll);
    c.load_dol(&dol);
    step("loaded program image");
    host::init(&disc, &mut c, window);
    step("initialised host devices");

    let sym = |name: &str| {
        game::LINKER_SYMS.iter().find(|s| s.0 == name).unwrap_or_else(|| panic!("missing linker symbol {name}")).1
    };
    // What __start sets up before calling main.
    c.r[1] = sym("_stack_addr");
    c.r[2] = sym("_SDA2_BASE_");
    c.r[13] = sym("_SDA_BASE_");

    let main = c.func_by_name("main").expect("game has no main");
    main(&mut c);
    println!("main returned {}", c.r[3] as i32);
}

/// Reports once per stall if the game stops making progress: which guest thread holds the
/// processor, what every thread is waiting for, and the last SDK function any of them entered.
/// Two kinds of stall are watched: retraces stopping (nothing reaches an interrupt delivery
/// point), and retraces continuing while no new GPU frame appears (the game is waiting on
/// something). The second also triggers during movies, which do not draw through the GPU.
fn spawn_watchdog() {
    spawn_call_sampler();
    spawn_look_out();
    std::thread::spawn(|| {
        let (mut last, mut stalled) = (0, 0);
        let (mut last_frame, mut frame_stalled) = (0, 0);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let count = host::RETRACE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
            let frames = host::FRAME_COUNT.load(std::sync::atomic::Ordering::Relaxed);
            stalled = if count == last { stalled + 1 } else { 0 };
            frame_stalled = if frames == last_frame && count != last { frame_stalled + 1 } else { 0 };
            (last, last_frame) = (count, frames);
            if stalled == 3 || frame_stalled == 6 {
                let id = runtime::LAST_HLE.load(std::sync::atomic::Ordering::Relaxed);
                if stalled == 3 {
                    eprintln!("[watchdog] no retrace for 3s (stuck at retrace {count})");
                } else {
                    eprintln!("[watchdog] no new frame for 6s (frame {frames}, retrace {count})");
                }
                let caller = runtime::LAST_HLE_CALLER.load(std::sync::atomic::Ordering::Relaxed);
                let i = game::FUNCS.partition_point(|f| f.0 <= caller);
                let from = if i == 0 { "?" } else { game::FUNCS[i - 1].2 };
                eprintln!(
                    "[watchdog] last SDK call: {} from {from} (returning to {caller:#010x})",
                    game::HLE_NAMES.get(id).copied().unwrap_or("?")
                );
                match host::interrupt_flags() {
                    Some((enabled, in_handler, queued)) => eprintln!(
                        "[watchdog] interrupts enabled: {enabled}, inside a handler: {in_handler}, queued events: {queued}"
                    ),
                    None => eprintln!("[watchdog] host state busy"),
                }
                // The host state is briefly held whenever a guest thread uses it; retry a little.
                let gpu = (0..50).find_map(|_| {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    host::gpu_state()
                });
                eprintln!("[watchdog] GPU: {}", gpu.as_deref().unwrap_or("host state busy"));
                eprint!("[watchdog] {}", sched::describe());
            }
        }
    });
}

/// The SDK call the game was last seen in, looked at every millisecond: the last 512 looks,
/// each with when it was made, the call and the address it was made from.
static LOOKS: std::sync::Mutex<Vec<(std::time::Instant, usize, u32)>> = std::sync::Mutex::new(Vec::new());

/// Looks at where the game is every millisecond, for `calls_since`.
fn spawn_look_out() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(1));
        let call = runtime::LAST_HLE.load(std::sync::atomic::Ordering::Relaxed);
        let caller = runtime::LAST_HLE_CALLER.load(std::sync::atomic::Ordering::Relaxed);
        let mut looks = LOOKS.lock().unwrap();
        if looks.len() >= 512 {
            looks.remove(0);
        }
        looks.push((std::time::Instant::now(), call, caller));
    });
}

/// The SDK calls the game was last seen in since `since`, most often first, with the share
/// of the looks each had: what a slow frame was busy with.
pub fn calls_since(since: std::time::Instant) -> String {
    let looks = LOOKS.lock().unwrap();
    let mut seen: Vec<((usize, u32), u32)> = Vec::new();
    let mut total = 0;
    for &(_, call, caller) in looks.iter().filter(|look| look.0 >= since) {
        total += 1;
        match seen.iter_mut().find(|entry| entry.0 == (call, caller)) {
            Some(entry) => entry.1 += 1,
            None => seen.push(((call, caller), 1)),
        }
    }
    seen.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    let name = |(call, caller): (usize, u32)| {
        let i = game::FUNCS.partition_point(|f| f.0 <= caller);
        format!("{} from {}", game::HLE_NAMES.get(call).copied().unwrap_or("?"), if i == 0 { "?" } else { game::FUNCS[i - 1].2 })
    };
    let parts: Vec<String> =
        seen.into_iter().take(4).map(|(what, count)| format!("{}% {}", count * 100 / total.max(1), name(what))).collect();
    parts.join(", ")
}

/// TSSM_TRACE_CALLS=1: every 5 s, lists the SDK calls the game was last seen in most often
/// (looked at every millisecond), with the function that made each: where it spends its
/// time, or what it is waiting in.
fn spawn_call_sampler() {
    if std::env::var_os("TSSM_TRACE_CALLS").is_none() {
        return;
    }
    std::thread::spawn(|| loop {
        let mut seen: std::collections::HashMap<(usize, u32), u32> = std::collections::HashMap::new();
        for _ in 0..5000 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let call = runtime::LAST_HLE.load(std::sync::atomic::Ordering::Relaxed);
            let caller = runtime::LAST_HLE_CALLER.load(std::sync::atomic::Ordering::Relaxed);
            *seen.entry((call, caller)).or_default() += 1;
        }
        let mut seen: Vec<_> = seen.into_iter().collect();
        seen.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        for ((call, caller), count) in seen.into_iter().take(6) {
            let i = game::FUNCS.partition_point(|f| f.0 <= caller);
            let from = if i == 0 { "?" } else { game::FUNCS[i - 1].2 };
            eprintln!("[calls] {:>4.1}% {} from {from}", count as f64 / 50.0, game::HLE_NAMES.get(call).copied().unwrap_or("?"));
        }
    });
}

/// Runs the game on its own large-stack thread; the process ends when the game does.
fn spawn_guest(disc: PathBuf, window: Option<WindowSurface>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("guest".into())
        .stack_size(GUEST_STACK)
        .spawn(move || {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(disc, window))).is_ok();
            std::process::exit(if ok { 0 } else { 1 });
        })
        .unwrap()
}

/// Where "fullscreen" or "windowed" is kept between runs (relative to the working folder).
const WINDOW_FILE: &str = "config/window.txt";

/// The window lives on the main thread, as the platform requires; the game renders to it from
/// the guest thread.
struct App {
    disc: PathBuf,
    window: Option<Arc<Window>>,
    /// Makes a new drawing surface when the system has taken the window's away (Android).
    instance: Option<wgpu::Instance>,
    focused: bool,
    /// Whether the cursor is currently hidden and held for mouse look.
    captured: bool,
    /// Whether an Alt key is down, for Alt+Enter.
    alt: bool,
}

impl App {
    /// Takes or gives up the mouse: while captured the cursor is hidden and held in the
    /// window, and its movement turns the camera.
    fn capture_mouse(&mut self, capture: bool) {
        let Some(window) = &self.window else { return };
        let grabbed = if capture {
            // Locking the cursor in place is not available everywhere; confining it is.
            window.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
        } else {
            window.set_cursor_grab(CursorGrabMode::None)
        };
        let captured = capture && grabbed.is_ok();
        window.set_cursor_visible(!captured);
        input::set_captured(captured);
        self.captured = captured;
    }

    /// Switches between a window and the whole screen (borderless, at the desktop's resolution).
    fn toggle_fullscreen(&self) {
        let Some(window) = &self.window else { return };
        let fullscreen = window.fullscreen().is_none().then(|| Fullscreen::Borderless(None));
        // Remembered for the next start.
        let _ = std::fs::write(WINDOW_FILE, if fullscreen.is_some() { "fullscreen\n" } else { "windowed\n" });
        window.set_fullscreen(fullscreen);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let (Some(window), Some(instance)) = (&self.window, &self.instance) {
            // Back on screen after `suspended`: the window has a new native surface.
            match instance.create_surface(window.clone()) {
                Ok(surface) => render::surface_replaced(surface),
                Err(e) => eprintln!("[window] cannot recreate the surface: {e}"),
            }
            return;
        }
        // The whole screen the first time, and after that as it was last left.
        let windowed = std::fs::read_to_string(WINDOW_FILE).is_ok_and(|last| last.trim() == "windowed");
        let attributes = Window::default_attributes()
            .with_title("The SpongeBob SquarePants Movie")
            .with_inner_size(LogicalSize::new(960.0, 720.0))
            .with_fullscreen((!windowed).then(|| Fullscreen::Borderless(None)));
        // The exe's icon (build.rs) on the title bar and taskbar too, when the build had one.
        #[cfg(windows)]
        let attributes = {
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            match winit::window::Icon::from_resource(1, None) {
                Ok(icon) => attributes.with_window_icon(Some(icon.clone())).with_taskbar_icon(Some(icon)),
                Err(_) => attributes,
            }
        };
        let window = Arc::new(event_loop.create_window(attributes).expect("cannot create window"));
        let size = window.inner_size();
        aspect::set_window(size.width, size.height);
        if let Some(rate) = window.current_monitor().and_then(|monitor| monitor.refresh_rate_millihertz()) {
            eprintln!("[window] the display runs at {:.2} Hz", rate as f64 / 1000.0);
            host::set_display_rate(rate);
        }
        self.window = Some(window.clone());
        // The surface has to be created here, on the thread that owns the window.
        let surface = WindowSurface::new(window);
        self.instance = Some(surface.instance());
        spawn_guest(self.disc.clone(), Some(surface));
    }

    /// Android takes the window's surface away while the app is off screen. The game stops at
    /// its next frame until `resumed` hands it a new one.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        render::surface_lost();
        input::release_all();
        touch::release_all();
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => std::process::exit(0),
            WindowEvent::Resized(size) => aspect::set_window(size.width, size.height),
            WindowEvent::ModifiersChanged(modifiers) => self.alt = modifiers.state().alt_key(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    // F11 and Alt+Enter belong to the window, not the game (Enter is Start).
                    let enter = matches!(key, KeyCode::Enter | KeyCode::NumpadEnter);
                    if pressed && (key == KeyCode::F11 || (enter && self.alt)) {
                        if !event.repeat {
                            self.toggle_fullscreen();
                        }
                    } else {
                        input::key_event(key, pressed);
                    }
                }
            }
            WindowEvent::Touch(touch) => {
                let Some(window) = &self.window else { return };
                let size = window.inner_size();
                let pressed = match touch.phase {
                    TouchPhase::Started | TouchPhase::Moved => true,
                    TouchPhase::Ended | TouchPhase::Cancelled => false,
                };
                let at = (touch.location.x as f32, touch.location.y as f32);
                touch::finger(touch.id, pressed, at, (size.width as f32, size.height as f32));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                input::mouse_button(button, state == ElementState::Pressed);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(window) = &self.window else { return };
                let size = window.inner_size();
                menu::cursor_moved((position.x as f32, position.y as f32), (size.width as f32, size.height as f32));
            }
            WindowEvent::CursorLeft { .. } => menu::cursor_left(),
            WindowEvent::MouseWheel { delta, .. } => input::mouse_wheel(match delta {
                MouseScrollDelta::LineDelta(_, lines) => lines,
                MouseScrollDelta::PixelDelta(pixels) => pixels.y as f32 / 40.0,
            }),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    input::release_all();
                }
            }
            _ => {}
        }
    }

    /// Runs between batches of events and at least every 50 ms. The cursor is hidden and
    /// held only while the window has focus and the game is in unpaused gameplay; menus,
    /// pause screens and movies leave it free.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::wait_duration(std::time::Duration::from_millis(50)));
        // A phone has no cursor to take, and asking would fail every time.
        if cfg!(target_os = "android") {
            return;
        }
        let wanted = self.focused && input::wants_mouse();
        if wanted != self.captured {
            self.capture_mouse(wanted);
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            input::mouse_motion(delta.0, delta.1);
        }
    }
}

/// Starts everything that does not depend on the window.
fn start_services() {
    spawn_watchdog();
    if std::env::var_os("TSSM_MUTE").is_none() {
        sound::start();
    }
    if std::env::var_os("TSSM_TRACE_BOOT").is_some() {
        eprintln!("[boot] process started");
    }
}

/// Runs the window's event loop.
fn start(disc: PathBuf, event_loop: EventLoop<()>) {
    start_services();
    input::start_gamepads();
    let mut app = App { disc, window: None, instance: None, focused: false, captured: false, alt: false };
    event_loop.run_app(&mut app).expect("event loop failed");
}

/// Without a disc folder given, the game's files (disc, config, saves) are the ones next to
/// the executable: Windows search and shortcuts can start it from another folder.
fn enter_game_folder() {
    let here = std::path::Path::new("disc/sys/main.dol");
    if here.exists() {
        return;
    }
    if let Some(folder) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)) {
        if folder.join(here).exists() {
            let _ = std::env::set_current_dir(&folder);
            return;
        }
    }
    let text = "The game's files were not found next to this program (a folder named disc).\n\n\
                Start the copy that build.bat made, next to build.bat: \"The SpongeBob Movie Game.exe\". \
                If there is none, run build.bat again.";
    eprintln!("{text}");
    message_box("The SpongeBob SquarePants Movie", text, MB_ICONERROR);
    std::process::exit(1);
}

const MB_ICONERROR: u32 = 0x10;
const MB_YESNO_QUESTION: u32 = 0x24;
const IDYES: i32 = 6;

/// A message box of that kind; the button chosen.
#[cfg(windows)]
fn message_box(title: &str, text: &str, kind: u32) -> i32 {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(window: *mut std::ffi::c_void, text: *const u16, title: *const u16, kind: u32) -> i32;
    }
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), kind) }
}

#[cfg(not(windows))]
fn message_box(_title: &str, _text: &str, _kind: u32) -> i32 {
    0
}

pub fn main() {
    if std::env::args().nth(1).is_none() {
        enter_game_folder();
    }
    // A texture pack next to the game: asked about (TSSM_PACK_ANSWER=yes or no answers, for tests).
    match std::env::var("TSSM_PACK_ANSWER") {
        Ok(answer) => pack::offer(|_| answer == "yes"),
        Err(_) if std::env::var_os("TSSM_HEADLESS").is_none() => {
            pack::offer(|text| message_box("The SpongeBob SquarePants Movie", text, MB_YESNO_QUESTION) == IDYES)
        }
        Err(_) => {}
    }
    let disc = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "disc".into()));
    if std::env::var_os("TSSM_HEADLESS").is_some() {
        spawn_watchdog();
        if std::env::var_os("TSSM_MUTE").is_none() {
            sound::start();
        }
        let _ = spawn_guest(disc, None).join();
        return;
    }
    start(disc, EventLoop::new().expect("cannot create event loop"));
}

#[cfg(target_os = "android")]
pub use winit::platform::android::activity::AndroidApp;

/// Android: the app's working directory becomes its private folder
/// (/data/data/com.campi.tssm/files), which is where the extracted disc is expected, as
/// `disc`, and where screenshots go. Files on shared storage will not do: the app is refused
/// anything that was copied there over USB. tools/android.ps1 copies the disc in; from a
/// computer the folder is reached with `adb shell run-as com.campi.tssm`. Two more files
/// there stand in for what a phone does not have:
///   log.txt   everything the desktop build prints to the console
///   env.txt   debug environment variables, one NAME=value per line (see main.rs)
#[cfg(target_os = "android")]
pub fn android_main(app: AndroidApp) {
    use std::os::fd::IntoRawFd;

    extern "C" {
        fn dup2(from: i32, to: i32) -> i32;
    }

    let folder = app.internal_data_path().expect("the app has no data folder");
    let _ = std::fs::create_dir_all(&folder);
    std::env::set_current_dir(&folder).expect("cannot enter the app's data folder");
    if let Ok(log) = std::fs::File::create("log.txt") {
        let fd = log.into_raw_fd();
        unsafe {
            dup2(fd, 1);
            dup2(fd, 2);
        }
    }
    eprintln!("[android] data folder: {}", folder.display());
    // A stop at an SDK call nobody has implemented looks like a crash on a phone; carry on
    // instead, as TSSM_SURVEY does. A name with nothing after the `=` in env.txt unsets it.
    std::env::set_var("TSSM_SURVEY", "1");
    for line in std::fs::read_to_string("env.txt").unwrap_or_default().lines() {
        match line.split('#').next().unwrap().split_once('=').map(|(name, value)| (name.trim(), value.trim())) {
            Some((name, "")) => std::env::remove_var(name),
            Some((name, value)) => std::env::set_var(name, value),
            None => {}
        }
    }
    if !std::path::Path::new("disc/sys/main.dol").exists() {
        eprintln!("[android] no game data: copy the extracted disc to {}/disc", folder.display());
    }
    touch::enable();
    // The window and the input do not go through winit here (see android.rs).
    start_services();
    android::run(app, PathBuf::from("disc"));
}
