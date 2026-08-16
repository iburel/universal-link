// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Version vectors (doc/sync-engine.md, section 3): components keyed
//! `node_id@gen`, so a wiped-and-rejoined device is a NEW component by
//! construction and can never alias values a partitioned member still
//! holds. Old generations simply stop growing: frozen history.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::records::valid_node_id;

/// Clock values stay within the canonical integer profile: entries travel
/// as plain JSON, and the same 2^53 bound applies wherever integers do.
const CLOCK_MAX: u64 = (1 << 53) - 1;

/// The component key of a device's generation: `node_id@gen`.
pub fn component(node_id: &str, generation: u64) -> String {
    format!("{node_id}@{generation}")
}

fn valid_component(key: &str) -> bool {
    let Some((node, generation)) = key.split_once('@') else {
        return false;
    };
    valid_node_id(node)
        && !generation.is_empty()
        && generation.len() <= 16
        && generation.bytes().all(|b| b.is_ascii_digit())
        && !(generation.len() > 1 && generation.starts_with('0'))
}

/// How two vectors relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Equal,
    /// `a` descends from `b`: `a` has seen everything `b` has, and more.
    Descends,
    /// `b` descends from `a`.
    Precedes,
    /// Neither covers the other: concurrent, the conflict case.
    Concurrent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vv(BTreeMap<String, u64>);

impl Vv {
    pub fn new() -> Vv {
        Vv::default()
    }

    pub fn get(&self, key: &str) -> u64 {
        self.0.get(key).copied().unwrap_or(0)
    }

    pub fn set(&mut self, key: &str, value: u64) {
        self.0.insert(key.to_string(), value);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Componentwise: does `self` cover `other` (self >= other everywhere)?
    pub fn covers(&self, other: &Vv) -> bool {
        other.0.iter().all(|(key, v)| self.get(key) >= *v)
    }

    pub fn relation(&self, other: &Vv) -> Relation {
        match (self.covers(other), other.covers(self)) {
            (true, true) => Relation::Equal,
            (true, false) => Relation::Descends,
            (false, true) => Relation::Precedes,
            (false, false) => Relation::Concurrent,
        }
    }

    /// The componentwise max: the join both the same-hash merge and the
    /// conflict winner take.
    pub fn merge_max(&mut self, other: &Vv) {
        for (key, v) in &other.0 {
            let held = self.get(key);
            if *v > held {
                self.0.insert(key.clone(), *v);
            }
        }
    }

    pub fn components(&self) -> impl Iterator<Item = (&str, u64)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn to_value(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(*v)))
                .collect::<Map<String, Value>>(),
        )
    }

    /// Strict parse: every key a well-formed `node_id@gen`, every value an
    /// integer within the canonical bound. `None` = malformed, the whole
    /// carrier is dropped.
    pub fn from_value(value: &Value) -> Option<Vv> {
        let map = value.as_object()?;
        let mut vv = BTreeMap::new();
        for (key, v) in map {
            if !valid_component(key) {
                return None;
            }
            let v = v.as_u64().filter(|n| *n <= CLOCK_MAX)?;
            vv.insert(key.clone(), v);
        }
        Some(Vv(vv))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn vv(pairs: &[(&str, u64)]) -> Vv {
        let mut out = Vv::new();
        for (k, v) in pairs {
            out.set(k, *v);
        }
        out
    }

    #[test]
    fn dominance_is_componentwise_and_absence_reads_zero() {
        let a1 = component(A, 1);
        let b1 = component(B, 1);
        let ours = vv(&[(&a1, 3), (&b1, 2)]);
        let theirs = vv(&[(&a1, 3)]);
        assert_eq!(ours.relation(&theirs), Relation::Descends);
        assert_eq!(theirs.relation(&ours), Relation::Precedes);
        assert_eq!(ours.relation(&ours.clone()), Relation::Equal);

        let concurrent = vv(&[(&a1, 4)]);
        assert_eq!(ours.relation(&concurrent), Relation::Concurrent);

        let mut joined = ours.clone();
        joined.merge_max(&concurrent);
        assert_eq!(joined.get(&a1), 4);
        assert_eq!(joined.get(&b1), 2);
        assert!(joined.covers(&ours) && joined.covers(&concurrent));
    }

    #[test]
    fn generations_are_distinct_components() {
        // The same device across a wipe: two components, forever
        // incomparable in the direction that matters - the new generation
        // does not cover the old one's history.
        let old = vv(&[(&component(A, 1), 100)]);
        let fresh = vv(&[(&component(A, 2), 1)]);
        assert_eq!(old.relation(&fresh), Relation::Concurrent);
    }

    #[test]
    fn parsing_is_strict_about_components_and_bounds() {
        let good = vv(&[(&component(A, 1), 3)]);
        assert_eq!(Vv::from_value(&good.to_value()), Some(good));
        for bad in [
            json!({ "not-a-component": 1 }),
            json!({ format!("{A}@"): 1 }),
            json!({ format!("{A}@01"): 1 }),
            json!({ format!("{A}@x"): 1 }),
            json!({ format!("{}@1", A.to_uppercase()): 1 }),
            json!({ component(A, 1): -1 }),
            json!({ component(A, 1): 9007199254740992_u64 }),
            json!({ component(A, 1): 1.5 }),
            json!([1]),
        ] {
            assert!(Vv::from_value(&bad).is_none(), "should refuse {bad}");
        }
    }
}
