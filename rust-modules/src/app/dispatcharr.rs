//! Live executor for the Setup and Guide screens' account and server work (`AppFx::Dispatcharr`).
//! Every command blocks — on the account file, on the server, or both — so each runs on its own
//! worker and answers on the reply channel the screen holds, waking the loop as it does.
//!
//! The event log gets one `dispatcharr:` line per outcome and never the password or the key.

use plx_dispatcharr::dispatcharr::{account, api, auth, prefs, Server};
use plx_screens::registry::{DispatcharrCmd, GuideOutcome, SetupOutcome};
use std::sync::mpsc::Sender;

pub(super) fn execute(command: DispatcharrCmd) {
    // A refused spawn drops the reply sender; the screen reads the disconnect as "no answer"
    // rather than waiting on a worker that never started.
    match command {
        DispatcharrCmd::Load { lineup, reply } => {
            plx_base::task::spawn_small("dispatcharr account", move || load(lineup, &reply));
        }
        DispatcharrCmd::SignIn { address, username, password, reply } => {
            plx_base::task::spawn_small("dispatcharr sign-in", move || sign_in(&address, &username, password, &reply));
        }
        DispatcharrCmd::SignOut { reply } => {
            plx_base::task::spawn_small("dispatcharr account", move || {
                account::clear();
                plx_base::eventlog::log("dispatcharr: signed out");
                answer(&reply, SetupOutcome::SignedOut);
            });
        }
        DispatcharrCmd::Guide { reply } => {
            plx_base::task::spawn_small("dispatcharr guide", move || guide(&reply));
        }
        DispatcharrCmd::SaveFavourites { favourites } => update_prefs(PrefsOp::Favourites(favourites)),
        DispatcharrCmd::SetResume { on } => update_prefs(PrefsOp::Resume(on)),
    }
}

/// One change to the viewer's stored choices ([`prefs`]).
enum PrefsOp {
    Favourites(Vec<String>),
    Last(String),
    Resume(bool),
}

/// The ordered writer of the viewer's choices: one worker reads the file, applies the change and
/// writes it, in the order the changes were made, so a last-channel write cannot put back the
/// favourites a newer save replaced (each op touches its own field of a fresh read).
static PREFS: std::sync::Mutex<Option<std::sync::mpsc::Sender<PrefsOp>>> = std::sync::Mutex::new(None);

fn update_prefs(op: PrefsOp) {
    let mut tx = PREFS.lock().unwrap_or_else(|e| e.into_inner());
    if tx.is_none() {
        let (send, recv) = std::sync::mpsc::channel::<PrefsOp>();
        if !plx_base::task::spawn_small("dispatcharr prefs", move || {
            for op in recv {
                apply_prefs(op);
            }
        }) {
            plx_base::eventlog::log("dispatcharr: no worker for the viewing preferences; change dropped");
            return;
        }
        *tx = Some(send);
    }
    if let Some(send) = tx.as_ref() {
        let _ = send.send(op);
    }
}

fn apply_prefs(op: PrefsOp) {
    let account::Loaded::Signed(record, _) = account::load() else { return };
    let server = record.account.server.base();
    let mut p = prefs::load(&server);
    let line = match op {
        PrefsOp::Favourites(favourites) => {
            p.favourites = favourites;
            format!("dispatcharr: favourites={}", p.favourites.len())
        }
        PrefsOp::Last(uuid) => {
            p.last = Some(uuid);
            "dispatcharr: last channel saved".to_owned()
        }
        PrefsOp::Resume(on) => {
            p.resume_on_launch = on;
            format!("dispatcharr: resume_on_launch={on}")
        }
    };
    if prefs::save(&server, &p).is_ok() {
        plx_base::eventlog::log(&line);
    }
}

/// The codecs each channel's stream turned out to carry this run, by UUID: a channel corrected once
/// (`player::live_codec_correction`) is declared right the next time it is tuned.
static CODECS: std::sync::Mutex<Option<std::collections::HashMap<String, (String, String)>>> =
    std::sync::Mutex::new(None);

pub(super) fn remember_codecs(uuid: &str, codecs: (String, String)) {
    let mut map = CODECS.lock().unwrap_or_else(|e| e.into_inner());
    map.get_or_insert_with(Default::default).insert(uuid.to_owned(), codecs);
}

/// `tune` with the codecs its channel was found to carry, when it has none of its own.
pub(super) fn with_known_codecs(mut tune: plx_media::route::LiveTune) -> plx_media::route::LiveTune {
    if tune.codecs.is_none() {
        if let Some(uuid) = tune.channel().map(|c| c.uuid.clone()) {
            let map = CODECS.lock().unwrap_or_else(|e| e.into_inner());
            tune.codecs = map.as_ref().and_then(|m| m.get(&uuid).cloned());
        }
    }
    tune
}

/// A live tune landed on channel `uuid`: it is the one the next launch starts on.
pub(super) fn remember_last(uuid: &str) {
    if !uuid.is_empty() {
        update_prefs(PrefsOp::Last(uuid.to_owned()));
    }
}

/// Whether this run's first guide has been answered: only that one may resume a channel, so coming
/// back to the Guide later never tunes on its own.
static RESUME_OFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The channel the first guide of this run starts on, if the viewer kept that on.
fn resume_for(p: &prefs::Prefs) -> Option<String> {
    if RESUME_OFFERED.swap(true, std::sync::atomic::Ordering::AcqRel) || !p.resume_on_launch {
        return None;
    }
    p.last.clone()
}

fn answer<T>(reply: &Sender<T>, outcome: T) {
    let _ = reply.send(outcome);
    plx_machine::idle::invalidate();
}

fn load(lineup_too: bool, reply: &Sender<SetupOutcome>) {
    match account::load() {
        account::Loaded::None => answer(reply, SetupOutcome::SignedOut),
        account::Loaded::Unreadable => {
            // Not an account this build can use, and nothing to ask the person about: clear it so
            // the next save starts clean, and show the form.
            account::clear();
            plx_base::eventlog::log("dispatcharr: the stored account was unreadable and has been removed");
            answer(reply, SetupOutcome::SignedOut);
        }
        account::Loaded::Signed(record, saved) => {
            plx_base::eventlog::log(&format!("dispatcharr: stored account for {}", record.account.server.base()));
            let account = record.account.clone();
            answer(reply, SetupOutcome::Connected { resume_on_launch: prefs::load(&record.account.server.base()).resume_on_launch, record, saved });
            if lineup_too {
                lineup(&account, reply);
            }
        }
    }
}

fn sign_in(address: &str, username: &str, password: String, reply: &Sender<SetupOutcome>) {
    let server = match Server::parse(address) {
        Ok(server) => server,
        Err(e) => {
            plx_base::eventlog::log(&format!("dispatcharr: address {address:?} not usable: {e}"));
            return answer(reply, SetupOutcome::Failed(e));
        }
    };
    let signed = auth::sign_in(&server, username, &password);
    drop(password);
    let signed = match signed {
        Ok(signed) => signed,
        Err(e) => {
            plx_base::eventlog::log(&format!("dispatcharr: sign-in to {} failed: {e}", server.base()));
            return answer(reply, SetupOutcome::Failed(e));
        }
    };
    let record = account::Record {
        account: api::Account { server: signed.server, api_key: signed.api_key },
        username: username.to_owned(),
    };
    let saved = match account::save(&record) {
        Ok(saved) => saved,
        Err(account::SaveFailed) => return answer(reply, SetupOutcome::SaveFailed),
    };
    plx_base::eventlog::log(&format!(
        "dispatcharr: signed in to {} version={} key={} saved={saved:?}",
        record.account.server.base(),
        signed.version.version,
        if signed.key_generated { "generated" } else { "reused" },
    ));
    // The boot page hands over to the Guide, which reads the lineup itself.
    answer(reply, SetupOutcome::Connected { resume_on_launch: prefs::load(&record.account.server.base()).resume_on_launch, record, saved });
}

/// Read the lineup behind `account` and answer with its size.
fn lineup(account: &api::Account, reply: &Sender<SetupOutcome>) {
    let channels = account.channels().map(|c| c.len());
    match &channels {
        Ok(n) => plx_base::eventlog::log(&format!("dispatcharr: channels={n}")),
        Err(e) => plx_base::eventlog::log(&format!("dispatcharr: channels failed: {e}")),
    }
    answer(reply, SetupOutcome::Lineup(channels));
}

/// The stored account's channels and the guide around now.
fn guide(reply: &Sender<GuideOutcome>) {
    let record = match account::load() {
        account::Loaded::Signed(record, _) => record,
        account::Loaded::None | account::Loaded::Unreadable => return answer(reply, GuideOutcome::SignedOut),
    };
    let account = &record.account;
    let read = account.channels().and_then(|channels| {
        let (guide, fetched_at) = account.guide_now(&channels)?;
        Ok((channels, guide, fetched_at))
    });
    match read {
        Ok((channels, guide, fetched_at)) => {
            plx_base::eventlog::log(&format!(
                "dispatcharr: guide channels={} with_programmes={}",
                channels.len(),
                guide.channels_with_programmes()
            ));
            let logos = channels.iter().map(|c| c.logo_id.map(|id| account.logo_url(id))).collect();
            let streams = channels.iter().map(|c| account.stream_url(c)).collect();
            let p = prefs::load(&account.server.base());
            let resume = resume_for(&p);
            answer(reply, GuideOutcome::Loaded { channels, logos, streams, favourites: p.favourites, resume, guide, fetched_at });
        }
        Err(e) => {
            plx_base::eventlog::log(&format!("dispatcharr: guide failed: {e}"));
            answer(reply, GuideOutcome::Failed(e));
        }
    }
}
