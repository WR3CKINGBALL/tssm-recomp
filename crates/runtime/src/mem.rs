//! Guest address space: 4 GB reserved so a guest address is a plain offset from the base,
//! with only the GameCube's 24 MB of main RAM committed. Anything else faults immediately.

pub const RAM_BASE: u32 = 0x8000_0000;
pub const RAM_SIZE: u32 = 0x0180_0000;

#[cfg(windows)]
mod sys {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, ty: u32, protect: u32) -> *mut u8;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_READWRITE: u32 = 0x04;

    pub unsafe fn reserve(size: usize) -> *mut u8 {
        VirtualAlloc(std::ptr::null_mut(), size, MEM_RESERVE, PAGE_NOACCESS)
    }

    pub unsafe fn commit(p: *mut u8, size: usize) -> bool {
        !VirtualAlloc(p, size, MEM_COMMIT, PAGE_READWRITE).is_null()
    }
}

#[cfg(unix)]
mod sys {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }
    const PROT_NONE: i32 = 0;
    const PROT_READ_WRITE: i32 = 3;
    // MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE (Linux/Android values)
    const MAP_FLAGS: i32 = 0x02 | 0x20 | 0x4000;

    pub unsafe fn reserve(size: usize) -> *mut u8 {
        let p = mmap(std::ptr::null_mut(), size, PROT_NONE, MAP_FLAGS, -1, 0);
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p
        }
    }

    pub unsafe fn commit(p: *mut u8, size: usize) -> bool {
        mprotect(p, size, PROT_READ_WRITE) == 0
    }
}

/// Memory-mapped hardware registers (CP, PE, VI, PI, ...). Backed by plain memory so SDK code
/// that pokes them is harmless; nothing reads meaning into the values.
pub const HW_BASE: u32 = 0xCC00_0000;
pub const HW_SIZE: u32 = 0x8000;
/// The GX write-gather pipe, directly above the register block. Deliberately left unmapped:
/// writes are captured by `Ctx::gw*`, and any store that slips past faults loudly.
pub const WGPIPE: u32 = 0xCC00_8000;

/// Reserves the guest address space and returns its base pointer. Never freed.
pub fn alloc_guest_memory() -> *mut u8 {
    unsafe {
        let base = sys::reserve(1 << 32);
        assert!(!base.is_null(), "cannot reserve 4 GB of address space for the guest");
        assert!(sys::commit(base.add(RAM_BASE as usize), RAM_SIZE as usize), "cannot commit guest RAM");
        assert!(sys::commit(base.add(HW_BASE as usize), HW_SIZE as usize), "cannot commit hardware registers");
        fault::install(base);
        base
    }
}

/// Reports guest memory faults with the guest address and a backtrace of recompiled
/// functions, instead of the process dying silently.
#[cfg(windows)]
mod fault {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static BASE: AtomicUsize = AtomicUsize::new(0);
    const ACCESS_VIOLATION: u32 = 0xC000_0005;

    #[repr(C)]
    struct Record {
        code: u32,
        flags: u32,
        next: *mut Record,
        address: *mut u8,
        num_params: u32,
        info: [usize; 15],
    }

    #[repr(C)]
    struct Pointers {
        record: *mut Record,
        context: *mut u8,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut Pointers) -> i32) -> *mut u8;
    }

    unsafe extern "system" fn handler(p: *mut Pointers) -> i32 {
        let r = &*(*p).record;
        let offset = r.info[1].wrapping_sub(BASE.load(Ordering::Relaxed));
        if r.code == ACCESS_VIOLATION && offset < 1 << 32 {
            let kind = if r.info[0] == 1 { "write to" } else { "read of" };
            eprintln!("guest memory fault: {kind} {offset:#010x}");
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
            std::process::exit(3);
        }
        0 // not ours: continue the search
    }

    pub fn install(base: *mut u8) {
        BASE.store(base as usize, Ordering::Relaxed);
        unsafe { AddVectoredExceptionHandler(1, handler) };
    }
}

#[cfg(not(windows))]
mod fault {
    pub fn install(_base: *mut u8) {}
}
