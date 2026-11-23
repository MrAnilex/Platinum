//! On-screen overlays, after GG's ("Superpositions"): when a keyboard shortcut changes a volume, a
//! mute or the output device, a small panel shows what changed for 1.5 s, then fades out.
//!
//! The panel is a top-most layered window that never takes the focus and lets every click through
//! (`WS_EX_NOACTIVATE | WS_EX_TRANSPARENT`), so that a game keeps the keyboard and the mouse. It is
//! drawn in Platinum's graphite style: a dark panel, light text, the channel's icon (the same line
//! drawings as the window's, ui/ui/icons.slint) and a platinum-white bar.
//!
//! Pure Win32, no crate: the shapes (panel, border, bar, icon strokes) are rasterized here into a
//! 32-bit bitmap with their own anti-aliasing, GDI draws the text on top, and
//! `UpdateLayeredWindow` shows the result with per-pixel alpha. Everything but the window itself
//! is pure (layout, placement, rasterizing), so that the tests check it without a screen.
//!
//! The window must live on a thread that pumps messages: the daemon's shortcuts thread
//! (`shortcuts`), or `preview` for `platinum overlay-preview` and the window's "Aperçu".

use crate::com::wide;
use crate::i18n::tr;
use crate::trf;
use std::ffi::c_void;
use std::time::Duration;

/// Where the panel shows, as GG offers it (its order: `OverlayPosition`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    TopLeft,
    TopRight,
    CenterLeft,
    CenterRight,
    BottomLeft,
    BottomRight,
}

impl Position {
    /// In the order the window lists them.
    pub const ALL: [Position; 6] = [Position::TopLeft, Position::TopRight, Position::CenterLeft, Position::CenterRight, Position::BottomLeft, Position::BottomRight];

    /// The name written in platinum.ini.
    pub fn key(self) -> &'static str {
        match self {
            Position::TopLeft => "top_left",
            Position::TopRight => "top_right",
            Position::CenterLeft => "center_left",
            Position::CenterRight => "center_right",
            Position::BottomLeft => "bottom_left",
            Position::BottomRight => "bottom_right",
        }
    }

    pub fn parse(text: &str) -> Option<Position> {
        let text = text.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Position::ALL.into_iter().find(|p| p.key() == text)
    }

    /// The label in the current language (`soundstage.settings.overlays.position.*`).
    pub fn label(self) -> &'static str {
        match self {
            Position::TopLeft => tr("Coin supérieur gauche"),
            Position::TopRight => tr("Coin supérieur droit"),
            Position::CenterLeft => tr("Centre gauche"),
            Position::CenterRight => tr("Centre droit"),
            Position::BottomLeft => tr("Coin inférieur gauche"),
            Position::BottomRight => tr("Coin inférieur droit"),
        }
    }

    pub fn index(self) -> usize {
        Position::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// What an overlay is about, for the per-type switches (GG: Volume, Mute / Unmute, Device change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Volume,
    Mute,
    Device,
}

/// The overlay options, kept in platinum.ini. GG ships them all on, top right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub position: Position,
    pub volume: bool,
    pub mute: bool,
    pub device: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { enabled: true, position: Position::TopRight, volume: true, mute: true, device: true }
    }
}

impl Settings {
    /// Whether an overlay of this kind is shown.
    pub fn shows(&self, kind: Kind) -> bool {
        self.enabled
            && match kind {
                Kind::Volume => self.volume,
                Kind::Mute => self.mute,
                Kind::Device => self.device,
            }
    }
}

/// The icon at the left of the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Master,
    Game,
    Chat,
    Media,
    Aux,
    Mic,
    Device,
    ChatMix,
}

/// The bar under the title.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bar {
    None,
    /// A level, 0..1, filled from the left.
    Level(f32),
    /// A balance, -1..1, filled from the middle (ChatMix).
    Balance(f32),
}

/// What one overlay says.
#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub kind: Kind,
    pub glyph: Glyph,
    /// Shown in capitals at the top left ("Gaming", "Périphérique suivant").
    pub title: String,
    /// At the top right: "45 %", "Son coupé".
    pub detail: String,
    pub bar: Bar,
    /// A second line instead of the bar: the device's name.
    pub line: String,
    /// Muted: the icon and the bar are drawn dim.
    pub dimmed: bool,
}

/// "45 %", as the window writes a volume.
pub fn percent(level: f32) -> String {
    format!("{}\u{a0}%", (level.clamp(0.0, 1.0) * 100.0).round() as i32)
}

impl Content {
    /// A volume changed.
    pub fn volume(glyph: Glyph, title: &str, level: f32, muted: bool) -> Content {
        Content { kind: Kind::Volume, glyph, title: title.into(), detail: percent(level), bar: Bar::Level(level), line: String::new(), dimmed: muted }
    }

    /// A mute toggled (GG's words: "Son coupé", "Son activé"); the bar keeps the level.
    pub fn mute(glyph: Glyph, title: &str, muted: bool, level: f32) -> Content {
        let detail = if muted { tr("Son coupé") } else { tr("Son activé") };
        Content { kind: Kind::Mute, glyph, title: title.into(), detail: detail.into(), bar: Bar::Level(level), line: String::new(), dimmed: muted }
    }

    /// The output channels moved to another device.
    pub fn device(name: &str, forward: bool) -> Content {
        let title = if forward { tr("Périphérique suivant") } else { tr("Périphérique précédent") };
        Content { kind: Kind::Device, glyph: Glyph::Device, title: title.into(), detail: String::new(), bar: Bar::None, line: name.into(), dimmed: false }
    }

    /// ChatMix moved: the balance, with each side's gain.
    pub fn chatmix(balance: f32, gains: (f32, f32)) -> Content {
        let detail = trf!("Jeu {game} · Chat {chat}", game = percent(gains.0), chat = percent(gains.1));
        Content { kind: Kind::Volume, glyph: Glyph::ChatMix, title: tr("ChatMix").into(), detail, bar: Bar::Balance(balance), line: String::new(), dimmed: false }
    }

    /// Something the shortcut could not do, said on the second line.
    pub fn unavailable(kind: Kind, glyph: Glyph, title: &str, why: &str) -> Content {
        Content { kind, glyph, title: title.into(), detail: String::new(), bar: Bar::None, line: why.into(), dimmed: true }
    }
}

// ---------------------------------------------------------------------------------------------
// Layout and placement (pure).

/// The panel at 96 dpi; everything scales with the screen's dpi.
const WIDTH: f32 = 320.0;
const HEIGHT: f32 = 76.0;
const RADIUS: f32 = 8.0;
const PAD: f32 = 16.0;
const GLYPH: f32 = 30.0;
const GAP: f32 = 14.0;
/// Distance from the edges of the screen's work area.
pub const MARGIN: f32 = 24.0;

/// A box in pixels: left, top, right, bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Box4 {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Box4 {
    fn scaled(left: f32, top: f32, right: f32, bottom: f32, scale: f32) -> Box4 {
        let px = |v: f32| (v * scale).round() as i32;
        Box4 { left: px(left), top: px(top), right: px(right), bottom: px(bottom) }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Where everything goes in the panel, in pixels, for one content at one scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub radius: f32,
    pub border: f32,
    /// Left, top and side of the icon's square.
    pub glyph: (f32, f32, f32),
    /// The title's row; the detail is drawn right-aligned in the same row.
    pub title: Box4,
    /// The bar (when there is one): left, top, width, height.
    pub bar: Option<(f32, f32, f32, f32)>,
    /// The second line (when there is one).
    pub line: Option<Box4>,
    /// Font heights in pixels, and the title's letter spacing.
    pub title_px: i32,
    pub detail_px: i32,
    pub line_px: i32,
    pub spacing: i32,
}

impl Layout {
    pub fn new(content: &Content, scale: f32) -> Layout {
        let scale = if scale.is_finite() && scale > 0.25 { scale } else { 1.0 };
        let text_left = PAD + GLYPH + GAP;
        let text_right = WIDTH - PAD;
        let has_line = !content.line.is_empty();
        let bar = match (content.bar, has_line) {
            (Bar::None, _) | (_, true) => None,
            _ => Some(((text_left * scale).round(), (46.0 * scale).round(), ((text_right - text_left) * scale).round(), (6.0 * scale).round().max(2.0))),
        };
        Layout {
            width: (WIDTH * scale).round() as i32,
            height: (HEIGHT * scale).round() as i32,
            radius: RADIUS * scale,
            border: scale.max(1.0).round(),
            glyph: (PAD * scale, ((HEIGHT - GLYPH) / 2.0 * scale).round(), GLYPH * scale),
            title: Box4::scaled(text_left, 14.0, text_right, 34.0, scale),
            bar,
            line: has_line.then(|| Box4::scaled(text_left, 37.0, text_right, 61.0, scale)),
            title_px: (12.0 * scale).round() as i32,
            detail_px: (12.0 * scale).round() as i32,
            line_px: (14.0 * scale).round() as i32,
            spacing: (1.4 * scale).round() as i32,
        }
    }
}

/// The panel's top-left corner for a work area (left, top, right, bottom), a panel size and a
/// margin, at one of GG's six places.
pub fn place(work: Box4, width: i32, height: i32, margin: i32, position: Position) -> (i32, i32) {
    let x = match position {
        Position::TopLeft | Position::CenterLeft | Position::BottomLeft => work.left + margin,
        Position::TopRight | Position::CenterRight | Position::BottomRight => work.right - margin - width,
    };
    let y = match position {
        Position::TopLeft | Position::TopRight => work.top + margin,
        Position::CenterLeft | Position::CenterRight => work.top + (work.height() - height) / 2,
        Position::BottomLeft | Position::BottomRight => work.bottom - margin - height,
    };
    (x, y)
}

/// How much of a bar `width` wide a level fills.
pub fn bar_fill(level: f32, width: f32) -> f32 {
    if level.is_finite() { level.clamp(0.0, 1.0) * width } else { 0.0 }
}

/// The part of a bar a balance fills, from its middle: (start, end) from the bar's left.
pub fn balance_span(balance: f32, width: f32) -> (f32, f32) {
    let middle = width / 2.0;
    let at = middle + if balance.is_finite() { balance.clamp(-1.0, 1.0) } else { 0.0 } * middle;
    (middle.min(at), middle.max(at))
}

// ---------------------------------------------------------------------------------------------
// Rasterizing (pure).

/// Platinum's palette (ui/ui/controls.slint), as 0xRRGGBB.
const PANEL: u32 = 0x16171a;
const EDGE: u32 = 0x33363b;
const TRACK: u32 = 0x2b2d32;
const LIGHT: u32 = 0xe9ecf1;
const TEXT: u32 = 0xebecee;
const DIM: u32 = 0x9b9ea5;
const FAINT: u32 = 0x63666d;

/// The panel's pixels before the text: colours (0xRRGGBB) and the coverage of the panel's
/// rounded shape (its alpha).
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    pub mask: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Canvas {
        Canvas { width, height, pixels: vec![PANEL; width * height], mask: vec![0; width * height] }
    }

    /// Mixes `color` into a pixel by `coverage` (0..1).
    fn blend(&mut self, x: usize, y: usize, color: u32, coverage: f32) {
        if coverage <= 0.0 || x >= self.width || y >= self.height {
            return;
        }
        let index = y * self.width + x;
        let c = coverage.min(1.0);
        let mix = |shift: u32| {
            let below = ((self.pixels[index] >> shift) & 0xff) as f32;
            let above = ((color >> shift) & 0xff) as f32;
            ((below + (above - below) * c).round() as u32) << shift
        };
        self.pixels[index] = mix(16) | mix(8) | mix(0);
    }

    /// The pixels a box covers, clipped to the canvas.
    fn span(&self, left: f32, top: f32, right: f32, bottom: f32) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let clip = |v: f32, most: usize| (v.max(0.0) as usize).min(most);
        (clip(left.floor(), self.width)..clip(right.ceil(), self.width), clip(top.floor(), self.height)..clip(bottom.ceil(), self.height))
    }
}

/// Signed distance from a point to a rounded rectangle (negative inside).
pub fn rounded_distance(x: f32, y: f32, left: f32, top: f32, width: f32, height: f32, radius: f32) -> f32 {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let (cx, cy) = (left + width / 2.0, top + height / 2.0);
    let qx = (x - cx).abs() - (width / 2.0 - radius);
    let qy = (y - cy).abs() - (height / 2.0 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// How much of a pixel a shape covers, from the signed distance of the pixel's centre.
pub fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Fills a rounded rectangle, optionally cut to the columns `[from, to)`.
fn fill_rounded(canvas: &mut Canvas, (left, top, width, height): (f32, f32, f32, f32), radius: f32, color: u32, cut: Option<(f32, f32)>) {
    let (columns, rows) = canvas.span(left - 1.0, top - 1.0, left + width + 1.0, top + height + 1.0);
    for y in rows {
        for x in columns.clone() {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut cover = coverage(rounded_distance(px, py, left, top, width, height, radius));
            if let Some((from, to)) = cut {
                cover *= ((to - px + 0.5).clamp(0.0, 1.0)) * ((px - from + 0.5).clamp(0.0, 1.0));
            }
            canvas.blend(x, y, color, cover);
        }
    }
}

/// Draws the panel, its border, the icon and the bar; the text is left to GDI.
pub fn paint(content: &Content, layout: &Layout) -> Canvas {
    let (w, h) = (layout.width.max(1) as usize, layout.height.max(1) as usize);
    let mut canvas = Canvas::new(w, h);
    let (fw, fh) = (w as f32, h as f32);
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let outer = rounded_distance(px, py, 0.0, 0.0, fw, fh, layout.radius);
            canvas.mask[y * w + x] = (coverage(outer) * 255.0).round() as u8;
            // The border: inside the outer edge, outside the edge moved in by its width.
            let inner = rounded_distance(px, py, layout.border, layout.border, fw - 2.0 * layout.border, fh - 2.0 * layout.border, (layout.radius - layout.border).max(0.0));
            let ring = coverage(outer) - coverage(inner);
            canvas.blend(x, y, EDGE, ring);
        }
    }
    let tint = if content.dimmed { FAINT } else { TEXT };
    if let Ok(strokes) = parse_path(glyph_path(content.glyph)) {
        let (gx, gy, size) = layout.glyph;
        stroke(&mut canvas, &strokes, gx, gy, size, tint);
    }
    if let Some(bar) = layout.bar {
        let (left, _, width, height) = bar;
        let radius = height / 2.0;
        fill_rounded(&mut canvas, bar, radius, TRACK, None);
        let fill = if content.dimmed { FAINT } else { LIGHT };
        match content.bar {
            Bar::Level(level) => fill_rounded(&mut canvas, bar, radius, fill, Some((left, left + bar_fill(level, width)))),
            Bar::Balance(balance) => {
                let (from, to) = balance_span(balance, width);
                fill_rounded(&mut canvas, bar, radius, fill, Some((left + from, left + to)));
                // The middle, where both sides are heard.
                let (_, top, _, height) = bar;
                let tick = (height / 3.0).max(1.0);
                fill_rounded(&mut canvas, (left + width / 2.0 - tick / 2.0, top - tick, tick, height + 2.0 * tick), tick / 2.0, DIM, None);
            }
            Bar::None => {}
        }
    }
    canvas
}

/// The icon's strokes (polylines on the 24 × 24 grid) drawn into the square at (`left`, `top`),
/// `size` wide, with round ends, as Slint draws ui/ui/icons.slint.
fn stroke(canvas: &mut Canvas, strokes: &[Vec<(f32, f32)>], left: f32, top: f32, size: f32, color: u32) {
    let unit = size / 24.0;
    let half = 0.8 * unit;
    let segments: Vec<((f32, f32), (f32, f32))> = strokes
        .iter()
        .flat_map(|line| {
            let points: Vec<(f32, f32)> = line.iter().map(|(x, y)| (left + x * unit, top + y * unit)).collect();
            if points.len() == 1 { vec![(points[0], points[0])] } else { points.windows(2).map(|p| (p[0], p[1])).collect() }
        })
        .collect();
    let (columns, rows) = canvas.span(left - 2.0, top - 2.0, left + size + 2.0, top + size + 2.0);
    for y in rows {
        for x in columns.clone() {
            let point = (x as f32 + 0.5, y as f32 + 0.5);
            let nearest = segments.iter().map(|(a, b)| segment_distance(point, *a, *b)).fold(f32::INFINITY, f32::min);
            canvas.blend(x, y, color, (half + 0.5 - nearest).clamp(0.0, 1.0));
        }
    }
}

/// Distance from a point to a segment.
pub fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Gives the canvas its alpha (the panel's shape) and premultiplies the colours, as
/// `UpdateLayeredWindow` wants them: 0xAARRGGBB.
pub fn finish(pixels: &mut [u32], mask: &[u8]) {
    for (pixel, &alpha) in pixels.iter_mut().zip(mask) {
        let a = u32::from(alpha);
        let channel = |shift: u32| ((((*pixel >> shift) & 0xff) * a + 127) / 255) << shift;
        *pixel = (a << 24) | channel(16) | channel(8) | channel(0);
    }
}

/// The icon of each glyph, on a 24 × 24 grid: the same drawings as ui/ui/icons.slint (`speaker`
/// for a device, `mixer` for ChatMix).
pub fn glyph_path(glyph: Glyph) -> &'static str {
    match glyph {
        Glyph::Game => "M7.5 8.5h9a4.5 4.5 0 0 1 4.4 5.5l-.6 2.6a2.4 2.4 0 0 1-4.2 1L14.6 16H9.4l-1.5 1.6a2.4 2.4 0 0 1-4.2-1L3.1 14a4.5 4.5 0 0 1 4.4-5.5Z M8 11v3 M6.5 12.5h3 M15.5 11.6h.01 M17.5 13.4h.01",
        Glyph::Chat => "M4 15v-3a8 8 0 0 1 16 0v3 M4 14.5h2.5a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1Z M20 14.5h-2.5a1 1 0 0 0-1 1v3a1 1 0 0 0 1 1H19a1 1 0 0 0 1-1Z M18.5 19.5c0 1.2-1.2 2-2.6 2H13",
        Glyph::Media => "M9 17.5V6.2l10-2.2v11.2 M9 17.5a2.5 2.5 0 1 1-5 0a2.5 2.5 0 0 1 5 0Z M19 15.2a2.5 2.5 0 1 1-5 0a2.5 2.5 0 0 1 5 0Z M9 9.6l10-2.2",
        Glyph::Aux => "M3 12h2.5l2-5.5 3.5 11 3-8 2 4.5H21",
        Glyph::Mic => "M12 3.5a3 3 0 0 0-3 3V12a3 3 0 0 0 6 0V6.5a3 3 0 0 0-3-3Z M18.5 11.5a6.5 6.5 0 0 1-13 0 M12 18v3 M9 21h6",
        Glyph::Master => "M3.5 15v-2.5a8.5 8.5 0 0 1 17 0V15 M3.5 14h3a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1h-2a1 1 0 0 1-1-1Z M20.5 14h-3a1 1 0 0 0-1 1v4a1 1 0 0 0 1 1h2a1 1 0 0 0 1-1Z",
        Glyph::Device => "M11 5.5 6.8 9H4v6h2.8l4.2 3.5Z M15 9.2a4 4 0 0 1 0 5.6 M17.8 6.6a7.6 7.6 0 0 1 0 10.8",
        Glyph::ChatMix => "M6 4v16 M12 4v16 M18 4v16 M4 14h4 M10 8h4 M16 16h4",
    }
}

/// Reads an SVG path (M, L, H, V, C, A, Z, absolute or relative, as icons.slint writes them) into
/// polylines, curves and arcs flattened. The error names what could not be read.
pub fn parse_path(d: &str) -> Result<Vec<Vec<(f32, f32)>>, String> {
    let mut reader = PathReader { text: d.as_bytes(), at: 0 };
    let mut lines: Vec<Vec<(f32, f32)>> = Vec::new();
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let mut start = (0.0, 0.0);
    let mut command = b' ';
    while let Some(next) = reader.command_or_number() {
        if let Some(letter) = next {
            command = letter;
        } else if command == b' ' {
            return Err("a path must start with a command".into());
        }
        let relative = command.is_ascii_lowercase();
        let (ox, oy) = if relative { (x, y) } else { (0.0, 0.0) };
        match command.to_ascii_uppercase() {
            b'M' => {
                let (nx, ny) = (reader.number()? + ox, reader.number()? + oy);
                (x, y) = (nx, ny);
                start = (x, y);
                lines.push(vec![(x, y)]);
                // Further pairs after a move are lines.
                command = if relative { b'l' } else { b'L' };
            }
            b'L' => {
                (x, y) = (reader.number()? + ox, reader.number()? + oy);
                push(&mut lines, (x, y));
            }
            b'H' => {
                x = reader.number()? + ox;
                push(&mut lines, (x, y));
            }
            b'V' => {
                y = reader.number()? + oy;
                push(&mut lines, (x, y));
            }
            b'C' => {
                let c1 = (reader.number()? + ox, reader.number()? + oy);
                let c2 = (reader.number()? + ox, reader.number()? + oy);
                let end = (reader.number()? + ox, reader.number()? + oy);
                for step in 1..=12 {
                    let t = step as f32 / 12.0;
                    let u = 1.0 - t;
                    let at = |a: f32, b: f32, c: f32, d: f32| u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d;
                    push(&mut lines, (at(x, c1.0, c2.0, end.0), at(y, c1.1, c2.1, end.1)));
                }
                (x, y) = end;
            }
            b'A' => {
                let (rx, ry, rotation) = (reader.number()?, reader.number()?, reader.number()?);
                let (large, sweep) = (reader.flag()?, reader.flag()?);
                let end = (reader.number()? + ox, reader.number()? + oy);
                for point in arc((x, y), (rx, ry), rotation, large, sweep, end) {
                    push(&mut lines, point);
                }
                (x, y) = end;
            }
            b'Z' => {
                push(&mut lines, start);
                (x, y) = start;
                // A number right after Z would have no command to belong to.
                command = b' ';
            }
            other => return Err(format!("path command {} is not supported", other as char)),
        }
    }
    Ok(lines)
}

/// Adds a point to the current polyline (a line starting at the origin when none was moved to).
fn push(lines: &mut Vec<Vec<(f32, f32)>>, point: (f32, f32)) {
    match lines.last_mut() {
        Some(line) => line.push(point),
        None => lines.push(vec![(0.0, 0.0), point]),
    }
}

/// An SVG elliptical arc from `from` to `to` as points (the start excluded), by the standard
/// conversion to a centre and two angles (SVG 1.1, appendix F.6.5).
fn arc(from: (f32, f32), radii: (f32, f32), rotation: f32, large: bool, sweep: bool, to: (f32, f32)) -> Vec<(f32, f32)> {
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if rx == 0.0 || ry == 0.0 || from == to {
        return vec![to];
    }
    let phi = rotation.to_radians();
    let (sin, cos) = phi.sin_cos();
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let numerator = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut factor = (numerator / denominator).max(0.0).sqrt();
    if large == sweep {
        factor = -factor;
    }
    let (cx1, cy1) = (factor * rx * y1 / ry, -factor * ry * x1 / rx);
    let (cx, cy) = (cos * cx1 - sin * cy1 + (from.0 + to.0) / 2.0, sin * cx1 + cos * cy1 + (from.1 + to.1) / 2.0);
    let angle = |ux: f32, uy: f32| uy.atan2(ux);
    let theta = angle((x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut delta = angle((-x1 - cx1) / rx, (-y1 - cy1) / ry) - theta;
    let tau = std::f32::consts::TAU;
    if sweep && delta < 0.0 {
        delta += tau;
    } else if !sweep && delta > 0.0 {
        delta -= tau;
    }
    let steps = ((delta.abs() / (std::f32::consts::PI / 16.0)).ceil() as usize).max(2);
    (1..=steps)
        .map(|step| {
            if step == steps {
                return to;
            }
            let t = theta + delta * step as f32 / steps as f32;
            let (ex, ey) = (rx * t.cos(), ry * t.sin());
            (cos * ex - sin * ey + cx, sin * ex + cos * ey + cy)
        })
        .collect()
}

/// Reads the tokens of a path: commands, numbers in SVG's compact writing ("-.6", "1-5"), and the
/// one-character flags of an arc.
struct PathReader<'a> {
    text: &'a [u8],
    at: usize,
}

impl PathReader<'_> {
    fn skip(&mut self) {
        while self.at < self.text.len() && (self.text[self.at].is_ascii_whitespace() || self.text[self.at] == b',') {
            self.at += 1;
        }
    }

    /// `Some(Some(letter))` for a command, `Some(None)` when a number follows (the command
    /// repeats), `None` at the end.
    fn command_or_number(&mut self) -> Option<Option<u8>> {
        self.skip();
        let byte = *self.text.get(self.at)?;
        if byte.is_ascii_alphabetic() {
            self.at += 1;
            Some(Some(byte))
        } else {
            Some(None)
        }
    }

    fn number(&mut self) -> Result<f32, String> {
        self.skip();
        let begin = self.at;
        if matches!(self.text.get(self.at), Some(b'-' | b'+')) {
            self.at += 1;
        }
        let mut dot = false;
        while let Some(&byte) = self.text.get(self.at) {
            if byte.is_ascii_digit() {
                self.at += 1;
            } else if byte == b'.' && !dot {
                dot = true;
                self.at += 1;
            } else {
                break;
            }
        }
        if matches!(self.text.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.text.get(self.at), Some(b'-' | b'+')) {
                self.at += 1;
            }
            while self.text.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
        }
        std::str::from_utf8(&self.text[begin..self.at]).ok().and_then(|t| t.parse().ok()).ok_or_else(|| format!("number expected at {begin}"))
    }

    fn flag(&mut self) -> Result<bool, String> {
        self.skip();
        match self.text.get(self.at) {
            Some(b'0') => {
                self.at += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.at += 1;
                Ok(true)
            }
            _ => Err(format!("arc flag expected at {}", self.at)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The window (Win32).

type Hwnd = *mut c_void;
type Hdc = *mut c_void;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;
type TimerProc = unsafe extern "system" fn(Hwnd, u32, usize, u32);

#[repr(C)]
struct WndClass {
    style: u32,
    procedure: WndProc,
    class_extra: i32,
    window_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct Msg {
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: [i32; 2],
    private: u32,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct Size {
    cx: i32,
    cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct Blend {
    operation: u8,
    flags: u8,
    alpha: u8,
    format: u8,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}

#[repr(C)]
struct BitmapInfo {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_per_meter: i32,
    y_per_meter: i32,
    used: u32,
    important: u32,
    colors: [u32; 1],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(class: *const WndClass) -> u16;
    fn CreateWindowExW(ex_style: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: *mut c_void, instance: *mut c_void, param: *mut c_void) -> Hwnd;
    fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn DestroyWindow(window: Hwnd) -> i32;
    fn ShowWindow(window: Hwnd, command: i32) -> i32;
    fn SetWindowPos(window: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn UpdateLayeredWindow(window: Hwnd, dst: Hdc, position: *const Point, size: *const Size, src: Hdc, source: *const Point, key: u32, blend: *const Blend, flags: u32) -> i32;
    fn GetDC(window: Hwnd) -> Hdc;
    fn ReleaseDC(window: Hwnd, dc: Hdc) -> i32;
    fn SetTimer(window: Hwnd, id: usize, milliseconds: u32, procedure: Option<TimerProc>) -> usize;
    fn KillTimer(window: Hwnd, id: usize) -> i32;
    fn GetMessageW(msg: *mut Msg, window: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    fn SetWindowLongPtrW(window: Hwnd, index: i32, value: isize) -> isize;
    fn GetWindowLongPtrW(window: Hwnd, index: i32) -> isize;
    fn DrawTextW(dc: Hdc, text: *const u16, count: i32, rect: *mut Rect, format: u32) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: Hdc) -> Hdc;
    fn DeleteDC(dc: Hdc) -> i32;
    fn CreateDIBSection(dc: Hdc, info: *const BitmapInfo, usage: u32, bits: *mut *mut c_void, section: *mut c_void, offset: u32) -> *mut c_void;
    fn SelectObject(dc: Hdc, object: *mut c_void) -> *mut c_void;
    fn DeleteObject(object: *mut c_void) -> i32;
    fn CreateFontW(height: i32, width: i32, escapement: i32, orientation: i32, weight: i32, italic: u32, underline: u32, strike: u32, charset: u32, precision: u32, clip: u32, quality: u32, pitch: u32, face: *const u16) -> *mut c_void;
    fn SetTextColor(dc: Hdc, color: u32) -> u32;
    fn SetBkMode(dc: Hdc, mode: i32) -> i32;
    fn SetTextCharacterExtra(dc: Hdc, extra: i32) -> i32;
    fn GdiFlush() -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
const SW_HIDE: i32 = 0;
const SW_SHOWNOACTIVATE: i32 = 4;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;
const ULW_ALPHA: u32 = 2;
const WM_TIMER: u32 = 0x0113;
const WM_NCHITTEST: u32 = 0x0084;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const HTTRANSPARENT: isize = -1;
const MA_NOACTIVATE: isize = 3;
const GWLP_USERDATA: i32 = -21;
const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
const DT_RIGHT: u32 = 0x0002;
const DT_VCENTER: u32 = 0x0004;
const DT_SINGLELINE: u32 = 0x0020;
const DT_CALCRECT: u32 = 0x0400;
const DT_NOPREFIX: u32 = 0x0800;
const DT_END_ELLIPSIS: u32 = 0x8000;
const HIDE_TIMER: usize = 1;
const FADE_TIMER: usize = 2;
const FADE_STEP_MS: u32 = 16;
const FADE_STEP: u8 = 24;
/// How long an overlay stays before it fades.
pub const HOLD: Duration = Duration::from_millis(1500);
/// In the window's user data: the current opacity in the low byte, and this bit when the thread's
/// loop ends once the overlay has faded (previews).
const QUIT_WHEN_HIDDEN: isize = 0x100;

/// GDI's colour order: 0x00BBGGRR.
fn colorref(rgb: u32) -> u32 {
    ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff)
}

/// Makes the calling thread's windows aware of each screen's dpi (Windows 10 1607 and later), so
/// that the overlay is drawn sharp instead of stretched. Best effort.
fn per_monitor_dpi() {
    let Ok(module) = crate::com::load_library("user32.dll", 0) else { return };
    let Ok(address) = crate::com::export(module, b"SetThreadDpiAwarenessContext\0") else { return };
    unsafe {
        let set: unsafe extern "system" fn(isize) -> isize = std::mem::transmute(address);
        // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
        set(-4);
    }
}

/// The primary screen's work area (without the taskbar) and its dpi.
fn primary_screen() -> (Box4, u32) {
    unsafe {
        let monitor = MonitorFromPoint(Point { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
        let work = if GetMonitorInfoW(monitor, &mut info) != 0 { info.work } else { Rect { left: 0, top: 0, right: 1920, bottom: 1040 } };
        let mut dpi = 96u32;
        if let Ok(module) = crate::com::load_library("shcore.dll", 0) {
            if let Ok(address) = crate::com::export(module, b"GetDpiForMonitor\0") {
                let get: unsafe extern "system" fn(*mut c_void, u32, *mut u32, *mut u32) -> i32 = std::mem::transmute(address);
                let (mut x, mut y) = (0u32, 0u32);
                if get(monitor, 0, &mut x, &mut y) == 0 && x > 0 {
                    dpi = x;
                }
            }
        }
        (Box4 { left: work.left, top: work.top, right: work.right, bottom: work.bottom }, dpi)
    }
}

unsafe extern "system" fn procedure(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        match message {
            WM_NCHITTEST => HTTRANSPARENT,
            WM_MOUSEACTIVATE => MA_NOACTIVATE,
            WM_TIMER if wparam == HIDE_TIMER => {
                KillTimer(window, HIDE_TIMER);
                SetTimer(window, FADE_TIMER, FADE_STEP_MS, None);
                0
            }
            WM_TIMER if wparam == FADE_TIMER => {
                let data = GetWindowLongPtrW(window, GWLP_USERDATA);
                let alpha = ((data & 0xff) as u8).saturating_sub(FADE_STEP);
                SetWindowLongPtrW(window, GWLP_USERDATA, (data & !0xff) | isize::from(alpha));
                if alpha == 0 {
                    KillTimer(window, FADE_TIMER);
                    ShowWindow(window, SW_HIDE);
                    if data & QUIT_WHEN_HIDDEN != 0 {
                        PostQuitMessage(0);
                    }
                } else {
                    let blend = Blend { operation: 0, flags: 0, alpha, format: 1 };
                    UpdateLayeredWindow(window, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), 0, &blend, ULW_ALPHA);
                }
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

/// The overlay window, hidden between overlays. It belongs to the thread that created it, which
/// must pump messages (the fade runs on its timers).
pub struct Overlay {
    window: Hwnd,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.window) };
    }
}

impl Overlay {
    /// Creates the (hidden) window on the calling thread.
    pub fn create() -> Option<Overlay> {
        per_monitor_dpi();
        static REGISTER: std::sync::Once = std::sync::Once::new();
        let class_name = wide("PlatinumOverlay");
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            REGISTER.call_once(|| {
                let class = WndClass {
                    style: 0,
                    procedure,
                    class_extra: 0,
                    window_extra: 0,
                    instance,
                    icon: std::ptr::null_mut(),
                    cursor: std::ptr::null_mut(),
                    background: std::ptr::null_mut(),
                    menu_name: std::ptr::null(),
                    class_name: class_name.as_ptr(),
                };
                RegisterClassW(&class);
            });
            let style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            let window = CreateWindowExW(style, class_name.as_ptr(), wide("Platinum").as_ptr(), WS_POPUP, 0, 0, 1, 1, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut());
            (!window.is_null()).then_some(Overlay { window })
        }
    }

    /// Shows `content` at `position` on the primary screen for `HOLD`, then fades it out. An
    /// overlay already on screen is replaced at once.
    pub fn show(&self, content: &Content, position: Position) {
        self.show_for(content, position, HOLD);
    }

    /// The same, held on screen for `hold`.
    pub fn show_for(&self, content: &Content, position: Position, hold: Duration) {
        let (work, dpi) = primary_screen();
        let scale = dpi as f32 / 96.0;
        let layout = Layout::new(content, scale);
        let (x, y) = place(work, layout.width, layout.height, (MARGIN * scale).round() as i32, position);
        let mut canvas = paint(content, &layout);
        unsafe {
            if !draw(self.window, &layout, content, &mut canvas, x, y) {
                return;
            }
            let data = GetWindowLongPtrW(self.window, GWLP_USERDATA);
            SetWindowLongPtrW(self.window, GWLP_USERDATA, (data & !0xff) | 0xff);
            ShowWindow(self.window, SW_SHOWNOACTIVATE);
            // Above the other top-most windows, without taking the focus.
            SetWindowPos(self.window, -1isize as Hwnd, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            KillTimer(self.window, FADE_TIMER);
            SetTimer(self.window, HIDE_TIMER, hold.as_millis().clamp(100, 60_000) as u32, None);
        }
    }

    /// Ends the calling thread's message loop once the overlay has faded out.
    fn quit_when_hidden(&self) {
        unsafe {
            let data = GetWindowLongPtrW(self.window, GWLP_USERDATA);
            SetWindowLongPtrW(self.window, GWLP_USERDATA, data | QUIT_WHEN_HIDDEN);
        }
    }
}

/// Puts the canvas in a bitmap, has GDI write the text on it, gives it its alpha and hands it to
/// the layered window at (`x`, `y`). False when Windows refused a step.
unsafe fn draw(window: Hwnd, layout: &Layout, content: &Content, canvas: &mut Canvas, x: i32, y: i32) -> bool {
    unsafe {
        let (w, h) = (canvas.width as i32, canvas.height as i32);
        let screen = GetDC(std::ptr::null_mut());
        let memory = CreateCompatibleDC(screen);
        let info = BitmapInfo { size: 40, width: w, height: -h, planes: 1, bit_count: 32, compression: 0, size_image: 0, x_per_meter: 0, y_per_meter: 0, used: 0, important: 0, colors: [0] };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(memory, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        let mut shown = false;
        if !bitmap.is_null() && !bits.is_null() {
            let previous = SelectObject(memory, bitmap);
            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, canvas.width * canvas.height);
            pixels.copy_from_slice(&canvas.pixels);
            write_texts(memory, layout, content);
            GdiFlush();
            finish(pixels, &canvas.mask);
            let blend = Blend { operation: 0, flags: 0, alpha: 255, format: 1 };
            shown = UpdateLayeredWindow(window, screen, &Point { x, y }, &Size { cx: w, cy: h }, memory, &Point { x: 0, y: 0 }, 0, &blend, ULW_ALPHA) != 0;
            SelectObject(memory, previous);
            DeleteObject(bitmap);
        }
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        shown
    }
}

/// A GDI font of Segoe UI, `height` pixels tall.
unsafe fn font(height: i32, weight: i32) -> *mut c_void {
    // DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_PITCH
    unsafe { CreateFontW(-height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, wide("Segoe UI").as_ptr()) }
}

/// Draws one text in a box with one font and colour; returns its width when `measure` (nothing
/// drawn then).
unsafe fn text(dc: Hdc, value: &str, area: Box4, format: u32, color: u32, measure: bool) -> i32 {
    unsafe {
        let units: Vec<u16> = value.encode_utf16().collect();
        let mut rect = Rect { left: area.left, top: area.top, right: area.right, bottom: area.bottom };
        SetTextColor(dc, colorref(color));
        let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | format | if measure { DT_CALCRECT } else { DT_END_ELLIPSIS };
        DrawTextW(dc, units.as_ptr(), units.len() as i32, &mut rect, flags);
        rect.right - rect.left
    }
}

/// The title (capitals, spaced like the window's labels), the detail at the right of its row, and
/// the second line.
unsafe fn write_texts(dc: Hdc, layout: &Layout, content: &Content) {
    unsafe {
        SetBkMode(dc, 1);
        let detail_font = font(layout.detail_px, 400);
        let mut title_area = layout.title;
        if !content.detail.is_empty() {
            let previous = SelectObject(dc, detail_font);
            let width = text(dc, &content.detail, layout.title, 0, DIM, true);
            let color = if content.dimmed || matches!(content.kind, Kind::Mute) { DIM } else { TEXT };
            text(dc, &content.detail, layout.title, DT_RIGHT, color, false);
            title_area.right = (layout.title.right - width - layout.title_px).max(title_area.left);
            SelectObject(dc, previous);
        }
        let title_font = font(layout.title_px, 600);
        let previous = SelectObject(dc, title_font);
        SetTextCharacterExtra(dc, layout.spacing);
        text(dc, &content.title.to_uppercase(), title_area, 0, if content.dimmed { DIM } else { TEXT }, false);
        SetTextCharacterExtra(dc, 0);
        SelectObject(dc, previous);
        if let Some(line) = layout.line {
            let line_font = font(layout.line_px, 400);
            let previous = SelectObject(dc, line_font);
            text(dc, &content.line, line, 0, if content.dimmed { DIM } else { LIGHT }, false);
            SelectObject(dc, previous);
            DeleteObject(line_font);
        }
        DeleteObject(title_font);
        DeleteObject(detail_font);
    }
}

/// Shows one overlay on a thread of its own and waits until it has faded out: `platinum
/// overlay-preview` and the window's "Aperçu". Touches no audio setting.
pub fn preview(content: &Content, position: Position, hold: Duration) -> Result<(), String> {
    let content = content.clone();
    let thread = std::thread::Builder::new()
        .name("superposition".into())
        .spawn(move || -> Result<(), String> {
            let overlay = Overlay::create().ok_or("the overlay window cannot be created")?;
            overlay.show_for(&content, position, hold);
            overlay.quit_when_hidden();
            unsafe {
                let mut msg: Msg = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    thread.join().map_err(|_| "the overlay thread ended abnormally".to_string())?
}

/// The rectangle an overlay of this content takes on the primary screen at `position`: left, top,
/// width, height (for `platinum overlay-preview`, to capture it).
pub fn screen_rect(content: &Content, position: Position) -> (i32, i32, i32, i32) {
    per_monitor_dpi();
    let (work, dpi) = primary_screen();
    let scale = dpi as f32 / 96.0;
    let layout = Layout::new(content, scale);
    let (x, y) = place(work, layout.width, layout.height, (MARGIN * scale).round() as i32, position);
    (x, y, layout.width, layout.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_places_follow_the_work_area() {
        let work = Box4 { left: 0, top: 0, right: 1920, bottom: 1040 };
        assert_eq!(place(work, 320, 76, 24, Position::TopLeft), (24, 24));
        assert_eq!(place(work, 320, 76, 24, Position::TopRight), (1576, 24));
        assert_eq!(place(work, 320, 76, 24, Position::CenterLeft), (24, 482));
        assert_eq!(place(work, 320, 76, 24, Position::CenterRight), (1576, 482));
        assert_eq!(place(work, 320, 76, 24, Position::BottomLeft), (24, 940));
        assert_eq!(place(work, 320, 76, 24, Position::BottomRight), (1576, 940));
        // A taskbar on the left moves the work area: so does the overlay.
        let shifted = Box4 { left: 60, top: 0, right: 1920, bottom: 1080 };
        assert_eq!(place(shifted, 320, 76, 24, Position::BottomLeft), (84, 980));
    }

    #[test]
    fn the_layout_scales_with_the_screen() {
        let content = Content::volume(Glyph::Game, "Gaming", 0.45, false);
        let normal = Layout::new(&content, 1.0);
        assert_eq!((normal.width, normal.height), (320, 76));
        assert_eq!(normal.bar, Some((60.0, 46.0, 244.0, 6.0)));
        assert!(normal.line.is_none());
        let large = Layout::new(&content, 1.5);
        assert_eq!((large.width, large.height, large.title_px), (480, 114, 18));
        assert_eq!(large.bar, Some((90.0, 69.0, 366.0, 9.0)));
        // A device change has a line instead of the bar.
        let device = Layout::new(&Content::device("Haut-parleurs (Arena 7)", true), 1.0);
        assert!(device.bar.is_none() && device.line.is_some());
        // A nonsense scale falls back to 1.
        assert_eq!(Layout::new(&content, f32::NAN).width, 320);
    }

    #[test]
    fn bars_fill_what_they_show() {
        assert_eq!(bar_fill(0.5, 200.0), 100.0);
        assert_eq!(bar_fill(1.7, 200.0), 200.0);
        assert_eq!(bar_fill(f32::NAN, 200.0), 0.0);
        assert_eq!(balance_span(0.0, 200.0), (100.0, 100.0));
        assert_eq!(balance_span(-1.0, 200.0), (0.0, 100.0));
        assert_eq!(balance_span(0.5, 200.0), (100.0, 150.0));
        assert_eq!(percent(0.454), "45\u{a0}%");
        assert_eq!(percent(2.0), "100\u{a0}%");
    }

    #[test]
    fn rounded_corners_are_smooth_and_the_middle_is_solid() {
        // Inside, on the edge, outside.
        assert!(rounded_distance(160.0, 38.0, 0.0, 0.0, 320.0, 76.0, 8.0) < -30.0);
        assert!(rounded_distance(0.0, 38.0, 0.0, 0.0, 320.0, 76.0, 8.0).abs() < 1e-4);
        assert!(rounded_distance(0.5, 0.5, 0.0, 0.0, 320.0, 76.0, 8.0) > 0.0);
        assert_eq!(coverage(-3.0), 1.0);
        assert_eq!(coverage(3.0), 0.0);
        assert_eq!(coverage(0.0), 0.5);
        let content = Content::mute(Glyph::Chat, "Chat", true, 0.7);
        let layout = Layout::new(&content, 1.0);
        let canvas = paint(&content, &layout);
        let at = |x: usize, y: usize| canvas.mask[y * canvas.width + x];
        assert_eq!(at(160, 38), 255);
        assert_eq!(at(0, 0), 0);
        assert!(at(2, 2) > 0 && at(2, 2) < 255, "{}", at(2, 2));
        // Premultiplied: a half-covered white pixel is half white.
        let mut pixels = [0xffffff, 0x16171a];
        finish(&mut pixels, &[128, 255]);
        assert_eq!(pixels, [0x8080_8080, 0xff16_171a]);
    }

    #[test]
    fn the_bar_shows_the_level() {
        let content = Content::volume(Glyph::Media, "Media", 0.5, false);
        let layout = Layout::new(&content, 1.0);
        let canvas = paint(&content, &layout);
        let (left, top, width, height) = layout.bar.unwrap();
        let row = (top + height / 2.0) as usize;
        let at = |x: f32| canvas.pixels[row * canvas.width + x as usize];
        assert_eq!(at(left + width * 0.25), LIGHT);
        assert_eq!(at(left + width * 0.75), TRACK);
        // Muted: the same bar, dim.
        let muted = Content::volume(Glyph::Media, "Media", 0.5, true);
        let canvas = paint(&muted, &layout);
        assert_eq!(canvas.pixels[row * canvas.width + (left + width * 0.25) as usize], FAINT);
    }

    #[test]
    fn every_icon_is_read_and_drawn() {
        for glyph in [Glyph::Master, Glyph::Game, Glyph::Chat, Glyph::Media, Glyph::Aux, Glyph::Mic, Glyph::Device, Glyph::ChatMix] {
            let lines = parse_path(glyph_path(glyph)).unwrap_or_else(|e| panic!("{glyph:?}: {e}"));
            assert!(!lines.is_empty(), "{glyph:?}");
            // Every point stays on the 24 × 24 grid.
            assert!(lines.iter().flatten().all(|(x, y)| (-0.5..=24.5).contains(x) && (-0.5..=24.5).contains(y)), "{glyph:?}");
        }
        // The same drawings as the window's.
        let icons = include_str!("../ui/ui/icons.slint");
        for glyph in [Glyph::Master, Glyph::Game, Glyph::Chat, Glyph::Media, Glyph::Aux, Glyph::Mic, Glyph::Device, Glyph::ChatMix] {
            assert!(icons.contains(glyph_path(glyph)), "{glyph:?} differs from icons.slint");
        }
        // The microphone's capsule: a closed outline from (12, 3.5) back to it.
        let mic = parse_path(glyph_path(Glyph::Mic)).unwrap();
        assert_eq!(mic[0].first(), Some(&(12.0, 3.5)));
        assert!(segment_distance(*mic[0].last().unwrap(), (12.0, 3.5), (12.0, 3.5)) < 1e-3);
        // A half circle of radius 6.5 passes 6.5 below its chord's middle.
        let lowest = mic[1].iter().map(|p| p.1).fold(f32::MIN, f32::max);
        assert!((lowest - 18.0).abs() < 0.05, "{lowest}");
        assert!(parse_path("Q1 2 3 4").is_err());
        assert!(parse_path("3 4").is_err());
        let content = Content::volume(Glyph::Game, "Gaming", 0.5, false);
        let layout = Layout::new(&content, 1.0);
        let canvas = paint(&content, &layout);
        let (gx, gy, size) = layout.glyph;
        let lit = (gy as usize..(gy + size) as usize).flat_map(|y| (gx as usize..(gx + size) as usize).map(move |x| (x, y))).filter(|(x, y)| canvas.pixels[y * canvas.width + x] != PANEL).count();
        assert!(lit > 40, "{lit}");
    }

    #[test]
    fn settings_say_which_overlays_show() {
        let settings = Settings::default();
        assert!(settings.enabled && settings.position == Position::TopRight);
        assert!(settings.shows(Kind::Volume) && settings.shows(Kind::Mute) && settings.shows(Kind::Device));
        let quiet = Settings { mute: false, ..settings };
        assert!(!quiet.shows(Kind::Mute) && quiet.shows(Kind::Volume));
        let off = Settings { enabled: false, ..settings };
        assert!(!off.shows(Kind::Volume));
        for position in Position::ALL {
            assert_eq!(Position::parse(position.key()), Some(position));
            assert_eq!(Position::ALL[position.index()], position);
        }
        assert_eq!(Position::parse("Bottom-Left"), Some(Position::BottomLeft));
        assert_eq!(Position::parse("middle"), None);
    }
}
