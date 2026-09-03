//! The CLI's random source for the `random` capability (D-M7-16).
//!
//! A tiny hand-rolled [SplitMix64] PRNG (zero dependencies, matching the audited minimal-dep house
//! style). It is **not** the engine's RNG — the engine has none; `random` is a suspending
//! capability, and this is the CLI's *host resolution policy* for it (E§5.3/§11). The stream is a
//! pure function of the seed, so `doodle run --seed N` replays bit-for-bit across CLI builds;
//! without `--seed` the CLI seeds from the wall clock (entropy-ish, non-reproducible by design;
//! a recorded run replays from its captured resolutions, not by re-seeding, E§11).
//!
//! [SplitMix64]: the finalizer from the SplitMix64 generator (Steele, Lea & Flood 2014), a
//! well-distributed 64-bit-state counter generator adequate for a teaching language's `random`.

/// A SplitMix64 random-number generator: 64 bits of state advanced by a fixed odd increment, each
/// output run through the SplitMix64 finalizing mix. Deterministic given its seed.
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator seeded with `seed` (the `--seed N` value, or a wall-clock reading for the
    /// default entropy seed). Every seed yields a distinct, reproducible stream.
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// The next 64-bit output (SplitMix64: advance the state by the golden-ratio odd increment,
    /// then finalize with the two-xorshift-multiply mix).
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform `f64` in `[0, 1)`: the top 53 bits of a fresh 64-bit output divided by 2^53, so
    /// every representable double in the unit interval's 53-bit grid is reachable and 1.0 never is.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn same_seed_same_stream() {
        // The `--seed N` reproducibility contract: two generators seeded alike agree step for step.
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_f64().to_bits(), b.next_f64().to_bits());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        assert_ne!(a.next_f64().to_bits(), b.next_f64().to_bits());
    }

    #[test]
    fn draws_stay_in_the_unit_interval() {
        let mut r = Rng::new(0);
        for _ in 0..10_000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x), "draw {x} outside [0, 1)");
        }
    }
}
