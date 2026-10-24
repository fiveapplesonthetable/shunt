//! Parity tests for `decide::ops::setup` (ST1–ST15, TP1–TP3, AP1–AP4).

use std::collections::BTreeMap;

use crate::decide::ops::setup::*;
use crate::decide::ops::OpsConfig;

fn base_inputs() -> SetupInputs {
    SetupInputs {
        home: "/home/dev".into(),
        repo: "/srv/fleet/repo".into(),
        work: "/srv/fleet/work".into(),
        code: "/srv/fleet/code".into(),
        user: "dev".into(),
        repo_slug: "org/project".into(),
        env_file_path: "/home/dev/.tacho.env".into(),
        existing_env: None,
        template_override: None,
        hook_states: BTreeMap::new(),
        bin_states: BTreeMap::new(),
        unit_states: BTreeMap::new(),
        probes: default_needs("/usr/bin/psql", "/usr/bin/gcloud", "/opt/android-sdk")
            .into_iter()
            .map(|n| ToolProbe {
                name: n.name,
                exit_code: 0,
                output: "v22.11.0 rustc 1.94.0".into(),
            })
            .collect(),
        docker_info: "Default Address Pools:\n Base: 10.210.0.0/16, Size: 24\n".into(),
        df_output: "Filesystem\n/dev/nvme0n1p2\n/dev/nvme0n1p2\n".into(),
        systemd_functional: true,
        linger_enabled: true,
        start_services: false,
    }
}

#[test]
fn st1_writes_env_file_from_template_when_absent() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let inputs = base_inputs();
    let rep = plan_setup(&inputs, &cfg, &policy);
    let wrote = rep.wrote_env.expect("should write env");
    assert!(wrote.contains("TACHO_HOME=/home/dev"));
    assert!(wrote.contains("TACHO_REPO=/srv/fleet/repo"));
    assert!(!wrote.contains("@HOME@"));
}

#[test]
fn st2_never_overwrites_an_existing_env_file() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let mut inputs = base_inputs();
    inputs.existing_env = Some("TACHO_HOME=/custom\n".into());
    let rep = plan_setup(&inputs, &cfg, &policy);
    assert!(rep.wrote_env.is_none());
    assert!(rep
        .lines
        .iter()
        .any(|l| l.contains("keep    /home/dev/.tacho.env")));
}

#[test]
fn st3_reports_variables_documented_in_template_that_an_existing_env_file_omits() {
    let tmpl = default_env_template("TACHO_");
    let partial = "TACHO_HOME=/home/dev\n#TACHO_NOTIFY=\n";
    let missing = missing_variables(&tmpl, partial);
    assert!(!missing.contains(&"TACHO_HOME".to_string()));
    assert!(!missing.contains(&"TACHO_NOTIFY".to_string()));
    assert!(missing.contains(&"TACHO_MAX_CPU_PERCENT".to_string()));
}

#[test]
fn st4_symlinks_hooks_binaries_and_systemd_units_idempotently() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let inputs = base_inputs();
    let rep = plan_setup(&inputs, &cfg, &policy);
    assert_eq!(rep.hook_actions.len(), 3);
    assert_eq!(rep.bin_actions.len(), 3);
    assert_eq!(rep.unit_actions.len(), 3);
    assert!(rep
        .hook_actions
        .iter()
        .all(|a| matches!(a, LinkAction::Create { .. })));
}

#[test]
fn st5_repoints_stale_symlink_without_touching_regular_files() {
    let act1 = plan_link(
        "/srv/fleet/repo/bin/tacho",
        "tacho",
        "/home/dev/.local/bin",
        &LinkState::Symlink {
            target: "/old/tacho".into(),
        },
    );
    assert!(matches!(act1, LinkAction::Repoint { .. }));
    let act2 = plan_link(
        "/srv/fleet/repo/bin/tacho",
        "tacho",
        "/home/dev/.local/bin",
        &LinkState::OtherFile,
    );
    assert!(matches!(act2, LinkAction::KeepNonLink { .. }));
}

#[test]
fn st6_returns_zero_when_all_required_tools_pass() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let rep = plan_setup(&base_inputs(), &cfg, &policy);
    assert!(rep.ok);
    assert!(rep.failed_needs.is_empty());
}

#[test]
fn st7_returns_one_and_prints_fix_command_when_a_tool_is_missing() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let mut inputs = base_inputs();
    inputs.probes.iter_mut().for_each(|p| {
        if p.name == "sccache" {
            p.exit_code = 1;
        }
    });
    let rep = plan_setup(&inputs, &cfg, &policy);
    assert!(!rep.ok);
    assert_eq!(rep.failed_needs.len(), 1);
    assert_eq!(rep.failed_needs[0].name, "sccache");
    assert!(rep
        .lines
        .iter()
        .any(|l| l.contains("MISSING sccache — fix: cargo install sccache --locked")));
}

#[test]
fn st8_rejects_wrong_node_major_version() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let mut inputs = base_inputs();
    inputs.probes.iter_mut().for_each(|p| {
        if p.name == "node v22" {
            p.output = "v18.20.0".into();
        }
    });
    let rep = plan_setup(&inputs, &cfg, &policy);
    assert!(!rep.ok);
    assert!(rep.failed_needs.iter().any(|n| n.name == "node v22"));
}

#[test]
fn st9_creates_warm_cache_and_gradle_directories() {
    let dirs = warm_directories(
        "/home/dev",
        "/home/dev/.cache/warm",
        "/home/dev/.cache/sccache",
        "/home/dev/.cache/tacho",
    );
    assert_eq!(
        dirs,
        vec![
            "/home/dev/.cache/warm",
            "/home/dev/.cache/sccache",
            "/home/dev/.cache/tacho",
            "/home/dev/.gradle",
        ]
    );
}

#[test]
fn st10_enables_units_with_now_only_when_start_is_requested() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let mut inputs = base_inputs();
    inputs.start_services = false;
    let rep1 = plan_setup(&inputs, &cfg, &policy);
    assert!(!rep1.systemd_commands[1].contains(&"--now".to_string()));

    inputs.start_services = true;
    let rep2 = plan_setup(&inputs, &cfg, &policy);
    assert!(rep2.systemd_commands[1].contains(&"--now".to_string()));
}

#[test]
fn st11_tolerates_non_functional_user_systemd_without_failing() {
    let cfg = OpsConfig::default();
    let policy = SetupPolicy::default();
    let mut inputs = base_inputs();
    inputs.systemd_functional = false;
    let rep = plan_setup(&inputs, &cfg, &policy);
    assert!(rep.ok);
    assert!(rep.systemd_commands.is_empty());
    assert_eq!(rep.unit_actions.len(), 3);
}

#[test]
fn st12_prints_self_hosted_ci_runner_verification_steps() {
    let steps = ci_runner_steps("/srv/fleet/repo", "org/project");
    assert!(steps[0].contains("actions.runner.*"));
    assert!(steps[1].contains("gh api repos/org/project/actions/runners"));
    assert!(steps[2].contains("admin"));
}

#[test]
fn st13_and_st14_prints_root_owned_tasks_with_sunday_4am_buildx_prune_under_flock() {
    let policy = SetupPolicy::default();
    let line = root_prune_line(&policy);
    assert!(line.starts_with("0 4 * * 0 /usr/bin/flock -n "));
    assert!(line.contains("buildx prune -f --keep-storage 50GB --filter until=168h"));
    let advice = root_advice("/srv/fleet/repo", "dev", &policy);
    assert_eq!(advice.len(), 2);
}

#[test]
fn st15_noop_when_symlink_already_points_to_target() {
    let act = plan_link(
        "/srv/fleet/repo/bin/tacho",
        "tacho",
        "/home/dev/.local/bin",
        &LinkState::Symlink {
            target: "/srv/fleet/repo/bin/tacho".into(),
        },
    );
    assert!(matches!(act, LinkAction::Noop { .. }));
}

#[test]
fn tp1_to_tp3_env_example_template_is_parseable_complete_and_secret_free() {
    let tmpl = default_env_template("TACHO_");
    let mut placeholders = BTreeMap::new();
    placeholders.insert("HOME".into(), "/home/dev".into());
    placeholders.insert("REPO".into(), "/srv/repo".into());
    placeholders.insert("WORK".into(), "/srv/work".into());
    placeholders.insert("CODE".into(), "/srv/code".into());
    let rendered = render_env(&tmpl, &placeholders);
    let parsed =
        crate::decide::ops::config::to_map(&crate::decide::ops::config::parse_env_file(&rendered));
    assert_eq!(
        parsed.get("TACHO_HOME").map(String::as_str),
        Some("/home/dev")
    );
    assert_eq!(
        parsed.get("TACHO_MAX_CPU_PERCENT").map(String::as_str),
        Some("90")
    );
    assert!(missing_variables(&tmpl, &rendered).is_empty());
    assert_eq!(crate::scrub::Scrubber::enabled().scrub(&tmpl), tmpl);
}

#[test]
fn ap1_to_ap4_docker_address_pool_advice() {
    let policy = SetupPolicy::default();
    // AP1: configured -> ok
    let ok = address_pool_advice(
        "Default Address Pools:\n Base: 10.210.0.0/16, Size: 24\n",
        &policy,
    );
    assert_eq!(ok.len(), 1);
    assert!(ok[0].starts_with("ok"));

    // AP2 & AP3: default pool -> advises daemon.json and waiting for batches
    let adv = address_pool_advice("Server Version: 26.1.0\n", &policy);
    assert_eq!(adv.len(), 3);
    assert!(adv[0].contains("31-network"));
    assert!(adv[1].contains("/etc/docker/daemon.json"));
    assert!(adv[2].contains("Wait until no batches are running"));

    // AP4: empty when docker info unavailable
    assert!(address_pool_advice("", &policy).is_empty());
}
