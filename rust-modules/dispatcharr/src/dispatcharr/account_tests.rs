//! The account file: what is written, where, and what a damaged or foreign file reads as.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::account::{self, Loaded, Record, Saved};
use super::api::Account;
use super::client::Server;

/// A fresh scratch directory and the candidate list `n` files in it, redirected for the test's
/// life; restored to the per-process default on drop.
struct Candidates {
    _serial: plx_base::testlock::Serial,
    dir: PathBuf,
    paths: Vec<PathBuf>,
}

impl Candidates {
    fn new(tag: &str, n: usize) -> Self {
        let serial = plx_base::testlock::serial();
        let dir = plx_base::testscratch::process_dir(&format!("dispatcharr-account-{tag}"));
        let paths: Vec<PathBuf> = (0..n).map(|i| dir.join(format!("candidate-{i}.json"))).collect();
        account::redirect_for_test(Some(durable(&paths)));
        Self { _serial: serial, dir, paths }
    }
}

impl Drop for Candidates {
    fn drop(&mut self) {
        account::redirect_for_test(None);
    }
}

fn durable(paths: &[PathBuf]) -> Vec<(PathBuf, bool)> {
    paths.iter().map(|p| (p.clone(), true)).collect()
}

fn record(key: &str) -> Record {
    Record {
        account: Account { server: Server::parse("192.168.1.10").unwrap(), api_key: key.into() },
        username: "andrew".into(),
    }
}

#[test]
fn nothing_saved_loads_as_none() {
    let _c = Candidates::new("none", 2);
    assert_eq!(account::load(), Loaded::None);
}

#[test]
fn a_saved_record_loads_back_whole() {
    let _c = Candidates::new("roundtrip", 2);
    assert_eq!(account::save(&record("KEY")), Ok(Saved::Durable));
    assert_eq!(account::load(), Loaded::Signed(record("KEY"), Saved::Durable));
}

#[test]
fn the_file_is_private_to_this_user() {
    let c = Candidates::new("mode", 1);
    account::save(&record("KEY")).unwrap();
    let mode = std::fs::metadata(&c.paths[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// **A dev-mode reboot widens the file, and loading it narrows it back.** LG's own
/// `/lib/systemd/system/scripts/devmode.sh` runs `chmod -R 777 /media/developer` on every boot of a
/// set in developer mode (found on the dev set, 2026-10-17: 533 of 559 entries there shared one
/// ctime just after a reboot, all 0777). The account is written only at sign-in, so until this the
/// API key sat world-readable from the first reboot on. It still loads — refusing it would sign the
/// owner out after every reboot — and the read leaves it 0600 again.
#[test]
fn a_file_a_reboot_widened_is_read_and_left_private_again() {
    let c = Candidates::new("widened", 1);
    account::save(&record("KEY")).unwrap();
    std::fs::set_permissions(&c.paths[0], std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(account::load(), Loaded::Signed(record("KEY"), Saved::Durable));
    let mode = std::fs::metadata(&c.paths[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the read must leave the API key private again");
}

#[test]
fn the_server_is_stored_as_its_normalised_base() {
    let c = Candidates::new("base", 1);
    account::save(&record("KEY")).unwrap();
    let text = std::fs::read_to_string(&c.paths[0]).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["server"], "http://192.168.1.10:9191");
    assert_eq!(value["format"], "dispatcharr-account");
}

#[test]
fn an_unwritable_first_candidate_falls_through_to_the_next() {
    let c = Candidates::new("fallthrough", 2);
    // The first candidate's parent does not exist, as `/media/developer` does not under the
    // production jail.
    let missing = c.dir.join("absent").join("dispatcharr.json");
    account::redirect_for_test(Some(durable(&[missing.clone(), c.paths[1].clone()])));
    account::save(&record("KEY")).unwrap();
    assert!(!missing.exists());
    assert!(c.paths[1].exists());
    assert_eq!(account::load(), Loaded::Signed(record("KEY"), Saved::Durable));
}

#[test]
fn a_save_removes_the_copy_at_every_other_candidate() {
    let c = Candidates::new("retire", 2);
    // An older sign-in at the LOWER-priority path, and a new one written at the higher: only the
    // new one may survive, or clearing the higher one would resurrect the old account.
    account::redirect_for_test(Some(durable(&c.paths[1..])));
    account::save(&record("OLD")).unwrap();
    account::redirect_for_test(Some(durable(&c.paths)));
    account::save(&record("NEW")).unwrap();
    assert!(c.paths[0].exists());
    assert!(!c.paths[1].exists(), "the stale account must not outlive the new save");
}

#[test]
fn a_save_that_lands_only_on_the_reboot_swept_fallback_says_so() {
    let c = Candidates::new("volatile", 2);
    let missing = c.dir.join("absent").join("dispatcharr.json");
    account::redirect_for_test(Some(vec![(missing, true), (c.paths[1].clone(), false)]));
    assert_eq!(account::save(&record("KEY")), Ok(Saved::UntilReboot));
    // And it reads back saying so.
    assert_eq!(account::load(), Loaded::Signed(record("KEY"), Saved::UntilReboot));
}

#[test]
fn clear_signs_out_everywhere() {
    let c = Candidates::new("clear", 2);
    std::fs::write(&c.paths[1], b"{}").unwrap();
    account::save(&record("KEY")).unwrap();
    account::clear();
    assert_eq!(account::load(), Loaded::None);
    assert!(c.paths.iter().all(|p| !p.exists()));
}

#[test]
fn a_damaged_or_foreign_file_is_unreadable_not_an_account() {
    let c = Candidates::new("damaged", 1);
    let bad: [&[u8]; 5] = [
        b"{\"format\":\"dispatcharr-account\",\"vers",
        br#"{"format":"dispatcharrtv-session","version":1,"server":"http://h:1","username":"u","api_key":"k"}"#,
        br#"{"format":"dispatcharr-account","version":2,"server":"http://h:1","username":"u","api_key":"k"}"#,
        br#"{"format":"dispatcharr-account","version":1,"server":"not a server","username":"u","api_key":"k"}"#,
        br#"{"format":"dispatcharr-account","version":1,"server":"http://h:1","username":"u","api_key":""}"#,
    ];
    for bytes in bad {
        std::fs::write(&c.paths[0], bytes).unwrap();
        assert_eq!(account::load(), Loaded::Unreadable, "{}", String::from_utf8_lossy(bytes));
    }
}

#[test]
fn a_symlink_at_the_account_path_is_not_followed() {
    let c = Candidates::new("symlink", 1);
    let elsewhere = c.dir.join("elsewhere.json");
    account::redirect_for_test(Some(durable(&[elsewhere.clone()])));
    account::save(&record("KEY")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &c.paths[0]).unwrap();
    account::redirect_for_test(Some(durable(&c.paths)));
    assert_eq!(account::load(), Loaded::Unreadable);
}

#[test]
fn the_debug_form_never_carries_the_key() {
    let shown = format!("{:?}", record("SECRETKEY"));
    assert!(!shown.contains("SECRETKEY"), "{shown}");
}
