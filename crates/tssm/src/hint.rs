//! Tells the system how long each frame's work takes, so that it runs the game's threads fast
//! enough to keep up (Android's performance hints; nothing happens elsewhere).
//!
//! A phone clocks its processor by how busy it looks. This game is busy for part of a frame
//! and then sleeps until the next retrace, which looks like a light load: the phone tried ran
//! its cores at a fraction of their speed unless the screen was being touched, and frames
//! that should have taken 8 ms took 16. With the hint the system is told the work has to fit
//! in `target` and how long it actually took.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a frame's work should take at most, in nanoseconds: three quarters of a retrace,
/// leaving room for whatever else the frame has to wait for.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn target() -> i64 {
    crate::host::retrace_period().as_nanos() as i64 * 3 / 4
}

/// Time slept waiting for the next retrace since the last frame was shown, in nanoseconds.
static SLEPT: AtomicU64 = AtomicU64::new(0);
/// The threads that share the frame's work besides the one that shows it.
static HELPERS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static STATE: Mutex<State> = Mutex::new(State { shown: None, session: None, threads: Vec::new(), showing: Vec::new() });

struct State {
    /// When the last frame was shown.
    shown: Option<Instant>,
    session: Option<system::Session>,
    /// The threads the session was opened for.
    threads: Vec<i32>,
    /// Every thread that has shown a frame: the game's main thread, and the one that draws
    /// the loading screens.
    showing: Vec<i32>,
}

/// Called with every wait for the next retrace: that time is not work.
pub fn slept(time: Duration) {
    SLEPT.fetch_add(time.as_nanos() as u64, Relaxed);
}

/// Called by a thread that helps with the frame's work, once, when it starts.
pub fn helper_started() {
    if let Some(thread) = system::thread() {
        HELPERS.lock().unwrap().push(thread);
    }
}

/// Called by the game's thread each time it has shown a frame.
pub fn frame_shown() {
    let Some(thread) = system::thread() else { return };
    let mut state = STATE.lock().unwrap();
    let now = Instant::now();
    let slept = Duration::from_nanos(SLEPT.swap(0, Relaxed));
    let Some(last) = state.shown.replace(now) else { return };
    let work = (now - last).saturating_sub(slept);
    // A pause or a level loading is not a frame.
    if work.is_zero() || work > Duration::from_millis(200) {
        return;
    }
    // The session covers the threads that show frames and the helpers; when another joins
    // them it is opened again.
    if !state.showing.contains(&thread) {
        // A thread that showed frames may be gone by now (each level load has its own for
        // the loading screen), and the system opens no session that names a dead thread.
        state.showing.retain(|&shown| system::alive(shown));
        state.showing.push(thread);
    }
    let mut threads = state.showing.clone();
    threads.extend(HELPERS.lock().unwrap().iter());
    if state.threads != threads {
        state.session = None;
        state.session = system::Session::open(&threads);
        if state.session.is_none() && state.threads.is_empty() {
            eprintln!("[hint] the system takes no performance hints");
        }
        state.threads = threads;
    }
    if let Some(session) = &state.session {
        session.report(work.as_nanos() as i64);
    }
}

#[cfg(not(target_os = "android"))]
mod system {
    pub struct Session;

    impl Session {
        pub fn open(_threads: &[i32]) -> Option<Session> {
            None
        }

        pub fn report(&self, _nanos: i64) {}
    }

    pub fn thread() -> Option<i32> {
        None
    }

    pub fn alive(_thread: i32) -> bool {
        true
    }
}

/// The NDK's APerformanceHint functions (Android 13 and later), looked up at run time because
/// the library is built to load on older systems too.
#[cfg(target_os = "android")]
mod system {
    use std::ffi::{c_char, c_int, c_void};

    extern "C" {
        fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(library: *mut c_void, name: *const c_char) -> *mut c_void;
        fn gettid() -> i32;
    }

    type GetManager = unsafe extern "C" fn() -> *mut c_void;
    type CreateSession = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
    type Report = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
    type Close = unsafe extern "C" fn(*mut c_void);

    pub struct Session {
        session: *mut c_void,
        report: Report,
        close: Close,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            unsafe { (self.close)(self.session) };
        }
    }

    // The session is only used under the lock in the parent module.
    unsafe impl Send for Session {}

    impl Session {
        pub fn open(threads: &[i32]) -> Option<Session> {
            unsafe {
                let library = dlopen(b"libandroid.so\0".as_ptr().cast(), 2);
                let find = |name: &[u8]| {
                    let symbol = if library.is_null() { library } else { dlsym(library, name.as_ptr().cast()) };
                    (!symbol.is_null()).then_some(symbol)
                };
                let get_manager: GetManager = std::mem::transmute(find(b"APerformanceHint_getManager\0")?);
                let create: CreateSession = std::mem::transmute(find(b"APerformanceHint_createSession\0")?);
                let report: Report = std::mem::transmute(find(b"APerformanceHint_reportActualWorkDuration\0")?);
                let close: Close = std::mem::transmute(find(b"APerformanceHint_closeSession\0")?);
                let manager = get_manager();
                if manager.is_null() {
                    return None;
                }
                let session = create(manager, threads.as_ptr(), threads.len(), super::target());
                eprintln!("[hint] performance hints for {} thread(s): {}", threads.len(), !session.is_null());
                // Not `then_some`: that would make a session of the null pointer and drop it,
                // and the system stops the app when asked to close a session that is null.
                (!session.is_null()).then(|| Session { session, report, close })
            }
        }

        pub fn report(&self, nanos: i64) {
            unsafe { (self.report)(self.session, nanos) };
        }
    }

    pub fn thread() -> Option<i32> {
        Some(unsafe { gettid() })
    }

    /// Whether this process still has a thread with that id.
    pub fn alive(thread: i32) -> bool {
        std::path::Path::new(&format!("/proc/self/task/{thread}")).exists()
    }
}
