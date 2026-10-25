//! Cooperative scheduler for the game's threads.
//!
//! Each guest thread runs on its own host thread with its own `Ctx`, all sharing guest memory.
//! Exactly one runs at a time: a thread keeps going until it blocks (sleeps on a queue,
//! suspends itself, waits for the retrace), yields, or an interrupt handler readies a thread
//! of higher priority. When every thread is blocked, whichever one is trying to hand over
//! idles: it waits for the next timed event and delivers it, until something can run.

use std::cell::Cell;
use std::sync::{Condvar, Mutex, MutexGuard};

use runtime::Ctx;

use crate::host;

/// Pseudo thread queue that the vertical retrace wakes.
pub const RETRACE_QUEUE: u32 = 1;
const MAIN_PRIORITY: i32 = 16;
/// Host stack for a guest thread other than the main one.
const THREAD_STACK: usize = 64 << 20;

#[derive(PartialEq)]
enum State {
    Ready,
    /// Asleep on the thread queue at this guest address.
    Waiting(u32),
    Exited,
}

struct Thread {
    /// Guest `OSThread*`.
    guest: u32,
    state: State,
    /// Suspend count; the thread only runs at zero.
    suspend: i32,
    /// Lower runs first, as in the SDK.
    priority: i32,
}

struct Sched {
    threads: Vec<Thread>,
    /// Index of the thread holding the baton.
    current: usize,
}

static SCHED: Mutex<Sched> = Mutex::new(Sched { threads: Vec::new(), current: 0 });
static BATON: Condvar = Condvar::new();

thread_local! {
    /// This host thread's index in the thread table.
    static ME: Cell<usize> = const { Cell::new(0) };
}

/// Unwind payload that ends a guest thread from inside its call stack.
struct ThreadExit;

/// A context being handed to the host thread that will own it.
struct SendCtx(Box<Ctx>);
// Safety: the context is used by one thread at a time, and guest memory it points to is
// shared deliberately under the scheduler's one-runner rule.
unsafe impl Send for SendCtx {}

fn lock() -> MutexGuard<'static, Sched> {
    SCHED.lock().unwrap()
}

impl Sched {
    fn runnable(&self, i: usize) -> bool {
        self.threads[i].state == State::Ready && self.threads[i].suspend == 0
    }

    /// The thread that should run next: best priority, and among equals the caller unless it
    /// is yielding.
    fn pick(&self, me: usize, yielding: bool) -> Option<usize> {
        let n = self.threads.len();
        let best = (0..n).filter(|&i| self.runnable(i)).map(|i| self.threads[i].priority).min()?;
        (0..n)
            .map(|k| (me + yielding as usize + k) % n)
            .find(|&i| self.runnable(i) && self.threads[i].priority == best)
    }

    fn find(&self, guest: u32) -> Option<usize> {
        self.threads.iter().position(|t| t.guest == guest && t.state != State::Exited)
    }
}

/// One line per guest thread, for diagnosing hangs.
pub fn describe() -> String {
    let s = lock();
    let mut out = format!("current thread: {}\n", s.current);
    for (i, t) in s.threads.iter().enumerate() {
        let state = match t.state {
            State::Ready => "ready".to_string(),
            State::Waiting(queue) => format!("waiting on {queue:#010x}"),
            State::Exited => "exited".to_string(),
        };
        out += &format!("  thread {i}: OSThread {:#010x}, priority {}, suspend {}, {state}\n", t.guest, t.priority, t.suspend);
    }
    out
}

/// Registers the calling host thread as the game's main thread.
pub fn init(main_guest: u32) {
    let mut s = lock();
    if s.threads.is_empty() {
        s.threads.push(Thread { guest: main_guest, state: State::Ready, suspend: 0, priority: MAIN_PRIORITY });
    } else {
        s.threads[0].guest = main_guest;
    }
}

/// Gives up the processor until the calling thread is the right one to run again.
/// With `yielding`, equal-priority threads go first.
pub fn reschedule(c: &mut Ctx, yielding: bool) {
    if host::in_handler() {
        eprintln!("[sched] blocking call from an interrupt handler ignored");
        return;
    }
    let me = ME.get();
    // Interrupt enable is per thread: park ours, and idle with interrupts on. GPU commands
    // this thread has written go to the shared stream so they are not stranded while it waits.
    let saved = host::with(|h| {
        h.gx.absorb(c);
        std::mem::replace(&mut h.interrupts_enabled, true)
    });
    let mut yielding = yielding;
    loop {
        let mut s = lock();
        match s.pick(me, yielding) {
            Some(next) if next == me => break,
            Some(next) => {
                s.current = next;
                BATON.notify_all();
                while s.current != me {
                    s = BATON.wait(s).unwrap();
                }
                yielding = false;
            }
            None => {
                drop(s);
                host::idle(c);
            }
        }
    }
    host::with(|h| h.interrupts_enabled = saved);
}

/// Switches away if an interrupt handler made a higher-priority thread runnable.
pub fn preempt(c: &mut Ctx) {
    if host::in_handler() {
        return;
    }
    let me = ME.get();
    let outranked = {
        let s = lock();
        s.threads.get(me).is_some_and(|mine| (0..s.threads.len()).any(|i| s.runnable(i) && s.threads[i].priority < mine.priority))
    };
    if outranked {
        reschedule(c, false);
    }
}

pub fn current_guest() -> u32 {
    let s = lock();
    s.threads.get(ME.get()).map_or(0, |t| t.guest)
}

pub fn priority(guest: u32) -> i32 {
    let s = lock();
    s.find(guest).map_or(MAIN_PRIORITY, |i| s.threads[i].priority)
}

/// OSSleepThread: block on `queue` until OSWakeupThread is called on it.
pub fn sleep(c: &mut Ctx, queue: u32) {
    if host::in_handler() {
        eprintln!("[sched] sleep on queue {queue:#010x} from an interrupt handler ignored");
        return;
    }
    lock().threads[ME.get()].state = State::Waiting(queue);
    reschedule(c, false);
}

/// Whether any thread is asleep on `queue`.
pub fn has_sleepers(queue: u32) -> bool {
    lock().threads.iter().any(|t| t.state == State::Waiting(queue))
}

/// OSWakeupThread: every thread asleep on `queue` becomes ready.
pub fn wake(queue: u32) {
    for t in lock().threads.iter_mut().filter(|t| t.state == State::Waiting(queue)) {
        t.state = State::Ready;
    }
}

/// OSResumeThread: returns the previous suspend count.
pub fn resume(guest: u32) -> i32 {
    let mut s = lock();
    let Some(i) = s.find(guest) else { return 0 };
    let old = s.threads[i].suspend;
    s.threads[i].suspend = (old - 1).max(0);
    old
}

/// OSSuspendThread: returns the previous suspend count. Suspending oneself blocks.
pub fn suspend(c: &mut Ctx, guest: u32) -> i32 {
    let (old, is_me) = {
        let mut s = lock();
        let Some(i) = s.find(guest) else { return 0 };
        s.threads[i].suspend += 1;
        (s.threads[i].suspend - 1, i == ME.get())
    };
    if is_me {
        reschedule(c, false);
    }
    old
}

/// OSIsThreadSuspended: whether the thread's suspend count is above zero.
pub fn is_suspended(guest: u32) -> bool {
    let s = lock();
    s.find(guest).is_some_and(|i| s.threads[i].suspend > 0)
}

pub fn is_terminated(guest: u32) -> bool {
    lock().find(guest).is_none()
}

/// OSExitThread: ends the calling guest thread by unwinding its host thread.
pub fn exit_current() -> ! {
    std::panic::resume_unwind(Box::new(ThreadExit))
}

/// Marks the calling thread finished and hands the baton on for good.
fn finish(c: &mut Ctx) {
    let me = ME.get();
    let guest = {
        let mut s = lock();
        s.threads[me].state = State::Exited;
        s.threads[me].guest
    };
    wake(guest); // joiners sleep on the thread itself
    host::with(|h| {
        h.gx.absorb(c);
        h.interrupts_enabled = true;
    });
    loop {
        let mut s = lock();
        if let Some(next) = s.pick(me, false) {
            s.current = next;
            BATON.notify_all();
            return;
        }
        drop(s);
        host::idle(c);
    }
}

/// OSCreateThread(thread, func, param, stack_top, stack_size, priority, attr).
/// The thread starts suspended, as the SDK specifies.
pub fn create(c: &mut Ctx) {
    let (guest, func, param, stack_top, priority) = (c.r[3], c.r[4], c.r[5], c.r[6], c.r[8] as i32);
    let index = {
        let mut s = lock();
        s.threads.push(Thread { guest, state: State::Ready, suspend: 1, priority });
        s.threads.len() - 1
    };

    let mut child = c.fork();
    child.r[1] = (stack_top - 16) & !7;
    child.w32(child.r[1], 0); // end of the stack frame chain
    child.r[2] = c.r[2];
    child.r[13] = c.r[13];
    child.r[3] = param;
    let child = SendCtx(child);

    std::thread::Builder::new()
        .name(format!("guest-{index}"))
        .stack_size(THREAD_STACK)
        .spawn(move || {
            let mut child = child;
            ME.set(index);
            {
                let mut s = lock();
                while s.current != index {
                    s = BATON.wait(s).unwrap();
                }
            }
            host::with(|h| h.interrupts_enabled = true);
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| child.0.call(func)));
            if run.is_err_and(|payload| !payload.is::<ThreadExit>()) {
                std::process::exit(1); // a real panic; its message has already been printed
            }
            finish(&mut child.0);
        })
        .expect("cannot start guest thread");
    c.r[3] = 1;
}
