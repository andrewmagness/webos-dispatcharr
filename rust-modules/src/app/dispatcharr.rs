//! Live executor for the Setup screen's account work (`AppFx::Dispatcharr`). Every command blocks —
//! on the account file, on the server, or both — so each runs on its own worker and answers on the
//! reply channel the screen holds, waking the loop as it does.
//!
//! The event log gets one `dispatcharr:` line per outcome and never the password or the key.

use plx_dispatcharr::dispatcharr::{account, api, auth, Server};
use plx_screens::registry::{DispatcharrCmd, SetupOutcome};
use std::sync::mpsc::Sender;

pub(super) fn execute(command: DispatcharrCmd) {
    // A refused spawn drops the reply sender; the screen reads the disconnect as "no answer"
    // rather than waiting on a worker that never started.
    match command {
        DispatcharrCmd::Load { reply } => {
            plx_base::task::spawn_small("dispatcharr account", move || load(&reply));
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
    }
}

fn answer(reply: &Sender<SetupOutcome>, outcome: SetupOutcome) {
    let _ = reply.send(outcome);
    plx_machine::idle::invalidate();
}

fn load(reply: &Sender<SetupOutcome>) {
    match account::load() {
        account::Loaded::None => answer(reply, SetupOutcome::SignedOut),
        account::Loaded::Unreadable => {
            // Not an account this build can use, and nothing to ask the person about: clear it so
            // the next save starts clean, and show the form.
            account::clear();
            plx_base::eventlog::log("dispatcharr: the stored account was unreadable and has been removed");
            answer(reply, SetupOutcome::SignedOut);
        }
        account::Loaded::Signed(record) => {
            plx_base::eventlog::log(&format!("dispatcharr: stored account for {}", record.account.server.base()));
            let account = record.account.clone();
            answer(reply, SetupOutcome::Connected { record, saved: None });
            lineup(&account, reply);
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
    let account = record.account.clone();
    answer(reply, SetupOutcome::Connected { record, saved: Some(saved) });
    lineup(&account, reply);
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
