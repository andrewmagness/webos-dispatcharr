//! **The time grid** — rows of spans laid against one horizontal time axis: a programme guide's
//! lanes. This is the library half only: the geometry (an instant to an x, a row to a y, what is
//! in view, where the view must go to reveal a span) and the painters for the axis, a row's label,
//! a span's cell and the now marker, all over plain values. The screen that uses it owns the
//! content, the focus and the two scroll springs; nothing here names a channel or a programme.
//!
//! **Two coordinates, one rule each.** Time runs left to right at a fixed scale (`span` seconds
//! across the lanes) and the view's left edge (`View::start`) only ever rests on a slot boundary,
//! so the axis labels stand still while the cursor walks inside the view. Rows run top to bottom at
//! one pitch and the view's first row (`View::top`) moves the least that keeps the cursor's row
//! inside the lanes. Both are TARGETS for the screen's springs; the painters take whatever the
//! springs currently hold, fractional values included.
//!
//! Instants are Unix seconds (`i64`) and a view's left edge is an `f64`, so a 2026 timestamp keeps
//! sub-second precision through a glide (an `f32` would not).

use std::ffi::CString;

use plx_machine::machine::Measure;

use crate::label::Label;
use crate::{theme, Painter, Rect};

/// Where the grid stands and how it is divided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeGrid {
    /// The whole grid: the axis band across the top, the label column down the left, the lanes.
    pub frame: Rect,
    /// The label column's width (a row's number and name).
    pub label_w: f32,
    /// The axis band's height.
    pub axis_h: f32,
    pub row_h: f32,
    pub row_gap: f32,
    /// Seconds shown across the lanes' width.
    pub span: i64,
    /// The axis step, and what a view's left edge rests on.
    pub slot: i64,
}

/// What the grid is showing: the instant at the lanes' left edge, and the first row (fractional
/// while a scroll glides).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub start: f64,
    pub top: f32,
}

/// The gap between two cells of one row, and between the label column and the lanes.
pub const CELL_GAP: f32 = 6.0;
/// The text inset inside a cell.
pub const CELL_PAD: f32 = 16.0;
/// The cell's corner radius.
pub const CELL_RAD: f32 = 10.0;
/// The now marker's width.
pub const NOW_W: f32 = 3.0;
/// A cell narrower than this carries no text: a sliver with a letter in it reads as a glitch.
pub const CELL_TEXT_MIN_W: f32 = 56.0;
/// The logo slot in a row's label, between the number and the name. Reserved whether or not the
/// row has a logo, so the names line up down the column.
pub const LOGO_W: f32 = 96.0;
pub const LOGO_H: f32 = 56.0;

impl TimeGrid {
    /// The lanes: right of the label column, under the axis band.
    pub fn lanes(&self) -> Rect {
        let x = self.frame.x + self.label_w + CELL_GAP;
        let y = self.frame.y + self.axis_h;
        Rect::new(x, y, (self.frame.x + self.frame.w - x).max(0.0), (self.frame.y + self.frame.h - y).max(0.0))
    }

    pub fn px_per_sec(&self) -> f64 {
        self.lanes().w as f64 / self.span.max(1) as f64
    }

    /// The x of instant `t` in `view` (unclipped: it may lie outside the lanes).
    pub fn x_at(&self, t: i64, view: View) -> f32 {
        self.lanes().x + ((t as f64 - view.start) * self.px_per_sec()) as f32
    }

    pub fn pitch(&self) -> f32 {
        self.row_h + self.row_gap
    }

    /// Row `row`'s top in `view` (unclipped).
    pub fn row_y(&self, row: usize, view: View) -> f32 {
        self.lanes().y + (row as f32 - view.top) * self.pitch()
    }

    /// How many whole rows the lanes hold.
    pub fn rows_fit(&self) -> usize {
        (((self.lanes().h + self.row_gap) / self.pitch()).floor() as usize).max(1)
    }

    /// The rows of `n` that any part of falls inside the lanes in `view`.
    pub fn visible_rows(&self, view: View, n: usize) -> std::ops::Range<usize> {
        let first = view.top.floor().max(0.0) as usize;
        let last = ((view.top + self.lanes().h / self.pitch()).ceil().max(0.0) as usize).min(n);
        first.min(last)..last
    }

    /// Row `row`'s label slot (the column left of the lanes).
    pub fn label(&self, row: usize, view: View) -> Rect {
        Rect::new(self.frame.x, self.row_y(row, view), self.label_w, self.row_h)
    }

    /// The cell for the span `[start, end)` on `row`, clipped to the lanes horizontally; `None`
    /// when nothing of it is in view. Vertically it is NOT clipped (a row half out of the lanes is
    /// the painter's clip to make), so a focus rect keeps its true height mid-glide.
    pub fn cell(&self, row: usize, start: i64, end: i64, view: View) -> Option<Rect> {
        let lanes = self.lanes();
        let x0 = self.x_at(start, view).max(lanes.x);
        // The gap belongs to the cell's right edge, so neighbours never touch.
        let x1 = (self.x_at(end, view) - CELL_GAP).min(lanes.x + lanes.w);
        (x1 - x0 >= 1.0).then(|| Rect::new(x0, self.row_y(row, view), x1 - x0, self.row_h))
    }

    /// Where the view's left edge must go to reveal the span `[start, end)`, from `current`. It
    /// stays put while the span's start is in view (so a walk inside the view moves no axis), and
    /// while a span that began before the view still shows at least half a slot of itself — a
    /// programme that started an hour ago is plainly on screen, and a step down the channels onto
    /// it must not swing the axis back to its start. Otherwise it moves to the slot boundary at or
    /// before the start — never before `floor` (the earliest instant there is anything to show).
    pub fn reveal_start(&self, current: f64, start: i64, end: i64, floor: i64) -> f64 {
        let (start_f, end_f) = (start as f64, end as f64);
        let in_view = start_f >= current && start_f < current + self.span as f64;
        let shows_enough = start_f < current && end_f - current >= (self.slot.max(1) / 2) as f64;
        if in_view || shows_enough {
            return current;
        }
        self.slot_start(start, floor)
    }

    /// The slot boundary at or before `t`, never before `floor`: where a view opens on `t`.
    pub fn slot_start(&self, t: i64, floor: i64) -> f64 {
        (t - t.rem_euclid(self.slot.max(1))).max(floor) as f64
    }

    /// Where the view's first row must go to keep `row` of `n` whole inside the lanes, from
    /// `current`: unmoved while it already is, else the least move that makes it so, clamped so the
    /// list never scrolls past its end.
    pub fn reveal_top(&self, current: f32, row: usize, n: usize) -> f32 {
        let fit = self.rows_fit();
        let max_top = n.saturating_sub(fit) as f32;
        let row = row as f32;
        let top = if row < current {
            row
        } else if row > current + fit as f32 - 1.0 {
            row - (fit as f32 - 1.0)
        } else {
            current
        };
        top.clamp(0.0, max_top.max(0.0))
    }

    /// The slot boundaries the axis labels in `view`: every one from the slot at or before the
    /// left edge to the right edge.
    pub fn axis_ticks(&self, view: View) -> impl Iterator<Item = i64> {
        let slot = self.slot.max(1);
        let first = (view.start.floor() as i64).div_euclid(slot) * slot;
        let end = view.start + self.span as f64;
        (0..).map(move |i| first + i * slot).take_while(move |t| (*t as f64) < end)
    }
}

/// How a cell is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellLook {
    /// The focus crossfade, 0 to 1.
    pub focus: f32,
    /// On the air at the view's "now".
    pub airing: bool,
    /// A gap the source lists nothing for: drawn as a quieter cell.
    pub unlisted: bool,
}

/// The time axis: each slot's label (`label(t)`, the caller's locale), left-aligned just past its
/// tick. A label whose tick is left of the lanes stands at the lanes' left edge until the next one
/// would overlap it, as a calendar's sticky heading does.
pub fn draw_axis(p: Painter, grid: &TimeGrid, view: View, measure: &dyn Measure, label: impl Fn(i64) -> String) {
    let lanes = grid.lanes();
    let band = Rect::new(lanes.x, grid.frame.y, lanes.w, grid.axis_h);
    let size = theme::size::CAPTION;
    let ticks: Vec<i64> = grid.axis_ticks(view).collect();
    for (i, &t) in ticks.iter().enumerate() {
        let text = label(t);
        let Ok(c) = CString::new(text) else { continue };
        let w = measure.width(&c, size, false);
        let tick_x = grid.x_at(t, view);
        let mut x = tick_x.max(lanes.x) + CELL_PAD * 0.5;
        if let Some(&next) = ticks.get(i + 1) {
            // A sticky label gives way to the next one rather than run under it.
            x = x.min(grid.x_at(next, view) - CELL_PAD - w);
        }
        if x + w <= lanes.x || x >= lanes.x + lanes.w {
            continue;
        }
        if tick_x >= lanes.x {
            p.rect(Rect::new(tick_x, band.y + band.h * 0.25, 1.0, band.h * 0.5), 0.0, theme::TEXT_SEPARATOR,
                theme::TEXT_SEPARATOR, 0.0);
        }
        Label::new(c.as_ptr(), size, theme::TEXT_SECONDARY).draw(p, Rect::new(x, band.y, w, band.h));
    }
}

/// Where a `w`×`h` image stands in the logo slot of a label at `rect`: as large as fits, aspect
/// kept, centred. `measure` sizes the number column the slot follows.
pub fn logo_rect(rect: Rect, w: f32, h: f32, measure: &dyn Measure) -> Rect {
    let slot_x = rect.x + measure.width_str("0000", theme::size::LABEL, true) + CELL_PAD;
    let slot = Rect::new(slot_x, rect.y + (rect.h - LOGO_H) * 0.5, LOGO_W, LOGO_H);
    if w <= 0.0 || h <= 0.0 {
        return slot;
    }
    let scale = (slot.w / w).min(slot.h / h);
    let (dw, dh) = (w * scale, h * scale);
    Rect::new(slot.x + (slot.w - dw) * 0.5, slot.y + (slot.h - dh) * 0.5, dw, dh)
}

/// One row's label: its number over a fixed column, its logo (`(texture, w, h)` once resident;
/// the slot stays empty until then and for a row without one), then its name, elided to the column.
pub fn draw_row_label(p: Painter, rect: Rect, number: &str, logo: Option<(u32, f32, f32)>, name: &str, hot: f32,
    measure: &dyn Measure) {
    let size = theme::size::LABEL;
    let ink = theme::cross(theme::TEXT_SECONDARY, theme::TEXT_PRIMARY, hot);
    let number_w = measure.width_str("0000", size, true);
    if !number.is_empty() {
        let n = measure.fit_line(number, number_w, size, true);
        Label::new(n.as_ptr(), size, ink).bold().draw(p, Rect::new(rect.x, rect.y, number_w, rect.h));
    }
    if let Some((tex, w, h)) = logo.filter(|l| l.0 != 0) {
        p.tex(tex, logo_rect(rect, w, h, measure), 0.0, theme::TINT_WHITE);
    }
    let x = rect.x + number_w + CELL_PAD + LOGO_W + CELL_PAD;
    let w = (rect.x + rect.w - x).max(0.0);
    let n = measure.fit_line(name, w, size, false);
    Label::new(n.as_ptr(), size, ink).draw(p, Rect::new(x, rect.y, w, rect.h));
}

/// One span's cell: its ground, then its title and (when there is room under it) its second line,
/// each elided to the cell.
pub fn draw_cell(p: Painter, rect: Rect, title: &str, second: Option<&str>, look: CellLook, measure: &dyn Measure) {
    let rest = if look.unlisted {
        theme::GUIDE_CELL_UNLISTED_FILL
    } else if look.airing {
        theme::GUIDE_CELL_AIRING_FILL
    } else {
        theme::GUIDE_CELL_FILL
    };
    let fill = theme::cross(rest, theme::ACCENT, look.focus);
    p.rect(rect, CELL_RAD, fill, fill, 0.0);
    if rect.w < CELL_TEXT_MIN_W {
        return;
    }
    let w = rect.w - 2.0 * CELL_PAD;
    let rest_ink = if look.unlisted { theme::TEXT_TERTIARY } else { theme::TEXT_PRIMARY };
    let ink = theme::cross(rest_ink, theme::INK_ON_ACCENT, look.focus);
    let title_size = theme::size::LABEL;
    let second_size = theme::size::CAPTION;
    let title_h = measure.line_h(title_size);
    let second_h = measure.line_h(second_size);
    let x = rect.x + CELL_PAD;
    let both = second.is_some() && title_h + second_h <= rect.h - CELL_PAD;
    let block_h = if both { title_h + second_h } else { title_h };
    let y = rect.y + (rect.h - block_h) * 0.5;
    let t = measure.fit_line(title, w, title_size, true);
    Label::new(t.as_ptr(), title_size, ink).bold().draw(p, Rect::new(x, y, w, title_h));
    if let (true, Some(second)) = (both, second) {
        let sub_ink = theme::cross(theme::TEXT_SECONDARY, theme::INK_ON_ACCENT, look.focus);
        let s = measure.fit_line(second, w, second_size, false);
        Label::new(s.as_ptr(), second_size, sub_ink).draw(p, Rect::new(x, y + title_h, w, second_h));
    }
}

/// The now marker: a rule down the lanes (and the axis band) at instant `now`, when it is in view.
/// Draw it before the cells: they are translucent, and their text belongs on top of it.
pub fn draw_now(p: Painter, grid: &TimeGrid, view: View, now: i64) {
    let lanes = grid.lanes();
    let x = grid.x_at(now, view);
    if x < lanes.x || x > lanes.x + lanes.w {
        return;
    }
    p.rect(Rect::new(x - NOW_W * 0.5, grid.frame.y, NOW_W, grid.frame.h), NOW_W * 0.5, theme::GUIDE_NOW,
        theme::GUIDE_NOW, 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> TimeGrid {
        // Lanes 1440 px wide: 2 hours at 0.2 px a second.
        TimeGrid {
            frame: Rect::new(100.0, 300.0, 1440.0 + 300.0 + CELL_GAP, 60.0 + 4.0 * 90.0 - 10.0),
            label_w: 300.0,
            axis_h: 60.0,
            row_h: 80.0,
            row_gap: 10.0,
            span: 7200,
            slot: 1800,
        }
    }

    const T0: i64 = 1_800_000_000 - 1_800_000_000 % 1800;

    #[test]
    fn an_instant_maps_to_the_lanes_at_the_grid_scale() {
        let g = grid();
        let v = View { start: T0 as f64, top: 0.0 };
        assert_eq!(g.lanes().x, 100.0 + 300.0 + CELL_GAP);
        assert_eq!(g.x_at(T0, v), g.lanes().x);
        assert!((g.x_at(T0 + 3600, v) - (g.lanes().x + 720.0)).abs() < 1e-3);
    }

    #[test]
    fn rows_fit_counts_whole_rows_and_visible_rows_include_partial_ones() {
        let g = grid();
        assert_eq!(g.rows_fit(), 4);
        assert_eq!(g.visible_rows(View { start: 0.0, top: 0.0 }, 10), 0..4);
        assert_eq!(g.visible_rows(View { start: 0.0, top: 0.5 }, 10), 0..5);
        assert_eq!(g.visible_rows(View { start: 0.0, top: 8.0 }, 10), 8..10);
    }

    #[test]
    fn a_cell_is_clipped_to_the_lanes_and_keeps_its_right_gap() {
        let g = grid();
        let v = View { start: T0 as f64, top: 0.0 };
        let lanes = g.lanes();
        // Started an hour before the view: its left edge is the lanes'.
        let c = g.cell(0, T0 - 3600, T0 + 1800, v).unwrap();
        assert_eq!(c.x, lanes.x);
        assert!((c.x + c.w - (g.x_at(T0 + 1800, v) - CELL_GAP)).abs() < 1e-3);
        // Running past the view: its right edge is the lanes'.
        let c = g.cell(0, T0 + 5400, T0 + 9000, v).unwrap();
        assert_eq!(c.x + c.w, lanes.x + lanes.w);
        // Wholly outside: nothing.
        assert!(g.cell(0, T0 - 3600, T0, v).is_none());
        assert!(g.cell(0, T0 + 7200, T0 + 9000, v).is_none());
    }

    #[test]
    fn the_view_stays_put_while_the_span_start_is_in_view_and_snaps_to_a_slot_otherwise() {
        let g = grid();
        let cur = T0 as f64;
        assert_eq!(g.reveal_start(cur, T0 + 600, T0 + 2400, 0), cur);
        assert_eq!(g.reveal_start(cur, T0 + 7199, T0 + 9000, 0), cur);
        // Past the right edge: the slot at or before its start comes to the left edge.
        assert_eq!(g.reveal_start(cur, T0 + 7200 + 900, T0 + 9900, 0), (T0 + 7200) as f64);
        // Ended (nearly) before the left edge: likewise, but never before the floor.
        assert_eq!(g.reveal_start(cur, T0 - 900, T0 + 300, 0), (T0 - 1800) as f64);
        assert_eq!(g.reveal_start(cur, T0 - 900, T0 + 300, T0 - 1000), (T0 - 1000) as f64);
    }

    #[test]
    fn a_span_that_began_before_the_view_and_shows_half_a_slot_keeps_the_view() {
        let g = grid();
        let cur = T0 as f64;
        // Began an hour before, runs a quarter of an hour into the view: enough of it shows.
        assert_eq!(g.reveal_start(cur, T0 - 3600, T0 + 900, 0), cur);
        // A sliver (ten minutes) does not: the view goes back to its start.
        assert_eq!(g.reveal_start(cur, T0 - 3600, T0 + 600, 0), (T0 - 3600) as f64);
    }

    #[test]
    fn the_top_row_moves_the_least_that_keeps_the_cursor_row_whole() {
        let g = grid();
        assert_eq!(g.reveal_top(0.0, 2, 10), 0.0);
        assert_eq!(g.reveal_top(0.0, 4, 10), 1.0);
        assert_eq!(g.reveal_top(5.0, 3, 10), 3.0);
        // Never past the end, and a list shorter than the lanes never scrolls.
        assert_eq!(g.reveal_top(0.0, 9, 10), 6.0);
        assert_eq!(g.reveal_top(0.0, 2, 3), 0.0);
    }

    #[test]
    fn a_logo_fits_its_slot_aspect_kept_and_centred() {
        let m = crate::fixture::FixtureMeasure;
        let label = Rect::new(100.0, 300.0, 440.0, 84.0);
        let slot = logo_rect(label, 0.0, 0.0, &m);
        assert_eq!((slot.w, slot.h), (LOGO_W, LOGO_H));
        assert_eq!(slot.y, 300.0 + (84.0 - LOGO_H) * 0.5);
        // A wide logo fills the slot's width; a tall one its height; both centred in it.
        let wide = logo_rect(label, 400.0, 100.0, &m);
        assert_eq!((wide.w, wide.h), (LOGO_W, LOGO_W / 4.0));
        assert!((wide.cy() - slot.cy()).abs() < 1e-3);
        let tall = logo_rect(label, 100.0, 200.0, &m);
        assert_eq!((tall.w, tall.h), (LOGO_H / 2.0, LOGO_H));
        assert!((tall.cx() - slot.cx()).abs() < 1e-3);
    }

    #[test]
    fn the_axis_ticks_every_slot_from_the_one_at_the_left_edge() {
        let g = grid();
        let ticks: Vec<i64> = g.axis_ticks(View { start: (T0 + 600) as f64, top: 0.0 }).collect();
        assert_eq!(ticks, vec![T0, T0 + 1800, T0 + 3600, T0 + 5400, T0 + 7200]);
    }
}
