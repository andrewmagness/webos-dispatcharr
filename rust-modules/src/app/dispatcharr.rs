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
        DispatcharrCmd::SaveFavourites { favourites } => {
            let generation = SAVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            plx_base::task::spawn_small("dispatcharr prefs", move || save_favourites(generation, favourites));
        }
    }
}

/// Favourites saves issued, and the newest one written: each save carries the whole list, so a
/// worker that lost the race to a newer one writes nothing rather than putting the old list back.
static SAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITTEN: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

fn save_favourites(generation: u64, favourites: Vec<String>) {
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if generation < *written {
        return;
    }
    let account::Loaded::Signed(record, _) = account::load() else { return };
    let server = record.account.server.base();
    let mut p = prefs::load(&server);
    p.favourites = favourites;
    if prefs::save(&server, &p).is_ok() {
        *written = generation;
        plx_base::eventlog::log(&format!("dispatcharr: favourites={}", p.favourites.len()));
    }
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
            answer(reply, SetupOutcome::Connected { record, saved });
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
    answer(reply, SetupOutcome::Connected { record, saved });
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
            let favourites = prefs::load(&account.server.base()).favourites;
            answer(reply, GuideOutcome::Loaded { channels, logos, streams, favourites, guide, fetched_at });
        }
        Err(e) => {
            plx_base::eventlog::log(&format!("dispatcharr: guide failed: {e}"));
            answer(reply, GuideOutcome::Failed(e));
        }
    }
}
