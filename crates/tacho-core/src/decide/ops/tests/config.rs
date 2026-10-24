//! Parity tests for `decide::ops::config` (CF1–CF12, PT1–PT4).

use std::collections::BTreeMap;

use crate::decide::ops::config::*;

#[test]
fn cf1_systemd_env_file_is_parsed_without_a_shell() {
    let text = r#"
# comment
A=plain
B="double quoted # not a comment"
C='single quoted'
D=val
E="escaped \"quote\" and \\ slash"
1BAD=ignored
"#;
    let map = to_map(&parse_env_file(text));
    assert_eq!(map.get("A").map(String::as_str), Some("plain"));
    assert_eq!(
        map.get("B").map(String::as_str),
        Some("double quoted # not a comment")
    );
    assert_eq!(map.get("C").map(String::as_str), Some("single quoted"));
    assert_eq!(map.get("D").map(String::as_str), Some("val"));
    assert_eq!(
        map.get("E").map(String::as_str),
        Some("escaped \"quote\" and \\ slash")
    );
    assert!(!map.contains_key("1BAD"));
}

#[test]
fn cf2_an_unmatched_quote_stops_parsing_safely() {
    let map = to_map(&parse_env_file("OK=1\nBROKEN=\"unterminated value\n"));
    assert_eq!(map.get("OK").map(String::as_str), Some("1"));
    assert!(!map.contains_key("BROKEN"));
}

#[test]
fn cf3_the_environment_overrides_the_file() {
    let file_map = to_map(&parse_env_file("A=from_file\nB=from_file\n"));
    let mut env = BTreeMap::new();
    env.insert("A".into(), "from_env".into());
    let merged = merge(&file_map, &env);
    assert_eq!(merged.get("A").map(String::as_str), Some("from_env"));
    assert_eq!(merged.get("B").map(String::as_str), Some("from_file"));
}

#[test]
fn cf4_cpu_percent_accepts_ninety_percent() {
    let (lim, warn) = cpu_limit("TACHO_", Some("90"), None, 16, None, 90.0);
    assert_eq!(lim, CpuLimit { percent: 90.0 });
    assert!(warn.is_none());
}

#[test]
fn cf5_cpu_percent_above_100_falls_back_to_90() {
    let (lim, warn) = cpu_limit("TACHO_", Some("150"), None, 16, None, 90.0);
    assert_eq!(lim, CpuLimit { percent: 90.0 });
    assert!(warn.is_some());
}

#[test]
fn cf6_invalid_cpu_percent_falls_back_to_90() {
    let (lim, warn) = cpu_limit("TACHO_", Some("not-a-number"), None, 16, None, 90.0);
    assert_eq!(lim, CpuLimit { percent: 90.0 });
    assert!(warn.is_some());
}

#[test]
fn cf7_unset_cpu_percent_with_a_load_limit_falls_back_to_load() {
    let (lim, warn) = cpu_limit("TACHO_", None, Some("8"), 16, None, 90.0);
    assert_eq!(lim, CpuLimit { percent: 50.0 });
    assert!(warn.is_none());
}

#[test]
fn cf8_busy_uses_cpu_percent_and_keeps_idle_cores_working() {
    let limit = CpuLimit { percent: 90.0 };
    let idle = MachineFacts {
        low_disk: vec![],
        quota_cpu_percent: Some(72.0),
        load1: 4.0,
        load5: 4.0,
        cpus: 16,
    };
    assert_eq!(busy(&idle, limit), None);
    let hot = MachineFacts {
        low_disk: vec![],
        quota_cpu_percent: Some(93.5),
        load1: 4.0,
        load5: 4.0,
        cpus: 16,
    };
    assert_eq!(
        busy(&hot, limit),
        Some(Busy::QuotaCpu {
            percent: 93.5,
            limit: 90.0
        })
    );
}

#[test]
fn cf9_busy_gates_on_low_disk() {
    let limit = CpuLimit { percent: 90.0 };
    let low_disk = MachineFacts {
        low_disk: vec!["/srv/fleet".into()],
        quota_cpu_percent: Some(15.0),
        load1: 1.0,
        load5: 1.0,
        cpus: 16,
    };
    assert_eq!(
        busy(&low_disk, limit),
        Some(Busy::LowDisk {
            path: "/srv/fleet".into()
        })
    );
}

#[test]
fn cf10_busy_falls_back_to_load_when_over_max_load() {
    let limit = CpuLimit { percent: 50.0 };
    let overloaded = MachineFacts {
        low_disk: vec![],
        quota_cpu_percent: None,
        load1: 12.0,
        load5: 10.0,
        cpus: 16,
    };
    assert_eq!(
        busy(&overloaded, limit),
        Some(Busy::Load {
            load: 12.0,
            max: 8.0
        })
    );
}

#[test]
fn cf11_refresh_reloads_env_file_while_preserving_process_environment_overrides() {
    let hot = &["A", "B", "C"];
    let initial_file = to_map(&parse_env_file("A=1\nB=2\n"));
    let mut real_env = BTreeMap::new();
    real_env.insert("A".into(), "pinned".into());
    let pinned = env_pinned(hot, &initial_file, &real_env);
    let new_file = to_map(&parse_env_file("A=changed\nB=20\nC=30\n"));
    let updated = refresh(hot, &pinned, &new_file);
    assert!(!updated.contains_key("A")); // pinned key untouched
    assert_eq!(updated.get("B"), Some(&Some("20".into())));
    assert_eq!(updated.get("C"), Some(&Some("30".into())));
}

#[test]
fn cf12_refresh_removes_keys_deleted_from_the_env_file() {
    let hot = &["A", "OLD"];
    let initial_file = to_map(&parse_env_file("A=1\nOLD=gone\n"));
    let pinned = env_pinned(hot, &initial_file, &BTreeMap::new());
    let new_file = to_map(&parse_env_file("A=1\n"));
    let updated = refresh(hot, &pinned, &new_file);
    assert_eq!(updated.get("A"), Some(&Some("1".into())));
    assert_eq!(updated.get("OLD"), Some(&None));
}

#[test]
fn pt1_private_tmpdir_lives_outside_world_writable_tmp_with_owner_only_mode() {
    let facts = TmpDirFacts {
        path: "/home/dev/.cache/tmp".into(),
        is_dir: true,
        is_symlink: false,
        owner_uid: 1000,
        mode: 0o700,
    };
    assert_eq!(check_tmpdir(&facts, 1000), Ok(()));
}

#[test]
fn pt2_private_tmpdir_rejects_group_or_other_permissions_on_existing_directory() {
    let facts = TmpDirFacts {
        path: "/home/dev/.cache/tmp".into(),
        is_dir: true,
        is_symlink: false,
        owner_uid: 1000,
        mode: 0o755,
    };
    assert!(check_tmpdir(&facts, 1000).is_err());
}

#[test]
fn pt3_private_tmpdir_rejects_a_symlink() {
    let facts = TmpDirFacts {
        path: "/home/dev/.cache/tmp".into(),
        is_dir: true,
        is_symlink: true,
        owner_uid: 1000,
        mode: 0o700,
    };
    let err = check_tmpdir(&facts, 1000).unwrap_err();
    assert!(err.contains("symlink"));
}

#[test]
fn pt4_command_argv_fills_model_and_preserves_brief_as_single_arg() {
    let argv = command_argv(
        "claude -p {brief} --model {model}",
        "sonnet",
        "hello 'world' with spaces",
    )
    .unwrap();
    assert_eq!(
        argv,
        vec![
            "claude",
            "-p",
            "hello 'world' with spaces",
            "--model",
            "sonnet"
        ]
    );
}
