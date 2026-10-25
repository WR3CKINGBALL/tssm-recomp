//! The port's own settings, Frame Rate and Field of View, in the game's settings screen
//! (Options, Options) below its own three: Rumble Feature, Music Volume and SFX Volume.
//!
//! They are not widgets of the game's. The game's rows are text widgets; once the last of
//! them has drawn itself, it is drawn twice more, lower down and with the port's names
//! (`draw`). Down on SFX Volume, which the game ignores, takes the focus off it (the game's
//! own event, so it dims and hides its bars) and gives it to Frame Rate. While one of the
//! port's rows has the focus the game sees no arrows, stick or A (`filter_pad`): Up and Down
//! move between the rows, Up from Frame Rate gives SFX Volume the focus back, and Left and
//! Right change the value. The title, the words above the bars and the bars are the game's
//! widgets for the volumes, drawn with other texts and at other places.
//!
//! Each setting has eleven values, one for each place on the game's row of bars. The frame
//! rate is saved to config/fps.txt and the field of view to config/fov.txt (see host.rs and
//! aspect.rs), as soon as they change.

use std::sync::Mutex;

use runtime::Ctx;

use crate::input::{A, DOWN, LEFT, RIGHT, UP};
use crate::menu::{self, Widget, DIMMED, FOCUS_OFF_UNSELECT, FOCUS_ON_SELECT, IMAGE_WIDGET, TEXT_WIDGET, WIDGET_TEXT};

struct Setting {
    name: &'static str,
    values: [u32; 11],
}

const SETTINGS: [Setting; 2] = [
    Setting { name: "Frame Rate", values: [60, 75, 90, 120, 144, 165, 200, 240, 300, 360, 500] },
    Setting { name: "Field of View", values: [60, 65, 70, 75, 80, 85, 90, 95, 100, 105, 110] },
];

/// The game's settings, by their texts as they are stored (lower case here).
const GAME_SETTINGS: [&str; 3] = ["rumble feature", "music volume", "sfx volume"];
/// Where the game's rows are while the screen is on show (left edge, then each one's top).
/// The screen slides in and out (up off the top when it is left, its widgets still on show),
/// and what is drawn here moves with it. A row also bounces while the cursor is on it, which
/// what is drawn here does not follow.
const ROWS_LEFT: f32 = 80.0;
const ROW_TOPS: [f32; 3] = [190.0, 230.0, 270.0];
/// Where all five rows are put instead, evenly, to fit in the panel: the game's three are
/// drawn and clicked this much higher (`row_shift`), and the port's two follow.
const LAYOUT_TOPS: [f32; 5] = [172.0, 210.0, 248.0, 286.0, 324.0];
const ROW_HEIGHT: f32 = 38.0;
/// A row with the focus bounces as the game's do: up and back down over the first part of
/// each period (milliseconds), this high at the top.
const BOUNCE_PERIOD: u64 = 500;
const BOUNCE_UP: u64 = 300;
const BOUNCE_HEIGHT: f32 = 5.0;
/// Where the game's volume widgets are: the title, the words above the bars, and the bars
/// (eleven places 20 apart from `BARS_LEFT`, the end ones taller and higher), for clicks.
const TITLE: [f32; 4] = [190.0, 100.0, 260.0, 40.0];
const WORDS_TOP: f32 = 210.0;
const WORD_LEFT: f32 = 295.0;
const WORD_RIGHT: f32 = 455.0;
const BARS_LEFT: f32 = 305.0;
const BAR_STEP: f32 = 20.0;
const END_TOP: f32 = 280.0;
/// The volumes' bars, by widget id: ten in a row with neighbouring ids, then the one at the
/// right end. The dark ones are always on show, the lit ones only at a volume's level.
const DARK_BARS: (u32, u32) = (0x5F72_9DFE, 0xD7A6_D9AD);
const LIT_BARS: (u32, u32) = (0x5AF6_E971, 0x8C59_7586);

fn bar_id(bars: (u32, u32), place: usize) -> u32 {
    if place < 10 { bars.0 + place as u32 } else { bars.1 }
}

/// A text widget's shadow's opacity (255 with the focus, 100 without).
const TEXT_SHADOW: u32 = 0x6F;

/// Where in the host's scratch block the texts drawn here are kept, a slot each.
const TEXT_SCRATCH: u32 = 0xDC0;
const TEXT_SLOT: u32 = 0x40;
const TEXT_SLOTS: u32 = 8;
/// A held direction repeats after this long, this often (milliseconds).
const REPEAT_AFTER: u64 = 400;
const REPEAT_EVERY: u64 = 90;
/// How far the stick must be pushed to count as a direction.
const STICK_PUSH: i8 = 40;

enum Action {
    Focus(u32, u16),
}

struct State {
    /// When the settings screen was last seen (milliseconds on `clock`).
    seen: u64,
    /// The game's three rows: widget addresses, and whether each has the focus.
    rows: [u32; 3],
    focused: [bool; 3],
    /// Which of the port's rows has the focus.
    focus: Option<usize>,
    /// Events to send when the game next updates its menus.
    actions: Vec<Action>,
    /// The directions held at the last pad poll (Up, Down, Left, Right as bits 0 to 3), when
    /// a held one next repeats, and whether directions are kept from the game until released.
    held: u8,
    repeat_at: u64,
    swallow: bool,
    /// Whether the bars are being dragged.
    dragging: bool,
    /// Texts drawn this frame.
    texts: u32,
    /// How far the screen has slid from its place.
    offset: (f32, f32),
    /// When the focus last moved to one of the port's rows (milliseconds on `clock`).
    focus_since: u64,
}

static STATE: Mutex<State> = Mutex::new(State {
    seen: 0,
    rows: [0; 3],
    focused: [false; 3],
    focus: None,
    actions: Vec::new(),
    held: 0,
    repeat_at: 0,
    swallow: false,
    dragging: false,
    texts: 0,
    offset: (0.0, 0.0),
    focus_since: 0,
});

fn clock() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

fn on_show(state: &State) -> bool {
    state.seen != 0 && clock().saturating_sub(state.seen) < 250
}

/// Whether the settings screen is on show and in its place. After it has been left the game
/// keeps its widgets, slid out of sight, while the options list is up: not on show then.
pub fn settings_on_show() -> bool {
    let state = STATE.lock().unwrap();
    on_show(&state) && !slid_away(&state)
}

fn slid_away(state: &State) -> bool {
    state.offset.1.abs() > 100.0
}

/// Until when A reaches the game although one of these rows has the focus (milliseconds on
/// `clock`): leaving the screen accepts its changes first (`menu::press`).
static ACCEPT_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Lets A through for a moment, for the accept that leaving the screen presses.
pub fn let_accept_through() {
    ACCEPT_UNTIL.store(clock() + 500, std::sync::atomic::Ordering::Relaxed);
}

/// Which of a setting's values is in use.
fn current(setting: usize) -> usize {
    let now = match setting {
        0 => crate::host::frame_rate_now(),
        _ => crate::aspect::fov() as f64,
    };
    let values = &SETTINGS[setting].values;
    (0..values.len()).min_by(|&a, &b| (values[a] as f64 - now).abs().total_cmp(&(values[b] as f64 - now).abs())).unwrap()
}

/// Puts a setting's value in use and saves it, if it is not the one in use.
fn choose(setting: usize, index: usize) {
    if index.min(10) == current(setting) {
        return;
    }
    let value = SETTINGS[setting].values[index.min(10)];
    match setting {
        0 => {
            crate::host::set_frame_rate(value as f64);
            save("config/fps.txt", value);
        }
        _ => {
            crate::aspect::set_fov(value as f32);
            save("config/fov.txt", value);
        }
    }
}

/// Writes a value to a config file, keeping its comments.
fn save(path: &str, value: u32) {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut text: String = old.lines().filter(|line| line.trim_start().starts_with('#')).map(|line| format!("{line}\n")).collect();
    if text.is_empty() && path.ends_with("fov.txt") {
        text = "# Field of view in play, in degrees across a 4:3 picture (wider windows see more at the\n\
                # sides). The game's own is 75. Cutscenes keep theirs.\n"
            .to_string();
    }
    text += &format!("{value}\n");
    if let Err(error) = std::fs::write(path, text) {
        eprintln!("[options] cannot save {path}: {error}");
    }
}

/// How much lower than the game puts it a widget is drawn and clicked (negative: higher).
/// The game's settings move up to make room for the port's.
pub fn row_shift(widget: u32) -> f32 {
    let state = STATE.lock().unwrap();
    match state.rows.iter().position(|&row| row == widget) {
        Some(i) if on_show(&state) => LAYOUT_TOPS[i] - ROW_TOPS[i],
        _ => 0.0,
    }
}

/// Draws one of the game's settings where `row_shift` puts it, and after the last of them
/// the port's. False for any other widget.
pub fn render_row(c: &mut Ctx, widget: u32) -> bool {
    let (shift, last) = {
        let state = STATE.lock().unwrap();
        match state.rows.iter().position(|&row| row == widget) {
            Some(i) if on_show(&state) => (LAYOUT_TOPS[i] - ROW_TOPS[i], i == 2),
            _ => return false,
        }
    };
    let y = c.r32(widget + 0x18);
    c.w32(widget + 0x18, (f32::from_bits(y) + shift).to_bits());
    menu::render_text_unhooked(c, widget);
    c.w32(widget + 0x18, y);
    if last {
        draw(c, widget);
    }
    true
}

/// Runs before the game updates its menus, with the widgets on show. Finds the settings
/// screen, sends the events asked for, and handles the mouse while one of the port's rows
/// has the focus or is clicked. Returns true when it has dealt with the mouse.
pub fn update(c: &mut Ctx, widgets: &[Widget]) -> bool {
    let mut state = STATE.lock().unwrap();
    state.texts = 0;
    let mut rows = [None; 3];
    for widget in widgets.iter().filter(|w| w.kind == TEXT_WIDGET && w.rect[2] == 240.0) {
        let text = menu::characters(c, c.r32(widget.addr + WIDGET_TEXT)).to_ascii_lowercase();
        if let Some(i) = GAME_SETTINGS.iter().position(|name| *name == text) {
            rows[i] = Some(*widget);
        }
    }
    let [Some(rumble), Some(music), Some(sfx)] = rows else {
        state.focus = None;
        return false;
    };
    let rows = [rumble, music, sfx];
    // The screen's slide: the middle one of the rows' offsets, so not a bouncing row's.
    let mut offsets = [0, 1, 2].map(|i| rows[i].rect[1] - LAYOUT_TOPS[i]);
    offsets.sort_by(f32::total_cmp);
    state.offset = (sfx.rect[0] - ROWS_LEFT, offsets[1]);
    // Not on show, or slid away: the focus is the game's again when it comes back.
    if !on_show(&state) || slid_away(&state) {
        state.focus = None;
    }
    state.seen = clock();
    state.rows = rows.map(|w| w.addr);
    state.focused = rows.map(|w| w.focused);
    // The game gave one of its own the focus (a click on it). Not while the focus is still
    // to be taken from it.
    if state.focus.is_some() && state.actions.is_empty() && state.focused.iter().any(|&f| f) {
        state.focus = None;
    }
    for action in std::mem::take(&mut state.actions) {
        match action {
            Action::Focus(widget, event) => menu::send(c, widget, event),
        }
    }

    // Slid away, its rows are not where a click on the options list could land.
    if slid_away(&state) {
        return false;
    }
    let (at, moved, clicked) = menu::peek_pointer();
    let row_at = |at: (f32, f32)| {
        (0..SETTINGS.len()).find(|&i| {
            let top = LAYOUT_TOPS[3 + i];
            (ROWS_LEFT..ROWS_LEFT + sfx.rect[2]).contains(&at.0) && (top..top + ROW_HEIGHT).contains(&at.1)
        })
    };
    if let (true, Some(at)) = (clicked, at) {
        if let Some(row) = row_at(at) {
            menu::take_pointer();
            if let Some(holder) = rows.iter().find(|w| w.focused) {
                menu::send(c, holder.addr, FOCUS_OFF_UNSELECT);
            }
            state.focus = Some(row);
            state.focus_since = clock();
            menu::keep_active();
            return true;
        }
    }
    let Some(focus) = state.focus else { return false };
    menu::keep_active();
    menu::take_pointer();
    let place = |x: f32| ((x - BARS_LEFT - 20.0) / BAR_STEP).round().clamp(0.0, 10.0) as usize;
    // A click on the bars chooses the one under the cursor, and while the button stays down
    // the choice follows the cursor across them.
    if state.dragging {
        if menu::button_held() {
            if let (Some(at), true) = (at, moved) {
                choose(focus, place(at.0));
            }
            return true;
        }
        state.dragging = false;
    }
    let Some(at) = at.filter(|_| clicked) else { return true };
    if let Some(row) = rows.iter().find(|w| w.contains(at)) {
        menu::send(c, row.addr, FOCUS_ON_SELECT);
        state.focus = None;
    } else if (BARS_LEFT..BARS_LEFT + BAR_STEP * 10.0 + 40.0).contains(&at.0) && (END_TOP..END_TOP + 50.0).contains(&at.1) {
        choose(focus, place(at.0));
        state.dragging = true;
    } else if (WORDS_TOP..WORDS_TOP + 40.0).contains(&at.1) && (WORD_LEFT + 20.0..WORD_LEFT + 80.0).contains(&at.0) {
        choose(focus, 0);
    } else if (WORDS_TOP..WORDS_TOP + 40.0).contains(&at.1) && (WORD_RIGHT + 20.0..WORD_RIGHT + 80.0).contains(&at.0) {
        choose(focus, 10);
    } else if let Some(bits) = menu::hint_at(c, widgets, at) {
        // Return accepts the changes first, as `menu::press` does (STATE is held here).
        if bits == crate::input::B {
            let_accept_through();
            crate::input::pulse(crate::input::A);
        }
        crate::input::pulse(bits);
    }
    true
}

/// Runs on controller 1's state just before the game reads it. While the settings screen is
/// on show: Down on SFX Volume moves to the port's rows, and while one of those has the focus
/// the arrows, stick and A are taken from the game and do their work here.
pub fn filter_pad(status: &mut [u8]) {
    let mut state = STATE.lock().unwrap();
    if !on_show(&state) {
        (state.focus, state.swallow, state.held) = (None, false, 0);
        return;
    }
    let buttons = u16::from_be_bytes([status[0], status[1]]) as u32;
    let (x, y) = (status[2] as i8, status[3] as i8);
    let held = (buttons & UP != 0 || y > STICK_PUSH) as u8
        | ((buttons & DOWN != 0 || y < -STICK_PUSH) as u8) << 1
        | ((buttons & LEFT != 0 || x < -STICK_PUSH) as u8) << 2
        | ((buttons & RIGHT != 0 || x > STICK_PUSH) as u8) << 3;
    let now = clock();
    let pressed = held & !state.held;
    let mut act = pressed;
    if pressed != 0 {
        state.repeat_at = now + REPEAT_AFTER;
    } else if held != 0 && held == state.held && now >= state.repeat_at {
        state.repeat_at = now + REPEAT_EVERY;
        act = held & 0b1100;
    }
    state.held = held;
    let strip = |status: &mut [u8], also: u32| {
        let kept = buttons & !(UP | DOWN | LEFT | RIGHT | also);
        status[0..2].copy_from_slice(&(kept as u16).to_be_bytes());
        (status[2], status[3]) = (0, 0);
    };

    let Some(focus) = state.focus else {
        if state.swallow {
            if held == 0 {
                state.swallow = false;
            } else {
                strip(status, 0);
            }
        } else if act & 2 != 0 && state.focused[2] {
            state.focus = Some(0);
            state.focus_since = clock();
            let sfx = state.rows[2];
            state.actions.push(Action::Focus(sfx, FOCUS_OFF_UNSELECT));
            strip(status, 0);
        }
        return;
    };
    strip(status, if clock() < ACCEPT_UNTIL.load(std::sync::atomic::Ordering::Relaxed) { 0 } else { A });
    if act & 1 != 0 {
        if focus == 0 {
            state.focus = None;
            state.swallow = true;
            let sfx = state.rows[2];
            state.actions.push(Action::Focus(sfx, FOCUS_ON_SELECT));
        } else {
            state.focus = Some(focus - 1);
            state.focus_since = clock();
        }
    } else if act & 2 != 0 && focus + 1 < SETTINGS.len() {
        state.focus = Some(focus + 1);
        state.focus_since = clock();
    }
    if act & 4 != 0 {
        choose(focus, current(focus).saturating_sub(1));
    } else if act & 8 != 0 {
        choose(focus, current(focus) + 1);
    }
}

/// All the game's widgets, shown or not, of one type: address and place.
fn all_widgets(c: &Ctx, kind: u8) -> Vec<(u32, u32, [f32; 4])> {
    const WIDGETS: u32 = 0x8040_72C0;
    const WIDGET_COUNT: u32 = 0xFFFF_93D0;
    let count = c.r32(c.r[13].wrapping_add(WIDGET_COUNT)).min(1024);
    let f = |addr: u32| f32::from_bits(c.r32(addr));
    (0..count)
        .map(|i| c.r32(WIDGETS + i * 4))
        .filter(|&ui| (0x8000_0000..0x8180_0000).contains(&ui) && c.r8(ui + 4) == kind)
        .map(|ui| (ui, c.r32(ui), [f(ui + 0x14), f(ui + 0x18), f(ui + 0x1C), f(ui + 0x20)]))
        .collect()
}

/// Has a widget draw itself at another place, at an opacity, shown whether or not the game
/// shows it, and (a text widget) with another text. A text's shadow has an opacity of its
/// own, set to the same.
fn draw_widget(c: &mut Ctx, state: &mut State, widget: u32, text: Option<&str>, at: (f32, f32), opacity: u8) {
    let saved = (c.r32(widget + 0x14), c.r32(widget + 0x18), c.r8(widget + 0x27), c.r8(widget + 0x54), c.r16(widget + 6), c.r32(widget + WIDGET_TEXT));
    let shadow = c.r8(widget + TEXT_SHADOW);
    if let Some(text) = text {
        let slot = state.texts % TEXT_SLOTS;
        state.texts += 1;
        let copy = crate::host::with(|h| h.scratch) + TEXT_SCRATCH + slot * TEXT_SLOT;
        for (i, byte) in text.bytes().take(TEXT_SLOT as usize - 1).chain([0]).enumerate() {
            c.w8(copy + i as u32, byte);
        }
        c.w32(widget + WIDGET_TEXT, copy);
    }
    c.w32(widget + 0x14, at.0.to_bits());
    c.w32(widget + 0x18, at.1.to_bits());
    c.w8(widget + 0x27, opacity);
    c.w8(widget + 0x54, 1);
    c.w16(widget + 6, saved.4 | 1);
    if text.is_some() {
        c.w8(widget + TEXT_SHADOW, opacity);
    }
    if text.is_some() {
        menu::render_text_unhooked(c, widget);
    } else {
        menu::call(c, menu::RENDER_IMAGE, &[widget]);
    }
    c.w32(widget + 0x14, saved.0);
    c.w32(widget + 0x18, saved.1);
    c.w8(widget + 0x27, saved.2);
    c.w8(widget + 0x54, saved.3);
    c.w16(widget + 6, saved.4);
    c.w32(widget + WIDGET_TEXT, saved.5);
    if text.is_some() {
        c.w8(widget + TEXT_SHADOW, shadow);
    }
}

/// Runs after the last of the game's settings (`row`) has drawn itself: draws the port's
/// rows under it, and while one of them has the focus, its title, its range and its bars.
fn draw(c: &mut Ctx, row: u32) {
    let mut state = STATE.lock().unwrap();
    let state = &mut *state;
    let (dx, dy) = state.offset;
    let since = clock().saturating_sub(state.focus_since) % BOUNCE_PERIOD;
    let bounce = if since < BOUNCE_UP { BOUNCE_HEIGHT * (1.0 - (since as f32 / BOUNCE_UP as f32 * 2.0 - 1.0).abs()) } else { 0.0 };
    for (i, setting) in SETTINGS.iter().enumerate() {
        let focused = state.focus == Some(i);
        let top = LAYOUT_TOPS[3 + i] + dy - if focused { bounce } else { 0.0 };
        draw_widget(c, state, row, Some(setting.name), (ROWS_LEFT + dx, top), if focused { 255 } else { DIMMED });
    }
    let Some(focus) = state.focus else { return };
    let setting = &SETTINGS[focus];
    let index = current(focus);

    let texts = all_widgets(c, TEXT_WIDGET);
    if let Some(&(title, ..)) = texts.iter().find(|(.., rect)| *rect == TITLE) {
        draw_widget(c, state, title, Some(setting.name), (TITLE[0] + dx, TITLE[1] + dy), 255);
    }
    let word = |x: f32| texts.iter().find(|(.., rect)| rect[0] == x && rect[1] == WORDS_TOP && rect[2] == 100.0).map(|w| w.0);
    if let (Some(low), Some(high)) = (word(WORD_LEFT), word(WORD_RIGHT)) {
        let unit = if focus == 0 { " FPS" } else { "" };
        draw_widget(c, state, low, Some(&setting.values[0].to_string()), (WORD_LEFT + dx, WORDS_TOP + dy), 255);
        draw_widget(c, state, high, Some(&setting.values[10].to_string()), (WORD_RIGHT + dx, WORDS_TOP + dy), 255);
        let middle = (WORD_LEFT + WORD_RIGHT) / 2.0;
        draw_widget(c, state, low, Some(&format!("{}{unit}", setting.values[index])), (middle + dx, WORDS_TOP + dy), 255);
    }

    // The bars of a volume: a dark one at each place, and the lit one at the place of its
    // level.
    let images = all_widgets(c, IMAGE_WIDGET);
    let by_id = |id: u32| images.iter().find(|w| w.1 == id).map(|w| (w.0, w.2));
    for place in 0..11 {
        if let Some((bar, rect)) = by_id(bar_id(DARK_BARS, place)) {
            draw_widget(c, state, bar, None, (rect[0] + dx, rect[1] + dy), 255);
        }
    }
    if let Some((lit, rect)) = by_id(bar_id(LIT_BARS, index)) {
        draw_widget(c, state, lit, None, (rect[0] + dx, rect[1] + dy), 255);
    }
}
