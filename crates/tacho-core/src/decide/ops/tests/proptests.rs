//! Property-based tests (`proptest`) for `decide::ops` invariants.

use proptest::prelude::*;

use crate::decide::ops::config::{parse_env_file, render_env_file, to_map};
use crate::decide::ops::kit::{export_env_file, parse_export_env};
use crate::decide::ops::snapshot::{redact_json, redact_text};
use crate::decide::ops::tidy::{decide, inside, Checkout, Snapshot, TidyPolicy, Tree};

proptest! {
    #[test]
    fn prop_systemd_env_file_roundtrip(
        entries in prop::collection::btree_map(
            "[A-Z][A-Z0-9_]{0,15}",
            "[a-zA-Z0-9_./:-]{0,30}",
            0..12
        )
    ) {
        let vec_entries: Vec<(String, String)> = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let rendered = render_env_file(&vec_entries);
        let parsed = to_map(&parse_env_file(&rendered));
        prop_assert_eq!(parsed, entries);
    }

    #[test]
    fn prop_export_env_file_roundtrip(
        entries in prop::collection::btree_map(
            "[A-Z][A-Z0-9_]{0,15}",
            "[a-zA-Z0-9_ ./:'-]{0,30}",
            0..12
        )
    ) {
        let rendered = export_env_file(&entries);
        let parsed = parse_export_env(&rendered);
        prop_assert_eq!(parsed, entries);
    }

    #[test]
    fn prop_redact_json_is_idempotent_and_preserves_keys_and_numbers(
        key in "[a-z_]{1,12}",
        num in -1_000_000i64..1_000_000i64,
        secret in "[A-Za-z0-9]{8,20}",
        prefix in "[a-z ]{0,15}",
    ) {
        let literals = vec![secret.clone()];
        let val = serde_json::json!({
            key.clone(): {
                "count": num,
                "msg": format!("{prefix}{secret}"),
            }
        });
        let once = redact_json(val, &literals);
        let twice = redact_json(once.clone(), &literals);
        prop_assert_eq!(&once, &twice);
        prop_assert_eq!(once[&key]["count"].as_i64(), Some(num));
        let msg = once[&key]["msg"].as_str().unwrap_or("");
        prop_assert!(!msg.contains(&secret));
        prop_assert_eq!(redact_text(msg, &literals), msg);
    }

    #[test]
    fn prop_tidy_never_removes_a_tree_or_checkout_held_by_a_live_process(
        slug in "[a-z0-9]{3,10}",
        sub in "[a-z0-9]{1,8}",
        idle in 0i64..200_000i64,
    ) {
        let build_path = format!("/builds/{slug}");
        let checkout_path = format!("/work/{slug}");
        let held_build = format!("{build_path}/{sub}");
        let held_checkout = format!("{checkout_path}/{sub}");
        let snap = Snapshot {
            builds: vec![Tree { path: build_path.clone(), idle }],
            checkouts: vec![Checkout {
                path: checkout_path.clone(),
                idle,
                dirty: true,
                unpublished: true,
            }],
            held_paths: vec![held_build.clone(), held_checkout.clone()],
            ..Default::default()
        };
        prop_assert!(inside(&held_build, &build_path));
        prop_assert!(inside(&held_checkout, &checkout_path));
        let actions = decide(&snap, &TidyPolicy::default());
        prop_assert!(actions.is_empty());
    }
}
