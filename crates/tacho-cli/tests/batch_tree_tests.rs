use std::fs;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::AreaRule;
use tacho_daemon::FleetStore;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn a_batch_checks_the_proposed_tree_and_preserves_the_main_checkout() {
    check_tree("proposed", true, "", false);
}

#[test]
fn a_tick_starts_and_completes_verification_of_the_proposed_tree() {
    check_tree("proposed", true, "", true);
}

#[test]
fn a_green_main_checkout_cannot_certify_a_broken_proposed_tree() {
    check_tree("main", false, "", false);
}

#[test]
fn a_check_cannot_certify_a_tree_it_replaces_after_running() {
    check_tree(
        "proposed",
        false,
        " && printf 'altered\\n' > value && git commit -am 'The check alters its tree'",
        false,
    );
}

fn check_tree(required: &str, should_pass: bool, suffix: &str, autonomous: bool) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Developer"]);
    git(&repo, &["config", "user.email", "dev@example.com"]);
    fs::write(repo.join("value"), "main\n").unwrap();
    git(&repo, &["add", "value"]);
    git(&repo, &["commit", "-m", "Main has its own value"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "proposed"]);
    fs::write(repo.join("value"), "proposed\n").unwrap();
    git(&repo, &["commit", "-am", "The proposal changes the value"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    git(&repo, &["checkout", "main"]);
    fs::write(repo.join("precious-untracked"), "keep this work\n").unwrap();
    let socket_path = temp.path().join("check.sock");
    let listener = autonomous.then(|| UnixListener::bind(&socket_path).unwrap());
    let barrier = if autonomous {
        format!(" && python3 -c \"import socket;s=socket.socket(socket.AF_UNIX);s.connect('{}');s.sendall(b'x');s.recv(1)\"", socket_path.display())
    } else {
        String::new()
    };
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.repo_root = Some(repo.to_string_lossy().to_string());
            state.open_prs_cache = vec![PullFacts {
                number: 1,
                branch: "proposed".to_string(),
                head: head.clone(),
                base: "main".to_string(),
                state: "OPEN".to_string(),
                mergeable: "MERGEABLE".to_string(),
                files: vec!["value".to_string()],
                ..Default::default()
            }];
            state.area_rules = vec![AreaRule {
                area: "value".to_string(),
                prefixes: vec!["value".to_string()],
                checks: vec![(
                    "value".to_string(),
                    format!("test \"$(cat value)\" = {required}{suffix}{barrier}"),
                )],
                ..Default::default()
            }];
        })
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(if autonomous {
            vec!["tick"]
        } else {
            vec!["batch", "1", "--foreground"]
        })
        .env("TACHO_HOME", &home)
        .env("TACHO_TEST_MODE", "1")
        .env_remove("TACHO_GH_REPO")
        .env_remove("TACHO_NOTIFY")
        .env_remove("TACHO_ROOT_PRUNE")
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        should_pass,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if autonomous {
        let claimed = store.load().unwrap();
        assert_eq!(
            claimed.batches.len(),
            1,
            "the tick must execute its batch plan"
        );
        let (mut stream, _) = listener.as_ref().unwrap().accept().unwrap();
        stream.read_exact(&mut [0u8]).unwrap();
        let second = Command::new(env!("CARGO_BIN_EXE_tacho"))
            .arg("tick")
            .env("TACHO_HOME", &home)
            .env("TACHO_TEST_MODE", "1")
            .env_remove("TACHO_GH_REPO")
            .env_remove("TACHO_NOTIFY")
            .env_remove("TACHO_ROOT_PRUNE")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            second.status.success(),
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(
            store.load().unwrap().batches,
            claimed.batches,
            "another tick must attach to the running work"
        );
        stream.write_all(b"x").unwrap();
        if let Some(pid) = claimed.batches[0].pid {
            use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if raw >= 0 {
                let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
                let mut pollfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(
                    unsafe { libc::poll(&mut pollfd, 1, 5000) },
                    1,
                    "the launched check must finish"
                );
            }
        }
    }
    let state = store.load().unwrap();
    if autonomous {
        assert_eq!(
            state.batches.len(),
            1,
            "completion must replace its running claim"
        );
        assert_eq!(state.batches[0].status, "passed");
    }
    if should_pass {
        let proof = state
            .passed
            .get(&1)
            .expect("verified proposal needs a proof");
        assert_eq!(proof.tree, tree);
        assert_eq!(proof.sha, head);
        assert_eq!(proof.main_sha, main);
    } else {
        assert!(!state.passed.contains_key(&1));
    }
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), main);
    assert_eq!(fs::read_to_string(repo.join("value")).unwrap(), "main\n");
    assert_eq!(
        fs::read_to_string(repo.join("precious-untracked")).unwrap(),
        "keep this work\n"
    );
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1
    );
}
