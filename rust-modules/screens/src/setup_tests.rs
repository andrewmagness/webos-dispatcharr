//! Setup: what each failure says, where focus lands after one, the keyboard walk across the
//! fields, and that the password does not outlive the sign-in it was typed for.

use super::*;
use crate::registry::{AppMsg, DispatcharrCmd, SetupOutcome};
use plx_dispatcharr::dispatcharr::account::Record;
use plx_dispatcharr::dispatcharr::api::Account;
use plx_dispatcharr::dispatcharr::Server;
use plx_machine::machine::{By, FocusRead, Host, PressRead, PresentHandle, Source, Stamped};
use plx_machine::present::Present;
use plx_ui::fixture::FixtureMeasure;

struct SetupHost;

/// The page reads no recording header.
struct TestInit;

impl LogicalState for TestInit {
    fn write(&self, _w: &mut Canon) {}
    fn probe(&self, _out: &mut String) {}
}

impl Host for SetupHost {
    type Arg = crate::registry::AppArg;
    type Fx = AppFx;
    type Msg = AppMsg;
    type Elem = u32;
    type Views<'a> = ();
    type Init = TestInit;
    type Memory = ();
}

fn cx(m: &FixtureMeasure, focus: Option<u32>) -> Cx<'_, SetupHost> {
    Cx {
        views: (),
        tick: Tick::default(),
        measure: m,
        press: PressRead::default(),
        focus: FocusRead { current: focus.map(|elem| FocusKey { entry: EntryId(1), elem }), ..FocusRead::default() },
        owner: InputOwner::Entry(EntryId(1)),
    }
}

/// Step `ev` and hand back what the page emitted.
fn step(screen: &mut SetupScreen, ev: ScreenEvent<SetupHost>, focus: Option<u32>) -> (Handled, Vec<Stamped<SetupHost>>) {
    let m = FixtureMeasure;
    let mut out = Vec::new();
    let mut present = Present::new();
    let handled = {
        let mut fx = Effects::from_handle(&mut out, MachineId::Instance(InstanceId(1)), PresentHandle::of(&mut present));
        screen.step(&ev, &cx(&m, focus), &mut fx)
    };
    (handled, out)
}

fn key(k: Key) -> ScreenEvent<SetupHost> {
    ScreenEvent::Input(InputEvent {
        at: Tick::default(),
        source: Source::Script,
        kind: InputKind::Key { key: k, sym: 0, wcode: 0, edge: Edge::Down, at_edge: false },
    })
}

fn text(s: &str) -> ScreenEvent<SetupHost> {
    ScreenEvent::Input(InputEvent { at: Tick::default(), source: Source::Script, kind: InputKind::Text(TextEdit::Commit(s.into())) })
}

fn tick(dt_us: u32) -> ScreenEvent<SetupHost> {
    ScreenEvent::Tick(Tick { ms: 0, dt_us })
}

/// The `DispatcharrCmd` the page asked for, taken out of its emissions.
fn command(out: Vec<Stamped<SetupHost>>) -> Option<DispatcharrCmd> {
    out.into_iter().find_map(|s| match s.fx {
        Fx::App(AppFx::Dispatcharr(cmd)) => Some(cmd),
        _ => None,
    })
}

fn reseated_on(out: &[Stamped<SetupHost>]) -> Option<u32> {
    out.iter().find_map(|s| match &s.fx {
        Fx::Deliver(_, Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(k) }))) => Some(k.elem),
        _ => None,
    })
}

fn keyboard_moves(out: &[Stamped<SetupHost>]) -> Vec<bool> {
    out.iter().filter_map(|s| match &s.fx { Fx::Deliver(_, Delivery::Keyboard { up }) => Some(*up), _ => None }).collect()
}

fn record() -> Record {
    Record {
        account: Account { server: Server::parse("192.168.1.10").unwrap(), api_key: "KEY".into() },
        username: "andrew".into(),
    }
}

fn loop_req(out: &[Stamped<SetupHost>]) -> Option<LoopReq> {
    out.iter().find_map(|s| match &s.fx {
        Fx::App(AppFx::Loop(req)) => Some(*req),
        _ => None,
    })
}

/// A boot page on the form, its Load answered with "nobody signed in".
fn form() -> SetupScreen {
    let mut screen = SetupScreen::new(EntryId(1), InstanceId(1), Mode::Entry);
    let (_, out) = step(&mut screen, ScreenEvent::Mount, None);
    let Some(DispatcharrCmd::Load { lineup, reply }) = command(out) else { panic!("mount asks for the stored account") };
    assert!(!lineup, "the boot page has no use for the lineup");
    reply.send(SetupOutcome::SignedOut).unwrap();
    step(&mut screen, tick(16_000), None);
    assert_eq!(screen.phase, Phase::Form);
    screen
}

/// Type `s` into field `elem` the way a person does: OK raises the keyboard, the panel commits.
fn type_into(screen: &mut SetupScreen, elem: u32, s: &str) {
    if screen.editing != Some(elem as usize) {
        step(screen, ScreenEvent::Activate(elem), Some(elem));
    }
    step(screen, text(s), Some(elem));
}

#[test]
fn every_sign_in_failure_names_its_own_fix() {
    let errors = [
        Error::BadAddress, Error::Unreachable, Error::NotDispatcharr, Error::Unauthorized,
        Error::Forbidden, Error::TooManyRequests, Error::Unavailable,
    ];
    let texts: Vec<String> = errors.iter().map(|e| failure_text(&Failure::Sign(e.clone())).into_owned()).collect();
    for (i, a) in texts.iter().enumerate() {
        for b in &texts[i + 1..] {
            assert_ne!(a, b, "two different failures read the same");
        }
    }
    // A 404 on the version probe is "not a Dispatcharr", not a missing page.
    assert_eq!(failure_text(&Failure::Sign(Error::NotFound)), failure_text(&Failure::Sign(Error::NotDispatcharr)));
    // The technical detail rides inside the app's own sentence.
    assert!(failure_text(&Failure::Sign(Error::Http(500))).contains("HTTP 500"));
}

#[test]
fn only_an_https_address_is_free_of_the_plaintext_note() {
    for plain in ["192.168.1.10", "192.168.1.10:9191", "http://tv.lan", "HTTP://tv.lan:80"] {
        assert!(address_is_plaintext(plain), "{plain}");
    }
    for quiet in ["", "  ", "https://tv.example.com", "HTTPS://tv.example.com:8443"] {
        assert!(!address_is_plaintext(quiet), "{quiet:?}");
    }
}

#[test]
fn the_caret_blink_redraws_on_each_flip_and_goes_quiet_at_rest() {
    let mut blink = 0;
    assert!(!step_blink(&mut blink, true, BLINK_US - 1));
    assert!(step_blink(&mut blink, true, 1), "the off phase starts");
    assert!(!step_blink(&mut blink, true, BLINK_US - 1));
    assert!(step_blink(&mut blink, true, 1), "and the on phase again");
    for _ in 0..100 {
        assert!(!step_blink(&mut blink, false, 16_000), "a field nobody types in asks for no frames");
    }
    assert_eq!(blink, 0);
}

#[test]
fn a_page_at_rest_asks_for_no_frames() {
    let mut screen = form();
    // Settle the focus springs on the address field, then step on: nothing moves, nothing presents.
    let m = FixtureMeasure;
    for _ in 0..600 {
        let mut out = Vec::new();
        let mut present = Present::new();
        let mut fx = Effects::from_handle(&mut out, MachineId::Instance(InstanceId(1)), PresentHandle::of(&mut present));
        screen.step(&tick(16_000), &cx(&m, Some(ADDRESS)), &mut fx);
    }
    let mut out = Vec::new();
    let mut present = Present::new();
    // A fresh gate is dirty ("the first frame always draws"): take that frame first.
    present.take(0);
    {
        let mut fx = Effects::from_handle(&mut out, MachineId::Instance(InstanceId(1)), PresentHandle::of(&mut present));
        screen.step(&tick(16_000), &cx(&m, Some(ADDRESS)), &mut fx);
    }
    assert!(screen.hot[0].pos > 0.99, "the focused field's spring arrived");
    assert!(!present.changed(), "a settled form with no keyboard up must not keep the loop awake");
}

#[test]
fn sign_in_with_a_field_left_empty_says_so_and_sends_nothing() {
    let mut screen = form();
    type_into(&mut screen, ADDRESS, "192.168.1.10");
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_IN), Some(SIGN_IN));
    assert!(command(out).is_none());
    assert_eq!(screen.failure, Some(Failure::Missing));
    assert_eq!(screen.phase, Phase::Form);
}

#[test]
fn ok_on_the_panel_walks_the_fields_with_the_keyboard_kept_up() {
    let mut screen = form();
    let (_, out) = step(&mut screen, ScreenEvent::Activate(ADDRESS), Some(ADDRESS));
    assert_eq!(keyboard_moves(&out), [true], "OK on a field raises the keyboard");
    step(&mut screen, text("192.168.1.10"), Some(ADDRESS));
    let (_, out) = step(&mut screen, key(Key::Ok), Some(ADDRESS));
    assert_eq!(reseated_on(&out), Some(USERNAME));
    assert!(keyboard_moves(&out).is_empty(), "the panel stays up between fields");
    assert_eq!(screen.editing, Some(1));
    step(&mut screen, ScreenEvent::FocusMoved { from: None, to: FocusKey { entry: EntryId(1), elem: USERNAME }, by: By::Dir }, Some(USERNAME));
    step(&mut screen, text("andrew"), Some(USERNAME));
    step(&mut screen, key(Key::Ok), Some(USERNAME));
    step(&mut screen, text("hunter2"), Some(PASSWORD));
    let (_, out) = step(&mut screen, key(Key::Ok), Some(PASSWORD));
    assert_eq!(reseated_on(&out), Some(SIGN_IN));
    assert_eq!(keyboard_moves(&out), [false], "after the password the panel closes");
    assert_eq!(screen.fields[0].text(), "192.168.1.10");
    assert_eq!(screen.fields[1].text(), "andrew");
    assert_eq!(screen.fields[2].text(), "hunter2");
}

#[test]
fn the_recorded_state_never_carries_the_password() {
    let mut screen = form();
    type_into(&mut screen, PASSWORD, "hunter2");
    let mut probe = String::new();
    screen.state.probe(&mut probe);
    let mut canon = Canon::new();
    screen.state.write(&mut canon);
    assert!(!probe.contains("hunter2"));
    assert_eq!(screen.state.password, (7, 7));
}

#[test]
fn a_refused_sign_in_seats_on_the_password_and_keeps_what_was_typed() {
    let mut screen = form();
    type_into(&mut screen, ADDRESS, "192.168.1.10");
    type_into(&mut screen, USERNAME, "andrew");
    type_into(&mut screen, PASSWORD, "wrong");
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_IN), Some(SIGN_IN));
    let Some(DispatcharrCmd::SignIn { address, username, password, reply }) = command(out) else { panic!("signs in") };
    assert_eq!((address.as_str(), username.as_str(), password.as_str()), ("192.168.1.10", "andrew", "wrong"));
    assert_eq!(screen.phase, Phase::Working);
    reply.send(SetupOutcome::Failed(Error::Unauthorized)).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), Some(SIGN_IN));
    assert_eq!(screen.phase, Phase::Form);
    assert_eq!(reseated_on(&out), Some(PASSWORD));
    assert_eq!(screen.fields[1].text(), "andrew");
}

#[test]
fn an_unreachable_server_seats_on_the_address() {
    let mut screen = form();
    type_into(&mut screen, ADDRESS, "10.0.0.99");
    type_into(&mut screen, USERNAME, "u");
    type_into(&mut screen, PASSWORD, "p");
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_IN), Some(SIGN_IN));
    let Some(DispatcharrCmd::SignIn { reply, .. }) = command(out) else { panic!() };
    reply.send(SetupOutcome::Failed(Error::Unreachable)).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), Some(SIGN_IN));
    assert_eq!(reseated_on(&out), Some(ADDRESS));
}

#[test]
fn a_successful_sign_in_wipes_the_password_and_hands_over_to_the_guide() {
    let mut screen = form();
    type_into(&mut screen, ADDRESS, "192.168.1.10");
    type_into(&mut screen, USERNAME, "andrew");
    type_into(&mut screen, PASSWORD, "hunter2");
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_IN), Some(SIGN_IN));
    let Some(DispatcharrCmd::SignIn { reply, .. }) = command(out) else { panic!() };
    reply.send(SetupOutcome::Connected { record: record(), saved: Saved::Durable, resume_on_launch: true }).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), Some(SIGN_IN));
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrGuide));
    assert_eq!(screen.fields[2].text(), "", "the password is gone once it has signed in");
    assert_ne!(screen.phase, Phase::Connected, "the boot page never shows the Server page's face");
}

#[test]
fn a_stored_account_at_boot_goes_straight_to_the_guide() {
    let mut screen = SetupScreen::new(EntryId(1), InstanceId(1), Mode::Entry);
    let (_, out) = step(&mut screen, ScreenEvent::Mount, None);
    let Some(DispatcharrCmd::Load { reply, .. }) = command(out) else { panic!() };
    reply.send(SetupOutcome::Connected { record: record(), saved: Saved::Durable, resume_on_launch: true }).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), None);
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrGuide));
    assert_eq!(screen.phase, Phase::Loading, "nothing is drawn for the frame the hand-over takes");
}

/// A Server page with the stored account landed.
fn server_page() -> (SetupScreen, std::sync::mpsc::Sender<SetupOutcome>) {
    let mut screen = SetupScreen::new(EntryId(1), InstanceId(1), Mode::Server);
    let (_, out) = step(&mut screen, ScreenEvent::Mount, None);
    let Some(DispatcharrCmd::Load { lineup, reply }) = command(out) else { panic!() };
    assert!(lineup, "the Server page shows the lineup");
    reply.send(SetupOutcome::Connected { record: record(), saved: Saved::Durable, resume_on_launch: true }).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), None);
    assert_eq!(screen.phase, Phase::Connected);
    assert_eq!(reseated_on(&out), Some(RESUME), "the page opens on the switch, never on Sign out");
    (screen, reply)
}

#[test]
fn the_start_on_the_last_channel_switch_flips_and_is_saved() {
    let (mut screen, _reply) = server_page();
    assert!(screen.connected.as_ref().unwrap().resume);
    assert_eq!(screen.resume_label(), msg::dispatcharr_setup_resume_on_c());
    let (_, out) = step(&mut screen, ScreenEvent::Activate(RESUME), Some(RESUME));
    assert!(matches!(command(out), Some(DispatcharrCmd::SetResume { on: false })));
    assert_eq!(screen.resume_label(), msg::dispatcharr_setup_resume_off_c());
    let (_, out) = step(&mut screen, ScreenEvent::Activate(RESUME), Some(RESUME));
    assert!(matches!(command(out), Some(DispatcharrCmd::SetResume { on: true })));
}

#[test]
fn the_switch_walks_down_to_sign_out() {
    let (screen, _reply) = server_page();
    assert_eq!(screen.elems(), &[RESUME, SIGN_OUT]);
}

#[test]
fn the_server_page_names_the_account_then_the_lineup_when_it_lands() {
    let (mut screen, reply) = server_page();
    let copy = screen.connected.as_ref().unwrap().lines().join(" | ");
    assert!(copy.contains("http://192.168.1.10:9191") && copy.contains("andrew"), "{copy}");
    assert!(!copy.contains("channel"), "no count before the lineup lands: {copy}");
    reply.send(SetupOutcome::Lineup(Ok(42))).unwrap();
    step(&mut screen, tick(16_000), Some(SIGN_OUT));
    assert_eq!(screen.connected.as_ref().unwrap().lines()[1], "42 channels");
    // BACK returns to the Guide, and the crumb says so.
    let m = FixtureMeasure;
    assert_eq!(<SetupScreen as Screen<SetupHost>>::crumb(&screen, &cx(&m, None)).as_deref(), Some(msg::dispatcharr_guide_title()));
    let (_, out) = step(&mut screen, key(Key::Back), Some(SIGN_OUT));
    assert!(out.iter().any(|s| matches!(s.fx, Fx::Nav(NavOp::Pop))));
}

#[test]
fn a_save_that_ends_at_reboot_says_so() {
    let c = Connected { server: "http://h:9191".into(), username: "u".into(), lineup: Lineup::Channels(1), resume: true,
        saved: Saved::UntilReboot };
    assert_eq!(c.lines()[1], "1 channel");
    assert_eq!(c.lines()[2], msg::dispatcharr_setup_until_reboot());
}

#[test]
fn signing_out_hands_the_app_back_to_the_boot_page() {
    let (mut screen, _) = server_page();
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_OUT), Some(SIGN_OUT));
    let Some(DispatcharrCmd::SignOut { reply }) = command(out) else { panic!("signs out") };
    reply.send(SetupOutcome::SignedOut).unwrap();
    let (_, out) = step(&mut screen, tick(16_000), Some(SIGN_OUT));
    assert_eq!(loop_req(&out), Some(LoopReq::DispatcharrSetup));
}

#[test]
fn a_worker_that_never_answers_ends_the_wait() {
    let mut screen = form();
    type_into(&mut screen, ADDRESS, "h");
    type_into(&mut screen, USERNAME, "u");
    type_into(&mut screen, PASSWORD, "p");
    let (_, out) = step(&mut screen, ScreenEvent::Activate(SIGN_IN), Some(SIGN_IN));
    drop(command(out));
    step(&mut screen, tick(16_000), Some(SIGN_IN));
    assert_eq!(screen.phase, Phase::Form);
    assert_eq!(screen.failure, Some(Failure::NoAnswer));
}

#[test]
fn back_off_the_keyboard_closes_it_and_back_on_the_page_leaves_the_app() {
    let mut screen = form();
    step(&mut screen, ScreenEvent::Activate(ADDRESS), Some(ADDRESS));
    let (_, out) = step(&mut screen, key(Key::Back), Some(ADDRESS));
    assert_eq!(keyboard_moves(&out), [false]);
    assert!(!out.iter().any(|s| matches!(s.fx, Fx::App(AppFx::Loop(LoopReq::BackAtRoot)))));
    let (_, out) = step(&mut screen, key(Key::Back), Some(ADDRESS));
    assert!(out.iter().any(|s| matches!(s.fx, Fx::App(AppFx::Loop(LoopReq::BackAtRoot)))));
}

/// **Every control label and field label fits, in every shipped language**, measured with the
/// device's whole-pixel advances: the pills against the column they stand in, the labels against
/// the field's width.
#[test]
fn the_setup_labels_fit_in_every_language() {
    use plx_base::fontcov::advances::ShippedMeasure;
    use plx_platform::i18n::{language_on_this_thread_for_test, SHIPPED};
    use plx_ui::fit::HEADROOM;
    let column = layout::column();
    let narrative = RouteLayout::screen().narrative;
    let mut out = Vec::new();
    for language in SHIPPED {
        let _guard = language_on_this_thread_for_test(language);
        for label in [msg::dispatcharr_setup_sign_in_c(), msg::dispatcharr_setup_sign_out_c()] {
            let w = Button::pill_w_measured(label, theme::size::BODY, false, false, &ShippedMeasure);
            if w > column.w.min(narrative.w) * HEADROOM {
                out.push(format!("{}: pill {label:?} is {w:.0}px", language.tag()));
            }
        }
        for label in [msg::dispatcharr_setup_address_c(), msg::dispatcharr_setup_username_c(),
            msg::dispatcharr_setup_password_c(), msg::dispatcharr_setup_plaintext_c()] {
            let w = plx_machine::machine::Measure::width(&ShippedMeasure, label, theme::size::CAPTION, false);
            if w > column.w * HEADROOM {
                out.push(format!("{}: label {label:?} is {w:.0}px", language.tag()));
            }
        }
        for placeholder in [msg::dispatcharr_setup_empty_field_c(), msg::dispatcharr_setup_address_example_c()] {
            let w = plx_machine::machine::Measure::width(&ShippedMeasure, placeholder, theme::size::TITLE, false);
            if w > column.w * HEADROOM {
                out.push(format!("{}: placeholder {placeholder:?} is {w:.0}px", language.tag()));
            }
        }
        // The title's longest word must fit the narrative column (it wraps between words).
        for title in [msg::dispatcharr_setup_title(), msg::dispatcharr_setup_connected()] {
            for word in title.split_whitespace() {
                let w = plx_machine::machine::Measure::width_str(&ShippedMeasure, word, theme::size::HERO, true);
                if w > narrative.w * HEADROOM {
                    out.push(format!("{}: title word {word:?} is {w:.0}px", language.tag()));
                }
            }
        }
    }
    assert!(out.is_empty(), "setup text that does not fit:\n  {}", out.join("\n  "));
}
