//! Property-based tests (`proptest`) for `decide::verify` invariants.

use super::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

proptest! {
    /// R2 & R3: prefix bisection finds any culprit index `k in 0..n` in at most
    /// `ceil(log2(n)) + 1` probes.
    #[test]
    fn prop_bisect_finds_exact_culprit_in_log_probes(
        n in 2usize..=32,
        culprit_ratio in 0.0f64..1.0f64,
    ) {
        let cand: Vec<u64> = (1..=(n as u64)).collect();
        let culprit_idx = ((culprit_ratio * (n as f64)).floor() as usize).min(n - 1);
        let expected_culprit = cand[culprit_idx];

        let mut probes = Vec::new();
        while let BisectDecision::Probe { prefix_len, .. } = bisect::step(&cand, &probes) {
            let passed = prefix_len <= culprit_idx;
            probes.push(BisectProbe { prefix_len, passed });
            prop_assert!(probes.len() <= 7);
        }
        let max_probes = (n as f64).log2().ceil() as usize + 1;
        prop_assert!(probes.len() <= max_probes);
        prop_assert_eq!(
            bisect::step(&cand, &probes),
            BisectDecision::Resolved {
                culprit: expected_culprit,
                reason: format!("prefix [..={}] passed (or empty) and prefix [..={}] failed; PR #{expected_culprit} is the first failing change", culprit_idx, culprit_idx + 1),
            }
        );
    }

    /// R2 & R3: `ordered` places every parent before its child, and `cap` never includes a child
    /// without its parent.
    #[test]
    fn prop_ordered_and_cap_respect_stack_parents(
        chain_len in 1usize..=12,
        limit in 1usize..=12,
    ) {
        let prs: Vec<u64> = (1..=(chain_len as u64)).map(|i| i * 10).collect();
        let mut up = BTreeMap::new();
        for w in prs.windows(2) {
            up.insert(w[1], w[0]);
        }
        let mut reversed = prs.clone();
        reversed.reverse();
        let ord = ordered(&reversed, &up);
        for (&child, &parent) in &up {
            let p_pos = ord.iter().position(|&x| x == parent).unwrap();
            let c_pos = ord.iter().position(|&x| x == child).unwrap();
            prop_assert!(p_pos < c_pos);
        }
        let capped = cap(&ord, &up, limit);
        let root_count = capped.iter().filter(|p| !up.contains_key(p)).count();
        prop_assert!(root_count <= limit);
        for &pr in &capped {
            if let Some(&parent) = up.get(&pr) {
                prop_assert!(capped.contains(&parent));
            }
        }
    }

    /// R35: `cpu_sets(total, n)` produces `n` non-empty CPU slices within `0..total`, and when
    /// `n <= total` they partition `0..total` disjointly.
    #[test]
    fn prop_cpu_sets_partition_validly(
        total in 1usize..=64,
        n in 1usize..=64,
    ) {
        let sets = cpu_sets(total, n);
        prop_assert_eq!(sets.len(), n);
        for s in &sets {
            prop_assert!(!s.is_empty());
            for &c in s {
                prop_assert!(c < total);
            }
        }
        if n <= total {
            let flat: Vec<usize> = sets.into_iter().flatten().collect();
            let unique: BTreeSet<usize> = flat.iter().copied().collect();
            prop_assert_eq!(flat.len(), total);
            prop_assert_eq!(unique.len(), total);
        }
    }

    /// R42: `shrink_plan` never deletes an active area directory.
    #[test]
    fn prop_shrink_plan_never_deletes_active_areas(
        sizes in prop::collection::vec(100u64..1000u64, 1..=10),
        max_bytes in 0u64..5000u64,
    ) {
        let entries: Vec<AreaDirEntry> = sizes
            .iter()
            .enumerate()
            .map(|(i, &bytes)| AreaDirEntry {
                name: format!("area-{i}"),
                bytes,
                mtime: (i as i64) * 10,
            })
            .collect();
        let active: BTreeSet<String> = ["area-0".to_string()].into_iter().collect();
        let to_delete = shrink_plan(&entries, &active, max_bytes);
        prop_assert!(!to_delete.contains(&"area-0".to_string()));
    }

    /// R24: `flake_key` is deterministic, always starts with `flake-`, and never exceeds 53 chars.
    #[test]
    fn prop_flake_key_bounded_and_deterministic(name in "[a-zA-Z0-9_: /›>-]{1,120}") {
        let k1 = flake_key(&name);
        let k2 = flake_key(&name);
        prop_assert_eq!(&k1, &k2);
        prop_assert!(k1.starts_with("flake-"));
        prop_assert!(k1.len() <= 53);
    }
}
