//! Host executable: loads the game image from an extracted disc and runs the recompiled code.
//!
//! usage: tssm [disc_dir]      (default: ./disc, as written by tools/extract_disc.py)
//!
//! Environment:
//!   TSSM_SURVEY=1          log unimplemented SDK calls and keep going instead of stopping
//!   TSSM_HEADLESS=1        run without a window
//!   TSSM_SCREENSHOT=60,600 save those frames to screenshots/frame_N.png
//!   TSSM_TRACE_GX=1        trace GPU FIFO breakpoint handling
//!   TSSM_INPUT=a:400       scripted controller presses (action names as in controls.txt)
//!
//! Keyboard and mouse bindings are in config/controls.txt. Gamepads work when plugged in.
//! In menus the mouse points, clicks and scrolls, and Escape goes back.
//! F11 or Alt+Enter switches between a window and the full screen.
//!   TSSM_ORIGINAL_CAMERA=1 keep the game's own camera even without a gamepad
//!   TSSM_DUMP_FRAME=4300   describe every draw of that frame (material state; vertices of
//!                          small draws) and save each framebuffer capture as a PNG
//!   TSSM_SKIP_DRAWS=7,40-52  leave those draw indices out of every frame
//!   TSSM_SCREENSHOT_VI=900 save the display buffer at those retraces (movies)
//!   TSSM_MUTE=1            do not open a sound device
//!   TSSM_TRACE_AUDIO=1     log voice count and peak level once a second
//!   TSSM_ASPECT=16:9       picture shape, instead of following the window (see aspect.rs)
//!   TSSM_BOOT=BB03         start directly in that level, skipping the menus (see dvd.rs)
//!   TSSM_WARP=BB01@6000    go through a portal to that level at that retrace (see hle.rs)
//!   TSSM_NO_FOG=1          draw without fog; TSSM_NO_CULL=1 draws both faces of triangles
//!   TSSM_SURVEY_GX=1       list the GPU features in use that the renderer does not model
//!   TSSM_NO_CARD=1         no memory card in either slot (saves are in saves/, see card.rs)
//!   TSSM_TRACE_CARD=1      log the memory card calls the game makes and their results
//!   TSSM_TRACE_PERF=1      every 2 s, log the time spent decoding graphics and rendering
//!   TSSM_HUD_CENTRE=1      keep the heads-up display in the middle 4:3 part of a wide picture
//!   TSSM_FPS=144           frames a second: a number, `display` or 60 (as config/fps.txt)
//!   TSSM_TRACE_PLAYER=1    log the time and the player's position every frame
//!                          (and the car's position and velocity in the driving levels)
//!   TSSM_CPU_VERTICES=1    transform every vertex on the CPU each frame, as before the vertex
//!                          shader and the display list cache; `even`: on even frames only
//!   TSSM_TRACE_CALLS=1     every 5 s, the SDK calls the game was last seen in most often
//!   TSSM_ORIGINAL_FPS=1    keep the 30 frames a second the game holds its driving levels to
//!   TSSM_GX_THREADS=1      threads that transform vertices (default: 3)
//!   TSSM_TRACE_CAMERA=1    log the view's angle and the game's camera state whenever it turns
//!   TSSM_UPGRADES=1        every move upgraded and all health slots, for testing (see hle.rs)
//!   TSSM_MOUSE="320,165@2900 click@2950"  scripted mouse in menus (see menu.rs)
//!   TSSM_TRACE_UI=1        every 2 s, list the menu widgets on show with their links
//!   TSSM_PACK_ANSWER=no    answer the question about a texture pack next to the game
//!   TSSM_TRACE_PACK=1      log each texture's name in a texture pack's terms and whether it has one
//!
//! Release builds are Windows programs, without a console window of their own. Started from
//! a terminal they print to it; their output can also be redirected to a file as usual.

// Debug builds keep the console, for the log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    use_parent_console();
    tssm::main();
}

/// Prints to the terminal the game was started from, if any, unless its output is already
/// going somewhere (a file or a pipe).
#[cfg(all(windows, not(debug_assertions)))]
fn use_parent_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn AttachConsole(process: u32) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe {
        if GetStdHandle(STD_ERROR_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}
