//! Version vectors: `device → counter`. They decide ordering between versions
//! of a file; wall-clock time never does (invariant 5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// How two versions relate causally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Causality {
    Equal,
    /// `self` has seen everything `other` has, and more.
    Newer,
    /// `other` has seen everything `self` has, and more.
    Older,
    /// Each side has changes the other has not seen.
    Concurrent,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<DeviceId, u64>);

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, device: &DeviceId) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.0.iter()
    }

    /// The vector after a local change on `device`.
    #[must_use]
    pub fn incremented(&self, device: DeviceId) -> Self {
        let mut out = self.clone();
        *out.0.entry(device).or_insert(0) += 1;
        out
    }

    /// Element-wise maximum: a version that has seen both.
    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (d, &c) in &other.0 {
            let e = out.0.entry(*d).or_insert(0);
            *e = (*e).max(c);
        }
        out
    }

    pub fn compare(&self, other: &Self) -> Causality {
        let mut self_ahead = false;
        let mut other_ahead = false;
        for d in self.0.keys().chain(other.0.keys()) {
            let (a, b) = (self.get(d), other.get(d));
            self_ahead |= a > b;
            other_ahead |= b > a;
        }
        match (self_ahead, other_ahead) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Newer,
            (false, true) => Causality::Older,
            (true, true) => Causality::Concurrent,
        }
    }

    /// `self` has seen everything in `other` (newer or equal).
    pub fn dominates(&self, other: &Self) -> bool {
        matches!(self.compare(other), Causality::Newer | Causality::Equal)
    }
}

impl<const N: usize> From<[(DeviceId, u64); N]> for VersionVector {
    fn from(entries: [(DeviceId, u64); N]) -> Self {
        Self(entries.into_iter().filter(|(_, c)| *c > 0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    #[test]
    fn compare_cases() {
        let a = VersionVector::new().incremented(d(1));
        let b = a.incremented(d(2));
        let c = a.incremented(d(3));
        assert_eq!(a.compare(&a), Causality::Equal);
        assert_eq!(b.compare(&a), Causality::Newer);
        assert_eq!(a.compare(&b), Causality::Older);
        assert_eq!(b.compare(&c), Causality::Concurrent);
        let m = b.merged(&c);
        assert_eq!(m.compare(&b), Causality::Newer);
        assert_eq!(m.compare(&c), Causality::Newer);
        assert_eq!(m, VersionVector::from([(d(1), 1), (d(2), 1), (d(3), 1)]));
        assert_eq!(
            VersionVector::new().compare(&VersionVector::new()),
            Causality::Equal
        );
    }

    /// Tiny deterministic PRNG so the property test needs no dependency.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    #[test]
    fn compare_is_antisymmetric_and_merge_dominates() {
        let mut s = 1;
        for _ in 0..2000 {
            let mut make = || {
                let mut v = VersionVector::new();
                for _ in 0..lcg(&mut s) % 6 {
                    v = v.incremented(d((lcg(&mut s) % 4) as u8));
                }
                v
            };
            let (x, y) = (make(), make());
            let flipped = match x.compare(&y) {
                Causality::Newer => Causality::Older,
                Causality::Older => Causality::Newer,
                other => other,
            };
            assert_eq!(y.compare(&x), flipped);
            let m = x.merged(&y);
            assert!(m.dominates(&x) && m.dominates(&y));
            assert_eq!(m, y.merged(&x));
        }
    }
}
