// Call moderation + join-policy rules, mirrored from the server (Lurker #680).
//
// The server is the authority: /api/voice/moderate needs q/a/o/h and
// /api/voice/policy (PUT) needs q/a/o. This module restates those two rules so
// the client can *hide* controls the user cannot use, rather than offering a
// button that only ever returns 403. Pure and unit-tested, like nickmenu.rs —
// and pinned against the server's letter sets so a drift is a failing test
// rather than a mystery 403 in the field.

use crate::nickmenu::Rank;

/// May mute or remove someone from a call: halfop and up (server:
/// `MODERATE_LETTERS = q a o h`).
pub fn can_moderate_call(mine: Rank) -> bool {
    mine >= Rank::Halfop
}

/// May set a channel's call join policy: op and up (server:
/// `OP_LETTERS = q a o`). Deliberately stricter than moderation — policy
/// outlives any single call.
pub fn can_set_policy(mine: Rank) -> bool {
    mine >= Rank::Op
}

/// The minimum channel status required to join a channel's call. Wire values
/// are exactly these lowercase strings; unknown input folds to `None` (anyone),
/// matching the server's `normalizeMinJoinMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MinJoin {
    None,
    Voice,
    Halfop,
    Op,
}

impl MinJoin {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "voice" => Self::Voice,
            "halfop" => Self::Halfop,
            "op" => Self::Op,
            // Includes "none" and anything unrecognized — the server normalizes
            // unknown values to 'none', so we must agree or the UI would show a
            // restriction the server isn't enforcing.
            _ => Self::None,
        }
    }

    pub fn to_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Voice => "voice",
            Self::Halfop => "halfop",
            Self::Op => "op",
        }
    }

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Anyone in the channel",
            Self::Voice => "Voiced (+v) and above",
            Self::Halfop => "Halfops (+h) and above",
            Self::Op => "Operators (+o) and above",
        }
    }

    /// Every value, in escalating order — the order the policy menu shows.
    pub const ALL: [MinJoin; 4] = [MinJoin::None, MinJoin::Voice, MinJoin::Halfop, MinJoin::Op];

    /// Whether a member of `mine` rank may join a call under this policy. The
    /// server enforces this at token-mint time (403); the client uses it to
    /// explain *why* the call button is unavailable instead of failing blankly.
    pub fn permits(self, mine: Rank) -> bool {
        mine >= self.required_rank()
    }

    pub fn required_rank(self) -> Rank {
        match self {
            Self::None => Rank::None,
            Self::Voice => Rank::Voice,
            Self::Halfop => Rank::Halfop,
            Self::Op => Rank::Op,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moderation_is_halfop_and_up() {
        // Mirrors the server's MODERATE_LETTERS = {q,a,o,h}.
        for r in [Rank::Halfop, Rank::Op, Rank::Admin, Rank::Owner] {
            assert!(can_moderate_call(r), "{r:?} should moderate");
        }
        for r in [Rank::None, Rank::Voice] {
            assert!(!can_moderate_call(r), "{r:?} should not moderate");
        }
    }

    #[test]
    fn setting_policy_is_op_and_up_stricter_than_moderation() {
        // Mirrors OP_LETTERS = {q,a,o}: a halfop may mute, but may not rewrite
        // the policy — the asymmetry is the point.
        for r in [Rank::Op, Rank::Admin, Rank::Owner] {
            assert!(can_set_policy(r), "{r:?} should set policy");
        }
        for r in [Rank::None, Rank::Voice, Rank::Halfop] {
            assert!(!can_set_policy(r), "{r:?} should not set policy");
        }
        assert!(can_moderate_call(Rank::Halfop) && !can_set_policy(Rank::Halfop));
    }

    #[test]
    fn min_join_round_trips_and_folds_unknown_to_none() {
        for m in MinJoin::ALL {
            assert_eq!(MinJoin::from_wire(m.to_wire()), m);
        }
        // The server normalizes anything unrecognized to 'none'; disagreeing
        // would show a restriction that isn't actually enforced.
        assert_eq!(MinJoin::from_wire("none"), MinJoin::None);
        assert_eq!(MinJoin::from_wire("wizard"), MinJoin::None);
        assert_eq!(MinJoin::from_wire(""), MinJoin::None);
    }

    #[test]
    fn policy_permits_by_rank() {
        assert!(MinJoin::None.permits(Rank::None), "open call admits anyone");
        assert!(!MinJoin::Voice.permits(Rank::None));
        assert!(MinJoin::Voice.permits(Rank::Voice));
        assert!(MinJoin::Op.permits(Rank::Owner), "owner outranks an op-only policy");
        assert!(!MinJoin::Op.permits(Rank::Halfop));
    }
}
