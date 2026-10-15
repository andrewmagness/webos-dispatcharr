//! The Guide: what a landing seats, how the cursor walks the lattice (a time kept across rows),
//! where the view goes to reveal it, what a refetch keeps, where each failure sends the viewer, and
//! that a settled grid lets the loop sleep.

use super::*;
use crate::registry::{AppMsg, DispatcharrCmd, GuideOutcome};
use plx_machine::machine::{By, FocusRead, Host, InputOwner, PressRead, PresentHandle, Source, Stamped};
use plx_machine::present::Present;
use plx_ui::fixture::FixtureMeasure;

struct GuideHost;

/// The page reads no recording header.
struct TestInit;

impl LogicalState for TestInit {
    fn write(&self, _w: &mut Canon) {}
    fn probe(&self, _out: &mut String) {}
}

impl Host for GuideHost {
    type Arg = crate::registry::AppArg;
    type Fx = AppFx;
    type Msg = AppMsg;
    type Elem = u32;
    type Views<'a> = ();
    type Init = TestInit;
    type Memory = ();
}

/// 2026-10-14T12:00:00Z, on a half hour.
const T0: i64 = 1_791_979_200;
/// The instant the test guide is fetched at: ten past.
const NOW: i64 = T0 + 600;

fn cx(m: &FixtureMeasure, focus: Option<u32>, ms: u32) -> Cx<'_, GuideHost> {
    Cx {
        views: (),
        tick: Tick { ms, dt_us: 16_000 },
        measure: m,
        press: PressRead::default(),
        focus: FocusRead { current: focus.map(|elem| FocusKey { entry: EntryId(1), elem }), ..FocusRead::default() },
        owner: InputOwner::Entry(EntryId(1)),
    }
}

fn step_at(screen: &mut GuideScreen, ev: ScreenEvent<GuideHost>, focus: Option<u32>, ms: u32)
    -> (Handled, Vec<Stamped<GuideHost>>, bool) {
    let m = FixtureMeasure;
    let mut out = Vec::new();
    let mut present = Present::new();
    present.take(0);
    let handled = {
        let mut fx = Effects::from_handle(&mut out, MachineId::Instance(InstanceId(1)), PresentHandle::of(&mut present));
        screen.step(&ev, &cx(&m, focus, ms), &mut fx)
    };
    (handled, out, present.changed())
}

fn step(screen: &mut GuideScreen, ev: ScreenEvent<GuideHost>, focus: Option<u32>) -> Vec<Stamped<GuideHost>> {
    step_at(screen, ev, focus, 0).1
}

fn tick(ms: u32) -> ScreenEvent<GuideHost> {
    ScreenEvent::Tick(Tick { ms, dt_us: 16_000 })
}

fn command(out: Vec<Stamped<GuideHost>>) -> Option<DispatcharrCmd> {
    out.into_iter().find_map(|s| match s.fx {
        Fx::App(AppFx::Dispatcharr(cmd)) => Some(cmd),
        _ => None,
    })
}

fn loop_req(out: &[Stamped<GuideHost>]) -> Option<LoopReq> {
    out.iter().find_map(|s| match &s.fx {
        Fx::App(AppFx::Loop(req)) => Some(*req),
        _ => None,
    })
}

fn content_req(out: &[Stamped<GuideHost>]) -> Option<&ContentReq> {
    out.iter().find_map(|s| match &s.fx {
        Fx::App(AppFx::Content(req)) => Some(req),
        _ => None,
    })
}

/// Each channel's stream URL, the way the application builds them.
fn streams(channels: &[Channel]) -> Vec<String> {
    channels.iter().map(|c| format!("http://192.0.2.10:9191/proxy/ts/stream/{}", c.uuid)).collect()
}

fn reseated_on(out: &[Stamped<GuideHost>]) -> Option<u32> {
    out.iter().find_map(|s| match &s.fx {
        Fx::Deliver(_, Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(k) }))) => Some(k.elem),
        _ => None,
    })
}

fn channel(id: i64, number: f64) -> Channel {
    Channel { id, uuid: format!("u-{id}"), name: format!("Channel {id}"), logo_id: None,
        channel_number: Some(number), epg_data_id: None, channel_group_id: None }
}

fn prog(start: i64, end: i64, title: &str) -> Programme {
    Programme { start, end, title: title.into(), sub_title: None, description: None, season: None,
        episode: None, is_new: false, is_live: false, is_premiere: false, is_finale: false }
}

/// Ten channels. Channel 1 runs half-hour programmes from T0; channel 2 one long programme
/// 11:45–13:15 then hourly; the rest have no listings.
fn lineup() -> (Vec<Channel>, Guide) {
    let channels: Vec<Channel> = (1..=10).map(|i| channel(i, i as f64)).collect();
    let mut rows = Vec::new();
    for k in 0..8 {
        rows.push(("u-1".to_string(), prog(T0 + k * 1800, T0 + (k + 1) * 1800, &format!("One {k}"))));
    }
    rows.push(("u-2".to_string(), prog(T0 - 900, T0 + 4500, "Two long")));
    for k in 0..4 {
        rows.push(("u-2".to_string(), prog(T0 + 4500 + k * 3600, T0 + 4500 + (k + 1) * 3600, &format!("Two {k}"))));
    }
    let guide = Guide::build(&channels, &[], rows, Window::around(NOW));
    (channels, guide)
}

/// A mounted page with `lineup()` landed at `NOW`; the element the landing seated.
fn landed() -> (GuideScreen, u32) {
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let out = step(&mut screen, ScreenEvent::Mount, None);
    let Some(DispatcharrCmd::Guide { reply }) = command(out) else { panic!("mount asks for the guide") };
    let (channels, guide) = lineup();
    let logos = vec![None; channels.len()];
    let streams = streams(&channels);
    reply.send(GuideOutcome::Loaded { channels, logos, streams, guide, fetched_at: NOW }).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(1000), None, 1000);
    let seat = reseated_on(&out).expect("a landing seats the cursor");
    step(&mut screen, ScreenEvent::FocusMoved { from: None, to: FocusKey { entry: EntryId(1), elem: seat }, by: By::Restore }, Some(seat));
    (screen, seat)
}

fn neighbour(screen: &GuideScreen, elem: u32, dir: Dir) -> Option<u32> {
    let m = FixtureMeasure;
    match <GuideScreen as Focusable<GuideHost>>::neighbour(screen, FocusKey { entry: EntryId(1), elem }, dir, &cx(&m, Some(elem), 0)) {
        Step::Move(k) => Some(k.elem),
        Step::Edge => None,
    }
}

/// Move the cursor the way the engine does: ask the page for the neighbour, then tell it.
fn walk(screen: &mut GuideScreen, from: u32, dir: Dir) -> u32 {
    let to = neighbour(screen, from, dir).unwrap_or_else(|| panic!("{dir:?} from {from:#x} is an edge"));
    step(screen, ScreenEvent::FocusMoved { from: Some(FocusKey { entry: EntryId(1), elem: from }),
        to: FocusKey { entry: EntryId(1), elem: to }, by: By::Dir }, Some(to));
    to
}

fn slot_of(screen: &GuideScreen, elem: u32) -> Slot {
    let (row, slot) = split(elem).unwrap();
    *screen.listing.as_ref().unwrap().slot(row, slot).unwrap()
}

#[test]
fn a_landing_seats_the_first_channel_on_what_airs_now_and_opens_the_view_on_its_half_hour() {
    let (screen, seat) = landed();
    assert_eq!(screen.phase, Phase::Ready);
    let (row, _) = split(seat).unwrap();
    assert_eq!(row, 0);
    assert!(slot_of(&screen, seat).contains(NOW));
    assert_eq!(screen.view.start, T0 as f64, "the view opens on the half hour now falls in");
    assert_eq!(screen.view.top, 0.0);
    assert_eq!(screen.now, NOW);
}

#[test]
fn left_and_right_step_a_rows_cells_and_stop_at_the_window() {
    let (mut screen, seat) = landed();
    let next = walk(&mut screen, seat, Dir::Right);
    assert_eq!(slot_of(&screen, next).start, T0 + 1800);
    let back = walk(&mut screen, next, Dir::Left);
    assert_eq!(back, seat);
    // The window's first cell (the unlisted hour before T0) is the left edge.
    let first = walk(&mut screen, back, Dir::Left);
    assert_eq!(slot_of(&screen, first).start, screen.listing.as_ref().unwrap().window.start);
    assert_eq!(neighbour(&screen, first, Dir::Left), None);
    // Up from the first channel is the grid's edge (the engine's geometric search takes it on to
    // the header).
    assert_eq!(neighbour(&screen, first, Dir::Up), None);
}

#[test]
fn up_and_down_keep_the_time_the_last_sideways_step_left() {
    let (mut screen, seat) = landed();
    // Two to the right on channel 1: 13:00–13:30. The anchor is that start.
    let a = walk(&mut screen, seat, Dir::Right);
    let b = walk(&mut screen, a, Dir::Right);
    assert_eq!(slot_of(&screen, b).start, T0 + 3600);
    assert_eq!(screen.anchor, T0 + 3600);
    // Down to channel 2: its long programme (11:45–13:15) airs at 13:00.
    let c = walk(&mut screen, b, Dir::Down);
    assert_eq!(split(c).unwrap().0, 1);
    assert_eq!(slot_of(&screen, c).start, T0 - 900);
    // ...and the anchor did not move to that programme's start: down again and up again come back
    // to 13:00, not to 11:45.
    assert_eq!(screen.anchor, T0 + 3600);
    let d = walk(&mut screen, c, Dir::Down);
    assert!(slot_of(&screen, d).contains(T0 + 3600));
    let e = walk(&mut screen, d, Dir::Up);
    let f = walk(&mut screen, e, Dir::Up);
    assert_eq!(f, b, "down, down, up, up comes back to the same cell");
}

#[test]
fn a_cell_that_began_before_the_view_anchors_at_the_views_edge() {
    let (mut screen, seat) = landed();
    // Down to channel 2's long programme (began 11:45, before the 12:00 view).
    let c = walk(&mut screen, seat, Dir::Down);
    assert_eq!(slot_of(&screen, c).start, T0 - 900);
    // A sideways step onto it from its neighbour sets the anchor to the view's edge, not 11:45.
    let right = walk(&mut screen, c, Dir::Right);
    let back = walk(&mut screen, right, Dir::Left);
    assert_eq!(back, c);
    assert!(screen.anchor >= screen.view.start as i64);
}

#[test]
fn the_view_moves_only_when_the_cursor_cell_starts_outside_it() {
    let (mut screen, seat) = landed();
    let mut at = seat;
    // 12:00 → 12:30 → 13:00 → 13:30: all start inside the two-hour view.
    for _ in 0..3 {
        at = walk(&mut screen, at, Dir::Right);
        assert_eq!(screen.view.start, T0 as f64);
    }
    // 14:00 starts at the view's right edge: the view moves so that half hour leads.
    at = walk(&mut screen, at, Dir::Right);
    assert_eq!(slot_of(&screen, at).start, T0 + 7200);
    assert_eq!(screen.view.start, (T0 + 7200) as f64);
    // The drawn view has not jumped: the lag holds it where it was, to glide.
    assert!((screen.drawn_view().start - T0 as f64).abs() < 1.0);
}

#[test]
fn walking_down_through_a_programme_that_began_earlier_does_not_move_the_view() {
    let (mut screen, seat) = landed();
    // Channel 1 at 14:00 (four steps right): the view moves to 14:00.
    let mut at = seat;
    for _ in 0..4 {
        at = walk(&mut screen, at, Dir::Right);
    }
    assert_eq!(screen.view.start, (T0 + 7200) as f64);
    // Down to channel 2: its programme at 14:00 began at 13:15, before the view, and runs on to
    // 14:15 — a quarter of an hour in view. The view must not jump back for it.
    at = walk(&mut screen, at, Dir::Down);
    let s = slot_of(&screen, at);
    assert!(s.start < T0 + 7200 && s.end > T0 + 7200, "{s:?}");
    assert_eq!(screen.view.start, (T0 + 7200) as f64, "a vertical step keeps the axis still");
}

#[test]
fn walking_down_past_the_last_whole_row_scrolls_by_one() {
    let (mut screen, seat) = landed();
    let fit = layout::grid().rows_fit();
    let mut at = seat;
    for _ in 0..fit - 1 {
        at = walk(&mut screen, at, Dir::Down);
    }
    assert_eq!(screen.view.top, 0.0);
    at = walk(&mut screen, at, Dir::Down);
    assert_eq!(split(at).unwrap().0, fit);
    assert_eq!(screen.view.top, 1.0);
    // The last row is an edge.
    for _ in fit + 1..10 {
        at = walk(&mut screen, at, Dir::Down);
    }
    assert_eq!(neighbour(&screen, at, Dir::Down), None);
    assert_eq!(screen.view.top, (10 - fit) as f32);
}

#[test]
fn a_settled_grid_lets_the_loop_sleep_and_a_glide_keeps_it_awake() {
    let (mut screen, seat) = landed();
    let mut at = seat;
    for _ in 0..4 {
        at = walk(&mut screen, at, Dir::Right);
    }
    // The view just moved: the next tick glides, and asks for a frame.
    let (_, _, asked) = step_at(&mut screen, tick(1016), Some(at), 1016);
    assert!(asked, "a gliding view must keep presenting");
    // Settle (well under the marker's one-pixel step: 600 ticks of 16 ms is 9.6 s at ~0.19 px/s).
    let mut ms = 1016;
    for _ in 0..600 {
        ms += 16;
        step_at(&mut screen, tick(ms), Some(at), ms);
    }
    assert!(screen.start_lag.pos.abs() < 0.5 && screen.top_lag.pos.abs() < 0.5);
    let (_, _, asked) = step_at(&mut screen, tick(ms + 16), Some(at), ms + 16);
    assert!(!asked, "a settled grid with a still marker must not keep the loop awake");
}

#[test]
fn the_now_marker_asks_for_a_frame_only_when_it_would_move_a_pixel() {
    let (mut screen, seat) = landed();
    let mut ms = 1000;
    for _ in 0..600 {
        ms += 16;
        step_at(&mut screen, tick(ms), Some(seat), ms);
    }
    // 30 s later the marker has moved ~5.7 px: that tick asks, the next does not.
    let later = ms + 30_000;
    let (_, _, asked) = step_at(&mut screen, tick(later), Some(seat), later);
    assert!(asked);
    assert_eq!(screen.now, NOW + (later - 1000) as i64 / 1000);
    let (_, _, asked) = step_at(&mut screen, tick(later + 16), Some(seat), later + 16);
    assert!(!asked);
}

#[test]
fn a_stale_window_is_fetched_again_and_the_landing_keeps_the_channel_and_the_time() {
    let (mut screen, seat) = landed();
    let c = walk(&mut screen, seat, Dir::Down); // channel 2
    let at_time = screen.anchor;
    // Half an hour on, the window is stale: the page asks again and keeps the old guide.
    let later = 1000 + (Window::MAX_AGE as u32) * 1000;
    let (_, out, _) = step_at(&mut screen, tick(later), Some(c), later);
    let Some(DispatcharrCmd::Guide { reply }) = command(out) else { panic!("a stale window asks again") };
    assert_eq!(screen.phase, Phase::Ready, "the old guide stays on screen");
    // The server now lists channel 2 FIRST: the cursor must follow the channel, not the row.
    let (mut channels, guide) = lineup();
    channels.swap(0, 1);
    let fetched = NOW + Window::MAX_AGE;
    let logos = vec![None; channels.len()];
    let streams = streams(&channels);
    reply.send(GuideOutcome::Loaded { channels, logos, streams, guide, fetched_at: fetched }).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(later + 16), Some(c), later + 16);
    let seat = reseated_on(&out).expect("a refetch re-seats");
    let (row, _) = split(seat).unwrap();
    assert_eq!(screen.listing.as_ref().unwrap().channels[row].id, 2);
    assert!(slot_of(&screen, seat).contains(at_time.max(fetched - Window::MAX_AGE)));
}

#[test]
fn no_account_and_a_refused_key_go_back_to_setup_and_an_unreachable_server_retries() {
    // No account stored.
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut screen, ScreenEvent::Mount, None)) else { panic!() };
    reply.send(GuideOutcome::SignedOut).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(16), None, 16);
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrSetup));

    // A refused key: the read-out's action is "Sign in again", and it goes to Setup.
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut screen, ScreenEvent::Mount, None)) else { panic!() };
    reply.send(GuideOutcome::Failed(Error::Unauthorized)).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(16), None, 16);
    assert_eq!(reseated_on(&out), Some(RETRY));
    assert_eq!(screen.retry_label(), msg::dispatcharr_guide_sign_in_again_c());
    let out = step(&mut screen, ScreenEvent::Activate(RETRY), Some(RETRY));
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrSetup));

    // Unreachable: "Try again" asks for the guide again.
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut screen, ScreenEvent::Mount, None)) else { panic!() };
    reply.send(GuideOutcome::Failed(Error::Unreachable)).unwrap();
    step_at(&mut screen, tick(16), None, 16);
    assert_eq!(screen.retry_label(), msg::dispatcharr_guide_retry_c());
    let out = step(&mut screen, ScreenEvent::Activate(RETRY), Some(RETRY));
    assert!(matches!(command(out), Some(DispatcharrCmd::Guide { .. })));
    assert_eq!(screen.phase, Phase::Loading);
}

#[test]
fn a_failed_refetch_keeps_the_guide_on_screen() {
    let (mut screen, seat) = landed();
    let later = 1000 + (Window::MAX_AGE as u32) * 1000;
    let (_, out, _) = step_at(&mut screen, tick(later), Some(seat), later);
    let Some(DispatcharrCmd::Guide { reply }) = command(out) else { panic!() };
    reply.send(GuideOutcome::Failed(Error::Unreachable)).unwrap();
    step_at(&mut screen, tick(later + 16), Some(seat), later + 16);
    assert_eq!(screen.phase, Phase::Ready);
    assert!(screen.listing.is_some());
}

#[test]
fn an_account_with_no_channels_says_so_and_seats_on_account() {
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut screen, ScreenEvent::Mount, None)) else { panic!() };
    reply.send(GuideOutcome::Loaded { channels: Vec::new(), logos: Vec::new(), streams: Vec::new(),
        guide: Guide::build(&[], &[], [], Window::around(NOW)),
        fetched_at: NOW }).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(16), None, 16);
    assert_eq!(screen.phase, Phase::Empty);
    assert_eq!(reseated_on(&out), Some(ACCOUNT));
}

#[test]
fn account_opens_the_server_page_and_back_leaves_the_app() {
    let (mut screen, seat) = landed();
    let out = step(&mut screen, ScreenEvent::Activate(ACCOUNT), Some(ACCOUNT));
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrAccount));
    let back = ScreenEvent::Input(InputEvent {
        at: Tick::default(),
        source: Source::Script,
        kind: InputKind::Key { key: Key::Back, sym: 0, wcode: 0, edge: Edge::Down, at_edge: false },
    });
    let out = step(&mut screen, back, Some(seat));
    assert_eq!(loop_req(&out), Some(LoopReq::BackAtRoot));
}

#[test]
fn ok_on_what_airs_now_tunes_its_channel() {
    let (mut screen, seat) = landed();
    let out = step(&mut screen, ScreenEvent::Activate(seat), Some(seat));
    let Some(ContentReq::Play { play: PlayIntent::Live { url, vcodec, acodec, title, context }, resume_ns }) =
        content_req(&out)
    else {
        panic!("OK on a programme tunes its channel")
    };
    assert_eq!(url, "http://192.0.2.10:9191/proxy/ts/stream/u-1");
    assert_eq!((vcodec.as_str(), acodec.as_str()), ("h264", "aac"));
    assert_eq!((title.as_str(), context.as_str()), ("Channel 1", "One 0"));
    assert_eq!(*resume_ns, 0);
}

#[test]
fn ok_on_a_later_programme_tunes_the_channel_to_what_airs_now() {
    let (mut screen, seat) = landed();
    let c = walk(&mut screen, seat, Dir::Down); // channel 2, "Two long"
    let later = walk(&mut screen, c, Dir::Right); // "Two 0", 13:15
    let out = step(&mut screen, ScreenEvent::Activate(later), Some(later));
    let Some(ContentReq::Play { play: PlayIntent::Live { url, context, .. }, .. }) = content_req(&out) else {
        panic!("a later programme still tunes the channel")
    };
    assert!(url.ends_with("/u-2"));
    assert_eq!(context, "Two long", "live TV plays what is on now");
}

#[test]
fn a_channel_with_no_stream_url_does_not_tune() {
    let mut screen = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut screen, ScreenEvent::Mount, None)) else { panic!() };
    let (channels, guide) = lineup();
    let logos = vec![None; channels.len()];
    reply.send(GuideOutcome::Loaded { channels, logos, streams: Vec::new(), guide, fetched_at: NOW }).unwrap();
    let (_, out, _) = step_at(&mut screen, tick(16), None, 16);
    let seat = reseated_on(&out).unwrap();
    let out = step(&mut screen, ScreenEvent::Activate(seat), Some(seat));
    assert!(content_req(&out).is_none());
}

#[test]
fn a_listing_keeps_one_logo_per_row_whatever_the_answer_held() {
    let (channels, guide) = lineup();
    let logos = vec![Some("http://192.0.2.10:9191/api/channels/logos/101/cache/".to_owned())];
    let listing = Listing::new(channels, logos, Vec::new(), guide, NOW).unwrap();
    assert_eq!(listing.logos.len(), listing.rows());
    assert_eq!(listing.streams.len(), listing.rows(), "and one stream URL, empty when absent");
    assert!(listing.logos[0].is_some());
    assert!(listing.logos[1..].iter().all(Option::is_none));
}

#[test]
fn a_cell_element_round_trips_its_row_and_slot_and_never_meets_the_reserved_ones() {
    for (row, slot) in [(0, 0), (1, 2), (MAX_ROWS - 1, MAX_SLOTS - 1)] {
        let elem = cell(row, slot);
        assert_eq!(split(elem), Some((row, slot)));
        assert!(elem < RETRY);
    }
    assert_eq!(split(ACCOUNT), None);
    assert_eq!(split(RETRY), None);
}

#[test]
fn the_detail_line_reads_times_episode_and_episode_title() {
    let slot = Slot { start: T0, end: T0 + 1800, programme: Some(0) };
    let mut p = prog(T0, T0 + 1800, "Show");
    p.season = Some(2);
    p.episode = Some(5);
    p.sub_title = Some("The One".into());
    let line = meta_line(&slot, Some(&p));
    // Read from first principles, not through the helpers under test: the generated accessors
    // take their arguments in alphabetical order, which is how both of these were once swapped.
    let (start, end) = (i18n::wall_time(T0), i18n::wall_time(T0 + 1800));
    assert_ne!(start, end);
    assert!(line.starts_with(&format!("{start} – {end}")), "{line}");
    assert!(line.contains(" · S2 E5 · "), "{line}");
    assert!(line.ends_with("The One"), "{line}");
    assert_eq!(meta_line(&slot, None), format!("{start} – {end}"));
}

/// The page acts on `ScreenEvent::Activate`, which the dispatcher sends on OK only for a `Bare`
/// element; a `Control` element's OK arms a press that ends in `PressCommit` instead, and the page
/// would never hear it. That is how *Account* once did nothing on the television.
#[test]
fn every_element_the_page_activates_is_one_ok_activates_directly() {
    let m = FixtureMeasure;
    let (ready, _) = landed();
    let mut failed = GuideScreen::new(EntryId(1), InstanceId(1));
    let Some(DispatcharrCmd::Guide { reply }) = command(step(&mut failed, ScreenEvent::Mount, None)) else { panic!() };
    reply.send(GuideOutcome::Failed(Error::Unreachable)).unwrap();
    step_at(&mut failed, tick(16), None, 16);
    for screen in [&ready, &failed] {
        let mut groups = Vec::new();
        <GuideScreen as Focusable<GuideHost>>::groups(screen, &cx(&m, None, 0), &mut groups);
        assert!(groups.len() >= 2);
        for g in groups {
            assert_eq!(g.elem, ElemKind::Bare, "group {:?}", g.id);
        }
    }
}

/// Elements at or above `STRIP_BASE` belong to the shared tab strip (`PageWithStrip`): the page is
/// never asked about them, so OK on one finds no element to activate. *Account* once sat at
/// `u32::MAX`, inside that range, and did nothing on the television.
#[test]
fn every_element_of_the_page_lies_below_the_strips_range() {
    let strip = plx_ui::dispatch::STRIP_BASE;
    assert!(ACCOUNT < strip, "{ACCOUNT:#x}");
    assert!(RETRY < strip, "{RETRY:#x}");
    assert!(cell(MAX_ROWS - 1, MAX_SLOTS - 1) < RETRY.min(ACCOUNT));
}
