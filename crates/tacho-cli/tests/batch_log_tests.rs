use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::AreaRule;
use tacho_daemon::FleetStore;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn probe_output_is_readable_while_running_and_survives_a_successful_verdict() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let home = temporary.path().join("home");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Developer"]);
    git(&root, &["config", "user.email", "dev@example.com"]);
    fs::write(root.join("value"), "base\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Main has a value"]);
    git(&root, &["checkout", "-b", "proposal"]);
    fs::write(root.join("value"), "proposal\n").unwrap();
    git(&root, &["commit", "-am", "The value changes"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "main"]);
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.open_prs_cache = vec![PullFacts {
                number: 7,
                state: "OPEN".into(),
                head: head.clone(),
                branch: "proposal".into(),
                files: vec!["value".into()],
                ..Default::default()
            }];
            state.area_rules = vec![AreaRule {
                area: "opaque".into(),
                prefixes: vec!["".into()],
                ..Default::default()
            }];
        })
        .unwrap();
    let socket = temporary.path().join("continue.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let script = format!("import socket,sys; print(\"stdout before\",flush=True); print(\"stderr after\",file=sys.stderr,flush=True); s=socket.socket(socket.AF_UNIX); s.connect({}); s.recv(1); print(\"complete\",flush=True)", serde_json::to_string(&socket).unwrap());
    let command = format!("python3 -u -c '{}'", script.replace('\'', "'\\''"));
    let child = Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(["batch", "--foreground", "--cmd", &command, "7"])
        .env("TACHO_HOME", &home)
        .env("TACHO_TEST_MODE", "1")
        .env_remove("TACHO_BATCH_CLAIMED")
        .env_remove("TACHO_BATCH_AREAS")
        .current_dir(&root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let directory = home.join("runs/b001/logs");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (paths, live) = loop {
        let paths: Vec<_> = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        let live = paths.iter().find_map(|path| fs::read_to_string(path).ok());
        if live.as_deref() == Some("stdout before\nstderr after\n")
            || std::time::Instant::now() >= deadline
        {
            break (paths, live);
        }
        std::thread::park_timeout(std::time::Duration::from_millis(10));
    };
    stream.write_all(b"x").unwrap();
    drop(stream);
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(live.as_deref(), Some("stdout before\nstderr after\n"));
    assert_eq!(paths.len(), 1);
    let path = paths.first().unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "stdout before\nstderr after\ncomplete\n"
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(store
        .load()
        .unwrap()
        .batches
        .first()
        .unwrap()
        .log
        .contains("Probe logs:"));
}
