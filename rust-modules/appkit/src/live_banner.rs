//! **The live-TV banner** — what the player shows over a channel instead of a transport: the
//! channel's number, logo and name and what is on now with how far through it is, and in a column
//! on the right what is up next (the owner's ask, 2026-10-15). Shown for a few seconds after every
//! tune and whenever OK asks for it.
//!
//! Plus the typed-number read-out in the top corner, while digits are being entered on the remote.
//!
//! Drawn over the hardware video plane on the transport's own dark ground
//! ([`crate::player_hud::draw_scrim`]), with the playbar's rail and ink for the progress bar, so a
//! live channel and a film read as one player. Pure geometry and the progress fraction are
//! host-tested; the pixels are the television's to judge.

use plx_media::route::{LiveChannel, LiveProgramme};
use plx_platform::i18n::{self, msg};
use plx_ui::consts::{MARGIN_X, MARGIN_Y, SCR_H, SCR_W};
use plx_ui::label::Label;
use plx_ui::theme;
use plx_ui::{Painter, Rect};

/// The logo's box: a channel logo is a wordmark, wider than tall.
pub const LOGO_W: f32 = 192.0;
pub const LOGO_H: f32 = 112.0;
/// The progress rail: as thick as the playbar's, and long enough to read a fraction at a glance.
const BAR_W: f32 = 480.0;
const BAR_H: f32 = 6.0;
/// The up-next column's width, at the banner's right edge.
pub const NEXT_W: f32 = 560.0;

/// How far through `p` instant `now` is, 0..=1.
pub fn progress(p: &LiveProgramme, now: i64) -> f32 {
    let span = (p.end - p.start).max(1) as f32;
    ((now - p.start) as f32 / span).clamp(0.0, 1.0)
}

/// `rect` shrunk to the largest box of `w`×`h`'s aspect inside it, centred.
pub fn fit(rect: Rect, w: f32, h: f32) -> Rect {
    if w <= 0.0 || h <= 0.0 {
        return rect;
    }
    let scale = (rect.w / w).min(rect.h / h);
    let (dw, dh) = (w * scale, h * scale);
    Rect::new(rect.x + (rect.w - dw) * 0.5, rect.y + (rect.h - dh) * 0.5, dw, dh)
}

/// The banner for `channel` at wall-clock second `now`.
pub fn draw(p: Painter, channel: &LiveChannel, now: i64, measure: &dyn plx_machine::machine::Measure) {
    crate::player_hud::draw_scrim(p);
    let on = channel.on_at(now);
    let next = channel.next_after(now);

    let head = theme::size::HEADLINE;
    let title = theme::size::TITLE;
    let body = theme::size::BODY;
    let caption = theme::size::CAPTION;
    let (head_h, title_h, body_h, caption_h) =
        (measure.line_h(head), measure.line_h(title), measure.line_h(body), measure.line_h(caption));
    let block_h = head_h + theme::space::XS + if on.is_some() { title_h + theme::space::XS + body_h } else { 0.0 };
    let top = SCR_H - MARGIN_Y - theme::space::LG - block_h;

    if let Some(url) = channel.logo.as_deref() {
        let (tex, w, h) = plx_ui::tex::resolve_wh_on(plx_ui::tex::DIRECT, url, LOGO_W as i32, LOGO_H as i32, true);
        if tex != 0 {
            let slot = Rect::new(MARGIN_X, top + (block_h - LOGO_H).max(0.0) * 0.5, LOGO_W, LOGO_H);
            p.tex(tex, fit(slot, w, h), 0.0, theme::TINT_WHITE);
        }
    }
    let x = MARGIN_X + LOGO_W + theme::space::LG;
    let next_x = SCR_W - MARGIN_X - NEXT_W;
    let w = if next.is_some() { next_x - theme::space::LG - x } else { SCR_W - MARGIN_X - x }.max(0.0);
    let mut y = top;

    // The number, then the name.
    let mut name_x = x;
    if !channel.number.is_empty() {
        let n = measure.fit_line(&channel.number, w, head, true);
        let nw = measure.width(&n, head, true);
        Label::new(n.as_ptr(), head, theme::TEXT_PRIMARY).bold().draw(p, Rect::new(x, y, nw, head_h));
        name_x += nw + theme::space::MD;
    }
    let name = measure.fit_line(&channel.name, (x + w - name_x).max(0.0), head, false);
    Label::new(name.as_ptr(), head, theme::TEXT_PRIMARY).draw(p, Rect::new(name_x, y, (x + w - name_x).max(0.0), head_h));
    y += head_h + theme::space::XS;

    // What is on now: its title, then its times beside how far through it is.
    if let Some(on) = on {
        let t = measure.fit_line(&on.title, w, title, true);
        Label::new(t.as_ptr(), title, theme::TEXT_PRIMARY).bold().draw(p, Rect::new(x, y, w, title_h));
        y += title_h + theme::space::XS;
        let bar_y = y + (body_h - BAR_H) * 0.5;
        p.rect(Rect::new(x, bar_y, BAR_W, BAR_H), BAR_H * 0.5, theme::RAIL_TRACK, theme::RAIL_TRACK, 0.0);
        let fill = BAR_W * progress(on, now);
        if fill > 0.0 {
            p.rrect(Rect::new(x, bar_y, fill, BAR_H), BAR_H * 0.5, 0.0, theme::TEXT_PRIMARY);
        }
        let times = msg::dispatcharr_guide_time_range(&i18n::wall_time(on.end), &i18n::wall_time(on.start));
        let tx = x + BAR_W + theme::space::MD;
        let t = measure.fit_line(&times, (x + w - tx).max(0.0), body, false);
        Label::new(t.as_ptr(), body, theme::TEXT_SECONDARY).draw(p, Rect::new(tx, y, (x + w - tx).max(0.0), body_h));
    }

    // Up next, in its own column on the right, level with what is on now.
    if let Some(next) = next {
        let y = top + head_h + theme::space::XS;
        // `time` is the only argument.
        let heading = msg::dispatcharr_live_up_next(&i18n::wall_time(next.start));
        let t = measure.fit_line(&heading, NEXT_W, caption, true);
        Label::new(t.as_ptr(), caption, theme::TEXT_SECONDARY).bold().draw(p, Rect::new(next_x, y, NEXT_W, caption_h));
        let title_y = y + caption_h + theme::space::XS;
        let t = measure.fit_line(&next.title, NEXT_W, body, true);
        Label::new(t.as_ptr(), body, theme::TEXT_PRIMARY).bold().draw(p, Rect::new(next_x, title_y, NEXT_W, body_h));
    }
}

/// The number being typed on the remote, top right; or, once it has matched nothing, that it
/// matched nothing.
pub fn draw_typed(p: Painter, typed: &str, missing: bool, measure: &dyn plx_machine::machine::Measure) {
    let (text, size) = if missing {
        (msg::dispatcharr_live_no_channel(typed), theme::size::HEADLINE)
    } else {
        (typed.to_owned(), theme::size::DISPLAY)
    };
    let h = measure.line_h(size);
    let budget = SCR_W - 2.0 * MARGIN_X;
    let t = measure.fit_line(&text, budget, size, true);
    let tw = measure.width(&t, size, true);
    let rect = Rect::new(SCR_W - MARGIN_X - tw, MARGIN_Y + theme::space::LG, tw, h);
    // A soft ground so the digits read over any picture.
    let pad = theme::space::MD;
    let ground = Rect::new(rect.x - pad, rect.y - theme::space::XS, rect.w + 2.0 * pad, rect.h + 2.0 * theme::space::XS);
    p.rrect(ground, ground.h * 0.5, 0.0, theme::scrim_black(0.6));
    Label::new(t.as_ptr(), size, theme::TEXT_PRIMARY).bold().draw(p, rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(start: i64, end: i64) -> LiveProgramme {
        LiveProgramme { start, end, title: "x".into() }
    }

    #[test]
    fn progress_runs_from_the_start_to_the_end_and_stays_in_bounds() {
        let p = prog(1000, 2800);
        assert_eq!(progress(&p, 1000), 0.0);
        assert!((progress(&p, 1900) - 0.5).abs() < 1e-6);
        assert_eq!(progress(&p, 2800), 1.0);
        assert_eq!(progress(&p, 500), 0.0, "before it starts");
        assert_eq!(progress(&p, 9999), 1.0, "after it ends");
        assert_eq!(progress(&prog(5, 5), 5), 0.0, "a zero-length programme does not divide by zero");
    }

    #[test]
    fn a_logo_keeps_its_shape_inside_the_box() {
        let slot = Rect::new(0.0, 0.0, LOGO_W, LOGO_H);
        let wide = fit(slot, 400.0, 100.0);
        assert!((wide.w - LOGO_W).abs() < 1e-3 && wide.h < LOGO_H, "a wordmark fills the width");
        assert!((wide.y - (LOGO_H - wide.h) * 0.5).abs() < 1e-3, "centred");
        let tall = fit(slot, 50.0, 100.0);
        assert!((tall.h - LOGO_H).abs() < 1e-3 && tall.w < LOGO_W);
        assert_eq!(fit(slot, 0.0, 0.0), slot, "unknown size: the whole box");
    }
}
