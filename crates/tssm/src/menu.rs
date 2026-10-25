//! The mouse in the game's menus.
//!
//! Menus are made of UI widgets (`zUI`: text, images, boxes) that the game's UI manager keeps
//! in a list. Some have the focus; the pad's buttons are signalled to those, and their links
//! (the game's general "when this event arrives, send that event to that object") decide what
//! happens: Down on NEW GAME takes the focus off it and gives it to LOAD GAME, A opens a
//! screen.
//!
//! The mouse works through the same links. The items of the menu on show are the widgets that
//! the focus can reach from where it is by Up, Down, Left and Right. Moving the cursor onto
//! one sends the two events its neighbour's link would have sent (focus off, focus on); a
//! click then presses A, Escape presses B (back), and the wheel presses Up and Down. A cursor that is not moving changes nothing, so the keyboard and a gamepad work as
//! before with the cursor resting on the menu.
//!
//! The hints along the bottom of a menu ("(A) Choose", "(B) Return") are texts with a
//! button's picture in them. They are drawn without the picture and a click on one presses
//! that button; those that only say what a click does are not drawn (`render_text`).
//!
//! The level select screen (the pause screen) is steered by the game's own code instead, and
//! gets its own treatment: its arrows and challenges are clicked (`custom_click`), the
//! challenges follow the cursor (`steer_challenges`, `answer_question`), and its hints lose
//! their pictures (`render_text`). All of that is only while the keyboard and mouse are what
//! the player last used; after a gamepad the screen is the game's own.
//!
//! Guest structures (offsets in bytes):
//!   zUI:      id @0, link count @5, flags @6 (bit 0: enabled), links @8, asset @0x10,
//!             x @0x14, y @0x18, width @0x1C, height @0x20 (on the 640 by 480 screen),
//!             opacity @0x27, visible @0x54, focused @0x55; a text widget's text @0x70
//!   zUIAsset: flags @0x24, neighbours for automatic menus (up, down, left, right) @0x38
//!   link:     event received @0 (u16), event sent @2 (u16), target id @4; 0x20 bytes each

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use runtime::Ctx;

use crate::glyph::Button;

/// The UI manager's list of widgets and (in the small data area) its length.
const WIDGETS: u32 = 0x8040_72C0;
const WIDGET_COUNT: u32 = 0xFFFF_93D0;

/// TSSM_TRACE_UI=1: every two seconds, lists the widgets that are enabled and visible.
fn trace(c: &Ctx) {
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("TSSM_TRACE_UI").is_some()) {
        return;
    }
    if CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 120 != 0 {
        return;
    }
    let count = c.r32(c.r[13].wrapping_add(WIDGET_COUNT));
    eprintln!("[ui] {count} widgets at retrace {}", crate::host::RETRACE_COUNT.load(std::sync::atomic::Ordering::Relaxed));
    let f = |addr: u32| f32::from_bits(c.r32(addr));
    for i in 0..count.min(512) {
        let ui = c.r32(WIDGETS + i * 4);
        if c.r16(ui + 6) & 1 == 0 || c.r8(ui + 0x54) == 0 {
            continue;
        }
        let asset = c.r32(ui + 0x10);
        let links: Vec<String> = (0..c.r8(ui + 5) as u32)
            .map(|n| c.r32(ui + 8) + n * 0x20)
            .map(|link| format!("{}>{}:{:08x}", c.r16(link), c.r16(link + 2), c.r32(link + 4)))
            .collect();
        eprintln!(
            "[ui]  {:08x} type {:#x} focus {} b27 {} b5a {} at {:.3},{:.3} size {:.3},{:.3} flags {:#x} next {:08x} {:08x} {:08x} {:08x} links {}",
            c.r32(ui),
            c.r8(ui + 4),
            c.r8(ui + 0x55),
            c.r8(ui + 0x27),
            c.r8(ui + 0x5A),
            f(ui + 0x14),
            f(ui + 0x18),
            f(ui + 0x1C),
            f(ui + 0x20),
            c.r32(asset + 0x24),
            c.r32(asset + 0x38),
            c.r32(asset + 0x3C),
            c.r32(asset + 0x40),
            c.r32(asset + 0x44),
            links.join(" "),
        );
    }
}

/// The game's events: the pad's buttons as they are signalled to the focused widget (A first,
/// Left last), and the two pairs that move the focus.
const PAD_EVENTS: std::ops::RangeInclusive<u16> = 63..=76;
const PAD_DIRECTIONS: std::ops::RangeInclusive<u16> = 73..=76;
const START_EVENT: u16 = 71;
const FOCUS_ON: u16 = 81;
const FOCUS_OFF: u16 = 82;
pub(crate) const FOCUS_ON_SELECT: u16 = 94;
pub(crate) const FOCUS_OFF_UNSELECT: u16 = 95;
/// zEntEvent(from, fromEvent, to, toEvent, params, paramWidget, toParamWidget, force)
const SEND_EVENT: u32 = 0x801C_9550;

/// What the window thread has seen of the mouse since the game last looked.
struct Pointer {
    /// Where the cursor is on the game's 640 by 480 screen, if it is over the window.
    at: Option<(f32, f32)>,
    moved: bool,
    clicked: bool,
    /// Whether the left button is down.
    held: bool,
}

static POINTER: Mutex<Pointer> = Mutex::new(Pointer { at: None, moved: false, clicked: false, held: false });
/// When a menu that takes the pad was last seen on show (milliseconds on `clock`, 0 for never).
static MENU_SEEN: AtomicU64 = AtomicU64::new(0);

fn clock() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Whether a menu is on show: the mouse's buttons then click instead of being the buttons
/// they are bound to.
pub fn active() -> bool {
    let seen = MENU_SEEN.load(Ordering::Relaxed);
    seen != 0 && clock().saturating_sub(seen) < 250
}

/// Whether something on show takes Start itself (the title screen's "Press Start").
static TAKES_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether Escape should go back a screen (B) rather than be the button it is bound to
/// (Start): in any menu that does not ask for Start. On the pause screen B is "Return to
/// game".
pub fn escape_goes_back() -> bool {
    crate::input::keyboard_in_use() && active() && !TAKES_START.load(Ordering::Relaxed)
}

/// Whether the menu on show is the level select (pause) screen, not asking anything: there
/// Enter stays Start, which goes back to the game.
static ON_LEVEL_SELECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether Enter should accept (A) rather than be the button it is bound to (Start): in the
/// same menus as Escape goes back, except the level select screen.
pub fn enter_accepts() -> bool {
    escape_goes_back() && !ON_LEVEL_SELECT.load(Ordering::Relaxed)
}

/// Presses buttons for the player: a hint clicked, or Escape. Going back from the settings
/// screen accepts its changes first (A, "Accept changes"), which the game otherwise drops.
pub(crate) fn press(bits: u32) {
    if bits == crate::input::B && crate::options::settings_on_show() {
        crate::options::let_accept_through();
        crate::input::pulse(crate::input::A);
    }
    crate::input::pulse(bits);
}

/// Called from the window thread when the cursor moves, in pixels from the window's top
/// left corner, with the window's size. The flat parts of the picture (menus) keep their 4:3
/// shape in the middle of a wide one, and the picture has bars when the window is narrower
/// than 4:3 or wider than the widest picture.
pub fn cursor_moved(at: (f32, f32), window: (f32, f32)) {
    if window.0 <= 0.0 || window.1 <= 0.0 {
        return;
    }
    let picture_height = window.1.min(window.0 / crate::aspect::picture());
    let menu_width = picture_height * 4.0 / 3.0;
    let x = ((at.0 - window.0 / 2.0) / menu_width + 0.5) * 640.0;
    let y = ((at.1 - window.1 / 2.0) / picture_height + 0.5) * 480.0;
    let mut pointer = POINTER.lock().unwrap();
    pointer.at = Some((x, y));
    pointer.moved = true;
    MOUSE_STEERS.store(true, Ordering::Relaxed);
}

/// The cursor on the 640 by 480 screen, and whether it moved or clicked since the game last
/// looked, without taking those.
pub(crate) fn peek_pointer() -> (Option<(f32, f32)>, bool, bool) {
    let pointer = POINTER.lock().unwrap();
    (pointer.at, pointer.moved, pointer.clicked)
}

/// Takes the cursor's movement and click: they have been dealt with.
pub(crate) fn take_pointer() {
    let mut pointer = POINTER.lock().unwrap();
    (pointer.moved, pointer.clicked) = (false, false);
}

/// Marks a menu as on show although no widget takes the pad (options.rs has the focus).
pub(crate) fn keep_active() {
    MENU_SEEN.store(clock(), Ordering::Relaxed);
}

/// Called from the window thread when the cursor leaves the window.
pub fn cursor_left() {
    POINTER.lock().unwrap().at = None;
}

/// Called from the window thread when the left button goes down while a menu is on show.
pub fn click() {
    POINTER.lock().unwrap().clicked = true;
}

/// Called from the window thread whenever the left button goes down or up.
pub fn left_button(down: bool) {
    POINTER.lock().unwrap().held = down;
}

/// Whether the left button is down.
pub(crate) fn button_held() -> bool {
    POINTER.lock().unwrap().held
}

/// A widget on show, as far as the mouse cares.
#[derive(Clone, Copy)]
pub(crate) struct Widget {
    pub(crate) addr: u32,
    pub(crate) id: u32,
    /// The game's type number for it (text, image, model, box).
    pub(crate) kind: u8,
    pub(crate) focused: bool,
    pub(crate) opacity: u8,
    pub(crate) rect: [f32; 4],
}

impl Widget {
    pub(crate) fn contains(&self, (x, y): (f32, f32)) -> bool {
        let [left, top, width, height] = self.rect;
        (left..left + width).contains(&x) && (top..top + height).contains(&y)
    }
}

/// A widget's links as (event received, event sent, target id).
fn links(c: &Ctx, widget: u32) -> Vec<(u16, u16, u32)> {
    let first = c.r32(widget + 8);
    (0..c.r8(widget + 5) as u32).map(|n| first + n * 0x20).map(|link| (c.r16(link), c.r16(link + 2), c.r32(link + 4))).collect()
}

/// The widgets that are enabled, visible and not fully transparent.
fn on_show(c: &Ctx) -> Vec<Widget> {
    listed(c, false)
}

/// The widgets that are enabled and visible, with or without the fully transparent ones.
fn listed(c: &Ctx, transparent: bool) -> Vec<Widget> {
    let count = c.r32(c.r[13].wrapping_add(WIDGET_COUNT)).min(1024);
    let f = |addr: u32| f32::from_bits(c.r32(addr));
    (0..count)
        .map(|i| c.r32(WIDGETS + i * 4))
        .filter(|&ui| (0x8000_0000..0x8180_0000).contains(&ui))
        .filter(|&ui| c.r16(ui + 6) & 1 != 0 && c.r8(ui + 0x54) != 0 && (transparent || c.r8(ui + 0x27) != 0))
        .map(|ui| Widget {
            addr: ui,
            id: c.r32(ui),
            kind: c.r8(ui + 4),
            opacity: c.r8(ui + 0x27),
            focused: c.r8(ui + 0x55) != 0,
            rect: [f(ui + 0x14), f(ui + 0x18) + crate::options::row_shift(ui), f(ui + 0x1C), f(ui + 0x20)],
        })
        .collect()
}

/// Sends a widget an event, as a link would.
pub(crate) fn send(c: &mut Ctx, to: u32, event: u16) {
    // This lands in the middle of the caller's code: keep every register as it was.
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    (c.r[3], c.r[4], c.r[5], c.r[6]) = (0, 0, to, event as u32);
    (c.r[7], c.r[8], c.r[9], c.r[10]) = (0, 0, 0, 1);
    c.call(SEND_EVENT);
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
}

/// xStrHash(const char *) and xSTFindAsset(id, size *): the game's name hashing and its lookup
/// of a loaded asset by hashed name.
const STR_HASH: u32 = 0x8022_2788;
const FIND_ASSET: u32 = 0x8022_3130;
/// Where in the host's scratch block a name is put for the game to hash.
const NAME_SCRATCH: u32 = 0xD80;
/// A text widget's base type, and where it keeps the address of its text's characters.
pub(crate) const TEXT_WIDGET: u8 = 0x50;
pub(crate) const WIDGET_TEXT: u32 = 0x70;
/// From a button texture to its texels, as the GPU is given them.
const TEXTURE_TEXELS: u32 = 0xA0;

/// The game's button textures: name, the button, and the pad bits a click on its hint presses.
const BUTTON_TEXTURES: [(&str, Button, u32); 10] = [
    ("pad_button1", Button::A, crate::input::A),
    ("pad_button2", Button::Y, crate::input::Y),
    ("pad_button3", Button::B, crate::input::B),
    ("pad_button4", Button::X, crate::input::X),
    ("pad_button_start", Button::Start, crate::input::START),
    ("pad_button_L1", Button::L, crate::input::L),
    ("pad_button_R1", Button::R, crate::input::R),
    ("pad_button_R2", Button::Z, crate::input::Z),
    ("pad_button_L_analog", Button::Stick, 0),
    ("pad_button_R_analog", Button::CStick, 0),
];

/// Calls a game function from the middle of the caller's code, keeping every register as it
/// was, and returns its result.
pub(crate) fn call(c: &mut Ctx, function: u32, arguments: &[u32]) -> u32 {
    let saved = (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr);
    for (i, &argument) in arguments.iter().enumerate() {
        c.r[3 + i] = argument;
    }
    c.call(function);
    let result = c.r[3];
    (c.r, c.f, c.ps1, c.cr, c.lr, c.ctr, c.ca, c.xer_rest, c.fpscr) = saved;
    result
}

/// The loaded asset with this name, or 0.
fn asset_named(c: &mut Ctx, name: &str) -> u32 {
    let text = crate::host::with(|h| h.scratch) + NAME_SCRATCH;
    for (i, byte) in name.bytes().take(63).chain([0]).enumerate() {
        c.w8(text + i as u32, byte);
    }
    let id = call(c, STR_HASH, &[text]);
    call(c, FIND_ASSET, &[id, 0])
}

/// Tells the renderer where the button textures are (glyph.rs). They are loaded with the
/// game's fonts and stay, but looking again now and then costs little.
fn find_button_textures(c: &mut Ctx) {
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if CALLS.fetch_add(1, Ordering::Relaxed) % 120 != 0 {
        return;
    }
    let mut found = Vec::new();
    for (name, button, _) in BUTTON_TEXTURES {
        let texture = asset_named(c, name);
        if (0x8000_0000..0x8180_0000).contains(&texture) {
            found.push((texture + TEXTURE_TEXELS, button));
        }
    }
    crate::glyph::set_textures(found);
}

/// The characters at `addr` up to a zero, if that is in memory.
pub(crate) fn characters(c: &mut Ctx, addr: u32) -> String {
    if !(0x8000_0000..0x8180_0000 - 1024).contains(&addr) {
        return String::new();
    }
    let bytes = c.read_bytes(addr, 1024);
    String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or_default()).into_owned()
}

/// The pad bits of the buttons pictured in a text: `{i:name}` inserts the text asset of that
/// name (its length, then its characters) and `{tex:name;...}` draws a texture.
fn pictured_buttons(c: &mut Ctx, text: &str, depth: u32) -> u32 {
    let mut bits = 0;
    for tag in text.split('{').skip(1).filter_map(|rest| rest.split('}').next()) {
        if let Some(name) = tag.strip_prefix("i:").filter(|_| depth < 3) {
            let inserted = asset_named(c, name);
            let inserted = characters(c, inserted.wrapping_add(4));
            bits |= pictured_buttons(c, &inserted, depth + 1);
        } else if let Some(name) = tag.strip_prefix("tex:").and_then(|rest| rest.split(';').next()) {
            bits |= BUTTON_TEXTURES.iter().find(|entry| entry.0 == name).map_or(0, |entry| entry.2);
        }
    }
    bits
}

/// Whether a focused widget takes these two pad events (Up and Down, or Left and Right)
/// without handing the focus to another widget: the game then steers a list with its own code.
fn steers_itself(c: &Ctx, holder: &Widget, events: [u16; 2]) -> bool {
    let links = links(c, holder.addr);
    links.iter().any(|(received, ..)| events.contains(received))
        && !links.iter().any(|(received, sent, _)| events.contains(received) && matches!(*sent, FOCUS_ON | FOCUS_ON_SELECT))
}

/// The level select screen's hints along the bottom, by widget id: "(A) Warp to level",
/// "(X) Options" and "(B) Return to game".
const WARP_HINT: u32 = 0xC912_E9DA;
const OPTIONS_HINT: u32 = 0x8CAD_1203;
/// The skills menu's middle hint, "Upgrade description". While the menu asks "Are you sure
/// you want this upgrade?" no widget takes the pad (the menu's controller reads it) and the
/// other two hints read Yes and No.
const UPGRADE_HINT: u32 = 0x3ABD_D395;
const RETURN_HINT: u32 = 0x6EBF_D80A;
/// zUIText::DoRender()
const RENDER_TEXT: u32 = 0x8018_53DC;
/// Where in the host's scratch block hints' texts without their pictures are kept, a slot
/// for each hint drawn in a frame.
const HINT_SCRATCH: u32 = 0xA00;
const HINT_SLOT: u32 = 0x80;
const HINT_SLOTS: u32 = 7;
/// How far the level select screen's "Options" is moved sideways on the 640 by 480 screen.
/// Without its picture it is drawn about the middle and "Return to game" at the right; this
/// puts Options as far left of the middle as Return is right of it.
const OPTIONS_SHIFT: f32 = -181.0;
/// About how wide a character of a hint is drawn, on the 640 by 480 screen.
const HINT_CHARACTER: f32 = 11.5;

/// A hint: a text widget whose text starts with a button's picture, like "(A) Choose".
#[derive(Clone)]
struct Hint {
    /// The text after the picture.
    plain: String,
    /// The pad bits of the pictured button; 0 for other pictures (the arrows of "Adjust").
    buttons: u32,
}

impl Hint {
    /// With the mouse these say nothing: things are chosen and adjusted by clicking them.
    fn pointless(&self) -> bool {
        let plain = self.plain.to_ascii_lowercase();
        ["choose", "adjust", "warp to level"].iter().any(|start| plain.starts_with(start))
    }

    /// Where the text is drawn without its picture, left to right: at the left of a widget
    /// at the screen's left, at the right of one at its right, otherwise in the middle.
    fn extent(&self, widget: &Widget) -> (f32, f32) {
        let width = self.plain.chars().count() as f32 * HINT_CHARACTER;
        let [left, _, across, _] = widget.rect;
        let start = if widget.id == OPTIONS_HINT {
            left + across / 2.0 - width / 2.0 + OPTIONS_SHIFT
        } else if left <= 60.0 {
            left
        } else if left + across >= 580.0 {
            left + across - width
        } else {
            left + across / 2.0 - width / 2.0
        };
        (start, start + width)
    }
}

/// What kind of hint a text widget is, if it is one. Remembered by where the text is and
/// how it starts.
fn hint_of(c: &mut Ctx, widget: u32) -> Option<Hint> {
    static KNOWN: Mutex<Option<std::collections::HashMap<(u32, u32, u32), Option<Hint>>>> = Mutex::new(None);
    if c.r8(widget + 4) != TEXT_WIDGET {
        return None;
    }
    let text = c.r32(widget + WIDGET_TEXT);
    if !(0x8000_0000..0x8180_0000 - 1024).contains(&text) {
        return None;
    }
    let key = (text, c.r32(text), c.r32(text + 4));
    if let Some(known) = KNOWN.lock().unwrap().get_or_insert_with(Default::default).get(&key) {
        return known.clone();
    }
    let shown = characters(c, text);
    // The picture is an inserted text at the start: "{i:button_picture_04}  Options".
    let hint = shown.strip_prefix("{i:").and_then(|rest| rest.split_once('}')).and_then(|(name, rest)| {
        let picture = asset_named(c, name);
        let picture = characters(c, picture.wrapping_add(4));
        // A picture is a texture or a model, drawn by a tag of its own.
        picture.contains('{').then(|| Hint { plain: rest.trim_start().to_string(), buttons: pictured_buttons(c, &picture, 1) })
    });
    // "< > Adjust" pictures Left and Right with two characters of the font.
    let hint = hint.or_else(|| shown.strip_prefix("< >").map(|rest| Hint { plain: rest.trim_start().to_string(), buttons: 0 }));
    KNOWN.lock().unwrap().get_or_insert_with(Default::default).insert(key, hint.clone());
    hint
}

/// Whether `draw_text_as` is having the game draw a text: the hook stands aside for that.
static RENDERING_TEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The title screen's "Press Start", by widget id. With the keyboard and mouse it reads
/// "Press any key to continue" and any key or click is Start.
const PRESS_START: u32 = 0x917A_20B7;
/// Whether the title screen is waiting for Start.
static ON_TITLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the title screen is on show, waiting for Start.
pub fn on_title() -> bool {
    active() && ON_TITLE.load(Ordering::Relaxed)
}

/// How many hints `render_text` has drawn this frame.
static HINTS_DRAWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Runs in place of zUIText::DoRender(), a text widget drawing itself, for hints while the
/// keyboard and mouse are in use. With a mouse the hints are themselves clicked, so they are
/// drawn without the button's picture, and those that only say what clicking does ("Choose",
/// "Adjust", "Warp to level") are not drawn at all. The title screen's "Press Start" becomes
/// "Press any key to continue". After a gamepad they are the game's own
/// again.
pub fn render_text(c: &mut Ctx) -> bool {
    let widget = c.r[3];
    if RENDERING_TEXT.load(Ordering::Relaxed) {
        return false;
    }
    // The last of the settings is followed by the port's own (options.rs).
    if crate::options::render_row(c, widget) {
        return true;
    }
    if !crate::input::keyboard_in_use() {
        return false;
    }
    let id = c.r32(widget);
    if id == PRESS_START {
        draw_text_as(c, widget, "Press any key to continue", 0.0);
        return true;
    }
    let Some(hint) = hint_of(c, widget) else { return false };
    // The level select screen's first and last hints are also the Yes and No of the question
    // A on a challenge leads to. When the question was asked by a click it is answered on
    // the challenge, and reaching for a hint would answer no on the way: then neither is
    // drawn.
    if hint.pointless() || (matches!(id, WARP_HINT | RETURN_HINT) && ASKING.load(Ordering::Relaxed) && ASKED_BY_CLICK.load(Ordering::Relaxed)) {
        return true;
    }
    draw_text_as(c, widget, &hint.plain, if id == OPTIONS_HINT { OPTIONS_SHIFT } else { 0.0 });
    true
}

/// Has a text widget draw itself with another text, moved sideways by `shift`.
fn draw_text_as(c: &mut Ctx, widget: u32, shown: &str, shift: f32) {
    let slot = HINTS_DRAWN.fetch_add(1, Ordering::Relaxed) % HINT_SLOTS;
    let copy = crate::host::with(|h| h.scratch) + HINT_SCRATCH + slot * HINT_SLOT;
    for (i, byte) in shown.bytes().take(HINT_SLOT as usize - 1).chain([0]).enumerate() {
        c.w8(copy + i as u32, byte);
    }
    let (text, x) = (c.r32(widget + WIDGET_TEXT), c.r32(widget + 0x14));
    c.w32(widget + WIDGET_TEXT, copy);
    c.w32(widget + 0x14, (f32::from_bits(x) + shift).to_bits());
    RENDERING_TEXT.store(true, Ordering::Relaxed);
    call(c, RENDER_TEXT, &[widget]);
    RENDERING_TEXT.store(false, Ordering::Relaxed);
    c.w32(widget + WIDGET_TEXT, text);
    c.w32(widget + 0x14, x);
}

/// Has the game draw a text widget as it is, without `render_text` standing in.
pub(crate) fn render_text_unhooked(c: &mut Ctx, widget: u32) {
    RENDERING_TEXT.store(true, Ordering::Relaxed);
    call(c, RENDER_TEXT, &[widget]);
    RENDERING_TEXT.store(false, Ordering::Relaxed);
}

/// The hint under the cursor, if it is one that is drawn: a click on it presses its button.
pub(crate) fn hint_at(c: &mut Ctx, widgets: &[Widget], at: (f32, f32)) -> Option<u32> {
    for widget in widgets.iter().filter(|w| (w.rect[1]..w.rect[1] + w.rect[3]).contains(&at.1)) {
        let Some(hint) = hint_of(c, widget.addr) else { continue };
        let (left, right) = hint.extent(widget);
        if hint.buttons != 0 && !hint.pointless() && (left - 10.0..right + 10.0).contains(&at.0) {
            return Some(hint.buttons);
        }
    }
    None
}

/// A model widget's base type (the challenge icons of the level select screen), where its
/// asset names its model, and the model of a locked challenge's question mark.
const MODEL_WIDGET: u8 = 0x52;
const MODEL_ASSET_MODEL: u32 = 0x50;
const QUESTION_MARK: u32 = 0x5C10_3310;
/// The opacity the game gives the challenges that are not the chosen one.
pub(crate) const DIMMED: u8 = 100;
/// The widest an image may be to be the glow behind the chosen challenge.
const GLOW_WIDEST: f32 = 250.0;
/// A challenge's model is drawn higher than its widget's place says: where the cursor counts
/// as on it, as a top and a bottom from the widget's top, on the 640 by 480 screen.
const ICON_TOP: f32 = -28.0;
const ICON_BOTTOM: f32 = 38.0;
/// zUIImage::DoRender()
pub(crate) const RENDER_IMAGE: u32 = 0x8018_6D6C;
/// zUIModel::DoRender()
const RENDER_MODEL: u32 = 0x8018_5B58;

/// Whether the mouse has moved since a key was last pressed: the level select screen's
/// challenges then follow the cursor.
static MOUSE_STEERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether the question on show ("Warp to this level?") was asked by a click on a challenge:
/// the cursor leaving that challenge then answers no.
static ASKED_BY_CLICK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// While the challenges follow the cursor: their widgets, the one to draw lit (0 for none),
/// whether the game's glow is behind that one, and the row's top and bottom on the screen.
static LIGHTING: Mutex<Option<(Vec<u32>, u32, bool, (f32, f32))>> = Mutex::new(None);
/// Whether the level select screen is asking its question, as `before_update` last found.
static ASKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// When a click last asked (milliseconds on `clock`): the question takes a moment to appear.
static ASKED_AT: AtomicU64 = AtomicU64::new(0);
/// The chosen challenge as last seen, for the cursor to be on or off while the game asks.
static LAST_CHOSEN: Mutex<Option<Widget>> = Mutex::new(None);
/// The challenge the cursor last went for, and when (or, when it is reached by presses, how
/// often), so that one that cannot be reached is not tried for ever.
static STEPS_TRIED: Mutex<(u32, u32)> = Mutex::new((0, 0));

/// Called from the window thread when a bound key goes down.
pub fn key_pressed() {
    MOUSE_STEERS.store(false, Ordering::Relaxed);
}

/// The level select screen's challenges, left to right, and which one is chosen. They are
/// model widgets in a row; the game dims all but the chosen one, which it fades in to full
/// strength, and moves the choice with Left and Right.
struct Row {
    icons: Vec<Widget>,
    chosen: usize,
}

impl Row {
    fn find(c: &Ctx, holders: &[Widget]) -> Option<Row> {
        if !holders.iter().any(|holder| links(c, holder.addr).iter().any(|(received, ..)| [75, 76].contains(received))) {
            return None;
        }
        // A challenge just chosen starts fully transparent and fades in: count those too.
        let widgets = listed(c, true);
        let first = widgets.iter().find(|w| w.kind == MODEL_WIDGET)?;
        let overlaps = |w: &Widget| w.rect[1] < first.rect[1] + first.rect[3] && first.rect[1] < w.rect[1] + w.rect[3];
        let centre = |w: &Widget| w.rect[0] + w.rect[2] / 2.0;
        let mut icons: Vec<Widget> = widgets.iter().copied().filter(|w| w.kind == MODEL_WIDGET && overlaps(w)).collect();
        icons.sort_by(|a, b| centre(a).total_cmp(&centre(b)));
        // The others are all dimmed alike; the chosen one is fading in, or lit. Should it pass
        // the others' strength on its way, it is the one that was chosen a moment ago.
        let standing_out: Vec<usize> = (0..icons.len()).filter(|&i| icons[i].opacity != DIMMED).collect();
        let last = LAST_CHOSEN.lock().unwrap().map(|w| w.addr);
        let chosen = match standing_out[..] {
            [chosen] => chosen,
            _ => icons.iter().position(|w| Some(w.addr) == last)?,
        };
        // From here on a challenge's place is where it is drawn.
        for icon in &mut icons {
            icon.rect = [icon.rect[0], icon.rect[1] + ICON_TOP, icon.rect[2], ICON_BOTTOM - ICON_TOP];
        }
        Some(Row { icons, chosen })
    }

    /// The presses that move the choice to `target`.
    fn steps_to(&self, target: usize) -> Vec<u32> {
        let press = if target > self.chosen { crate::input::STICK_RIGHT } else { crate::input::STICK_LEFT };
        vec![press; target.abs_diff(self.chosen).min(8)]
    }
}

/// The level select screen with the mouse: the challenge under the cursor is the chosen one,
/// and none is lit while the cursor is on none of them. The game always has one chosen and
/// moves the choice a place at a time, lighting each on the way, so the lighting is done
/// here instead: the cursor's challenge is drawn lit at once and every other dimmed
/// (`render_model`), and the game's glow is left out until its choice has arrived under the
/// cursor (`render_image`).
fn steer_challenges(row: &Row, at: Option<(f32, f32)>) -> Option<usize> {
    let hovered = at.and_then(|at| row.icons.iter().position(|w| w.contains(at)));
    let band = (row.icons[0].rect[1], row.icons[0].rect[1] + row.icons[0].rect[3]);
    *LIGHTING.lock().unwrap() =
        Some((row.icons.iter().map(|w| w.addr).collect(), hovered.map_or(0, |i| row.icons[i].addr), hovered == Some(row.chosen), band));
    hovered.filter(|&target| target != row.chosen)
}

/// Makes the challenge at `target` the game's chosen one. Each challenge has a text widget
/// (its name, under the row) and the chosen one's has the focus; they are numbered in a row,
/// so the target's is found from the chosen one's. Handing the focus straight over, as Left
/// and Right hand it to a neighbour, chooses the target without passing the ones between.
/// Failing that, Left or Right is pressed as often as it takes.
fn choose_challenge(c: &mut Ctx, row: &Row, holders: &[Widget], target: usize) {
    let name = holders.iter().find(|holder| links(c, holder.addr).iter().any(|(received, ..)| [75, 76].contains(received)));
    let wanted = name.map(|name| name.id.wrapping_add(target as u32).wrapping_sub(row.chosen as u32));
    let count = c.r32(c.r[13].wrapping_add(WIDGET_COUNT)).min(1024);
    let found = wanted.and_then(|id| (0..count).map(|i| c.r32(WIDGETS + i * 4)).find(|&ui| (0x8000_0000..0x8180_0000).contains(&ui) && c.r32(ui) == id));
    let mut tried = STEPS_TRIED.lock().unwrap();
    if let (Some(name), Some(found)) = (name, found) {
        // Once, and again only if the game has not followed after a while.
        if tried.0 != found || clock().saturating_sub(tried.1 as u64) > 1000 {
            *tried = (found, clock() as u32);
            // The focus goes to the target's neighbour on this side, and the last step is
            // the game's own Right or Left, which walks SpongeBob over with his footsteps.
            let right = target > row.chosen;
            let neighbour_id = c.r32(found).wrapping_add(if right { u32::MAX } else { 1 });
            let neighbour = if target.abs_diff(row.chosen) == 1 {
                Some(name.addr)
            } else {
                (0..count).map(|i| c.r32(WIDGETS + i * 4)).find(|&ui| (0x8000_0000..0x8180_0000).contains(&ui) && c.r32(ui) == neighbour_id)
            };
            match neighbour {
                Some(neighbour) => {
                    if neighbour != name.addr {
                        send(c, name.addr, FOCUS_OFF_UNSELECT);
                        send(c, neighbour, FOCUS_ON_SELECT);
                    }
                    send(c, neighbour, if right { RIGHT_EVENT } else { LEFT_EVENT });
                }
                None => {
                    send(c, name.addr, FOCUS_OFF_UNSELECT);
                    send(c, found, FOCUS_ON_SELECT);
                }
            }
        }
        return;
    }
    if !crate::input::pulses_idle() {
        return;
    }
    if tried.0 != row.icons[target].addr {
        *tried = (row.icons[target].addr, 0);
    }
    if tried.1 < 3 {
        tried.1 += 1;
        for press in row.steps_to(target) {
            crate::input::pulse(press);
        }
    }
}

/// Whether the level select screen is asking "Warp to this level?" (and then "Are you sure
/// you're sure?"), which A on a challenge leads to. While it asks, no widget has the focus,
/// the Options hint's text is empty and the other two hints read Yes and No.
fn asking(c: &mut Ctx, widgets: &[Widget]) -> bool {
    let Some(options) = widgets.iter().find(|w| w.id == OPTIONS_HINT) else { return false };
    let text = c.r32(options.addr + WIDGET_TEXT);
    widgets.iter().any(|w| w.id == RETURN_HINT) && characters(c, text).is_empty()
}

/// The mouse while the level select screen is asking. A click on the challenge answers yes
/// (A), as the click that asked did. The game does not let the choice move while it asks, so
/// when the question came from a click, the cursor leaving the challenge answers no (B) and
/// the challenges follow the cursor again. A question asked from the keyboard stays until it
/// is answered there or by a click on Yes or No.
fn answer_question(widgets: &[Widget], at: Option<(f32, f32)>, clicked: bool) {
    let chosen = *LAST_CHOSEN.lock().unwrap();
    let on_challenge = matches!((at, chosen), (Some(at), Some(chosen)) if chosen.contains(at));
    let by_click = ASKED_BY_CLICK.load(Ordering::Relaxed);
    if clicked {
        if on_challenge {
            crate::input::pulse(crate::input::A);
        } else if let (false, Some(at), Some(hints)) = (by_click, at, widgets.iter().find(|w| w.id == RETURN_HINT)) {
            if (hints.rect[1]..hints.rect[1] + hints.rect[3]).contains(&at.1) {
                crate::input::pulse(if at.0 < 320.0 { crate::input::A } else { crate::input::B });
            }
        }
    } else if by_click && !on_challenge && MOUSE_STEERS.load(Ordering::Relaxed) && crate::input::pulses_idle() {
        ASKED_BY_CLICK.store(false, Ordering::Relaxed);
        crate::input::pulse(crate::input::B);
    }
}

/// Runs in place of zUIModel::DoRender(), a model widget drawing itself, for the challenges
/// while they follow the cursor: the one under it is drawn lit, the others dimmed.
pub fn render_model(c: &mut Ctx) -> bool {
    static INSIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let widget = c.r[3];
    if INSIDE.load(Ordering::Relaxed) {
        return false;
    }
    let lit = match &*LIGHTING.lock().unwrap() {
        Some((icons, lit, ..)) if icons.contains(&widget) => *lit == widget,
        _ => return false,
    };
    let opacity = c.r8(widget + 0x27);
    c.w8(widget + 0x27, if lit { 255 } else { DIMMED });
    INSIDE.store(true, Ordering::Relaxed);
    call(c, RENDER_MODEL, &[widget]);
    INSIDE.store(false, Ordering::Relaxed);
    c.w8(widget + 0x27, opacity);
    true
}

/// Runs in place of zUIImage::DoRender(), an image widget drawing itself: the glow behind
/// the game's chosen challenge (a focused image across the row) is left out while that is
/// not the challenge under the cursor.
pub fn render_image(c: &mut Ctx) -> bool {
    let Some((false, (top, bottom))) = LIGHTING.lock().unwrap().as_ref().map(|(_, _, glow, band)| (*glow, *band)) else {
        return false;
    };
    let widget = c.r[3];
    let f = |offset: u32| f32::from_bits(c.r32(widget + offset));
    c.r8(widget + 0x55) != 0 && f(0x1C) < GLOW_WIDEST && f(0x18) < bottom && top < f(0x18) + f(0x20)
}

/// Whether an image widget has the size of a volume setting's bar.
fn is_bar(w: &Widget) -> bool {
    w.kind == IMAGE_WIDGET && w.rect[2] == 40.0 && (w.rect[3] == 40.0 || w.rect[3] == 50.0)
}

/// A volume setting on show: eleven bars in a row, an image each, with one more image on
/// top of the bar of the current level. Gives the bars' left edges, left to right, and which
/// is the current one.
fn slider(widgets: &[Widget]) -> Option<(Vec<f32>, usize)> {
    let mut bars: Vec<f32> = widgets.iter().filter(|w| is_bar(w)).map(|w| w.rect[0]).collect();
    bars.sort_by(f32::total_cmp);
    let current = bars.windows(2).find(|pair| pair[0] == pair[1])?[0];
    bars.dedup();
    (bars.len() >= 8).then(|| bars.iter().position(|&bar| bar == current).map(|current| (bars, current)))?
}

/// The base type of image widgets.
pub(crate) const IMAGE_WIDGET: u8 = 0x51;
/// How far above or below a list's title its arrows are clicked, on the 640 by 480 screen.
const ARROW_REACH: f32 = 60.0;

/// Clicks on the parts of a screen that the game steers with its own code rather than by
/// moving the focus between widgets, as the level select screen (the pause screen) does: the
/// presses that do what was clicked, if anything was.
///
/// - A focused widget that takes Up and Down without handing the focus on is a list showing
///   one entry (the level's name). Its arrows are drawn just above and below it: a click
///   there presses Up or Down.
/// - A row of models is a list shown whole (the level's challenges), the chosen one at full
///   strength and the others dimmed. A click on one chooses it and starts it (see
///   `steer_challenges`).
fn custom_click(c: &Ctx, widgets: &[Widget], holders: &[Widget], row: Option<&Row>, at: (f32, f32)) -> Option<Vec<u32>> {
    use crate::input::{STICK_DOWN, STICK_UP};
    let takes = |holder: &Widget, events: [u16; 2]| steers_itself(c, holder, events);
    let shown = |id: u32| widgets.iter().find(|w| w.id == id);
    if let (true, Some(hints)) = (crate::input::keyboard_in_use(), shown(OPTIONS_HINT)) {
        // The hints as `render_text` shows them: Options on the left, Return on the right.
        if (hints.rect[1]..hints.rect[1] + hints.rect[3]).contains(&at.1) {
            return Some(vec![if at.0 < 320.0 { crate::input::X } else { crate::input::B }]);
        }
    }
    if let Some(row) = row {
        if let Some(target) = row.icons.iter().position(|w| w.contains(at)) {
            // (The cursor has chosen it already, unless the keyboard moved the choice since.)
            let mut presses = row.steps_to(target);
            // A challenge that is open is also started (A: the game's "Warp to level", which
            // asks before it does); one still locked, shown as a question mark, is only chosen.
            if c.r32(c.r32(row.icons[target].addr + 0x10) + MODEL_ASSET_MODEL) != QUESTION_MARK {
                presses.push(crate::input::A);
                ASKED_BY_CLICK.store(true, Ordering::Relaxed);
                ASKED_AT.store(clock(), Ordering::Relaxed);
            }
            return Some(presses);
        }
    }
    if let Some(presses) = setting_presses(c, widgets, holders, at) {
        return Some(presses);
    }
    for holder in holders.iter().filter(|holder| takes(holder, [73, 74])) {
        let [left, top, width, height] = holder.rect;
        if !(left..left + width).contains(&at.0) {
            continue;
        }
        if (top - ARROW_REACH..top).contains(&at.1) {
            return Some(vec![STICK_UP]);
        }
        if (top + height..top + height + ARROW_REACH).contains(&at.1) {
            return Some(vec![STICK_DOWN]);
        }
    }
    None
}

/// The pad events Right and Left, as the focused widget is sent them.
const RIGHT_EVENT: u16 = 75;
const LEFT_EVENT: u16 = 76;
/// The setting whose bars are being dragged: the left button went down on them and is
/// still down.
static DRAGGING: Mutex<Option<u32>> = Mutex::new(None);

/// A click on a setting's value (its bars, Min or Max, Off or On): changes it at once, by
/// sending the setting's widget Right or Left as often as it takes, as the pad would one at
/// a time. A click on the bars starts a drag (`drag`). True if the click was on a value.
fn set_value(c: &mut Ctx, widgets: &[Widget], holders: &[Widget], at: (f32, f32)) -> bool {
    let Some(holder) = holders.iter().find(|holder| steers_itself(c, holder, [75, 76])) else { return false };
    let Some(presses) = setting_presses(c, widgets, holders, at) else { return false };
    let on_bars = slider(widgets).is_some_and(|(levels, _)| (levels[0]..levels[levels.len() - 1] + 40.0).contains(&at.0))
        && !widgets.iter().any(|w| w.kind == TEXT_WIDGET && w.contains(at));
    if on_bars {
        *DRAGGING.lock().unwrap() = Some(holder.addr);
    }
    for press in presses {
        send(c, holder.addr, if press == crate::input::STICK_RIGHT { RIGHT_EVENT } else { LEFT_EVENT });
    }
    true
}

/// While the bars are dragged: the level follows the cursor across them.
fn drag(c: &mut Ctx, widgets: &[Widget], holder: u32, at: (f32, f32)) {
    let Some((levels, current)) = slider(widgets) else { return };
    let nearest = |a: &usize, b: &usize| (levels[*a] + 20.0 - at.0).abs().total_cmp(&(levels[*b] + 20.0 - at.0).abs());
    let Some(target) = (0..levels.len()).min_by(nearest) else { return };
    for _ in 0..target.abs_diff(current) {
        send(c, holder, if target > current { RIGHT_EVENT } else { LEFT_EVENT });
    }
}

/// The presses that change a setting to the value clicked: a volume's bar, Min or Max, Off or
/// On. A setting is on show beside the list of settings while a focused widget takes Left and
/// Right without handing the focus on.
fn setting_presses(c: &Ctx, widgets: &[Widget], holders: &[Widget], at: (f32, f32)) -> Option<Vec<u32>> {
    use crate::input::{STICK_LEFT, STICK_RIGHT};
    if !holders.iter().any(|holder| steers_itself(c, holder, [75, 76])) {
        return None;
    }
    // A setting is on show beside the list of settings: a row of bars for a volume, or
    // a pair of words (Off and On, with Min and Max above the bars).
    if let Some((levels, current)) = slider(widgets) {
        let across = levels[0] + 5.0..levels[levels.len() - 1] + 35.0;
        let bars = widgets.iter().filter(|w| is_bar(w));
        let (top, bottom) = bars.fold((f32::MAX, f32::MIN), |(top, bottom), w| (top.min(w.rect[1]), bottom.max(w.rect[1] + w.rect[3])));
        if across.contains(&at.0) && (top..bottom).contains(&at.1) {
            let nearest = |a: &usize, b: &usize| (levels[*a] + 20.0 - at.0).abs().total_cmp(&(levels[*b] + 20.0 - at.0).abs());
            let target = (0..levels.len()).min_by(nearest)?;
            let press = if target > current { STICK_RIGHT } else { STICK_LEFT };
            return Some(vec![press; target.abs_diff(current)]);
        }
    }
    // (Not hints: their texts start with a picture's tag.)
    let plain = |w: &&Widget| w.kind == TEXT_WIDGET && links(c, w.addr).is_empty() && c.r8(c.r32(w.addr + WIDGET_TEXT)) != b'{';
    if let Some(word) = widgets.iter().filter(plain).find(|w| w.contains(at)) {
        let twin = widgets.iter().filter(plain).find(|w| w.addr != word.addr && w.rect[1] == word.rect[1] && w.rect[2] == word.rect[2]);
        if let Some(twin) = twin {
            let left = word.rect[0] < twin.rect[0];
            // Min and Max go all the way; Off and On are a step apart.
            let steps = slider(widgets).map_or(1, |(levels, current)| if left { current } else { levels.len() - 1 - current });
            return Some(vec![if left { STICK_LEFT } else { STICK_RIGHT }; steps]);
        }
    }
    None
}

/// Whether a hint on show stands for a button other than A and B: something done to the
/// item that has the focus.
fn acts_on_focus(c: &mut Ctx, widgets: &[Widget]) -> bool {
    widgets.iter().any(|w| hint_of(c, w.addr).is_some_and(|hint| hint.buttons & !(crate::input::A | crate::input::B) != 0 && !hint.pointless()))
}

/// Whether a click here is on a setting's value rather than on the item whose box it lies in.
fn custom_wins(c: &Ctx, widgets: &[Widget], holders: &[Widget], at: (f32, f32)) -> bool {
    setting_presses(c, widgets, holders, at).is_some()
}

/// When a movie last decoded a frame (milliseconds on `clock`).
static MOVIE_SEEN: AtomicU64 = AtomicU64::new(0);

/// Whether a movie is playing.
pub fn in_movie() -> bool {
    clock().saturating_sub(MOVIE_SEEN.load(Ordering::Relaxed)) < 250
}

/// Runs before iFMVPlay: a movie that some button skips is skipped by any button.
pub fn before_movie(c: &mut Ctx) {
    if c.r[4] != 0 {
        c.r[4] = u32::MAX;
    }
}

/// Runs before BinkDoFrame, once for each frame of a movie.
pub fn movie_frame(_c: &mut Ctx) {
    MOVIE_SEEN.store(clock(), Ordering::Relaxed);
}

/// Runs before zUIMgrUpdate(float dt), once a frame while a scene with a UI is running.
pub fn before_update(c: &mut Ctx) {
    trace(c);
    find_button_textures(c);
    *LIGHTING.lock().unwrap() = None;
    HINTS_DRAWN.store(0, Ordering::Relaxed);
    let widgets = on_show(c);
    // The widgets the pad is being signalled to.
    let holders: Vec<Widget> = widgets
        .iter()
        .copied()
        .filter(|w| w.focused && links(c, w.addr).iter().any(|(received, ..)| PAD_EVENTS.contains(received)))
        .collect();
    let keyboard = crate::input::keyboard_in_use();
    let asking = keyboard && asking(c, &widgets);
    ASKING.store(asking, Ordering::Relaxed);
    // "Press Start" blinks, and is not on show while it is fully transparent.
    let title = listed(c, true).iter().any(|w| w.id == PRESS_START && w.focused);
    ON_TITLE.store(title, Ordering::Relaxed);
    if crate::options::update(c, &widgets) {
        return;
    }
    let buying = keyboard && holders.is_empty() && widgets.iter().any(|w| w.id == UPGRADE_HINT);
    if holders.is_empty() && !asking && !title && !buying {
        return;
    }
    MENU_SEEN.store(clock(), Ordering::Relaxed);
    TAKES_START.store(widgets.iter().any(|w| links(c, w.addr).iter().any(|(received, ..)| *received == START_EVENT)), Ordering::Relaxed);
    ON_LEVEL_SELECT.store(!asking && widgets.iter().any(|w| w.id == OPTIONS_HINT), Ordering::Relaxed);
    let (at, moved, clicked) = {
        let mut pointer = POINTER.lock().unwrap();
        (pointer.at, std::mem::take(&mut pointer.moved), std::mem::take(&mut pointer.clicked))
    };
    if asking {
        answer_question(&widgets, at, clicked);
        return;
    }
    // The skills menu's question: a click on Yes or No presses its button.
    if buying {
        if let (Some(at), true) = (at, clicked) {
            if let Some(bits) = hint_at(c, &widgets, at) {
                crate::input::pulse(bits);
            }
        }
        return;
    }
    if clock().saturating_sub(ASKED_AT.load(Ordering::Relaxed)) > 500 {
        ASKED_BY_CLICK.store(false, Ordering::Relaxed);
    }
    let row = Row::find(c, &holders);
    if let Some(row) = &row {
        *LAST_CHOSEN.lock().unwrap() = Some(row.icons[row.chosen]);
        if keyboard && MOUSE_STEERS.load(Ordering::Relaxed) {
            if let Some(target) = steer_challenges(row, at) {
                choose_challenge(c, row, &holders, target);
            }
        }
    }
    // A drag on a setting's bars lasts while the button is down.
    let dragging = *DRAGGING.lock().unwrap();
    if let Some(holder) = dragging {
        if button_held() && holders.iter().any(|w| w.addr == holder) {
            if let (Some(at), true) = (at, moved) {
                drag(c, &widgets, holder, at);
            }
            return;
        }
        *DRAGGING.lock().unwrap() = None;
    }
    let Some(at) = at.filter(|_| moved || clicked) else { return };

    // Each menu item with the focus holder it can be reached from and the event that gives
    // it the focus there.
    let mut items: Vec<(Widget, Widget, u16)> = holders.iter().map(|&w| (w, w, FOCUS_ON_SELECT)).collect();
    let mut next = 0;
    while next < items.len() {
        let (from, holder, _) = items[next];
        next += 1;
        let asset = c.r32(from.addr + 0x10);
        let mut reached: Vec<(u16, u32)> = links(c, from.addr)
            .into_iter()
            .filter(|(received, sent, _)| PAD_DIRECTIONS.contains(received) && matches!(*sent, FOCUS_ON | FOCUS_ON_SELECT))
            .map(|(_, sent, id)| (sent, id))
            .collect();
        reached.extend((0..4).map(|n| (FOCUS_ON_SELECT, c.r32(asset + 0x38 + n * 4))).filter(|&(_, id)| id != 0));
        for (event, id) in reached {
            if items.iter().any(|(item, ..)| item.id == id) {
                continue;
            }
            if let Some(&widget) = widgets.iter().find(|w| w.id == id) {
                items.push((widget, holder, event));
            }
        }
    }
    let area = |w: &Widget| w.rect[2] * w.rect[3];
    // Where several items share a place (one text for each entry of a list, one of them
    // shown), the one with the focus is the one under the cursor.
    // A box that takes the pad for the whole screen (the autosave notice, with Yes and No
    // inside it) is not what the cursor is on while it is on an item inside the box.
    let inside = |inner: &Widget, outer: &Widget| {
        let ([l, t, w, h], [ol, ot, ow, oh]) = (inner.rect, outer.rect);
        inner.id != outer.id && w * h < ow * oh && l >= ol && t >= ot && l + w <= ol + ow && t + h <= ot + oh
    };
    let under: Vec<&(Widget, Widget, u16)> = items.iter().filter(|(item, ..)| item.contains(at)).collect();
    let hovered = under
        .iter()
        .copied()
        .filter(|(item, ..)| !under.iter().any(|(other, ..)| inside(other, item)))
        .min_by(|a, b| (!a.0.focused, area(&a.0)).partial_cmp(&(!b.0.focused, area(&b.0))).unwrap_or(std::cmp::Ordering::Equal));
    let hovered = hovered.filter(|_| !(clicked && (custom_wins(c, &widgets, &holders, at) || hint_at(c, &widgets, at).is_some())));
    let Some(&(item, holder, event)) = hovered else {
        if clicked {
            if set_value(c, &widgets, &holders, at) {
                return;
            }
            if let Some(bits) = custom_click(c, &widgets, &holders, row.as_ref(), at) {
                for press in bits {
                    crate::input::pulse(press);
                }
                return;
            }
            // Not on an item: a click on a hint ("Return") presses the button it stands for.
            if let Some(bits) = hint_at(c, &widgets, at) {
                press(bits);
            }
        }
        return;
    };
    // A setting (an item that takes Left and Right to change a value) is chosen by a click,
    // not by the cursor passing over it on its way to the value shown beside the list.
    let setting = steers_itself(c, &item, [75, 76]);
    // Where a hint is clicked for something done to the item with the focus ("Upgrade
    // description" on the skills menu), the cursor crosses other items on its way there.
    // There the focus does not follow the cursor: a click gives an item the focus, and a
    // click on the item that has it chooses it.
    let selects = !setting && !item.focused && acts_on_focus(c, &widgets);
    if selects && !clicked {
        return;
    }
    if !item.focused && (clicked || !setting) {
        send(c, holder.addr, if event == FOCUS_ON { FOCUS_OFF } else { FOCUS_OFF_UNSELECT });
        send(c, item.addr, event);
    }
    // A list the game steers itself is not started by a click on its title (the level's name
    // on the level select screen): its entries are.
    if clicked && !setting && !selects && !steers_itself(c, &item, [73, 74]) {
        crate::input::pulse(crate::input::A);
    }
}

/// TSSM_MOUSE scripts the mouse for unattended runs, as TSSM_INPUT does the pad:
/// "320,165@2900 click@2950 right@3100 wheel-1@3200" puts the cursor at that point of the
/// 640 by 480 screen at retrace 2900, clicks at 2950, presses the right button at 3100 and
/// turns the wheel a notch down at 3200; "gamepad@3300" and "keyboard@3400" make that the
/// device last used; "press" and "release" put the left button down and up, for drags;
/// "escape" and "enter" press those keys.
/// Called once per pad poll.
pub fn script(retrace: u32) {
    static SCRIPT: std::sync::OnceLock<Vec<(u32, String)>> = std::sync::OnceLock::new();
    static DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let script = SCRIPT.get_or_init(|| {
        let spec = std::env::var("TSSM_MOUSE").unwrap_or_default();
        let mut steps: Vec<(u32, String)> =
            spec.split_whitespace().filter_map(|step| step.split_once('@').and_then(|(what, at)| Some((at.parse().ok()?, what.to_string())))).collect();
        steps.sort_by_key(|step| step.0);
        steps
    });
    while let Some((at, what)) = script.get(DONE.load(Ordering::Relaxed)) {
        if *at > retrace {
            break;
        }
        DONE.fetch_add(1, Ordering::Relaxed);
        if what == "escape" || what == "enter" {
            let key = if what == "escape" { winit::keyboard::KeyCode::Escape } else { winit::keyboard::KeyCode::Enter };
            crate::input::key_event(key, true);
            crate::input::key_event(key, false);
        } else if what == "gamepad" || what == "keyboard" {
            crate::input::set_gamepad_last(what == "gamepad");
        } else if what == "click" {
            crate::input::mouse_button(winit::event::MouseButton::Left, true);
            crate::input::mouse_button(winit::event::MouseButton::Left, false);
        } else if what == "press" || what == "release" {
            crate::input::mouse_button(winit::event::MouseButton::Left, what == "press");
        } else if what == "right" {
            crate::input::mouse_button(winit::event::MouseButton::Right, true);
        } else if let Some(notches) = what.strip_prefix("wheel").and_then(|n| n.parse().ok()) {
            crate::input::mouse_wheel(notches);
        } else if let Some((x, y)) = what.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?))) {
            let mut pointer = POINTER.lock().unwrap();
            pointer.at = Some((x, y));
            pointer.moved = true;
            MOUSE_STEERS.store(true, Ordering::Relaxed);
        }
    }
}
