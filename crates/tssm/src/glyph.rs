//! Keyboard and mouse pictures in place of the controller's buttons.
//!
//! The game's texts show buttons as small textures (`pad_button1` is the A button, and so on;
//! menu.rs finds where each one is in memory). While the keyboard and mouse are what the
//! player last used, the renderer is handed a picture drawn here instead: a key cap with the
//! name of the key bound to that button in config/controls.txt, or a mouse with one button
//! lit. After a gamepad the game's own pictures are back, from the next frame on.

use std::sync::Mutex;

use crate::input;

/// The buttons the game has pictures of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Button {
    A,
    B,
    X,
    Y,
    L,
    R,
    Z,
    Start,
    Stick,
    CStick,
}

/// What stands for a button on the keyboard and mouse.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Label {
    /// A key, by the name on its cap.
    Key(String),
    /// The mouse with its left (0), right (1) or middle (2) button lit, or none (3).
    Mouse(u8),
}

/// The pictures' width and height in texels.
pub const SIZE: u32 = 128;

/// Where the game's button textures are in memory: the address of the texels, as the GPU is
/// given it.
static TEXTURES: Mutex<Vec<(u32, Button)>> = Mutex::new(Vec::new());

/// Called by menu.rs with the button textures it has found.
pub fn set_textures(textures: Vec<(u32, Button)>) {
    *TEXTURES.lock().unwrap() = textures;
}

/// If the texture at `address` is a button's and the keyboard and mouse are in use: what to
/// show instead, with a number that identifies the picture.
pub fn for_texture(address: u32) -> Option<(Label, u64)> {
    if !input::key_pictures() {
        return None;
    }
    let button = TEXTURES.lock().unwrap().iter().find(|(at, _)| *at == address)?.1;
    let label = input::label(button, crate::menu::active())?;
    let bytes = match &label {
        Label::Key(name) => name.as_bytes().to_vec(),
        Label::Mouse(lit) => vec![0, *lit],
    };
    Some((label, crate::texture::hash(&bytes)))
}

/// Capital letters, digits and arrows, five columns by seven rows, a row a byte.
fn letter(ch: char) -> [u8; 7] {
    match ch {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '^' => [0x04, 0x0E, 0x15, 0x04, 0x04, 0x04, 0x04],
        'v' => [0x04, 0x04, 0x04, 0x04, 0x15, 0x0E, 0x04],
        '<' => [0x00, 0x04, 0x08, 0x1F, 0x08, 0x04, 0x00],
        '>' => [0x00, 0x04, 0x02, 0x1F, 0x02, 0x04, 0x00],
        _ => [0; 7],
    }
}

const EDGE: [u8; 3] = [36, 38, 48];
const FACE: [u8; 3] = [240, 240, 244];
const LIP: [u8; 3] = [176, 178, 192];
const INK: [u8; 3] = [28, 30, 40];
const LIT: [u8; 3] = [255, 176, 0];

struct Canvas {
    rgba: Vec<u8>,
}

impl Canvas {
    /// Lays `color` over the texel at (x, y) with the given coverage, 0 to 1.
    fn put(&mut self, x: i32, y: i32, color: [u8; 3], coverage: f32) {
        if !(0..SIZE as i32).contains(&x) || !(0..SIZE as i32).contains(&y) || coverage <= 0.0 {
            return;
        }
        let texel = &mut self.rgba[(y as usize * SIZE as usize + x as usize) * 4..][..4];
        let coverage = coverage.min(1.0);
        let under = texel[3] as f32 / 255.0;
        let alpha = coverage + under * (1.0 - coverage);
        for (channel, value) in texel.iter_mut().zip(color) {
            *channel = ((value as f32 * coverage + *channel as f32 * under * (1.0 - coverage)) / alpha) as u8;
        }
        texel[3] = (alpha * 255.0) as u8;
    }

    /// A rectangle with rounded corners and soft edges; `only` limits it to part of the
    /// picture (left, top, right, bottom).
    fn rounded(&mut self, (left, top, right, bottom): (f32, f32, f32, f32), radius: f32, color: [u8; 3], only: (f32, f32, f32, f32)) {
        let centre = ((left + right) / 2.0, (top + bottom) / 2.0);
        let half = ((right - left) / 2.0 - radius, (bottom - top) / 2.0 - radius);
        for y in (top.floor().max(only.1) as i32)..(bottom.ceil().min(only.3) as i32) {
            for x in (left.floor().max(only.0) as i32)..(right.ceil().min(only.2) as i32) {
                let (dx, dy) = ((x as f32 + 0.5 - centre.0).abs() - half.0, (y as f32 + 0.5 - centre.1).abs() - half.1);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - radius;
                self.put(x, y, color, 0.5 - outside);
            }
        }
    }

    fn rect(&mut self, (left, top, right, bottom): (i32, i32, i32, i32), color: [u8; 3]) {
        for y in top..bottom {
            for x in left..right {
                self.put(x, y, color, 1.0);
            }
        }
    }

    /// `text` in the middle of the box, as large as whole-texel letters fit it.
    fn text(&mut self, text: &str, (left, top, right, bottom): (i32, i32, i32, i32)) {
        let count = text.chars().count().max(1) as i32;
        let scale = ((right - left) / (count * 6 - 1)).min((bottom - top) / 7).clamp(1, 10);
        let (width, height) = ((count * 6 - 1) * scale, 7 * scale);
        let (x0, y0) = ((left + right - width) / 2, (top + bottom - height) / 2);
        for (i, ch) in text.chars().enumerate() {
            for (row, bits) in letter(ch).into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (0x10 >> column) != 0 {
                        let (x, y) = (x0 + (i as i32 * 6 + column) * scale, y0 + row as i32 * scale);
                        self.rect((x, y, x + scale, y + scale), INK);
                    }
                }
            }
        }
    }
}

/// Draws the picture for a label as SIZE by SIZE RGBA texels.
pub fn draw(label: &Label) -> Vec<u8> {
    let mut canvas = Canvas { rgba: vec![0; (SIZE * SIZE * 4) as usize] };
    let all = (0.0, 0.0, SIZE as f32, SIZE as f32);
    match label {
        Label::Key(name) => {
            // One letter gets a square cap, a word the picture's whole width.
            let (left, right) = if name.chars().count() == 1 { (20.0, 108.0) } else { (2.0, 126.0) };
            canvas.rounded((left, 18.0, right, 114.0), 16.0, EDGE, all);
            canvas.rounded((left + 4.0, 22.0, right - 4.0, 110.0), 12.0, LIP, all);
            canvas.rounded((left + 4.0, 22.0, right - 4.0, 100.0), 12.0, FACE, all);
            canvas.text(name, (left as i32 + 8, 28, right as i32 - 8, 96));
        }
        Label::Mouse(lit) => {
            let body = (32.0, 8.0, 96.0, 120.0);
            canvas.rounded(body, 30.0, EDGE, all);
            canvas.rounded((36.0, 12.0, 92.0, 116.0), 26.0, FACE, all);
            match lit {
                0 => canvas.rounded((36.0, 12.0, 92.0, 116.0), 26.0, LIT, (0.0, 0.0, 62.0, 54.0)),
                1 => canvas.rounded((36.0, 12.0, 92.0, 116.0), 26.0, LIT, (66.0, 0.0, 128.0, 54.0)),
                _ => {}
            }
            // The line between the buttons and the body, the one between the buttons, and
            // the wheel.
            canvas.rect((36, 54, 92, 58), EDGE);
            canvas.rect((62, 12, 66, 54), EDGE);
            canvas.rounded((56.0, 20.0, 72.0, 46.0), 8.0, EDGE, all);
            canvas.rounded((60.0, 24.0, 68.0, 42.0), 4.0, if *lit == 2 { LIT } else { FACE }, all);
        }
    }
    canvas.rgba
}
