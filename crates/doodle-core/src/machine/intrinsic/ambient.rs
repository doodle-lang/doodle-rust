//! The **ambient nondeterministic capabilities** `time` and `random` (engine spec E§5.3/§11):
//! a clock read and a random draw. Unlike a synchronous native (`sin`, `length`), an ambient
//! read **must** be a suspending capability ([`ForeignBody::Capability`]) — its value comes from
//! outside the instance's own state, so it has to cross the recordable resolution boundary (E§11,
//! S-19/D-M7-4): the host resolves the request, and that resolution is exactly what a recording
//! captures and a replay feeds back, keeping execution bit-for-bit reproducible. An engine that
//! read the wall clock or an RNG inline would leak nondeterminism no recording could pin down.
//!
//! Both are zero-argument `fn`s (the [`read_line`](super::read_line) shape): they suspend, and the
//! host supplies the value via `resolve(Value)`. What that value *is* (real wall clock vs. a fixed
//! time, entropy-seeded vs. a `--seed` stream) is entirely the host's resolution **policy** (the
//! CLI's, D-M7-16); the engine fixes only that each is a suspending request whose result the host
//! provides. The values a host resolves with are ordinary numbers the program then uses.

use super::{ForeignBody, Intrinsic};
use crate::resolve::BodyKind;

/// The ambient capability `time()` (E§5.3): a zero-argument `fn` that **suspends** so the host
/// resolves it with the current time (its unit and epoch are the host's policy; the CLI uses real
/// wall-clock seconds, D-M7-16). A capability, not a sync native, because a clock read is
/// nondeterministic and must cross the recordable boundary (E§11) to stay replayable.
pub fn time() -> Intrinsic {
    Intrinsic {
        name: "time".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Capability,
    }
}

/// The ambient capability `random()` (E§5.3): a zero-argument `fn` that **suspends** so the host
/// resolves it with a random draw (the CLI resolves a `Float` in `[0, 1)` from its RNG, seedable
/// with `--seed` for a reproducible stream, D-M7-16). A capability, not a sync native, because the
/// draw is nondeterministic and must cross the recordable boundary (E§11) to stay replayable — a
/// seeded stream then replays as the recorded resolutions, not as an engine-side PRNG.
pub fn random() -> Intrinsic {
    Intrinsic {
        name: "random".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Capability,
    }
}
