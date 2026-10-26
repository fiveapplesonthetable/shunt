//! Parity tests for `decide::land` (Spec §3: `test_trainpr.py`, `test_train.py` assembly/CI,
//! plus submodules `tests_shape`, `tests_release`, and `tests_landed`).

#[path = "tests_landed.rs"]
mod tests_landed;
#[path = "tests_release.rs"]
mod tests_release;
#[path = "tests_shape.rs"]
mod tests_shape;

use super::assemble::{
    body, ci_proof_pending, drop_breakers, eligible, green_at_head, left_out, left_out_reason,
    order, rebase_brief, settle, shot_screens, solo_ci, BatchSummary, BodyParts, Candidate, MainCi,
    OpenPr,
};
use super::config::LandConfig;
use super::decide::{decide_land, LandActionKind, LandEvent, LandView};
use super::release::{letter, status_row};
use super::train::{Member, Train};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const SHOT: &str = "frontend/admin/screenshots/a.png";
pub(crate) const COMPARISON: &str = "frontend/admin/docs/comparison.html";
pub(crate) const CODE: &str = "crates/app/src/lib.rs";

pub(crate) fn fixture_cfg() -> LandConfig {
    LandConfig {
        tool: "tacho".into(),
        project: "acme".into(),
        upstream: "acme-org/acme".into(),
        queue_issue_title: "Merge queue".into(),
        train_branch_prefix: "train-".into(),
        ci_paths: vec![
            r"^(\.github/workflows/|scripts/checks\.sh$|scripts/areas(/|$)|scripts/changed-areas\.sh$)".into(),
        ],
        ui_roots: vec!["frontend/".into()],
        generated: vec![
            (
                "console".into(),
                r"^crates/acme-wire/|^frontend/admin/(screenshots/|docs/comparison\.html$)".into(),
            ),
            ("android".into(), r"^frontend/android/docs/screenshots/".into()),
        ],
        generators: BTreeMap::from([(COMPARISON.into(), "make comparison".into())]),
        shot_dirs: vec![
            ("frontend/admin/screenshots/".into(), "console".into()),
            ("frontend/android/docs/screenshots/".into(), "android".into()),
        ],
        redraw_roots: vec![
            ("console".into(), "frontend/admin/".into()),
            ("android".into(), "frontend/android/".into()),
        ],
        gate_area: "compile".into(),
        post_merge_areas: vec!["formal".into()],
        train_inputs: BTreeMap::from([
            (
                "console".into(),
                r"^crates/acme-wire/|^frontend/admin/|^crates/(acme-operator-[a-z]+/src|acme-platform/src/web)/|^scripts/(contract\.sh|typeshare\.toml)$".into(),
            ),
            (
                "console-crawl".into(),
                r"^crates/acme-wire/|^frontend/admin/|^crates/(acme-operator-[a-z]+/src|acme-platform/src/web)/|^scripts/(contract\.sh|typeshare\.toml)$".into(),
            ),
            (
                "android".into(),
                r"^crates/acme-wire/|^frontend/android/|^crates/acme-platform/src/(api/|web/code\.rs$|notify/mod\.rs$)|^scripts/(contract\.sh|typeshare\.toml)$".into(),
            ),
        ]),
        area_inputs: BTreeMap::from([
            (
                "console".into(),
                r"^crates/acme-wire/|^frontend/admin/|^crates/acme-platform/src/|^crates/acme-operator-[a-z]+/src/|^scripts/shots?\.sh$|(^|/)(golden|goldens|screenshots)(/|$)|^scripts/shot-report|^\.github/workflows/shots\.yaml$".into(),
            ),
            (
                "android".into(),
                r"^frontend/android/|^crates/acme-platform/src/|(^|/)(golden|goldens|screenshots)(/|$)|^scripts/shot-report|^\.github/workflows/shots\.yaml$".into(),
            ),
            ("fuzz".into(), r"^crates/|^fuzz/|^Cargo\.(toml|lock)$".into()),
        ]),
        pixel_files: vec![
            r"(^|/)(golden|goldens|screenshots)(/|$)|^scripts/shot-report|^\.github/workflows/shots\.yaml$".into(),
        ],
        pixel_areas: vec!["console".into(), "android".into()],
        change_file_areas: vec![(r"\.tla$".into(), "formal".into())],
        ..LandConfig::default()
    }
}

pub(crate) fn pr(n: u64, files: &[&str]) -> Candidate {
    Candidate::new(n, files)
}

#[test]
fn changes_without_frontend_files_go_first_and_smaller_before_larger() {
    let cfg = fixture_cfg();
    let got = order(
        &[
            pr(1, &["frontend/admin/src/a.tsx"]),
            pr(2, &["a.rs", "b.rs"]),
            pr(3, &["c.rs"]),
        ],
        &cfg,
    );
    assert_eq!(got, vec![3, 2, 1]);
}

#[test]
fn changes_to_the_same_screen_are_adjacent() {
    let cfg = fixture_cfg();
    let got = order(
        &[
            pr(1, &["frontend/admin/src/a/x.tsx"]),
            pr(2, &["frontend/admin/src/b/y.tsx"]),
            pr(
                3,
                &["frontend/admin/src/a/z.tsx", "frontend/admin/src/a/w.tsx"],
            ),
        ],
        &cfg,
    );
    assert_eq!(got, vec![1, 3, 2]);
}

#[test]
fn a_screenshot_is_taken_from_the_later_change() {
    let cfg = fixture_cfg();
    let got = settle(&[SHOT.to_string()], &cfg);
    assert_eq!(got.take, vec![SHOT.to_string()]);
    assert!(got.regenerate.is_empty());
    assert!(got.by_hand.is_empty());
}

#[test]
fn a_generated_file_is_taken_and_drawn_again() {
    let cfg = fixture_cfg();
    let got = settle(&[COMPARISON.to_string()], &cfg);
    assert_eq!(got.take, vec![COMPARISON.to_string()]);
    assert_eq!(got.regenerate, vec![COMPARISON.to_string()]);
    assert!(got.by_hand.is_empty());
}

#[test]
fn one_source_file_leaves_the_change_out() {
    let cfg = fixture_cfg();
    let got = settle(&[SHOT.to_string(), "crates/a.rs".to_string()], &cfg);
    assert_eq!(got.by_hand, vec!["crates/a.rs".to_string()]);
}

#[test]
fn each_screen_is_named_once_by_frontend() {
    let cfg = fixture_cfg();
    let got = shot_screens(
        &[
            SHOT.to_string(),
            SHOT.to_string(),
            "frontend/android/docs/screenshots/Home.png".to_string(),
            COMPARISON.to_string(),
        ],
        &cfg,
    );
    assert_eq!(got, vec!["android: Home", "console: a"]);
}

#[test]
fn only_the_change_that_breaks_the_build_is_dropped() {
    let mut calls = Vec::new();
    let got = drop_breakers(&[1, 2, 3, 4, 5], |prefix| {
        calls.push(prefix.to_vec());
        prefix.contains(&3).then(|| "error[E0308]".to_string())
    });
    assert_eq!(got, (vec![1, 2, 4, 5], vec![(3, "error[E0308]".into())]));
}

#[test]
fn two_breakers_are_both_found() {
    let got = drop_breakers(&[1, 2, 3, 4], |ns| {
        (ns.contains(&2) || ns.contains(&4)).then(|| "bad".to_string())
    });
    assert_eq!(
        got,
        (vec![1, 3], vec![(2, "bad".into()), (4, "bad".into())])
    );
}

#[test]
fn a_tree_that_compiles_is_built_once() {
    let mut calls = Vec::new();
    let (kept, dropped) = drop_breakers(&[1, 2], |ns| {
        calls.push(ns.to_vec());
        None
    });
    assert_eq!(kept, vec![1, 2]);
    assert!(dropped.is_empty());
    assert_eq!(calls, vec![vec![1, 2]]);
}

fn open_fixture() -> Vec<OpenPr> {
    vec![
        OpenPr {
            number: 1,
            branch: "a".into(),
            head: "sha1".into(),
        },
        OpenPr {
            number: 2,
            branch: "b".into(),
            head: "sha2".into(),
        },
        OpenPr {
            number: 3,
            branch: "c".into(),
            head: "sha3".into(),
        },
        OpenPr {
            number: 4,
            branch: "train-2026-10-10-1".into(),
            head: "sha4".into(),
        },
        OpenPr {
            number: 5,
            branch: "e".into(),
            head: "sha5".into(),
        },
    ]
}

fn green_fixture() -> Vec<BatchSummary> {
    (1..=4)
        .map(|n| BatchSummary {
            status: "green".into(),
            members: vec![(n, format!("sha{n}"))],
        })
        .collect()
}

#[test]
fn running_held_and_train_pull_requests_are_not_taken() {
    let cfg = fixture_cfg();
    let green = green_at_head(&green_fixture());
    assert_eq!(
        eligible(
            &open_fixture(),
            &BTreeSet::from([1]),
            &BTreeSet::from([2]),
            &green,
            &cfg
        ),
        vec![3]
    );
}

#[test]
fn a_pull_request_without_a_green_batch_of_its_own_is_not_taken() {
    let cfg = fixture_cfg();
    let green = green_at_head(&green_fixture());
    assert_eq!(
        eligible(
            &open_fixture(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &green,
            &cfg
        ),
        vec![1, 2, 3]
    );
}

#[test]
fn a_green_batch_at_an_older_head_does_not_count() {
    let cfg = fixture_cfg();
    let mut batches = green_fixture();
    batches[2].members = vec![(3, "old".into())];
    let green = green_at_head(&batches);
    assert_eq!(
        eligible(
            &open_fixture(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &green,
            &cfg
        ),
        vec![1, 2]
    );
}

#[test]
fn a_red_batch_or_a_batch_of_several_does_not_count() {
    let cfg = fixture_cfg();
    let batches = vec![
        BatchSummary {
            status: "red".into(),
            members: vec![(1, "sha1".into())],
        },
        BatchSummary {
            status: "green".into(),
            members: vec![(2, "sha2".into()), (3, "sha3".into())],
        },
    ];
    let green = green_at_head(&batches);
    assert_eq!(
        eligible(
            &open_fixture(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &green,
            &cfg
        ),
        Vec::<u64>::new()
    );
}

#[test]
fn a_workflow_change_is_a_train_of_its_own() {
    let cfg = fixture_cfg();
    let got = solo_ci(
        vec![
            pr(1, &["crates/a.rs"]),
            pr(2, &[".github/workflows/ci.yaml"]),
            pr(3, &["scripts/checks.sh"]),
        ],
        &cfg,
    );
    assert_eq!(got.iter().map(|c| c.number).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn each_pipeline_file_counts() {
    let cfg = fixture_cfg();
    for path in [
        ".github/workflows/ci.yaml",
        "scripts/checks.sh",
        "scripts/areas/rust.sh",
        "scripts/changed-areas.sh",
    ] {
        let got = solo_ci(vec![pr(1, &["a.rs"]), pr(2, &[path])], &cfg);
        assert_eq!(
            got.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![2],
            "{path}"
        );
    }
}

#[test]
fn other_scripts_do_not_make_a_train_solo() {
    let cfg = fixture_cfg();
    let got = solo_ci(
        vec![
            pr(1, &["scripts/new-migration.sh"]),
            pr(2, &["crates/a.rs"]),
        ],
        &cfg,
    );
    assert_eq!(got.iter().map(|c| c.number).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn nothing_lands_until_a_main_run_has_passed_after_the_change() {
    let ancestors: BTreeSet<(&str, &str)> = BTreeSet::from([("c1", "m1"), ("c1", "m2")]);
    let is_anc = |a: &str, b: &str| ancestors.contains(&(a, b));
    let m0 = MainCi {
        sha: "m0".into(),
        ok: true,
    };
    let m1 = MainCi {
        sha: "m1".into(),
        ok: false,
    };
    let m2 = MainCi {
        sha: "m2".into(),
        ok: true,
    };
    assert!(ci_proof_pending(Some("c1"), &m0, is_anc)
        .unwrap()
        .contains("nothing else lands"));
    assert!(ci_proof_pending(Some("c1"), &m1, is_anc)
        .unwrap()
        .contains("nothing else lands"));
    assert_eq!(ci_proof_pending(Some("c1"), &m2, is_anc), None);
}

#[test]
fn opens_ready_for_review() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "sha1")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    assert!(dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::OpenPr { branch, draft: false, .. } if branch == "train-2026-10-15-1"
    )));
}

#[test]
fn members_missing_from_the_final_assembly_reach_status_and_owner_mail() {
    let cfg = fixture_cfg();
    let lo = left_out(
        &[(1, vec![])],
        &[2, 3],
        &[3],
        &[(2, vec![CODE.to_string()])],
    );
    assert_eq!(lo, vec![(1, vec![]), (2, vec![CODE.to_string()])]);
    let t = Train::make_combined(
        901,
        "train-2026-10-15-1",
        "head",
        vec![Member::new(3, "s3")],
        lo,
        vec![],
        false,
    );
    let row = status_row(901, &t, &[]);
    assert_eq!(row.left_out, vec![1, 2]);
    let (_, mail_body) = letter(
        901,
        &[3],
        &[1, 2],
        &[],
        false,
        &cfg.upstream,
        &cfg.project,
        "head",
    );
    assert!(mail_body.contains("Left out: #1, #2."));
}

#[test]
fn a_member_dropped_by_the_final_merge_is_not_called_a_conflict() {
    let reason = left_out_reason(&[]);
    assert!(!reason.contains("conflict"));
    let pr_body = body(&BodyParts {
        members: &[3],
        left_out: &[(2, reason.clone())],
        ..Default::default()
    });
    assert!(pr_body.contains(&format!("- #2: {reason}")));
    assert!(rebase_brief(2, 901, &[]).contains("missing from the final merge"));
    assert!(rebase_brief(2, 901, &[CODE.to_string()]).contains(CODE));
}

#[test]
fn run_records_and_words_a_compiled_member_the_final_merge_omitted() {
    let cfg = fixture_cfg();
    let lo = left_out(&[], &[1, 2], &[1], &[]);
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1")],
            resolutions: vec![],
            left_out: lo,
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    let rec = dec
        .next
        .trains
        .iter()
        .find(|t| t.combined_pr() == Some(901))
        .unwrap();
    assert_eq!(rec.numbers(), vec![1]);
    assert_eq!(rec.left_out, vec![(2, vec![])]);
    let opened_body = dec
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::OpenPr { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(opened_body.contains(&format!("- #2: {}", left_out_reason(&[]))));
    assert!(!opened_body.contains("conflict in"));
    let task = dec.next.todo.iter().find(|t| t.key == "rebase-2").unwrap();
    assert!(task.brief.contains("missing from the final merge"));
}

// =============================================================================
// Golden Screenshot Fix Classification Tests (`test_shots_fix.py`)
// =============================================================================

const ANDROID_SHOTS_DIR: &str = "frontend/android/docs/screenshots/";
const HOME_SHOT: &str = "frontend/android/docs/screenshots/03-home.png";
const SIGNIN_SHOT: &str = "frontend/android/docs/screenshots/01-signin.png";
const HOME_SCREEN_KT: &str = "frontend/android/app/src/main/java/app/mobile/ui/home/HomeScreen.kt";
const THEME_KT: &str = "frontend/android/app/src/main/java/app/mobile/ui/theme/Theme.kt";

fn draw_shot_report(dir: &std::path::Path, path: &str, before: &[u8], after: &[u8]) {
    for (side, data) in [("before", before), ("after", after)] {
        let full = dir.join("android").join(side).join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, data).unwrap();
    }
    let changed_path = dir.join("android").join("changed.txt");
    let mut existing = std::fs::read_to_string(&changed_path).unwrap_or_default();
    existing.push_str(&format!("changed\t{path}\tgolden\n"));
    std::fs::write(&changed_path, existing).unwrap();
}

fn eval_shot_verdicts(
    dir: &std::path::Path,
    touched: &[&str],
    catalogue: usize,
) -> Vec<super::shots::ShotFixVerdict> {
    let rules = super::shots::ShotRules::default();
    let golden = BTreeSet::from([HOME_SHOT.to_string(), SIGNIN_SHOT.to_string()]);
    let report = super::shots::read_report_goldens(dir, &golden, &rules);
    let (goldens, total) = report.get("android").cloned().unwrap_or_default();
    super::shots::classify_moved_goldens(&goldens, total, catalogue, touched, &rules)
}

#[test]
fn test_a_shot_that_changed_size_is_a_rendering_break_naming_both_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let tall = super::shots::encode_rgba_png(40, 70, |_, _| [10, 10, 200, 255]);
    draw_shot_report(tmp.path(), HOME_SHOT, &old, &tall);
    let verdicts = eval_shot_verdicts(tmp.path(), &[THEME_KT], 10);
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].kind, super::shots::ShotFixKind::Canvas);
    assert!(verdicts[0]
        .line
        .contains("every shot is 40x70, expected 40x80"));
    assert!(verdicts[0].line.contains("Theme.kt"));
}

#[test]
fn test_most_of_the_catalogue_moving_at_once_is_a_canvas_even_at_the_same_size() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let red = super::shots::encode_rgba_png(40, 80, |_, _| [200, 10, 10, 255]);
    draw_shot_report(tmp.path(), HOME_SHOT, &old, &red);
    draw_shot_report(tmp.path(), SIGNIN_SHOT, &old, &red);
    let verdicts = eval_shot_verdicts(tmp.path(), &[HOME_SCREEN_KT], 3);
    let kinds: BTreeSet<_> = verdicts.iter().map(|v| v.kind).collect();
    assert_eq!(kinds, BTreeSet::from([super::shots::ShotFixKind::Canvas]));
}

#[test]
fn test_a_canvas_break_exits_non_zero_and_commits_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let tall = super::shots::encode_rgba_png(40, 70, |_, _| [10, 10, 200, 255]);
    draw_shot_report(tmp.path(), HOME_SHOT, &old, &tall);
    let results = eval_shot_verdicts(tmp.path(), &[HOME_SCREEN_KT], 10);
    assert_eq!(super::shots::shots_fix_verdict(&results).1, 1);
    assert!(!results
        .iter()
        .any(|r| r.kind == super::shots::ShotFixKind::Intended));
}

#[test]
fn test_a_golden_whose_screen_the_diff_touches_is_intended() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let red = super::shots::encode_rgba_png(40, 80, |_, _| [200, 10, 10, 255]);
    draw_shot_report(tmp.path(), HOME_SHOT, &old, &red);
    let verdicts = eval_shot_verdicts(tmp.path(), &[HOME_SCREEN_KT], 10);
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].kind, super::shots::ShotFixKind::Intended);
    assert_eq!(super::shots::shots_fix_verdict(&verdicts).1, 0);
}

#[test]
fn test_a_golden_nothing_in_the_diff_reaches_is_drift_with_its_pixel_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let red = super::shots::encode_rgba_png(40, 80, |_, _| [200, 10, 10, 255]);
    draw_shot_report(tmp.path(), SIGNIN_SHOT, &old, &red);
    let verdicts = eval_shot_verdicts(tmp.path(), &[HOME_SCREEN_KT], 10);
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].kind, super::shots::ShotFixKind::Drift);
    assert!(verdicts[0].line.contains("50.000% of its channels differ"));
    assert_eq!(super::shots::shots_fix_verdict(&verdicts).1, 1);
}

#[test]
fn test_a_noise_move_of_a_non_golden_shot_is_not_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let red = super::shots::encode_rgba_png(40, 80, |_, _| [200, 10, 10, 255]);
    draw_shot_report(
        tmp.path(),
        &format!("{ANDROID_SHOTS_DIR}09-other.png"),
        &old,
        &red,
    );
    let verdicts = eval_shot_verdicts(tmp.path(), &[HOME_SCREEN_KT], 10);
    assert!(verdicts.is_empty());
    assert_eq!(
        super::shots::shots_fix_verdict(&[]),
        ("no golden screenshot moved".to_string(), 0)
    );
}

#[test]
fn test_an_unproven_pipeline_change_requires_a_full_batch_for_the_repair() {
    let all_areas = BTreeSet::from([
        "rust".to_string(),
        "console".to_string(),
        "android".to_string(),
    ]);
    let is_anc = |a: &str, b: &str| a == b || (a == "c1" && b == "c2");
    let red_main = MainCi {
        sha: "c1".into(),
        ok: false,
    };
    let need =
        super::assemble::ci_proof_areas(Some("c1"), &red_main, is_anc, &all_areas, "compile");
    for a in &all_areas {
        assert!(need.contains(a));
    }
    assert!(need.contains("android-release"));
    assert!(need.contains("console-browser"));
    assert!(need.contains("compile"));

    let green_main = MainCi {
        sha: "c2".into(),
        ok: true,
    };
    assert!(super::assemble::ci_proof_areas(
        Some("c1"),
        &green_main,
        is_anc,
        &all_areas,
        "compile",
    )
    .is_empty());
}

#[test]
fn test_plan_accept_run_shots_copies_goldens_by_default_all_when_requested_and_refuses_moved_head()
{
    let tmp = tempfile::tempdir().unwrap();
    let old = super::shots::encode_rgba_png(40, 80, |_, _| [10, 10, 200, 255]);
    let red = super::shots::encode_rgba_png(40, 80, |_, _| [200, 10, 10, 255]);
    let other_shot = format!("{ANDROID_SHOTS_DIR}09-other.png");
    let before_files = BTreeMap::from([
        (HOME_SHOT.to_string(), old.clone()),
        (other_shot.clone(), old.clone()),
    ]);
    let after_files = BTreeMap::from([
        (HOME_SHOT.to_string(), red.clone()),
        (other_shot.clone(), red.clone()),
    ]);
    let shots_dir = super::shots::save_run_shot_artifacts(
        tmp.path(),
        "batch-42",
        "aaaa1111bbbb2222",
        "android",
        &before_files,
        &after_files,
    )
    .unwrap();

    let golden = BTreeSet::from([HOME_SHOT.to_string()]);
    let rules = super::shots::ShotRules::default();

    // Default: only moved golden is accepted.
    let plan_default = super::shots::plan_accept_run_shots(
        &shots_dir,
        None,
        "aaaa1111bbbb2222",
        false,
        &golden,
        &rules,
    )
    .unwrap();
    assert_eq!(plan_default.copy_after.len(), 1);
    assert_eq!(plan_default.copy_after[0].1, HOME_SHOT);

    // --all: both golden and non-golden moved images are accepted.
    let plan_all = super::shots::plan_accept_run_shots(
        &shots_dir,
        None,
        "aaaa1111bbbb2222",
        true,
        &golden,
        &rules,
    )
    .unwrap();
    let paths: Vec<_> = plan_all
        .copy_after
        .iter()
        .map(|(_, p)| p.as_str())
        .collect();
    assert_eq!(paths, vec![HOME_SHOT, other_shot.as_str()]);

    // Refuses when branch HEAD moved.
    let err = super::shots::plan_accept_run_shots(
        &shots_dir,
        None,
        "cccc3333dddd4444",
        false,
        &golden,
        &rules,
    )
    .unwrap_err();
    assert!(err.contains("HEAD"), "unexpected err: {err}");
    assert_eq!(
        super::shots::accept_run_shots_command("tacho", "batch-42"),
        "tacho shots accept batch-42"
    );
}

#[test]
fn test_screen_tag_caps_require_written_reason_when_raised_and_enforce_limits() {
    let mut rules = super::shots::ShotRules::default();
    assert!(super::shots::check_screen_tags("treasury", 4, 1, 1, &rules).is_ok());
    assert!(super::shots::check_screen_tags("treasury", 5, 1, 1, &rules).is_err());
    assert!(super::shots::check_screen_tags("treasury", 3, 2, 1, &rules).is_err());

    // Raising the cap without a written reason is rejected.
    rules.screen_tag_caps.insert(
        "treasury".into(),
        super::shots::ScreenTagCap {
            max_tags: 6,
            max_per_row: 2,
            max_in_header: 1,
            reason: String::new(),
        },
    );
    assert!(super::shots::validate_screen_tag_caps(&rules).is_err());

    // Raising with a written reason succeeds.
    rules.screen_tag_caps.get_mut("treasury").unwrap().reason =
        "multi-rail settlement table shows both rail and payout state".into();
    assert!(super::shots::validate_screen_tag_caps(&rules).is_ok());
    assert!(super::shots::check_screen_tags("treasury", 6, 2, 1, &rules).is_ok());
}
