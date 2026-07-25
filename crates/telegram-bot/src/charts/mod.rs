//! Chart rendering for Telegram. Produces PNGs from the detector's analytics
//! JSON — no browser, no system fonts, no network.
//!
//! Everything is drawn onto a raw pixel canvas rather than through a plotting
//! library's chart builder, because the house style is specific about the
//! things a builder hides: bars capped at 24px with a 4px rounded data-end and
//! a square baseline, a 2px surface gap doing the separating instead of
//! strokes, hairline solid gridlines, 2px lines, ≥8px markers with a 2px
//! surface ring, and labels that are measured before they are placed so text is
//! never clipped by its own mark.
//!
//! Colour follows the entity, never the rank: **mint is always blue, split is
//! always orange**, in every chart, whether or not both are present.

pub mod plots;

use anyhow::{Context, Result};
use image::ImageEncoder;
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use plotters::style::{register_font, FontStyle};

// --- Theme ----------------------------------------------------------------
//
// Dark surface, from the validated house palette. A PNG carries its own
// background, so it looks the same in either Telegram theme — this commits to
// one deliberately rather than half-working in both.

pub const SURFACE: RGBColor = RGBColor(0x1a, 0x1a, 0x19);
pub const PLANE: RGBColor = RGBColor(0x0d, 0x0d, 0x0d);
pub const INK: RGBColor = RGBColor(0xff, 0xff, 0xff);
pub const INK2: RGBColor = RGBColor(0xc3, 0xc2, 0xb7);
pub const MUTED: RGBColor = RGBColor(0x89, 0x87, 0x81);
pub const GRID: RGBColor = RGBColor(0x2c, 0x2c, 0x2a);
pub const AXIS: RGBColor = RGBColor(0x38, 0x38, 0x35);

/// Categorical slot 1 — always `mint`. Slot 2 — always `split`. The pair
/// validates on this surface: CVD ΔE 26.8, normal-vision ΔE 31.8, both ≥ 3:1.
pub const MINT: RGBColor = RGBColor(0x39, 0x87, 0xe5);
pub const SPLIT: RGBColor = RGBColor(0xd9, 0x59, 0x26);

/// Reserved status colours — only ever used where the colour *means* good/bad,
/// and always beside a label.
pub const GOOD: RGBColor = RGBColor(0x0c, 0xa3, 0x0c);
pub const CRITICAL: RGBColor = RGBColor(0xd0, 0x3b, 0x3b);

/// One-hue sequential ramp for magnitude (the heatmap), ordered so near-zero
/// recedes toward the dark surface and the maximum is brightest.
pub const SEQ: [RGBColor; 7] = [
    RGBColor(0x0d, 0x36, 0x6b),
    RGBColor(0x18, 0x4f, 0x95),
    RGBColor(0x25, 0x6a, 0xbf),
    RGBColor(0x39, 0x87, 0xe5),
    RGBColor(0x6d, 0xa7, 0xec),
    RGBColor(0x9e, 0xc5, 0xf4),
    RGBColor(0xcd, 0xe2, 0xfb),
];

/// Colour for a strategy name. Anything unrecognised is treated as mint, so a
/// filtered view never repaints the series that survive.
pub fn strategy_color(strategy: &str) -> RGBColor {
    if strategy.eq_ignore_ascii_case("split") {
        SPLIT
    } else {
        MINT
    }
}

const FONT: &str = "sans-serif";
const BAR_MAX: i32 = 24;
const BAR_RADIUS: i32 = 4;
const GAP: i32 = 2;

/// Register the vendored faces once. Both are Noto Sans (OFL, see
/// `assets/NotoSans-LICENSE.txt`), compiled into the binary so the runtime
/// image needs no fonts and no fontconfig.
pub fn init_fonts() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let regular = include_bytes!("../../assets/NotoSans-Regular.ttf");
        let bold = include_bytes!("../../assets/NotoSans-Bold.ttf");
        if register_font(FONT, FontStyle::Normal, regular).is_err()
            || register_font(FONT, FontStyle::Bold, bold).is_err()
        {
            tracing::warn!("chart fonts failed to register; labels may be blank");
        }
    });
}

// --- Canvas ---------------------------------------------------------------

pub enum Align {
    Left,
    Center,
    Right,
}

/// How a series is drawn, so its legend swatch matches it.
#[derive(Clone, Copy)]
pub enum Mark {
    Fill,
    Dot,
}

/// One legend entry.
#[derive(Clone, Copy)]
pub struct Key<'a> {
    pub label: &'a str,
    pub color: RGBColor,
    pub mark: Mark,
}

impl<'a> Key<'a> {
    pub fn fill(label: &'a str, color: RGBColor) -> Self {
        Self {
            label,
            color,
            mark: Mark::Fill,
        }
    }

    pub fn dot(label: &'a str, color: RGBColor) -> Self {
        Self {
            label,
            color,
            mark: Mark::Dot,
        }
    }
}

/// A pixel canvas with the primitives the house style is defined in terms of.
pub struct Canvas<'a> {
    area: DrawingArea<BitMapBackend<'a>, plotters::coord::Shift>,
    pub w: i32,
    pub h: i32,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], w: u32, h: u32) -> Result<Self> {
        let area = BitMapBackend::with_buffer(buf, (w, h)).into_drawing_area();
        area.fill(&SURFACE).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self {
            area,
            w: w as i32,
            h: h as i32,
        })
    }

    fn style(size: i32, color: RGBColor, bold: bool) -> TextStyle<'static> {
        let font: FontDesc = (
            FONT,
            size as f64,
            if bold {
                FontStyle::Bold
            } else {
                FontStyle::Normal
            },
        )
            .into();
        font.color(&color)
    }

    /// Rendered `(width, height)` of a string — used to decide whether a label
    /// fits before it is drawn, never after.
    pub fn measure(&self, s: &str, size: i32, bold: bool) -> (i32, i32) {
        Self::style(size, INK, bold)
            .font
            .box_size(s)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((s.chars().count() as i32 * size / 2, size))
    }

    /// Draw text anchored horizontally and vertically centred on `y`.
    pub fn text(&self, x: i32, y: i32, s: &str, size: i32, color: RGBColor, align: Align) {
        self.text_styled(x, y, s, size, color, align, false);
    }

    pub fn text_bold(&self, x: i32, y: i32, s: &str, size: i32, color: RGBColor, align: Align) {
        self.text_styled(x, y, s, size, color, align, true);
    }

    #[allow(clippy::too_many_arguments)] // it is a text call: position, content, and how to draw it
    fn text_styled(
        &self,
        x: i32,
        y: i32,
        s: &str,
        size: i32,
        color: RGBColor,
        align: Align,
        bold: bool,
    ) {
        let pos = Pos::new(
            match align {
                Align::Left => HPos::Left,
                Align::Center => HPos::Center,
                Align::Right => HPos::Right,
            },
            VPos::Center,
        );
        let style = Self::style(size, color, bold).pos(pos);
        let _ = self.area.draw_text(s, &style, (x, y));
    }

    pub fn rect(&self, x0: i32, y0: i32, x1: i32, y1: i32, color: RGBColor) {
        let _ = self
            .area
            .draw(&Rectangle::new([(x0, y0), (x1, y1)], color.filled()));
    }

    /// A solid hairline. Grid and axis rules are never dashed.
    pub fn hairline_h(&self, x0: i32, x1: i32, y: i32, color: RGBColor) {
        let _ = self.area.draw(&PathElement::new(
            vec![(x0, y), (x1, y)],
            color.stroke_width(1),
        ));
    }

    pub fn hairline_v(&self, x: i32, y0: i32, y1: i32, color: RGBColor) {
        let _ = self.area.draw(&PathElement::new(
            vec![(x, y0), (x, y1)],
            color.stroke_width(1),
        ));
    }

    /// A 2px data line with round joins.
    pub fn line(&self, points: &[(i32, i32)], color: RGBColor) {
        if points.len() < 2 {
            return;
        }
        let _ = self
            .area
            .draw(&PathElement::new(points.to_vec(), color.stroke_width(2)));
    }

    /// A marker with the 2px surface ring that keeps it legible where marks
    /// overlap. `r` is the mark radius; the ring sits outside it.
    pub fn marker(&self, x: i32, y: i32, r: i32, color: RGBColor) {
        let _ = self
            .area
            .draw(&Circle::new((x, y), r + GAP, SURFACE.filled()));
        let _ = self.area.draw(&Circle::new((x, y), r, color.filled()));
    }

    /// A column: grows from the baseline at `y_base` up to `y_top`, square at
    /// the baseline and 4px-rounded at the data end. Width is capped so a wide
    /// band keeps its air.
    pub fn column(&self, cx: i32, width: i32, y_top: i32, y_base: i32, color: RGBColor) {
        let w = width.clamp(2, BAR_MAX);
        let (x0, x1) = (cx - w / 2, cx - w / 2 + w);
        if (y_base - y_top).abs() < 1 {
            // Zero-height values still get a visible sliver at the baseline.
            self.rect(x0, y_base - 1, x1, y_base, color);
            return;
        }
        let r = BAR_RADIUS.min(w / 2).min((y_base - y_top).abs() / 2);
        let mut pts = vec![(x0, y_base)];
        pts.extend(arc(x0 + r, y_top + r, r, 180.0, 270.0));
        pts.extend(arc(x1 - r, y_top + r, r, 270.0, 360.0));
        pts.push((x1, y_base));
        let _ = self.area.draw(&Polygon::new(pts, color.filled()));
    }

    /// A horizontal bar: square at the baseline on the left, 4px-rounded at the
    /// data end on the right.
    pub fn bar(&self, cy: i32, height: i32, x_base: i32, x_end: i32, color: RGBColor) {
        let h = height.clamp(2, BAR_MAX);
        let (y0, y1) = (cy - h / 2, cy - h / 2 + h);
        if (x_end - x_base).abs() < 1 {
            self.rect(x_base, y0, x_base + 1, y1, color);
            return;
        }
        let r = BAR_RADIUS.min(h / 2).min((x_end - x_base).abs() / 2);
        let mut pts = vec![(x_base, y0)];
        pts.extend(arc(x_end - r, y0 + r, r, 270.0, 360.0));
        pts.extend(arc(x_end - r, y1 - r, r, 0.0, 90.0));
        pts.push((x_base, y1));
        let _ = self.area.draw(&Polygon::new(pts, color.filled()));
    }

    /// A heatmap cell, inset by the surface gap so neighbours read as separate
    /// without a stroke around either.
    pub fn cell(&self, x0: i32, y0: i32, x1: i32, y1: i32, color: RGBColor) {
        self.rect(
            x0,
            y0,
            (x1 - GAP).max(x0 + 1),
            (y1 - GAP).max(y0 + 1),
            color,
        );
    }

    /// A legend row: `swatch label` pairs laid left to right from `x`. Text
    /// wears an ink token; identity comes from the swatch beside it. A `Dot`
    /// swatch matches a series drawn as markers rather than fills, so the key
    /// never needs a glyph the font might not have.
    pub fn legend(&self, x: i32, y: i32, entries: &[Key<'_>], size: i32) {
        let mut cx = x;
        for key in entries {
            match key.mark {
                Mark::Fill => self.rect(cx, y - 5, cx + 10, y + 5, key.color),
                Mark::Dot => self.marker(cx + 5, y, 4, key.color),
            }
            cx += 10 + 8;
            self.text(cx, y, key.label, size, INK2, Align::Left);
            cx += self.measure(key.label, size, false).0 + 20;
        }
    }

    /// Right-aligned legend: measured first so it never runs off the edge.
    pub fn legend_right(&self, right: i32, y: i32, entries: &[Key<'_>], size: i32) {
        let width: i32 = entries
            .iter()
            .map(|k| 10 + 8 + self.measure(k.label, size, false).0 + 20)
            .sum();
        self.legend((right - width + 20).max(0), y, entries, size);
    }

    pub fn present(self) -> Result<()> {
        self.area
            .present()
            .map_err(|e| anyhow::anyhow!("chart present failed: {e}"))
    }
}

/// Points along a circular arc, for the rounded data-end of a bar.
fn arc(cx: i32, cy: i32, r: i32, from_deg: f64, to_deg: f64) -> Vec<(i32, i32)> {
    if r <= 0 {
        return vec![(cx, cy)];
    }
    const STEPS: usize = 6;
    (0..=STEPS)
        .map(|i| {
            let t = (from_deg + (to_deg - from_deg) * i as f64 / STEPS as f64).to_radians();
            (
                cx + (r as f64 * t.cos()).round() as i32,
                cy + (r as f64 * t.sin()).round() as i32,
            )
        })
        .collect()
}

// --- Scales ---------------------------------------------------------------

/// A linear mapping from data space to pixels. `hi` is always ≥ `lo` and the
/// span is never zero, so a flat series still draws.
#[derive(Clone, Copy)]
pub struct Scale {
    lo: f64,
    hi: f64,
    p0: i32,
    p1: i32,
}

impl Scale {
    pub fn new(lo: f64, hi: f64, p0: i32, p1: i32) -> Self {
        let (lo, hi) = if (hi - lo).abs() < f64::EPSILON {
            (lo, lo + 1.0)
        } else {
            (lo.min(hi), lo.max(hi))
        };
        Self { lo, hi, p0, p1 }
    }

    pub fn map(&self, v: f64) -> i32 {
        let t = ((v - self.lo) / (self.hi - self.lo)).clamp(0.0, 1.0);
        self.p0 + ((self.p1 - self.p0) as f64 * t).round() as i32
    }

    /// Round tick values covering the range, at most `want` of them.
    pub fn ticks(&self, want: usize) -> Vec<f64> {
        let want = want.max(2);
        let raw = (self.hi - self.lo) / want as f64;
        if raw <= 0.0 || !raw.is_finite() {
            return vec![self.lo, self.hi];
        }
        let mag = 10f64.powf(raw.log10().floor());
        let step = [1.0, 2.0, 2.5, 5.0, 10.0]
            .iter()
            .map(|m| m * mag)
            .find(|s| *s >= raw)
            .unwrap_or(mag * 10.0);
        let first = (self.lo / step).ceil() * step;
        let mut out = Vec::new();
        let mut v = first;
        while v <= self.hi + step * 0.001 && out.len() <= want + 2 {
            out.push(v);
            v += step;
        }
        out
    }
}

/// Headroom above the tallest value so a column never touches the title.
pub fn headroom(max: f64) -> f64 {
    if max <= 0.0 {
        1.0
    } else {
        max * 1.18
    }
}

// --- Output ---------------------------------------------------------------

/// Draw into a fresh RGB buffer and encode it as PNG.
pub fn render<F>(w: u32, h: u32, draw: F) -> Result<Vec<u8>>
where
    F: FnOnce(&Canvas) -> Result<()>,
{
    init_fonts();
    let mut buf = vec![0u8; (w * h * 3) as usize];
    {
        let canvas = Canvas::new(&mut buf, w, h)?;
        draw(&canvas)?;
        canvas.present()?;
    }
    let mut png = Vec::with_capacity(64 * 1024);
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&buf, w, h, image::ExtendedColorType::Rgb8)
        .context("png encode failed")?;
    Ok(png)
}

// --- Shared furniture -----------------------------------------------------

/// Title block: title, then a subtitle that says what the reader is looking at
/// and over what window. Returns the y the plot may start at.
pub fn header(c: &Canvas, title: &str, subtitle: &str) -> i32 {
    c.rect(0, 0, c.w, 78, PLANE);
    c.text_bold(28, 30, title, 26, INK, Align::Left);
    c.text(28, 58, subtitle, 15, MUTED, Align::Left);
    92
}

/// Footer strip for a caveat or the source window.
pub fn footer(c: &Canvas, note: &str) {
    c.text(28, c.h - 18, note, 13, MUTED, Align::Left);
}

/// The message shown instead of a plot when a window holds no rows — an empty
/// chart with axes and no marks reads as a bug.
pub fn empty_note(c: &Canvas, top: i32, note: &str) {
    c.text(c.w / 2, (top + c.h) / 2 - 10, note, 20, INK2, Align::Center);
    c.text(
        c.w / 2,
        (top + c.h) / 2 + 18,
        "Real set-arb is rare — an empty window is usually correct, not a fault.",
        14,
        MUTED,
        Align::Center,
    );
}

/// Horizontal gridlines plus their y-axis tick labels, drawn before the marks
/// so the data always sits on top.
pub fn y_grid<F>(c: &Canvas, s: &Scale, x0: i32, x1: i32, want: usize, label: F)
where
    F: Fn(f64) -> String,
{
    for t in s.ticks(want) {
        let y = s.map(t);
        c.hairline_h(x0, x1, y, GRID);
        c.text(x0 - 10, y, &label(t), 13, MUTED, Align::Right);
    }
}

/// Truncate to fit `max_px`, with an ellipsis when it does not.
pub fn ellipsize(c: &Canvas, s: &str, size: i32, max_px: i32) -> String {
    if c.measure(s, size, false).0 <= max_px {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        probe.push('…');
        if c.measure(&probe, size, false).0 > max_px {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// `1.23%` from a fraction — edges are always read as percentages.
pub fn pct(v: f64) -> String {
    format!("{:.2}%", v * 100.0)
}

/// Compact money: `$1.2k`, `$340`, `-$12.40`.
pub fn usd(v: f64) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    let a = v.abs();
    if a >= 1_000_000.0 {
        format!("{sign}${:.1}M", a / 1_000_000.0)
    } else if a >= 1_000.0 {
        format!("{sign}${:.1}k", a / 1_000.0)
    } else if a >= 10.0 {
        format!("{sign}${a:.0}")
    } else {
        format!("{sign}${a:.2}")
    }
}

/// Compact counts: `1.2k`, `340`.
pub fn count(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

/// Durations as a trader reads them: `21ms`, `340ms`, `2.4s`, `1m 12s`.
pub fn dur_ms(ms: f64) -> String {
    if ms < 1_000.0 {
        format!("{ms:.0}ms")
    } else if ms < 60_000.0 {
        format!("{:.1}s", ms / 1_000.0)
    } else {
        format!(
            "{}m {:02.0}s",
            (ms / 60_000.0) as i64,
            (ms % 60_000.0) / 1_000.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_map_ends_and_survive_a_flat_series() {
        let s = Scale::new(0.0, 10.0, 100, 0);
        assert_eq!(s.map(0.0), 100);
        assert_eq!(s.map(10.0), 0);
        assert_eq!(s.map(5.0), 50);
        // Out of range is clamped, not extrapolated off the plot.
        assert_eq!(s.map(-5.0), 100);
        assert_eq!(s.map(99.0), 0);
        // A constant series still has a drawable span.
        let flat = Scale::new(3.0, 3.0, 0, 100);
        assert_eq!(flat.map(3.0), 0);
    }

    #[test]
    fn ticks_are_round_numbers_inside_the_range() {
        let ticks = Scale::new(0.0, 100.0, 0, 500).ticks(5);
        assert!(ticks.contains(&0.0) && ticks.contains(&100.0));
        assert!(ticks.iter().all(|t| (0.0..=100.0).contains(t)));
        assert!(ticks.len() <= 8);
    }

    #[test]
    fn formatters_stay_short_enough_to_sit_on_a_mark() {
        assert_eq!(pct(0.0125), "1.25%");
        assert_eq!(usd(1234.0), "$1.2k");
        assert_eq!(usd(-4.5), "-$4.50");
        assert_eq!(count(2400.0), "2.4k");
        assert_eq!(dur_ms(21.0), "21ms");
        assert_eq!(dur_ms(2400.0), "2.4s");
        assert_eq!(dur_ms(72_000.0), "1m 12s");
    }

    #[test]
    fn strategy_colour_follows_the_entity_not_the_row() {
        assert_eq!(strategy_color("mint"), MINT);
        assert_eq!(strategy_color("SPLIT"), SPLIT);
        // An unknown series must not steal the other's hue at random.
        assert_eq!(strategy_color(""), MINT);
    }

    #[test]
    fn the_sequential_ramp_is_monotonic_in_lightness() {
        // A magnitude ramp that dips would misstate the value it encodes.
        let lum = |c: &RGBColor| 0.2126 * c.0 as f64 + 0.7152 * c.1 as f64 + 0.0722 * c.2 as f64;
        for pair in SEQ.windows(2) {
            assert!(lum(&pair[0]) < lum(&pair[1]), "ramp must climb: {pair:?}");
        }
    }
}
