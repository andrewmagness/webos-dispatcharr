//! **The live player's one pure input policy**: what a key does while the terminal failure
//! read-out owns the frame. Pure, so the host suite grades it without a player.

/// A key, as far as the failure read-out cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailedKey {
    Ok,
    Back,
    Left,
    Right,
    Other,
}

/// What a key does while the player is showing its terminal failure read-out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailedKeyAction {
    /// Move the row's focus to this control.
    Focus(usize),
    /// Perform this control of the row (`player::failure_actions` order).
    Activate(usize),
    /// BACK leaves the failed playback (or closes whatever panel is over it).
    Return,
    /// Everything else is swallowed by the read-out.
    Ignore,
}

/// The failure read-out's key policy, pure so it can be graded without a player: LEFT/RIGHT walk
/// the row (no wrap), OK performs the focused control, BACK leaves. `n` is the row's length.
pub fn failed_key_action(key: FailedKey, sel: usize, n: usize) -> FailedKeyAction {
    let last = n.saturating_sub(1);
    let sel = sel.min(last);
    match key {
        FailedKey::Ok if n > 0 => FailedKeyAction::Activate(sel),
        FailedKey::Back => FailedKeyAction::Return,
        FailedKey::Left if sel > 0 => FailedKeyAction::Focus(sel - 1),
        FailedKey::Right if sel < last => FailedKeyAction::Focus(sel + 1),
        _ => FailedKeyAction::Ignore,
    }
}

#[cfg(test)]
mod failed_player_input_tests {
    use super::{failed_key_action, FailedKey, FailedKeyAction};

    #[test]
    fn a_terminal_failure_walks_its_row_performs_the_focused_control_and_backs_out() {
        assert_eq!(failed_key_action(FailedKey::Ok, 0, 3), FailedKeyAction::Activate(0));
        assert_eq!(failed_key_action(FailedKey::Ok, 2, 3), FailedKeyAction::Activate(2));
        assert_eq!(failed_key_action(FailedKey::Right, 0, 3), FailedKeyAction::Focus(1));
        assert_eq!(failed_key_action(FailedKey::Right, 2, 3), FailedKeyAction::Ignore, "no wrap");
        assert_eq!(failed_key_action(FailedKey::Left, 0, 3), FailedKeyAction::Ignore, "no wrap");
        assert_eq!(failed_key_action(FailedKey::Left, 2, 3), FailedKeyAction::Focus(1));
        // a stale selection past a row that shrank clamps to its last control
        assert_eq!(failed_key_action(FailedKey::Ok, 5, 2), FailedKeyAction::Activate(1));
        assert_eq!(failed_key_action(FailedKey::Back, 1, 3), FailedKeyAction::Return);
        assert_eq!(failed_key_action(FailedKey::Other, 1, 3), FailedKeyAction::Ignore);
    }
}
