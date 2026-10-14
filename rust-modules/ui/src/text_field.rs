//! **A single-line text field**: the value (or its placeholder) with an insertion caret, scrolled
//! to keep the caret in view, and — for a password — drawn masked.
//!
//! Focus is carried by INK ALONE, the design system's text-field contract that Search set: no
//! capsule, rim, rule or plate (`ui/CLAUDE.md`, product idiom 5). A field crossfades from
//! `TEXT_SECONDARY` to [`ink_target`] on its focus spring, and the caret shows only while the
//! keyboard is up. The geometry helpers below are Search's, moved here when the Setup screen
//! needed the same field: Search draws its HERO field through them, and [`TextField`] is the
//! retained view any other screen draws one with.
//!
//! The app draws no keyboard: the television's own panel types into the field and its text
//! arrives as `TextEdit`s, which the owning screen applies to its `TextBuffer`.

use std::ffi::{CStr, CString};

use plx_machine::machine::Measure;

use crate::label::Label;
use crate::{theme, Painter, Rect};

/// The caret's width.
pub const CARET_W: f32 = 5.0;
/// The air between the run's last glyph and the caret.
pub const CARET_GAP: f32 = 8.0;
/// What a masked field draws in place of each character. The shipped regular and bold faces both
/// carry it (`the_mask_glyph_is_in_both_shipped_faces`).
pub const MASK: char = '\u{2022}';

/// Where a run of width `run` sits in a box `width` wide, and where the caret goes, for a caret
/// `caret` px into the run: `(run_dx, caret_dx)`, both from the box's left edge. The run slides left
/// only as far as it must to keep the CARET (not the tail) in the box, and the caret never leaves it.
pub fn run_layout(run: f32, caret: f32, width: f32, editing: bool) -> (f32, f32) {
    let available = (width - if editing { CARET_W + CARET_GAP } else { 0.0 }).max(0.0);
    let overflow = (caret - available).max(0.0).min((run - available).max(0.0));
    (
        -overflow,
        (caret - overflow + CARET_GAP).min((width - CARET_W).max(0.0)),
    )
}

/// The drawable run of `q` (up to any NUL, which a C string cannot carry) and its head: the text
/// before the insertion point, never splitting a character.
pub fn run_and_head(q: &str, caret: usize) -> (&str, &str) {
    let run = &q[..q.find('\0').unwrap_or(q.len())];
    let mut caret = caret.min(run.len());
    while caret > 0 && !run.is_char_boundary(caret) {
        caret -= 1;
    }
    (run, &run[..caret])
}

/// The caret is drawn only on an editing field, and only during the on half of its blink.
pub fn caret_shown(editing: bool, phase_on: bool) -> bool {
    editing && phase_on
}

/// How far below a box of height `box_h` the clip must reach so a centred run's descenders are not
/// cut: only what centring on the cap band does not already clear.
pub fn descent_pad(box_h: f32, full_h: f32, cap_top: f32, cap_base: f32) -> f32 {
    (full_h - box_h * 0.5 - (cap_top + cap_base) * 0.5).max(0.0)
}

/// The value's ink at full focus: [`theme::FIELD_WAITING_INK`] while focus waits for a press,
/// [`theme::FIELD_EDITING_INK`] while the keyboard is up (the design system's ink-alone focus).
pub fn ink_target(editing: bool) -> [f32; 4] {
    if editing {
        theme::FIELD_EDITING_INK
    } else {
        theme::FIELD_WAITING_INK
    }
}

/// How a [`TextField`] is drawn this frame: its owner's state, read by the draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// The keyboard is up and typing goes into this field.
    pub editing: bool,
    /// The focus spring, 0 (unfocused) to 1 (focused).
    pub hot: f32,
    /// The caret blink is in its on half.
    pub caret_on: bool,
}

/// One field's retained, measured run. The owner holds the text and the caret (a `TextBuffer`);
/// [`TextField::prepare`] re-measures only when either changed, so the draw does no text work.
pub struct TextField {
    size: i32,
    masked: bool,
    text: String,
    caret: usize,
    blank: bool,
    run: CString,
    run_w: f32,
    head_w: f32,
    measured: bool,
}

impl TextField {
    /// A field whose value is drawn at the type rung `size` (a `theme::size` token).
    pub fn new(size: i32) -> Self {
        Self { size, masked: false, text: String::new(), caret: 0, blank: true, run: CString::default(),
            run_w: 0.0, head_w: 0.0, measured: false }
    }

    /// Draw every character as [`MASK`]: a password.
    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn size(&self) -> i32 {
        self.size
    }

    /// Measure `text` with the caret at byte `caret`, or `placeholder` while `text` is blank.
    /// Returns whether anything drawn changed.
    pub fn prepare(&mut self, text: &str, caret: usize, placeholder: &CStr, measure: &dyn Measure) -> bool {
        let (run, head) = run_and_head(text, caret);
        if self.measured && self.text == run && self.caret == head.len() {
            return false;
        }
        self.text = run.to_owned();
        self.caret = head.len();
        self.blank = run.is_empty();
        let shown = |s: &str| -> CString {
            if self.masked {
                CString::new(MASK.to_string().repeat(s.chars().count())).unwrap_or_default()
            } else {
                CString::new(s).unwrap_or_default()
            }
        };
        if self.blank {
            self.run = placeholder.to_owned();
            self.run_w = measure.width(&self.run, self.size, false);
            self.head_w = 0.0;
        } else {
            self.run = shown(run);
            self.run_w = measure.width(&self.run, self.size, true);
            self.head_w = measure.width(&shown(head), self.size, true);
        }
        self.measured = true;
        true
    }

    /// Draw into `rect` (the value's line box). The run is clipped to the box, reaching down only as
    /// far as its descenders need.
    pub fn draw(&self, p: Painter, rect: Rect, look: Look) {
        let ink = if self.blank {
            theme::cross(theme::TEXT_TERTIARY, theme::TEXT_SECONDARY, look.hot)
        } else {
            theme::cross(theme::TEXT_SECONDARY, ink_target(look.editing), look.hot)
        };
        let (run_dx, caret_dx) = run_layout(self.run_w, self.head_w, rect.w, look.editing);
        let (cap_top, cap_base) = plx_gfx::text::text_cap_band(self.size, 1);
        let pad = descent_pad(rect.h, plx_gfx::text::text_height(self.size, 1), cap_top, cap_base);
        let _clip = crate::screen::ClipScope::open_in(p, Rect::new(rect.x, rect.y, rect.w, rect.h + pad));
        let label = Label::new(self.run.as_ptr(), self.size, ink);
        let label = if self.blank { label } else { label.bold() };
        label.draw(p, Rect::new(rect.x + run_dx, rect.y, rect.w - run_dx, rect.h));
        if caret_shown(look.editing, look.caret_on) {
            let text_y = plx_gfx::text::text_vcenter_y(self.size, 1, rect.cy());
            p.rect(
                Rect::new(rect.x + caret_dx, text_y + cap_top, CARET_W, cap_base - cap_top),
                0.0,
                theme::TEXT_PRIMARY,
                theme::TEXT_PRIMARY,
                0.0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ink_target_picks_pure_white_off_editing_and_the_editing_stop_on() {
        assert_eq!(ink_target(false), theme::FIELD_WAITING_INK);
        assert_eq!(ink_target(true), theme::FIELD_EDITING_INK);
        assert_ne!(theme::FIELD_WAITING_INK, theme::FIELD_EDITING_INK);
    }

    #[test]
    fn focus_is_carried_by_a_wide_ink_step_and_nothing_else() {
        let idle = theme::cross(theme::TEXT_SECONDARY, ink_target(false), 0.0);
        assert_eq!(idle, theme::TEXT_SECONDARY);
        let waiting = theme::cross(theme::TEXT_SECONDARY, ink_target(false), 1.0);
        let editing = theme::cross(theme::TEXT_SECONDARY, ink_target(true), 1.0);
        assert_eq!(waiting, theme::FIELD_WAITING_INK);
        assert_eq!(editing, theme::FIELD_EDITING_INK);
        let luma = |c: [f32; 4]| (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) * c[3];
        assert!(luma(waiting) - luma(idle) > 0.15);
        assert!(luma(editing) - luma(idle) > 0.15);
        assert!(luma(waiting) > luma(editing));
    }

    #[test]
    fn the_caret_is_visible_only_during_the_on_phase_of_an_editing_field() {
        assert!(caret_shown(true, true));
        assert!(!caret_shown(true, false));
        assert!(!caret_shown(false, true));
        assert!(!caret_shown(false, false));
    }

    #[test]
    fn a_run_that_fits_starts_at_the_edge_and_the_caret_trails_it() {
        let (run, caret) = run_layout(100.0, 100.0, 756.0, true);
        assert_eq!(run, 0.0);
        assert_eq!(caret, 100.0 + CARET_GAP);
    }

    #[test]
    fn an_overlong_run_slides_left_so_the_caret_lands_on_the_boxs_right_edge() {
        let box_w = 756.0;
        let (run, caret) = run_layout(900.0, 900.0, box_w, true);
        assert!(run < 0.0);
        assert_eq!(run + 900.0, box_w - CARET_GAP - CARET_W);
        assert_eq!(caret + CARET_W, box_w);
    }

    #[test]
    fn with_no_caret_the_run_uses_the_whole_box() {
        let box_w = 756.0;
        let (run, _) = run_layout(900.0, 900.0, box_w, false);
        assert_eq!(run + 900.0, box_w);
    }

    #[test]
    fn an_empty_run_keeps_the_designs_caret_gap() {
        let (run, caret) = run_layout(0.0, 0.0, 756.0, true);
        assert_eq!(run, 0.0);
        assert_eq!(caret, CARET_GAP);
    }

    #[test]
    fn the_caret_follows_the_insertion_point_whether_or_not_the_run_has_slid() {
        for run_w in [0.0, 1.0, 400.0, 753.0, 900.0, 5000.0] {
            let (run, caret) = run_layout(run_w, run_w, 756.0, true);
            assert_eq!(caret, run + run_w + CARET_GAP, "run_w={run_w}");
        }
    }

    #[test]
    fn the_run_scrolls_to_the_caret_rather_than_to_the_tail() {
        let box_w = 756.0;
        let avail = box_w - CARET_GAP - CARET_W;
        assert_eq!(run_layout(2000.0, 0.0, box_w, true), (0.0, CARET_GAP));
        assert_eq!(
            run_layout(2000.0, avail - 10.0, box_w, true),
            (0.0, avail - 10.0 + CARET_GAP)
        );
        let (run, caret) = run_layout(2000.0, 1000.0, box_w, true);
        assert_eq!(run, -(1000.0 - avail));
        assert_eq!(caret + CARET_W, box_w);
        for c in [0.0, 1.0, 500.0, 1999.0, 2000.0] {
            let (_, caret) = run_layout(2000.0, c, box_w, true);
            assert!(
                (0.0..=box_w - CARET_W).contains(&caret),
                "caret at {c} drew at {caret}"
            );
        }
    }

    #[test]
    fn the_head_is_the_text_before_the_insertion_point() {
        assert_eq!(run_and_head("wallace", 0), ("wallace", ""));
        assert_eq!(run_and_head("wallace", 3), ("wallace", "wal"));
        assert_eq!(run_and_head("wallace", 7), ("wallace", "wallace"));
        assert_eq!(run_and_head("wallace ", 8), ("wallace ", "wallace "));
        assert_eq!(run_and_head("", 0), ("", ""));
    }

    #[test]
    fn a_caret_inside_a_multi_byte_character_never_splits_it() {
        for q in ["суббота", "千と千尋", "Amélie", "🎬🎬", "aé千🎬z"] {
            for c in 0..=q.len() + 4 {
                let (run, head) = run_and_head(q, c);
                assert_eq!(run, q);
                assert!(q.starts_with(head), "the head must be a prefix: {head:?}");
                assert!(head.len() <= c.min(q.len()));
                assert!(c.min(q.len()) - head.len() < 4);
            }
        }
    }

    #[test]
    fn a_nul_cuts_the_run_and_the_caret_lands_inside_what_is_left() {
        assert_eq!(run_and_head("wal\0lace", 3), ("wal", "wal"));
        assert_eq!(run_and_head("wal\0lace", 8), ("wal", "wal"));
        assert_eq!(run_and_head("\0wallace", 4), ("", ""));
        assert_eq!(run_and_head("су\0ббота", 99), ("су", "су"));
    }

    #[test]
    fn a_box_too_narrow_for_the_caret_does_not_go_negative() {
        assert_eq!(run_layout(0.0, 0.0, 2.0, true), (0.0, 0.0));
    }

    #[test]
    fn the_clip_reserves_only_the_descent_centering_does_not_already_clear() {
        assert_eq!(descent_pad(80.0, 87.0, 17.0, 70.0), 3.5);
        assert_eq!(descent_pad(80.0, 40.0, 17.0, 20.0), 0.0);
        assert_eq!(descent_pad(80.0, 10.0, 17.0, 70.0), 0.0);
    }

    struct Mono;
    impl Measure for Mono {
        fn width(&self, s: &CStr, sz: i32, _bold: bool) -> f32 {
            s.to_str().unwrap().chars().count() as f32 * sz as f32 * 0.5
        }
        fn cap_h(&self, sz: i32) -> f32 { sz as f32 * 0.7 }
        fn line_h(&self, sz: i32) -> f32 { sz as f32 * 1.2 }
    }

    #[test]
    fn a_masked_field_measures_one_mask_per_character_never_per_byte() {
        let mut field = TextField::new(theme::size::TITLE).masked();
        let placeholder = CString::new("Password").unwrap();
        field.prepare("pä§s", 2, &placeholder, &Mono);
        let mask = MASK.to_string();
        assert_eq!(field.run.to_str().unwrap(), mask.repeat(4));
        // Byte 2 is inside `ä` (bytes 1..3), so the caret backs off to after `p`: one mask in.
        assert_eq!(field.head_w, Mono.width(&CString::new(mask.clone()).unwrap(), theme::size::TITLE, true));
        assert!(!field.run.to_str().unwrap().contains('p'));
    }

    #[test]
    fn a_blank_field_shows_its_placeholder_with_the_caret_at_the_start() {
        let mut field = TextField::new(theme::size::TITLE).masked();
        let placeholder = CString::new("Password").unwrap();
        field.prepare("", 0, &placeholder, &Mono);
        assert_eq!(field.run.as_c_str(), placeholder.as_c_str(), "a placeholder is never masked");
        assert_eq!(field.head_w, 0.0);
    }

    #[test]
    fn prepare_reports_a_change_only_when_the_text_or_the_caret_moved() {
        let mut field = TextField::new(theme::size::TITLE);
        let placeholder = CString::new("Address").unwrap();
        assert!(field.prepare("abc", 3, &placeholder, &Mono));
        assert!(!field.prepare("abc", 3, &placeholder, &Mono));
        assert!(field.prepare("abc", 1, &placeholder, &Mono));
        assert!(field.prepare("abcd", 1, &placeholder, &Mono));
    }

    #[test]
    fn the_mask_glyph_is_in_both_shipped_faces() {
        for face in ["appfont.ttf", "appfont-bold.ttf"] {
            let coverage = plx_base::fontcov::shipped(face).as_ref().unwrap();
            assert!(coverage.contains(MASK as u32), "{face} cannot draw {MASK:?}");
        }
    }
}
