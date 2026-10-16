//! What the live player draws over the picture: subtitles (text, image and ASS, composited directly
//! over the video plane on the raw text/tex primitives) and the read-out (connecting, buffering, and
//! the failure page with its row of actions), plus the scrim the live banner stands on. Reads the
//! playback state through `plx_media::player`.
#![allow(dead_code)]
use plx_gfx::gfx::{delete_tex, upload_rgba};
use plx_ui::consts::{SCR_H, SCR_W};
use plx_ui::theme;
use plx_ui::widgets::{StatusKind, StatusOverlay};
use plx_ui::{Env, Painter, Rect, View};
use std::ffi::CString;
use std::os::raw::c_uint;

/// The HUD widgets draw purely from their own fields and ignore their Env.
fn hud_env() -> Env {
    Env::inert()
}

/// naive word-wrap to `max` chars/line (on word boundaries)
fn wrap(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > max {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// client-rendered subtitle line(s), bottom-center, synced to the video clock. Drawn
/// every frame independent of the transport HUD; hidden when subtitles are off or no
/// cue is active at the current position.
///
/// A cue too tall for the area below the Timing capsule is shown a page at a time
/// ([`CaptionPlan`]); `pager` holds its layout so the wrap and fit run once per cue, not per frame.
pub fn draw_subtitles(pager: &CaptionPager, hud_up: bool) {
    let Some(page) = pager.refresh(hud_up) else { return };
    let slot = pager.slot.borrow();
    if let Some((_, plan)) = slot.as_ref() {
        draw_caption_block(plan.page_lines(page), &plan.page_layout(page));
    }
}

/// Which page of the active cue is showing (0 for a cue that fits whole, and for no cue), so the
/// player's frame fingerprint can owe a frame at the instant a long cue turns its page: nothing
/// else on the screen changes then.
pub fn caption_page(pager: &CaptionPager, hud_up: bool) -> u32 {
    pager.refresh(hud_up).unwrap_or(0) as u32
}

/// Shared caption placement for plain text and a styled-renderer failure message. Text size and
/// vertical placement follow the viewer's Settings > Playback picks (`route::subtitle_size`,
/// `route::subtitle_position`) — native ASS/SSA keeps its authored style instead
/// (`docs/ass-subtitles.md`). The message is a short fixed string, so it is never paged: a block
/// taller than the area is drawn whole at the smallest step.
pub fn draw_subtitle_message(text: &str, hud_up: bool) {
    let (scale, baseline, floor) = caption_geometry(hud_up);
    let lines = caption_wrap(text, scale);
    if lines.is_empty() {
        return;
    }
    let lay = caption_layout(lines.len(), scale, baseline, floor);
    draw_caption_block(&lines, &lay);
}

/// The viewer's size and the y a caption block is placed from: sit near the bottom normally; lift
/// above the scrubber/tabs while the HUD is up; the viewer's Position pick lifts it further still,
/// same direction, on top of that. The third value is the lowest y the block may reach — with the
/// HUD up that is the transport's ceiling, so a block that grows down cannot run into the scrubber.
fn caption_geometry(hud_up: bool) -> (f32, f32, f32) {
    let scale = subtitle_size_scale(plx_media::route::subtitle_size());
    let baseline = (if hud_up { SUB_CEIL_Y } else { SUB_BASE_Y })
        - subtitle_position_lift(plx_media::route::subtitle_position());
    let floor = if hud_up { SUB_CEIL_Y } else { CAPTION_FLOOR_Y };
    (scale, baseline, floor)
}

fn draw_caption_block(lines: &[String], lay: &CaptionLayout) {
    let cx = SCR_W * 0.5;
    let ink = subtitle_ink(); // white unless the viewer picked a dimmer tone (track menu)
    let outline = theme::scrim_black(0.85);
    let p = Painter::root();
    for (i, ln) in lines.iter().enumerate() {
        let top = lay.line_top(i);
        if let Ok(cs) = CString::new(ln.as_str()) {
            // dark outline (4 offsets) then the bold caption in its tone — legible over any scene
            for (dx, dy) in [(-2.0f32, 0.0f32), (2.0, 0.0), (0.0, -2.0), (0.0, 2.0)] {
                p.text(cs.as_ptr(), cx + dx, top + 4.0 + dy, lay.face, outline, 1, 1);
            }
            p.text(cs.as_ptr(), cx, top + 4.0, lay.face, ink, 1, 1);
        }
    }
}

/// One cue's caption, wrapped and fitted once: every wrapped line, the layout they share, and how
/// many lines a page holds. A cue that fits the area at some size step is ONE page (`per_page ==
/// lines.len()`), laid out exactly as [`caption_layout`] places it. One that does not fit even at
/// the smallest step is split into the fewest equal pages that each fit at that step, so no line is
/// ever below the screen: the viewer reads them in order, the cue's display time shared evenly
/// between the pages ([`caption_page_at`]).
struct CaptionPlan {
    lines: Vec<String>,
    layout: CaptionLayout,
    per_page: usize,
}

impl CaptionPlan {
    fn new(lines: Vec<String>, scale: f32, baseline: f32, floor: f32) -> Self {
        let (layout, fits) = caption_fit(lines.len(), scale, baseline, floor);
        let per_page = if fits {
            lines.len()
        } else {
            // the smallest step's capacity below the anchored top (always >= the 3 anchored lines)
            let room = (((floor - layout.top) / layout.pitch).floor() as usize).max(1);
            lines.len().div_ceil(lines.len().div_ceil(room))
        };
        Self { lines, layout, per_page }
    }

    fn pages(&self) -> usize {
        self.lines.len().div_ceil(self.per_page.max(1))
    }

    fn page_lines(&self, page: usize) -> &[String] {
        let from = (page * self.per_page).min(self.lines.len());
        &self.lines[from..(from + self.per_page).min(self.lines.len())]
    }

    /// Every page shares the top and size; only its line count differs (the last may be shorter).
    fn page_layout(&self, page: usize) -> CaptionLayout {
        CaptionLayout { lines: self.page_lines(page).len(), ..self.layout }
    }
}

/// The page of `pages` showing at `clock` for a cue shown over `[start, end)`: the cue's time cut
/// into equal parts, in order, the last page holding until the end. A cue with no real end (the
/// failure notice's `i64::MAX`) stays on its first page.
fn caption_page_at(pages: usize, start: i64, end: i64, clock: i64) -> usize {
    let dur = end.saturating_sub(start);
    if pages <= 1 || dur <= 0 {
        return 0;
    }
    let elapsed = clock.saturating_sub(start).clamp(0, dur - 1) as i128;
    (elapsed * pages as i128 / dur as i128) as usize
}

/// What a [`CaptionPager`] planned for: the cue (its start and text) and the geometry it was laid
/// out for, which changes with the viewer's size/position pick and the transport lifting.
#[derive(PartialEq)]
struct CaptionKey {
    start_ns: i64,
    text: String,
    geometry: [u32; 3],
}

/// The active cue's [`CaptionPlan`], kept between frames — a render resource of the player's
/// instance (`PlayerRender`). The wrap and the fit run when the cue or the geometry changes; a
/// frame is one cue lookup, one comparison and an index.
#[derive(Default)]
pub struct CaptionPager {
    slot: std::cell::RefCell<Option<(CaptionKey, CaptionPlan)>>,
}

impl CaptionPager {
    /// Look the active cue up, re-plan it if it is not the one planned, and return the page to
    /// show. `None` for no caption.
    fn refresh(&self, hud_up: bool) -> Option<usize> {
        let now_ns = plx_media::player::playpos_ns();
        // the SUBTITLE clock (the playhead less the viewer's timing offset) pages the cue; the cue
        // store applies the same subtraction inside `active_subtitle_span`
        let clock = plx_media::player::subtitle_clock_ns(now_ns);
        let (text, start, end) = plx_media::player::active_subtitle_span(now_ns)?;
        if text.trim().is_empty() {
            return None;
        }
        let (scale, baseline, floor) = caption_geometry(hud_up);
        let key = CaptionKey {
            start_ns: start,
            text,
            geometry: [scale.to_bits(), baseline.to_bits(), floor.to_bits()],
        };
        self.page_of(key, (scale, baseline, floor), end, clock)
    }

    /// The planning half of [`Self::refresh`], on a cue already looked up (the tests drive it).
    fn page_of(&self, key: CaptionKey, geometry: (f32, f32, f32), end: i64, clock: i64) -> Option<usize> {
        let (scale, baseline, floor) = geometry;
        let start = key.start_ns;
        let mut slot = self.slot.borrow_mut();
        if slot.as_ref().map(|(k, _)| k) != Some(&key) {
            let lines = caption_wrap(&key.text, scale);
            if lines.is_empty() {
                *slot = None;
                return None;
            }
            let plan = CaptionPlan::new(lines, scale, baseline, floor);
            *slot = Some((key, plan));
        }
        let (_, plan) = slot.as_ref()?;
        Some(caption_page_at(plan.pages(), start, end, clock))
    }
}

/// The caption's lines: each newline-separated segment word-wrapped at the viewer's size. Every
/// wrapped line is kept, so a long cue is never cut short; [`caption_layout`] fits the block.
fn caption_wrap(text: &str, scale: f32) -> Vec<String> {
    let max_chars = ((42.0 / scale).round() as usize).max(20);
    let mut lines: Vec<String> = Vec::new();
    for seg in text.split('\n') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        lines.extend(wrap(seg, max_chars));
    }
    lines
}

/// Where one caption block is drawn: `face` is the text size in px, `pitch` the line pitch, `top`
/// the y of the first line's box, `lines` how many lines the block holds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CaptionLayout {
    face: i32,
    pitch: f32,
    top: f32,
    lines: usize,
}

impl CaptionLayout {
    fn line_top(&self, i: usize) -> f32 {
        self.top + i as f32 * self.pitch
    }
}

/// The steps a caption block too tall for its area is stepped down through, as factors of the
/// viewer's own size. The first step is that size, so a block that fits is drawn as it always was.
const CAPTION_FIT_STEPS: [f32; 4] = [1.0, 0.875, 0.75, 0.625];

/// The lowest y a caption block may reach with the transport down: the screen's bottom margin.
const CAPTION_FLOOR_Y: f32 = SCR_H - plx_ui::consts::MARGIN_Y;

/// Place one caption block of `lines` lines at the viewer's `scale`, its bottom on `baseline` for a
/// block of up to [`CAPTION_ANCHOR_LINES`] lines. A taller block keeps that anchor's top and grows
/// DOWN, so it moves away from the Timing capsule rather than up into it. If the block would pass
/// `floor`, the size steps down through [`CAPTION_FIT_STEPS`] until it fits; at the smallest step it
/// is returned anyway (a cue that tall is paged by [`CaptionPlan`], not drawn from here).
fn caption_layout(lines: usize, scale: f32, baseline: f32, floor: f32) -> CaptionLayout {
    caption_fit(lines, scale, baseline, floor).0
}

/// [`caption_layout`] and whether the block fit: `false` means even the smallest step passes `floor`,
/// and the caller must page the cue ([`CaptionPlan`]).
fn caption_fit(lines: usize, scale: f32, baseline: f32, floor: f32) -> (CaptionLayout, bool) {
    let anchored = lines.min(CAPTION_ANCHOR_LINES) as f32;
    let top = baseline - anchored * CAPTION_LINE_PITCH * scale;
    let mut lay = CaptionLayout { face: 0, pitch: 0.0, top, lines };
    let mut fits = false;
    for &step in CAPTION_FIT_STEPS.iter() {
        let s = scale * step;
        // subtitle caption: media chrome, a documented carve-out from theme::size (see HUD_TITLE_SZ)
        lay = CaptionLayout { face: (36.0 * s).round() as i32, pitch: CAPTION_LINE_PITCH * s, top, lines };
        if top + lines as f32 * lay.pitch <= floor {
            fits = true;
            break;
        }
    }
    (lay, fits)
}

/// The ink both subtitle draws use this frame: the viewer's tone ([`plx_media::player::subtitle_tone`])
/// resolved on [`theme::SUBTITLE_INKS`]. A rung the table does not have is white, never a
/// neighbour.
pub fn subtitle_ink() -> [f32; 4] {
    subtitle_ink_for(plx_media::player::subtitle_tone())
}

fn subtitle_ink_for(tone: plx_plex::plex::session::SubtitleTone) -> [f32; 4] {
    theme::SUBTITLE_INKS
        .get(tone.index() as usize)
        .copied()
        .unwrap_or(theme::SUBTITLE_INKS[0])
}

/// The face-size multiplier for the viewer's Settings > Playback pick, applied to the plain-text
/// caption's 36 px base face ([`draw_subtitle_message`]) and its line pitch/wrap width together,
/// so a larger caption still wraps before it runs off either edge.
fn subtitle_size_scale(size: plx_plex::plex::session::SubtitleSize) -> f32 {
    use plx_plex::plex::session::SubtitleSize;
    match size {
        SubtitleSize::Small => 0.78,
        SubtitleSize::Medium => 1.0,
        SubtitleSize::Large => 1.25,
        SubtitleSize::ExtraLarge => 1.5,
    }
}

/// How far ABOVE [`SUB_BASE_Y`]/[`SUB_CEIL_Y`] the viewer's Position pick lifts the plain-text
/// caption ([`draw_subtitle_message`]) — Low is the baseline every build before this preference
/// drew, so it lifts nothing.
fn subtitle_position_lift(position: plx_plex::plex::session::SubtitlePosition) -> f32 {
    use plx_plex::plex::session::SubtitlePosition;
    match position {
        SubtitlePosition::Low => 0.0,
        SubtitlePosition::Middle => 160.0,
        SubtitlePosition::High => 320.0,
    }
}

/// Subtitle baseline: where the caption block sits with the transport DOWN, and the ceiling it
/// lifts to with the transport UP (clear of the scrubber, buttons and tabs). Both paths share
/// them: text lifts by moving its baseline, images lift by however much they overhang the
/// ceiling — otherwise a bottom-positioned PGS cue sits behind the scrim while the user seeks.
const SUB_BASE_Y: f32 = SCR_H - 100.0;
const SUB_CEIL_Y: f32 = SCR_H - 300.0;

/// The caption line count the block is bottom-anchored at, and the reason the Timing capsule's y is
/// fixed rather than following the caption block (`appkit::timing_capsule`, plan
/// `subtitle-menu-capsule` §4): a live cue's line count changes frame to frame, and a capsule that
/// tracked it would jump under a viewer's thumb mid-hold. A block taller than this keeps the
/// anchor's top and grows down ([`caption_layout`]); it never has lines cut — one too tall for
/// the area at the smallest size is shown a page at a time ([`CaptionPlan`]).
const CAPTION_ANCHOR_LINES: usize = 3;
/// The vertical pitch of one caption line in [`draw_subtitle_message`] — its 36 px face plus
/// leading.
const CAPTION_LINE_PITCH: f32 = 48.0;

/// Map a decoded image-subtitle rect from the stream's `cw`×`ch` authoring canvas onto the video
/// rect — which is always the full panel here (the video track is authored 1920×1080; see the
/// `docs/agent-reference.md`). A subtitle canvas is the picture's own storage grid, so this is exactly the
/// stretch the TV applies to the video itself: identity for 1080p PGS, 2.67×/2.25 for a 720×480
/// NTSC VobSub, 0.5× for a 4K PGS track. `cw`/`ch` of 0 means the decoder never declared a canvas
/// — then the rect is used 1:1, which is what this path did unconditionally before.
///
/// Deliberately NOT snapped to whole pixels: this is scaled content, and the crispness contract
/// (`docs/agent-reference.md`) snaps 1:1-texel content only.
fn sub_screen_rect(r: (i32, i32, i32, i32), cw: i32, ch: i32) -> Rect {
    let sx = if cw > 0 { SCR_W / cw as f32 } else { 1.0 };
    let sy = if ch > 0 { SCR_H / ch as f32 } else { 1.0 };
    Rect::new(
        r.0 as f32 * sx,
        r.1 as f32 * sy,
        r.2 as f32 * sx,
        r.3 as f32 * sy,
    )
}

/// How far UP to shift the rects of a display set that overhang the transport, so they clear it.
/// Nothing moves while the HUD is down, and the shift never exceeds the overhanging group's own
/// top, so a set too tall to lift is clamped rather than pushed off the top of the screen.
///
/// The lift is measured over the OVERHANGING rects only, and (per [`overhangs`]) applies only to
/// them. Measuring the whole set breaks the case multi-rect adds: a set carrying a sign at
/// y≈100 and dialogue at y≈950 would clamp the lift to 100 — sliding the sign for no reason
/// while leaving the dialogue still behind the scrim. Measuring the group and moving it as ONE
/// unit is also why this is not a per-rect lift: two-line dialogue is two rects with different
/// overhangs, and lifting each by its own would collapse them onto each other.
fn hud_lift<I: Iterator<Item = Rect>>(rects: I, hud_up: bool) -> f32 {
    if !hud_up {
        return 0.0;
    }
    let (mut top, mut bottom) = (f32::MAX, f32::MIN);
    for r in rects.filter(|r| overhangs(*r)) {
        top = top.min(r.y);
        bottom = bottom.max(r.y + r.h);
    }
    if top > bottom {
        return 0.0; // nothing overhangs
    }
    (bottom - SUB_CEIL_Y).max(0.0).min(top.max(0.0))
}

/// Does this rect reach below the transport ceiling? Only such rects take the lift.
fn overhangs(r: Rect) -> bool {
    r.y + r.h > SUB_CEIL_Y
}

/// Client-rendered IMAGE subtitles (PGS/VobSub): composite the active decoded display set over
/// the video, scaled from the subtitle stream's own authoring canvas into the video rect
/// (`sub_screen_rect`) and lifted clear of the transport when `hud_up` (`hud_lift`). EVERY rect
/// of the set is drawn — two-line dialogue and sign-plus-dialogue are authored as separate rects.
/// Caches one GL texture per rect and re-uploads only when the active cue changes (every few
/// seconds); the cached rects are the unlifted screen rects, so the lift can follow the HUD
/// frame by frame without re-uploading. Main-thread only (GL). This is the image counterpart to
/// draw_subtitles — a selected track is either text or image, so at most one of the two draws a
/// cue at a time.
pub fn draw_subtitle_bitmap(cache: &mut SubtitleBitmaps, hud_up: bool) {
    let set = &mut cache.set;
    let sel = plx_media::player::desired_sub_idx();
    if sel < 0 {
        for (t, _) in set.drain(..) {
            delete_tex(t);
        }
        cache.bytes = 0;
        cache.key = i64::MIN;
        cache.sel = i32::MIN; // reset BOTH halves of the key — neither should prop the other up
        return;
    }
    match plx_media::player::active_bitmap_key(plx_media::player::playpos_ns()) {
        None => cache.key = i64::MIN, // gap between cues — draw nothing this frame
        Some(k) => {
            if k != cache.key || sel != cache.sel {
                let (cw, ch, rects) = match plx_media::player::bitmap_by_key(k) {
                    Some(v) => v,
                    None => return, // cue evicted between key lookup and fetch
                };
                // retire the surplus first, then re-spec the ids we keep: upload_rgba reuses
                // a non-zero id, so a steady 1-rect stream never allocates a texture twice.
                for (t, _) in set.drain(rects.len().min(set.len())..) {
                    delete_tex(t);
                }
                for (i, r) in rects.iter().enumerate() {
                    let dst = sub_screen_rect((r.x, r.y, r.w, r.h), cw, ch);
                    let prev = set.get(i).map_or(0, |(t, _)| *t);
                    // the store holds the set indexed; expand it once, here, per cue change
                    let rgba = r.to_rgba();
                    let tex = upload_rgba(prev, r.w, r.h, rgba.as_ptr());
                    match set.get_mut(i) {
                        Some(slot) => *slot = (tex, dst),
                        None => set.push((tex, dst)),
                    }
                }
                // The whole set is re-uploaded above, so its residency is the new set's, not a
                // running total: `set.drain(rects.len()..)` already retired any surplus and every
                // surviving id was re-spec'd to a rect of THIS display set.
                cache.bytes = rects
                    .iter()
                    .map(|r| r.w.max(0) as usize * r.h.max(0) as usize * 4)
                    .sum();
                cache.key = k;
                cache.sel = sel;
            }
            let lift = hud_lift(set.iter().map(|(_, r)| *r), hud_up);
            // the tone TINTS a bitmap: the image shader multiplies by it, so a PGS cue keeps its
            // authored colours and outline and only gives up light (white is the identity)
            let tint = subtitle_ink();
            let p = Painter::root();
            for (tex, r) in set.iter() {
                let dy = if overhangs(*r) { lift } else { 0.0 };
                p.tex(*tex, Rect::new(r.x, r.y - dy, r.w, r.h), 0.0, tint);
            }
        }
    }
}

/// **The image-subtitle display set on screen, and the two halves of its cache key** — a RENDER
/// resource of the player's instance (`PlayerScreen::render`), not module state.
///
/// It was three `static mut`s inside [`draw_subtitle_bitmap`] until restructure phase 9. The key is
/// `(track, start_ns)` and not `start_ns` alone: two image tracks of the same file routinely start
/// a display set on the SAME pts, so keying on the timestamp alone leaves the outgoing track's
/// bitmap on screen after a switch between two image tracks.
///
/// The GL names in `set` are OWNED: [`Self::release`] deletes them, and the player's instance calls
/// it when the screen is unmounted — a static could never be told that a playback had ended.
pub struct SubtitleBitmaps {
    set: Vec<(c_uint, Rect)>,
    /// The SOURCE pixels behind `set`, in bytes — `sum(w * h * 4)` over the display set's rects as
    /// they were uploaded, which the screen rects in `set` cannot answer (those are where each
    /// bitmap LANDS, scaled into the video rect, not how large the texture is).
    ///
    /// It exists so `PlayerScreen` can state what it holds of the frame's render residency
    /// (`Screen::render_report`, §8.3 rule (c)) instead of the frame plan guessing. It moves with
    /// the textures in every path that uploads, retires or releases them.
    bytes: usize,
    key: i64,
    sel: i32,
}

impl Default for SubtitleBitmaps {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtitleBitmaps {
    pub const fn new() -> Self {
        Self {
            set: Vec::new(),
            bytes: 0,
            key: i64::MIN,
            sel: i32::MIN,
        }
    }
    /// What this display set holds of the frame's render residency: one texture per rect of the
    /// set, and the pixels behind them.
    pub fn render_report(&self) -> plx_ui::frame::RenderReport {
        plx_ui::frame::RenderReport {
            textures: self.set.len() as u32,
            bytes: self.bytes,
        }
    }
    /// A display set of the given SOURCE pixel sizes, without GL — the seam the residency tests
    /// use, since a host test uploads nothing (`gfx::delete_tex` no-ops under `cfg(test)`, so the
    /// stand-in ids are safe to release). The ids are stand-ins; only the count and the bytes are
    /// what anything reads off this.
    #[cfg(any(test, feature = "test-support"))]
    pub fn stub(sizes: &[(i32, i32)]) -> Self {
        Self {
            set: sizes
                .iter()
                .enumerate()
                .map(|(i, _)| (i as c_uint + 1, Rect::new(0.0, 0.0, 0.0, 0.0)))
                .collect(),
            bytes: sizes.iter().map(|(w, h)| *w as usize * *h as usize * 4).sum(),
            key: 1,
            sel: 0,
        }
    }
    /// Retire every uploaded texture. Main thread only (GL), like every other call in this module.
    pub fn release(&mut self) {
        for (t, _) in self.set.drain(..) {
            delete_tex(t);
        }
        self.bytes = 0;
        self.key = i64::MIN;
        self.sel = i32::MIN;
    }
}

// ---- geometry ----
const BTN_S: f32 = 64.0;
const BTN_Y: f32 = SCR_H - 288.0;

/// The top of the band the player's controls stood in, kept as the line the diagnostics panel must
/// stay above (`app::diagnostics`' layout tests).
pub const CTRL_Y: f32 = BTN_Y;

/// The bottom scrim's height — the transport's dark ground, transparent at its top edge and
/// `theme::scrim_black(0.86)` at the panel bottom. Named because the read-out's placement is graded
/// against it (see [`readout_frame`] and its test) rather than against a second hand-typed 470.
const SCRIM_H: f32 = 470.0;

/// Which surface owns the "the pipeline is working" signal. There is exactly ONE, ever.
///
/// Two of them used to fire together for the whole of every load and every seek: the centred
/// [`StatusOverlay`] was gated on `is_busy() && frames() == 0`, but `pump` zeroes `frames` as part
/// of APPLYING a seek — so the guard that was meant to say "we have never had a picture" was true
/// for the entire seek, and the transport's own spinner (gated on plain `is_busy()`) was up beside
/// it. Two indicators for one fact read as two facts, which is exactly how it was reported.
///
/// It carries the caption rather than letting each draw re-read `state().caption()`: `Resolving` is
/// derived from `route::play_pending()`, an off-thread flag, so a second read could disagree with
/// the kind this value was chosen for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Busy {
    /// nobody — frames are on the panel, or there is no session
    None,
    /// the centred [`StatusOverlay`] at [`readout_frame`], carrying its treatment + its caption
    Readout(StatusKind, &'static core::ffi::CStr),
    /// the inline spinner beside the elapsed clock
    Transport,
}

/// PURE: which surface owns the busy signal, given the playback state and whether this SESSION has
/// ever presented a frame (`player::seen_frame` — NOT `frames() > 0`, see [`Busy`]).
///
/// **The rule: whichever surface owns the thing being waited for.** No picture yet → the wait is
/// about the whole panel, so the whole panel says so. A picture is up and only the POSITION is in
/// flight → the wait is about the playhead, so the mark goes on the playhead, which the HUD has
/// already frozen at the seek target. `Error` → the centred read-out either way; there is no
/// picture and no position, only a message.
///
/// It takes `seen_frame` rather than the state alone because `Buffering` is genuinely ambiguous —
/// it is both the cold-start tail AND the 1-3 frame tail of every seek (between prime→Play, where
/// `engine` clears `seeking`, and the first presented frame). Keyed on the state alone, one of
/// those two flashes the wrong surface every time.
pub fn busy_surface(ps: &plx_media::route::PlaybackSession, st: plx_media::player::PlaybackState, seen_frame: bool) -> Busy {
    busy_surface_with(ps, st, seen_frame, plx_media::player::audio_switching())
}

/// [`busy_surface`] with the audio-switch fact given: while the rebuild loading is the one the
/// viewer's audio-track pick asked for, the read-out says "Switching audio…" instead of
/// "Buffering…" (`player::busy_caption_with`). Any other rebuild keeps the plain caption.
pub fn busy_surface_with(
    ps: &plx_media::route::PlaybackSession,
    st: plx_media::player::PlaybackState,
    seen_frame: bool,
    audio_switch: bool,
) -> Busy {
    use plx_media::player::PlaybackState;
    match st {
        // not `st.caption()`: the Error caption is shaped by WHY (an audio-only stream names the
        // server; see `player::error_shape`), which a method on the bare state cannot know.
        PlaybackState::Error => Busy::Readout(StatusKind::Failed, plx_media::player::error_caption(ps)),
        s if s.is_busy() && !seen_frame => Busy::Readout(StatusKind::Working, plx_media::player::busy_caption_with(st, audio_switch)),
        s if s.is_busy() => Busy::Transport,
        _ => Busy::None,
    }
}

/// PURE: does the read-out own the WHOLE frame, so [`draw_hud`] draws nothing at all?
///
/// **Only a failure**, and the asymmetry is the point. A `Failed` read-out is terminal: there is no
/// picture and no position, so every piece of transport under it is a lie — `Player Screen.dc.html`
/// hides both the transport (`hudDisplay:none`) and the bottom tabs (`tabsDisplay:none`) on that
/// variant. A `Working` read-out is the opposite: the pipeline is mid-flight, the position is real,
/// and the transport is what the user reads the moment the first frame lands. Hiding it there would
/// blank the HUD through every cold start and every pre-roll — which is why this asks the KIND and
/// not merely "is a read-out up".
pub fn readout_owns_frame(busy: Busy) -> bool {
    matches!(busy, Busy::Readout(StatusKind::Failed, _))
}

/// The same question for `crate::screens::player::PlayerScreen::handle_key`, which asks it
/// FIRST, before any transport arm: is the transport (and every panel over it) absent from the
/// frame right now?
///
/// It was `app/run.rs`'s own guard, high in that ladder's chain, until restructure phase 12
/// (PX-PLAYER) retired the loop's player input path — so the precedence that used to be an arm's
/// HEIGHT is one condition at the top of one function.
///
/// A control that is not drawn must not be activatable — the rule `PlayerScreen::record_stops` keeps for
/// the pointer, at the row's own altitude. Without this the failure was hidden but
/// still drivable: `start_playback` stamps a ~4.5 s HUD linger on the way in, so a `/decision`
/// refusal lands with `hud_visible` true and focus parked on the scrubber, and two blind presses
/// (DOWN, OK) opened the Info card over the read-out from a tab row nothing had painted.
///
/// It resamples [`busy`] rather than taking one, because the event loop runs before the frame's
/// single resolve exists; both reads are of the same main-thread state within one iteration.
pub fn transport_hidden(ps: &plx_media::route::PlaybackSession) -> bool {
    readout_owns_frame(busy(ps))
}

/// Sample the live globals ONCE and resolve the owner. Call this once per frame and pass the result
/// to both [`draw_hud`] and [`draw_readout`] — the same discipline [`slot`] keeps, and for the same
/// reason: two independent derivations of one three-way choice is how the two indicators drifted
/// apart in the first place.
pub fn busy(ps: &plx_media::route::PlaybackSession) -> Busy {
    // dev: `/tmp/dispatcharrtv-failtest` forces the failure read-out — the other half of
    // `player::failtest_arm`, which shapes WHICH failure. It is forced HERE, on the one impure
    // sampler, rather than in `player::state()`: the pump acts on that state, and a dev switch
    // that made the engine believe it had failed would be testing a different thing than the
    // screen. `busy_surface` stays pure and ungated, so what draws is still the real rule.
    if plx_base::devtrig::flag("failtest") {
        return Busy::Readout(StatusKind::Failed, plx_media::player::error_caption(ps));
    }
    busy_surface(ps, plx_media::player::state(ps), plx_media::player::seen_frame())
}

// ---- the transport STATE READ-OUT (the glyph slot just past the elapsed clock) ---------------
//
// One slot, one glyph, four states. It is a READ-OUT and not an action toggle — there is no
// transport button row in this app and a standing owner decision forbids adding one, so nothing
// here is focusable, hit-tested or in `BTN_N`. The glyphs are the design system's transport
// family (`Icon::Rewind` / `Icon::Pause` / `Icon::FastForward`, all in one 14-unit band, with
// `Icon::Play` swapping in for `Pause`), which is what keeps the slot from shifting optical
// weight as the state flips under a running clock.

/// The read-out's frame: **the whole panel**, because the wait is about the whole picture.
///
/// It is deliberately NOT carved down to dodge the transport. The block [`StatusOverlay`] builds is
/// only ~91 px tall, so centred on the panel it spans y≈482…573 — clear of the scrim's top edge at
/// `SCR_H - SCRIM_H` (610) and of the transport's highest ink (the Up Next still at ≈597). The
/// carve-out this replaces (`SCR_H - 340`) centred it at y=370, visibly high, and bought nothing:
/// at 370 the block was already 200 px above a scrim that is fully TRANSPARENT at its top edge
/// anyway. Every "sit above the transport" framing pushes it UP — the un-scrimmed band's own centre
/// is 305 — which is the opposite of the fix the report asked for.
fn readout_frame() -> Rect {
    Rect::FULL
}

/// The load / seek / failure read-out. Drawn EVERY player frame, independent of the transport's
/// auto-hide, because its visibility is the PLAYBACK STATE and not the HUD timer.
///
/// It used to live inside [`draw_hud`], and two things came of that. Historically: `PlaybackState`
/// was published every frame and NOTHING read it, so the initial load drew a live-looking transport
/// at 0:00 / -0:00 and a dead producer drew a fully black screen with no message and no hint that
/// BACK is the way out. Then, once it did read it: in `Error` — which is deliberately not
/// `is_busy()`, hence not covered by `app.rs`'s `|| player::loading()` HUD pin — "Playback failed"
/// disappeared 4.5 s in with the HUD linger, leaving exactly the silent black screen the read-out
/// exists to prevent. A read-out is not transport chrome.
pub fn draw_readout(
    ps: &plx_media::route::PlaybackSession,
    busy: Busy,
    now: u32,
    failure_focus: usize,
    measure: &dyn plx_machine::machine::Measure,
) {
    let Busy::Readout(kind, caption) = busy else {
        return;
    };
    if kind == StatusKind::Failed {
        // The failure is a page read-out (`FailureReadout`): the shared page placement, a
        // constant verdict, one reason, a row of controls. The caption is not drawn here: the
        // verdict line is constant by design, so a viewer who has seen it once recognises it
        // before reading, and the caption's suffix is re-derived as the reason.
        draw_failed_readout(ps, Painter::root(), failure_focus, measure);
        return;
    }
    StatusOverlay::new(readout_frame(), caption, kind)
        .phase(now)
        .draw(&hud_env(), Painter::root());
}

// ---- the failure read-out -------------------------------------------------------------------
//
// **The shared page read-out, not a layout of its own.** A failed playback is a page that failed,
// exactly as Home's, a Library section's and the sign-in read-out are, so it is the same
// component at the same placement: `StatusOverlay::page` hangs the verdict from
// `FULL_ANCHOR_TOP` (540) with its 112 px glyph 44 px above it, the reason in the reserved
// two-line slot, and a row of real controls stacked directly under it. Until 2026-09-28 this was
// a bespoke composition (a 96 px glyph, a primary-ink verdict, three centred prose lines and key
// caps drawn into the sentences, a support line at the same weight as the reason); the owner's
// photograph of it is why it is not any more.
//
// What each block says is decided in ONE place per question:
//  * the controls — `player::failure_actions`, the table that offers an action only if it can
//    change the outcome;
//  * the words — `player::error_now`'s `ErrorShape`, resolved to one reason sentence and at most
//    one quiet note here (`FailureReadout::of`);
//  * the support facts (version · firmware · set · failure code) — one dim footer line at the
//    foot of the screen, always; there is no *Details* control.

/// The support line's cap top this far above the canvas bottom — inside the safe area's
/// bottom margin band's upper edge, clear of the row by construction, and
/// `the_diagnostics_footer_sits_below_the_row_inside_the_safe_bottom_band` fails if a taller row or
/// note ever meets it.
const FOOTER_TOP: f32 = SCR_H - plx_ui::consts::MARGIN_Y - theme::space::XL;

/// Everything the failure read-out draws, resolved once from the live failure. The draw, the
/// pointer stops and the key ladder all build it the same way, so the row a click lands on is the
/// row that was drawn.
pub struct FailureReadout {
    glyph: plx_ui::icons::Icon,
    reason: CString,
    note: Option<CString>,
    actions: Vec<plx_media::player::FailureAction>,
    footer: CString,
}

/// A control's label. Sentence case, as every `StatusOverlay` row is (the sign-in read-out's *Try
/// again*): these are capsule buttons, not the ALL-CAPS clickable TEXT marks.
pub fn failure_action_label(a: plx_media::player::FailureAction) -> &'static std::ffi::CStr {
    use plx_platform::i18n::msg;
    use plx_media::player::FailureAction as A;
    match a {
        A::TryAgain => msg::browse_action_retry_c(),
        A::Repair => msg::widgets_repair_action_c(),
        A::Back => msg::settings_back_c(),
    }
}

/// The glyph above the verdict, by cause — read off the same `FailureKind` the words come from, so
/// the two cannot disagree (the rule `StatusOverlay::page` states for its callers).
fn failure_glyph(kind: plx_media::player::FailureKind) -> plx_ui::icons::Icon {
    use plx_media::player::FailureKind as K;
    use plx_ui::icons::Icon;
    match kind {
        K::JailMissingRtkmem => Icon::LockBadgeAlert,
        K::LoadTimeout => Icon::ClockBadgeAlert,
        _ => Icon::Alert,
    }
}

impl FailureReadout {
    /// The read-out for the failure on screen now (main thread).
    pub fn now(ps: &plx_media::route::PlaybackSession) -> Self {
        let mut e = plx_media::player::error_now(ps);
        if e.kind == plx_media::player::FailureKind::JailMissingRtkmem {
            use plx_platform::tv::sandbox::State;
            match ps.repair_status {
                State::Idle => {}
                State::Running => {
                    e.readout = plx_platform::i18n::msg::widgets_repair_running();
                    e.detail = plx_platform::i18n::msg::widgets_repair_wait().into();
                }
                State::Repaired => {
                    e.readout = plx_platform::i18n::msg::widgets_repair_completed();
                    e.detail = plx_platform::i18n::msg::widgets_repair_reopen().into();
                }
                State::Failed(reason) => {
                    e.readout = plx_platform::i18n::msg::widgets_repair_failed();
                    e.detail = repair_failure_message(reason).into();
                }
            }
        }
        let actions = plx_media::player::failure_actions(e.kind, plx_media::player::failure_context(ps));
        Self::of(&e, actions, plx_media::player::support_line(e.kind))
    }

    /// PURE: the read-out for one failure shape.
    ///
    /// **Say it once.** The reason is ONE sentence: what happened and why. The shape's `detail`
    /// goes to exactly one place, the note under the row (the sandbox's remedy, a repair's
    /// progress). The support facts are always the footer.
    pub fn of(
        e: &plx_media::player::ErrorShape,
        actions: Vec<plx_media::player::FailureAction>,
        support: String,
    ) -> Self {
        let reason: &str = e.readout;
        let note: Option<String> = (!e.detail.is_empty()).then(|| e.detail.to_string());
        Self {
            glyph: failure_glyph(e.kind),
            reason: CString::new(reason).unwrap_or_default(),
            note: note.and_then(|n| CString::new(n).ok()),
            actions,
            footer: CString::new(support).unwrap_or_default(),
        }
    }

    /// The row, in draw order.
    pub fn actions(&self) -> &[plx_media::player::FailureAction] {
        &self.actions
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reason(&self) -> &str {
        self.reason.to_str().unwrap_or("")
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref().and_then(|n| n.to_str().ok())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn footer(&self) -> &str {
        self.footer.to_str().unwrap_or("")
    }

    fn overlay(&self, labels: &[&'static std::ffi::CStr], focus: Option<usize>) -> StatusOverlay<'_> {
        StatusOverlay::new(Rect::FULL, plx_platform::i18n::msg::widgets_status_failed_c(), StatusKind::Failed)
            .page(self.glyph)
            .reason(&self.reason)
            .row(labels)
            .note(self.note.as_deref())
            .focus(focus)
    }

    fn labels(&self) -> Vec<&'static std::ffi::CStr> {
        self.actions.iter().map(|a| failure_action_label(*a)).collect()
    }

    /// Each control's rect, by row index — the geometry the draw uses.
    pub fn frames(&self, measure: &dyn plx_machine::machine::Measure) -> [Option<Rect>; plx_ui::widgets::STATUS_ROW_MAX] {
        let labels = self.labels();
        self.overlay(&labels, None).row_frames_measured(measure)
    }

    fn draw(&self, p: Painter, focus: usize, measure: &dyn plx_machine::machine::Measure) {
        let labels = self.labels();
        self.overlay(&labels, Some(focus.min(labels.len().saturating_sub(1))))
            .draw_measured(&hud_env(), p, measure);
        plx_ui::text_view::TextView::new(self.footer.to_str().unwrap_or(""), theme::size::CAPTION, theme::TEXT_TERTIARY)
            .h(plx_ui::label::HAlign::Center)
            .max_lines(1)
            // the full safe width, not the reason's: the failure code is the line's LAST field
            // and the one support needs, so it must not be the part an ellipsis eats
            .draw(p, Rect::new(plx_ui::consts::MARGIN_X, FOOTER_TOP, SCR_W - 2.0 * plx_ui::consts::MARGIN_X, 0.0));
    }
}

// ---- element addresses for the Engine's hit map / `Focusable` groups (restructure phase 12) --
/// `+ 0..actions().len()` — the failure read-out's row, one element per
/// [`plx_media::player::FailureAction`] in [`FailureReadout::actions`] order: the only controls a
/// FAILED playback draws.
pub const ELEM_FAILURE_BASE: u32 = 30;
/// One past the failure row's last possible element.
pub const ELEM_FAILURE_END: u32 = ELEM_FAILURE_BASE + plx_ui::widgets::STATUS_ROW_MAX as u32;

/// Is the failure read-out's row ON SCREEN — a failure owns the frame. The row always holds at
/// least *Back*, so a failure always has something to press.
pub fn failure_row_drawn(busy: Busy) -> bool {
    readout_owns_frame(busy)
}

/// Product copy is resolved here; the worker's technical error identity stays unchanged.
fn repair_failure_message(reason: plx_platform::tv::sandbox::Failure) -> &'static str {
    use plx_platform::tv::sandbox::Failure;
    match reason {
        Failure::StartFailed => plx_platform::i18n::msg::widgets_repair_start_failed(),
        Failure::HbcUnavailable => plx_platform::i18n::msg::widgets_repair_hbc_unavailable(),
        Failure::NotRoot => plx_platform::i18n::msg::widgets_repair_not_root(),
        Failure::CommandFailed => plx_platform::i18n::msg::widgets_repair_command_failed(),
        Failure::Timeout => plx_platform::i18n::msg::widgets_repair_timeout(),
        Failure::Unreadable => plx_platform::i18n::msg::widgets_repair_unreadable(),
        Failure::Unsupported => plx_platform::i18n::msg::widgets_repair_unsupported(),
    }
}

fn draw_failed_readout(
    ps: &plx_media::route::PlaybackSession,
    p: Painter,
    focus: usize,
    measure: &dyn plx_machine::machine::Measure,
) {
    // The GROUND, first: a full-bleed opaque black, one quad. Without it the read-out stood on
    // whatever the video plane happened to be holding: `app.rs` clears the graphics plane to alpha
    // 0 on the player route, and the transport is not drawn at all under a failure. "The plane is
    // black by the time an error is up" is usually true and not always — `busy_surface` resolves
    // Error to Failed for BOTH values of `seen_frame`, i.e. a mid-playback failure over a held
    // frame is a state the code contemplates.
    p.rrect(Rect::FULL, 0.0, 0.0, theme::scrim_black(1.0));
    FailureReadout::now(ps).draw(p, focus, measure);
}

// (`scrub_hit` stood here — the scrub band's own `(cx,cy) -> Option<frac>` predicate, `icon_hit`'s
// twin for the bar. Restructure phase 12 (PX-PLAYER) retired it: the band is a registered `Stop`
// now ([`scrub_hit_rect`], the SAME rectangle it tested), so the shared hit map answers "is the
// pointer on the bar" and the screen asks only "where along it" — which is `scrub_frac_x` below,
// the half that was always x-only. Deleting it is also what makes `ci/check-deps.sh`'s `hittest`
// gate meaningful rather than merely satisfied: there is no raw hit-tester left for `app/` to
// call.)

// ---- the rail carries NO marks ------------------------------------------------------------
// Nothing is drawn on the scrubber but the track, the watched fill and the playhead. Two kinds of
// mark have been tried here and both were removed, for the same reason and by the same judgement:
//
//   * chapter-boundary ticks — the rail reads cleaner as one continuous bar. The chapter LIST is
//     the affordance (`appkit/chapters_panel.rs`, off `metadata::playing_chapters()`).
//   * intro/credits segment bands — removed 2026-08-04. They were `RAIL_MARKER`, white at 0.42
//     against a 0.20 track, so a credits band on a feature film was a bright unlabelled patch
//     floating near the right end of an otherwise empty rail. Reviewed cold on a screenshot it
//     read as a RENDERING ARTIFACT, not as information, which is a complete failure of the thing.
//
// Neither loses anything: the Skip Intro / Skip Credits pill (`appkit/skip_pill.rs`) is driven from
// the very same `metadata::playing_markers()` and appears exactly when a marker is reachable, so
// the band was decoration duplicating a control that already announces itself. Do not re-add
// either as a "cheap win" — the marker data is already in memory, which is precisely what makes
// drawing it tempting and still wrong.

// ---- scrub tuning, shared by the player HUD's scrubber and the trailer transport's own --------

/// The transport's dark ground: transparent at its top edge, `theme::scrim_black(0.86)` at the
/// panel bottom, [`SCRIM_H`] tall. Drawn under every transport, the player's and the trailer's.
pub fn draw_scrim(p: Painter) {
    p.rect(
        Rect::new(0.0, SCR_H - SCRIM_H, SCR_W, SCRIM_H),
        0.0,
        theme::scrim_black(0.0),
        theme::scrim_black(0.86),
        0.0,
    );
}

/// **The transport's outermost drawn chrome, for the overscan audit**
/// ([`plx_ui::consts::SAFE`]). Private geometry, so the rects are built where they are drawn
/// rather than restated in the module that grades them.
///
/// The bottom tab row is measured at its widest — `Info` + `Chapters`, the pair an item with
/// chapters draws — because a pill's width is the label's and the audit has to grade the widest
/// state a screen can be in, not the one a screenshot happened to catch. That measurement needs a
/// font, which the host suite cannot link, so it is bounded by the row's OWN height instead: what
/// the audit is about here is the row's left edge and its BOTTOM, and both are font-independent.
#[cfg(any(test, feature = "test-support"))]
pub fn overscan_rects(out: &mut Vec<(&'static str, Rect)>) {
    // The failure page's footer (its failure code) spans the safe width at `FOOTER_TOP`, and the
    // bottom caption line rests on `SUB_BASE_Y`: the two lowest things this module prints.
    let safe_w = SCR_W - 2.0 * plx_ui::consts::MARGIN_X;
    out.push(("player failure footer", Rect::new(plx_ui::consts::MARGIN_X, FOOTER_TOP, safe_w, 0.0)));
    out.push(("player caption baseline", Rect::new(plx_ui::consts::MARGIN_X, SUB_BASE_Y - CAPTION_LINE_PITCH, safe_w, CAPTION_LINE_PITCH)));
}

#[cfg(test)]
mod tests {

    /// #585: a cue of authored line breaks that each wrap again ran past three lines and the tail
    /// was silently dropped. Every line of an ordinary long cue must survive.
    #[test]
    fn a_long_cue_keeps_every_line_past_three() {
        let cue = "First authored line that is long enough to wrap onto a second row\n\
                   Second authored line that is long enough to wrap onto a second row\n\
                   Third authored line that is long enough to wrap onto a second row";
        let lines = super::caption_wrap(cue, 1.0);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines.last().unwrap().ends_with("second row"), "{lines:?}");
    }

    use super::*;


    /// **The image-subtitle display set is a render, and it says how much of one** (§8.3 rule (c)):
    /// one texture per rect, the SOURCE pixels behind them, and nothing left claimed once the set
    /// is released. The bytes cannot be derived from the screen rects the cache keeps — those are
    /// where each bitmap lands, scaled into the video rect — which is why the count is carried.
    #[test]
    fn the_image_subtitle_set_reports_its_own_textures_and_bytes() {
        let mut subs = SubtitleBitmaps::stub(&[(720, 120), (300, 80)]);
        let r = subs.render_report();
        assert_eq!(r.textures, 2, "a two-rect display set is two textures");
        assert_eq!(r.bytes, (720 * 120 + 300 * 80) * 4);
        subs.release();
        assert_eq!(
            subs.render_report(),
            plx_ui::frame::RenderReport::NONE,
            "a released set holds nothing and must stop claiming bytes"
        );
    }

    // A rail 1000px wide over a 1000ms item: 1px per ms, so an offset reads straight off the x.
    const SX: f32 = 100.0;
    const SW: f32 = 1000.0;
    const DUR: i64 = 1_000;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }
    fn assert_rect(r: Rect, x: f32, y: f32, w: f32, h: f32) {
        assert!(
            close(r.x, x) && close(r.y, y) && close(r.w, w) && close(r.h, h),
            "got ({}, {}, {}, {}), want ({x}, {y}, {w}, {h})",
            r.x,
            r.y,
            r.w,
            r.h
        );
    }

    /// **Every tone has an ink, the first is the white every earlier build drew, and each rung
    /// under it is strictly darker** — the ladder's whole promise is "down is dimmer", and the
    /// table is indexed by position, so a rung added to `SubtitleTone::LADDER` without an ink
    /// here would silently draw white. Opaque and achromatic throughout: a tone gives up light,
    /// never coverage (the outline depends on it) and never hue (it tints image subtitles).
    #[test]
    fn the_subtitle_tones_are_a_strictly_darkening_opaque_grey_ladder_from_white() {
        use plx_plex::plex::session::SubtitleTone;
        assert_eq!(theme::SUBTITLE_INKS.len(), SubtitleTone::LADDER.len());
        assert_eq!(subtitle_ink_for(SubtitleTone::White), [1.0, 1.0, 1.0, 1.0]);
        let mut prev = f32::MAX;
        for tone in SubtitleTone::LADDER {
            let ink = subtitle_ink_for(tone);
            assert!(ink[0] == ink[1] && ink[1] == ink[2], "{tone:?} has a hue");
            assert_eq!(ink[3], 1.0, "{tone:?} is not opaque");
            assert!(ink[0] < prev, "{tone:?} is not darker than the rung above it");
            prev = ink[0];
        }
        // the darkest rung still has to be READ over the 0.85 black outline it is drawn on
        assert!(prev > 0.25, "the darkest tone has sunk into its own outline");
    }

    /// A 1080p-authored PGS rect must land EXACTLY where it always did — the scale is identity,
    /// not merely close to it, or every Blu-ray subtitle in the library moves a pixel.
    #[test]
    fn image_sub_1080p_canvas_is_identity() {
        assert_rect(
            sub_screen_rect((300, 900, 1320, 96), 1920, 1080),
            300.0,
            900.0,
            1320.0,
            96.0,
        );
    }

    /// The bug this unit exists for: a DVD VobSub rip is authored on a 720×480 canvas, so drawn
    /// 1:1 it is a postage stamp covering the top-left third of the panel. Scaled from its own
    /// canvas it spans the picture and sits at the bottom, exactly like the PGS case above.
    #[test]
    fn image_sub_dvd_canvas_fills_the_panel() {
        // a full-width, near-bottom VobSub line
        assert_rect(
            sub_screen_rect((0, 400, 720, 60), 720, 480),
            0.0,
            900.0,
            1920.0,
            135.0,
        );
        // and an inset one keeps its position within the picture
        let r = sub_screen_rect((180, 240, 360, 48), 720, 480);
        assert_rect(r, 480.0, 540.0, 960.0, 108.0);
        assert!(
            close(r.x + r.w * 0.5, SCR_W * 0.5),
            "a canvas-centred rect stays centred"
        );
    }

    /// PAL (720×576) and 4K PGS (3840×2160) are the other two canvases in the wild; the 4K one
    /// scales DOWN, which the old 1:1 path drew at double size and half off-screen.
    #[test]
    fn image_sub_pal_and_4k_canvases() {
        assert_rect(
            sub_screen_rect((0, 480, 720, 72), 720, 576),
            0.0,
            900.0,
            1920.0,
            135.0,
        );
        assert_rect(
            sub_screen_rect((600, 1800, 2640, 192), 3840, 2160),
            300.0,
            900.0,
            1320.0,
            96.0,
        );
    }

    /// A decoder that never declares a canvas (0) must fall back to 1:1 — the behaviour of the
    /// whole path before this change — rather than divide by zero or blank the cue.
    #[test]
    fn image_sub_unknown_canvas_stays_1to1() {
        assert_rect(
            sub_screen_rect((300, 900, 1320, 96), 0, 0),
            300.0,
            900.0,
            1320.0,
            96.0,
        );
        assert_rect(
            sub_screen_rect((300, 900, 1320, 96), -1, 1080),
            300.0,
            900.0,
            1320.0,
            96.0,
        );
    }

    /// The HUD lift: image cues rise just enough to clear the transport, and only then.
    #[test]
    fn image_sub_lifts_only_over_the_transport() {
        let dialogue = Rect::new(300.0, 900.0, 1320.0, 135.0); // bottom 1035, overhangs
                                                               // transport down: nothing moves, ever
        assert_eq!(hud_lift([dialogue].into_iter(), false), 0.0);
        // transport up: a bottom-anchored cue rises exactly its overhang, landing ON the ceiling
        let lift = hud_lift([dialogue].into_iter(), true);
        assert!(
            close(1035.0 - lift, SUB_CEIL_Y),
            "lifted bottom should sit at the ceiling"
        );
        // a sign at the top of frame already clears it and must stay put
        let sign = Rect::new(100.0, 100.0, 200.0, 100.0);
        assert_eq!(hud_lift([sign].into_iter(), true), 0.0);
        assert!(!overhangs(sign) && overhangs(dialogue));
        // a set too tall to lift is clamped instead of being pushed off the top of the screen
        assert_eq!(
            hud_lift([Rect::new(0.0, 0.0, 1920.0, SCR_H)].into_iter(), true),
            0.0
        );
        assert_eq!(
            hud_lift(
                [Rect::new(0.0, 50.0, 1920.0, SCR_H - 50.0)].into_iter(),
                true
            ),
            50.0
        );
    }

    /// The multi-rect case the lift has to get right, and the reason it is measured over the
    /// OVERHANGING rects rather than the whole set: a sign plus dialogue must lift the dialogue
    /// clear WITHOUT dragging the sign (whose top would otherwise clamp the whole shift), and
    /// two-line dialogue must move as one unit or the two lines collapse onto each other.
    #[test]
    fn image_sub_lift_spans_only_the_rects_that_overhang() {
        let sign = Rect::new(100.0, 100.0, 200.0, 100.0);
        let dialogue = Rect::new(300.0, 900.0, 1320.0, 135.0);
        let lift = hud_lift([sign, dialogue].into_iter(), true);
        assert!(
            close(lift, 1035.0 - SUB_CEIL_Y),
            "the sign's top must not clamp the lift"
        );
        assert!(
            close(dialogue.y + dialogue.h - lift, SUB_CEIL_Y),
            "dialogue clears the transport"
        );
        assert!(!overhangs(sign), "and the sign is not shifted at all");

        // two-line dialogue: different overhangs, ONE shift, so the gap between them survives
        let l1 = Rect::new(300.0, 900.0, 1320.0, 50.0); // bottom 950
        let l2 = Rect::new(300.0, 960.0, 1320.0, 50.0); // bottom 1010
        let two = hud_lift([l1, l2].into_iter(), true);
        assert!(
            close(two, 1010.0 - SUB_CEIL_Y),
            "measured over the group, not per rect"
        );
        assert!(overhangs(l1) && overhangs(l2), "both take the same shift");
        assert!(
            close((l2.y - two) - (l1.y + l1.h - two), 10.0),
            "their 10px gap is preserved"
        );
    }

    // ---- who owns the "the pipeline is working" signal -----------------------------------------

    use plx_media::player::PlaybackState as S;

    /// Every state the enum has, so a state added later cannot quietly escape the table below.
    const ALL_STATES: [S; 7] = [
        S::Idle,
        S::Resolving,
        S::Connecting,
        S::Buffering,
        S::Seeking,
        S::Playing,
        S::Error,
    ];

    /// The whole point of routing both surfaces through one function: for any state, and either
    /// answer to "has this session shown a picture", there is exactly ONE indicator — never two
    /// (the reported bug) and never none while the pipeline is working.
    ///
    /// Driven off `is_busy()` itself rather than a hand-copied list, so a FUTURE busy state cannot
    /// silently lose its indicator; the membership guard underneath pins what `is_busy()` means, so
    /// the coverage claim can't be satisfied by quietly narrowing it.
    #[test]
    fn exactly_one_surface_owns_the_busy_signal() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        for st in ALL_STATES {
            for seen in [false, true] {
                let b = busy_surface(&ps, st, seen);
                if st.is_busy() || st == S::Error {
                    assert_ne!(b, Busy::None, "{st:?}/seen={seen} lost its indicator");
                } else {
                    assert_eq!(b, Busy::None, "{st:?}/seen={seen} must show nothing");
                }
            }
        }
        for st in ALL_STATES {
            let want = matches!(st, S::Resolving | S::Connecting | S::Buffering | S::Seeking);
            assert_eq!(st.is_busy(), want, "{st:?} changed sides of is_busy()");
        }
    }

    /// The regression, named after the bug. The shipped guard was `is_busy() && frames() == 0`, and
    /// `pump` zeroes `frames` as part of APPLYING a seek — so the centred read-out fired on every
    /// seek, on top of the transport's own spinner. Keyed on the SESSION bit instead, a seek over a
    /// live picture is the transport's alone.
    #[test]
    fn a_seek_over_a_live_picture_belongs_to_the_transport() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        assert_eq!(busy_surface(&ps, S::Seeking, true), Busy::Transport);
        assert!(!matches!(busy_surface(&ps, S::Seeking, true), Busy::Readout(..)));
    }

    /// The 1-3 frame window between `engine`'s prime→Play clear of `seeking` and the first
    /// presented frame, which the pump publishes as `Buffering`. Keyed on the state alone this
    /// flashes the centred block at the END of every seek; it is precisely why the rule takes
    /// `seen_frame`, and without the assertion someone will "simplify" it back.
    #[test]
    fn the_post_seek_buffering_tail_does_not_flash_the_centre_readout() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        assert_eq!(busy_surface(&ps, S::Buffering, true), Busy::Transport);
        assert_eq!(
            busy_surface(&ps, S::Resolving, true),
            Busy::Transport,
            "auto-advance over a live picture"
        );
        assert_eq!(busy_surface(&ps, S::Connecting, true), Busy::Transport);
    }

    /// **An audio-track pick's reload says "Switching audio…"; every other wait keeps its caption.**
    /// Only `Buffering` is renamed (the state a rebuild's load tail reads); a cold start's
    /// Preparing/Connecting and a seek stay as they were.
    #[test]
    fn an_audio_pick_rebuild_reads_switching_audio_and_nothing_else_does() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        assert_eq!(
            busy_surface_with(&ps, S::Buffering, false, true),
            Busy::Readout(StatusKind::Working, c"Switching audio\u{2026}")
        );
        assert_eq!(
            busy_surface_with(&ps, S::Buffering, false, false),
            Busy::Readout(StatusKind::Working, c"Buffering\u{2026}")
        );
        assert_eq!(
            busy_surface_with(&ps, S::Connecting, false, true),
            Busy::Readout(StatusKind::Working, c"Connecting\u{2026}")
        );
        assert_eq!(busy_surface_with(&ps, S::Buffering, true, true), Busy::Transport);
    }

    /// No picture yet → the wait is about the whole panel. With the captions, which locks the
    /// kind↔caption pairing the enum carries (both are chosen once, by this function).
    #[test]
    fn a_cold_start_and_a_reload_both_own_the_whole_panel() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        assert_eq!(
            busy_surface(&ps, S::Resolving, false),
            Busy::Readout(StatusKind::Working, c"Preparing\u{2026}")
        );
        assert_eq!(
            busy_surface(&ps, S::Connecting, false),
            Busy::Readout(StatusKind::Working, c"Connecting\u{2026}")
        );
        assert_eq!(
            busy_surface_with(&ps, S::Buffering, false, false),
            Busy::Readout(StatusKind::Working, c"Buffering\u{2026}")
        );
        // tapping RIGHT during pre-roll (or `/tmp/dispatcharrtv-autoseek`): no picture to mark up
        assert_eq!(
            busy_surface(&ps, S::Seeking, false),
            Busy::Readout(StatusKind::Working, c"Seeking\u{2026}")
        );
    }

    /// A dead producer has no picture and no position, only a message — so it is the centred
    /// read-out either way. And it is a FAULT, never `Empty`, which is the "this is an answer"
    /// treatment.
    #[test]
    fn a_dead_producer_reads_out_whether_or_not_a_picture_was_up() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        for seen in [false, true] {
            assert_eq!(
                busy_surface(&ps, S::Error, seen),
                Busy::Readout(StatusKind::Failed, c"Playback failed")
            );
        }
    }

    fn readout_of(e: &plx_media::player::ErrorShape) -> FailureReadout {
        let cx = plx_media::player::FailureContext { repair_idle: true };
        FailureReadout::of(e, plx_media::player::failure_actions(e.kind, cx), "0.7.0 · webOS 4.5 · code".into())
    }

    /// A dropped channel's read-out offers *Try again* · *Back*, and the support facts are the dim
    /// footer — which the draw really paints, below the row.
    #[test]
    fn the_readout_offers_try_again_and_draws_the_diagnostics_as_the_footer() {
        use plx_media::player::FailureAction as A;
        let e = plx_media::player::interrupted_shape_for_test();
        let r = readout_of(&e);
        assert_eq!(r.actions(), &[A::TryAgain, A::Back]);
        assert_eq!(r.footer(), "0.7.0 · webOS 4.5 · code");
        let log = plx_ui::draw_census::capture(|| {
            r.draw(plx_ui::Painter::recording(), 0, &plx_ui::fixture::FixtureMeasure);
        });
        let texts: Vec<Rect> = log.iter().filter(|(tag, _)| *tag == 100).map(|(_, r)| *r).collect();
        assert!(texts.iter().any(|t| (t.y - FOOTER_TOP).abs() < theme::size::CAPTION as f32 * 2.0),
            "the footer line is drawn at the foot of the screen: {texts:?}");
    }

    /// The read-out's row, note and footer stack top to bottom inside the safe band, and the
    /// footer never meets the row.
    #[test]
    fn the_diagnostics_footer_sits_below_the_row_inside_the_safe_bottom_band() {
        use plx_base::fontcov::advances::ShippedMeasure;
        let e = plx_media::player::interrupted_shape_for_test();
        let r = readout_of(&e);
        let row_bottom = r.frames(&ShippedMeasure).iter().flatten().map(|f| f.y + f.h).fold(0.0, f32::max);
        assert!(row_bottom > 0.0, "the row was laid out");
        // a two-line CAPTION note sits between them, with room to spare
        let note_box = theme::size::CAPTION as f32 * 1.5 * 2.0;
        assert!(FOOTER_TOP > row_bottom + note_box + theme::space::MD,
            "footer top {FOOTER_TOP} must clear the row ({row_bottom}) and its note");
        let line_box = theme::size::CAPTION as f32 * 1.5;
        assert!(FOOTER_TOP + line_box < SCR_H - theme::space::XL + 1.0);
    }

    /// **Every row fits the screen, in every shipped language**: the longest row each kind can
    /// draw, measured with the device's advances, stays inside the read-out's width.
    #[test]
    fn every_failure_row_fits_the_screen_in_every_language() {
        use plx_base::fontcov::advances::ShippedMeasure;
        use plx_ui::fit::HEADROOM;
        use plx_platform::i18n::{language_on_this_thread_for_test, Preference};
        use plx_media::player::FailureAction as A;
        let rows: [&[A]; 2] = [
            &[A::TryAgain, A::Back],
            &[A::Repair, A::Back],
        ];
        let mut out = Vec::new();
        for language in [Preference::En, Preference::Es, Preference::Be, Preference::Fr] {
            let _guard = language_on_this_thread_for_test(language);
            for row in rows {
                let labels: Vec<&'static std::ffi::CStr> = row.iter().map(|a| failure_action_label(*a)).collect();
                let o = StatusOverlay::new(Rect::FULL, c"", StatusKind::Failed).page(plx_ui::icons::Icon::Alert).reason(c"x").row(&labels);
                let frames = o.row_frames_measured(&ShippedMeasure);
                let lo = frames.iter().flatten().map(|f| f.x).fold(f32::MAX, f32::min);
                let hi = frames.iter().flatten().map(|f| f.x + f.w).fold(0.0, f32::max);
                let limit = SCR_W - 2.0 * plx_ui::consts::MARGIN_X;
                if (hi - lo) > limit * HEADROOM || lo < plx_ui::consts::MARGIN_X {
                    out.push(format!("{}: {row:?} spans {:.0}px of {limit:.0}", language.tag(), hi - lo));
                }
            }
        }
        assert!(out.is_empty(), "rows wider than the screen:\n  {}", out.join("\n  "));
    }

    /// The sandbox failure's help sentence ends in a URL that cannot break, and it is drawn as the
    /// read-out's note — centred CAPTION, two lines of [`StatusOverlay::REASON_W`]. It must fit
    /// that in every shipped language, or the pointer to the fix is the part an ellipsis eats.
    #[test]
    fn the_jail_help_note_fits_the_note_slot_in_every_language() {
        use plx_base::fontcov::advances::ShippedMeasure;
        use plx_ui::fit::HEADROOM;
        use plx_ui::text_view::TextView;
        use plx_platform::i18n::{language_on_this_thread_for_test, msg, Preference};
        let mut out = Vec::new();
        for language in [Preference::En, Preference::Es, Preference::Be, Preference::Fr] {
            let _guard = language_on_this_thread_for_test(language);
            let help = msg::widgets_reason_jail_help();
            let fits = !TextView::new(help, theme::size::CAPTION, theme::TEXT_TERTIARY)
                .h(plx_ui::label::HAlign::Center)
                .max_lines(2)
                .with_measure(&ShippedMeasure)
                .truncates(StatusOverlay::REASON_W * HEADROOM);
            if !fits {
                out.push(format!("{}: {help}", language.tag()));
            }
        }
        assert!(out.is_empty(), "jail help cut short:\n  {}", out.join("\n  "));
    }

    /// Every Force verdict fits the read-out's two-line reason slot, in every shipped language.
    #[test]
    fn every_forced_verdict_fits_the_reason_slot_in_every_language() {
        use plx_base::fontcov::advances::ShippedMeasure;
        use plx_ui::fit::HEADROOM;
        use plx_platform::i18n::{language_on_this_thread_for_test, msg, Preference};
        let mut out = Vec::new();
        for language in [Preference::En, Preference::Es, Preference::Be, Preference::Fr] {
            let _guard = language_on_this_thread_for_test(language);
            for s in [msg::widgets_verdict_forced_no_original(), msg::widgets_verdict_forced_container(),
                msg::widgets_verdict_forced_video(), msg::widgets_verdict_forced_audio(),
                msg::widgets_verdict_forced_unauthorized(), msg::widgets_verdict_forced_open(),
                msg::widgets_verdict_forced_audio_conversion(), msg::widgets_reason_forced_playback_help()] {
                let c = std::ffi::CString::new(s).unwrap();
                if StatusOverlay::failed_reason_truncates(&c, &ShippedMeasure, HEADROOM) {
                    out.push(format!("{}: {s}", language.tag()));
                }
            }
        }
        assert!(out.is_empty(), "verdicts cut short:\n  {}", out.join("\n  "));
    }

    /// …and when it does, it owns the WHOLE frame: `draw_hud` draws nothing — no scrim, no
    /// scrubber, no clocks, no bottom tabs (`Player Screen.dc.html` sets `hudDisplay:none` and
    /// `tabsDisplay:none` on the failed variant, because a live-looking scrubber at 0:00 over a
    /// black panel is the bug the read-out exists to end).
    ///
    /// The other half is the one that would be easy to break by simplifying this to "a read-out is
    /// up": every WORKING read-out must leave the transport alone, or the HUD blanks through every
    /// cold start, every reconnect and every pre-roll seek — states where the position is real and
    /// the transport is what the user reads the instant the first frame lands.
    #[test]
    fn only_a_failure_takes_the_frame_away_from_the_transport() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        for seen in [false, true] {
            assert!(
                readout_owns_frame(busy_surface(&ps, S::Error, seen)),
                "a failure hides the transport"
            );
        }
        for st in [S::Resolving, S::Connecting, S::Buffering, S::Seeking] {
            let b = busy_surface(&ps, st, false);
            assert!(
                matches!(b, Busy::Readout(StatusKind::Working, _)),
                "{st:?} is a read-out"
            );
            assert!(
                !readout_owns_frame(b),
                "{st:?} is mid-flight — the transport stays up under it"
            );
        }
        // and the two non-read-out surfaces are never a reason to blank the frame
        assert!(!readout_owns_frame(busy_surface(&ps, S::Playing, true)));
        assert!(
            !readout_owns_frame(busy_surface(&ps, S::Seeking, true)),
            "a seek over a live picture"
        );
    }

    /// The geometry regression, guarding against a new `OVERLAY_BOTTOM`: the read-out centres on
    /// the PANEL, and doing so still clears the transport — the constraint the carve-out claimed to
    /// be enforcing and was not.
    ///
    /// The spinner half of the block is pure geometry (the caption half needs `text_height`, which
    /// the host suite cannot link) and is the LARGER half, so `above()` is a conservative stand-in
    /// for the whole block. Numerically 540 + 58.16 = 598.16 < 610: deliberately tight, so it fails
    /// if someone bumps `R_PAGE`, thickens the scrim, or re-carves the frame.
    #[test]
    fn the_readout_is_centred_and_still_clears_the_transport() {
        let f = readout_frame();
        assert_eq!(
            f.cy(),
            SCR_H * 0.5,
            "the wait is about the whole picture, so it centres on it"
        );
        assert_eq!(f.cx(), SCR_W * 0.5);
        assert!(
            f.cy() + StatusOverlay::above() < SCR_H - SCRIM_H,
            "the read-out must not reach into the transport scrim"
        );
        assert!(
            f.cy() - StatusOverlay::above() > 0.0,
            "nor off the top of the panel"
        );
    }

    // ---- the transport state read-out ---------------------------------------------------------
    //
    // `transport_mark` is the whole rule, factored out of the draw so it can be graded here: the
    // draw itself is one `match` over the returned value, and the slot's geometry is unchanged.
    // 1 s in ns, so a position reads as a second.
    const S: i64 = 1_000_000_000;

    /// **A cue is never cut at a line count**: every wrapped line of every segment survives the
    /// wrap, however many there are.
    #[test]
    fn a_six_line_cue_wraps_to_six_lines() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix";
        assert_eq!(caption_wrap(text, 1.0), ["one", "two", "three", "four", "five", "six"]);
        let long = "word ".repeat(120);
        let wrapped = caption_wrap(long.trim(), 1.0);
        assert!(wrapped.len() > 3, "a long cue wraps past the anchor count, got {}", wrapped.len());
        assert_eq!(wrapped.join(" ").split_whitespace().count(), 120, "no word is lost");
    }

    /// **A 1-3 line caption lays out exactly as it did before the fit rule**: 36 px at the
    /// viewer's default size, a 48 px pitch, and the block's bottom on the baseline.
    #[test]
    fn a_three_line_caption_keeps_its_size_and_position() {
        let lay = caption_layout(3, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!(lay.face, 36);
        assert_eq!(lay.pitch, 48.0);
        assert_eq!(lay.top, 836.0);
        assert_eq!(lay.line_top(0), 836.0);
        assert_eq!(lay.line_top(2), 932.0);
        let one = caption_layout(1, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!((one.face, one.top), (36, 932.0), "one line sits on the baseline");
        let two = caption_layout(2, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!((two.face, two.top), (36, 884.0));
    }

    /// **A six-line block at the default size is too tall for the area below the capsule, so it
    /// steps down, and every one of its lines is still laid out inside the floor.**
    #[test]
    fn a_six_line_block_steps_down_and_keeps_every_line() {
        let lay = caption_layout(6, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert!(lay.face < 36, "the face steps down, got {}", lay.face);
        assert_eq!(lay.lines, 6);
        let bottom = lay.line_top(5) + lay.pitch;
        assert!(bottom <= CAPTION_FLOOR_Y, "the block ends at {bottom}, past the floor");
    }

    /// **A block that cannot fit even at the smallest step is still drawn whole.** Lines are never
    /// dropped to make room.
    #[test]
    fn a_block_too_tall_for_any_step_keeps_all_its_lines() {
        let lay = caption_layout(12, 1.5, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!(lay.lines, 12);
        assert_eq!(lay.face, (36.0 * 1.5 * CAPTION_FIT_STEPS[3]).round() as i32, "smallest step");
    }

    /// A cue of `n` one-word lines (each its own segment, so the wrap keeps them as given).
    fn cue_of(n: usize) -> String {
        (0..n).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n")
    }

    /// **A cue that fits is one page, laid out exactly as before**, and a cue that does not is cut
    /// into equal pages at the smallest step (no page ends in a lone line), the last page holding
    /// to the cue's end.
    #[test]
    fn a_fitting_cue_is_one_page_and_a_tall_one_is_equal_pages() {
        let plan = CaptionPlan::new(vec!["a".into(), "b".into()], 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!(plan.pages(), 1);
        assert_eq!(plan.page_layout(0), caption_layout(2, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y));
        let lines: Vec<String> = (0..8).map(|i| i.to_string()).collect();
        let plan = CaptionPlan::new(lines, 1.0, SUB_BASE_Y, CAPTION_FLOOR_Y);
        assert_eq!(plan.layout.face, (36.0 * CAPTION_FIT_STEPS[3]).round() as i32, "paged at the smallest step");
        assert_eq!((plan.pages(), plan.per_page), (2, 4), "8 lines: 2 pages of 4, not 6 + 2");
        assert_eq!(plan.page_layout(1).lines, 4);
        // pages turn at equal fractions of the span and the last holds to the end
        assert_eq!((0..4).map(|k| caption_page_at(2, 0, 100, k * 25)).collect::<Vec<_>>(), [0, 0, 1, 1]);
        assert_eq!(caption_page_at(2, 0, 100, 99), 1);
        assert_eq!(caption_page_at(2, 0, 100, 1_000), 1, "past the end holds the last page");
        assert_eq!(caption_page_at(5, 0, i64::MAX, 50), 0, "no real end stays on the first page");
    }

    /// **Every line of a cue is drawn, inside the screen, in order, at some time within the cue's
    /// own duration.** Cues of 3, 8, 20 and 60 wrapped lines, at every viewer size and with the
    /// transport up and down: step through the cue's span the way frames do, take the page the
    /// pager shows, and collect what it draws.
    #[test]
    fn every_line_of_a_tall_cue_is_shown_on_screen_in_order() {
        const S: i64 = 1_000_000_000;
        for hud_up in [false, true] {
            for scale in [0.8f32, 1.0, 1.25, 1.5] {
                let (baseline, floor) = if hud_up { (SUB_CEIL_Y, SUB_CEIL_Y) } else { (SUB_BASE_Y, CAPTION_FLOOR_Y) };
                for n in [3usize, 8, 20, 60] {
                    let pager = CaptionPager::default();
                    let (start, end) = (10 * S, 10 * S + 24 * S);
                    let mut shown: Vec<String> = Vec::new();
                    let mut last_page = None;
                    let mut frame = start;
                    while frame < end {
                        let key = CaptionKey {
                            start_ns: start,
                            text: cue_of(n),
                            geometry: [scale.to_bits(), baseline.to_bits(), floor.to_bits()],
                        };
                        let page = pager.page_of(key, (scale, baseline, floor), end, frame).expect("a caption");
                        let slot = pager.slot.borrow();
                        let plan = &slot.as_ref().unwrap().1;
                        let lay = plan.page_layout(page);
                        for i in 0..plan.page_lines(page).len() {
                            let (top, bottom) = (lay.line_top(i), lay.line_top(i) + lay.pitch);
                            assert!(top >= 0.0 && bottom <= floor + 0.01, "{n} lines, scale {scale}, hud {hud_up}: line {i} spans {top}..{bottom}, floor {floor}");
                        }
                        if last_page != Some(page) {
                            shown.extend(plan.page_lines(page).iter().cloned());
                            last_page = Some(page);
                        }
                        frame += S / 60; // a 60 fps frame
                    }
                    let want: Vec<String> = (0..n).map(|i| format!("line{i}")).collect();
                    assert_eq!(shown, want, "{n} lines at scale {scale}, hud {hud_up}: every line, once, in order");
                }
            }
        }
    }
}
