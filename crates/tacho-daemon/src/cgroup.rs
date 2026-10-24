//! Cgroup v2 resource accounting for agent process trees.
//!
//! Provides zero-privilege CPU and memory monitoring via transient systemd scopes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Handle to an active agent cgroup scope under `/sys/fs/cgroup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCgroup {
    pub(crate) dir: PathBuf,
}

impl AgentCgroup {
    /// Returns the cgroup of `pid` if it is the tacho scope named `unit`.
    ///
    /// Matching the exact unit keeps a sub-agent that still sits in its parent agent's
    /// scope from being credited with the parent's totals.
    pub fn of_pid(pid: u32, unit: &str) -> Option<Self> {
        let content = fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()?;
        for line in content.lines() {
            if let Some(path) = parse_cgroup_line(line) {
                if path.rsplit('/').next() != Some(unit) {
                    continue;
                }
                let dir = Path::new("/sys/fs/cgroup").join(path.strip_prefix('/').unwrap_or(path));
                if dir.is_dir() {
                    return Some(Self { dir });
                }
            }
        }
        None
    }

    /// Spawns `busctl` to attach an existing process tree to a transient systemd scope.
    ///
    /// Uses a 2s timeout and kills the busctl process on timeout.
    pub fn attach(unit: &str, pids: &[u32]) -> io::Result<()> {
        use std::os::unix::process::CommandExt;

        if pids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot attach empty pids list to cgroup scope",
            ));
        }

        let mut cmd = Command::new("busctl");
        cmd.arg("--user")
            .arg("call")
            .arg("org.freedesktop.systemd1")
            .arg("/org/freedesktop/systemd1")
            .arg("org.freedesktop.systemd1.Manager")
            .arg("StartTransientUnit")
            .arg("ssa(sv)a(sa(sv))")
            .arg(unit)
            .arg("fail")
            .arg("3")
            .arg("PIDs")
            .arg("au")
            .arg(pids.len().to_string());
        for pid in pids {
            cmd.arg(pid.to_string());
        }
        cmd.arg("CollectMode")
            .arg("s")
            .arg("inactive-or-failed")
            .arg("Description")
            .arg("s")
            .arg("tacho agent")
            .arg("0");

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);

        let mut child = cmd.spawn()?;
        let exited = crate::sampler::wait_child_exit(&child, std::time::Duration::from_secs(2))?;
        if !exited {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "busctl StartTransientUnit timed out",
            ));
        }
        match child.wait()? {
            status if status.success() => Ok(()),
            status => {
                let mut err_msg = String::new();
                if let Some(mut err_pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err_pipe.read_to_string(&mut err_msg);
                }
                Err(io::Error::other(format!(
                    "busctl failed with status {}: {}",
                    status,
                    err_msg.trim()
                )))
            }
        }
    }

    /// Reads cumulative CPU usage in microseconds from `cpu.stat` (`usage_usec`).
    ///
    /// Includes exited children.
    pub fn cpu_usage_us(&self) -> Option<u64> {
        let content = fs::read_to_string(self.dir.join("cpu.stat")).ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("usage_usec ") {
                if let Ok(val) = rest.trim().parse::<u64>() {
                    return Some(val);
                }
            }
        }
        None
    }

    /// Reads current memory usage in bytes from `memory.current`.
    pub fn memory_current(&self) -> Option<u64> {
        let content = fs::read_to_string(self.dir.join("memory.current")).ok()?;
        content.trim().parse::<u64>().ok()
    }

    /// Reads peak memory usage in bytes from `memory.peak` if available.
    pub fn memory_peak(&self) -> Option<u64> {
        let content = fs::read_to_string(self.dir.join("memory.peak")).ok()?;
        content.trim().parse::<u64>().ok()
    }

    /// Discovers all member PIDs in the cgroup from `cgroup.procs`.
    pub fn pids(&self, out: &mut Vec<u32>) {
        if let Ok(content) = fs::read_to_string(self.dir.join("cgroup.procs")) {
            for line in content.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    out.push(pid);
                }
            }
        }
    }
}

/// Constructs a transient systemd scope unit name for an agent run:
/// `tacho-<sanitized agent>-r<n>-<pid>.scope`
///
/// Sanitizes `agent_id` to `[A-Za-z0-9_-]`, replacing any other character with `_`,
/// and capping the sanitized string so the whole unit name is at most 64 chars.
pub fn unit_name(agent_id: &str, run_n: u32, pid: u32) -> String {
    let suffix = format!("-r{}-{}.scope", run_n, pid);
    const MAX_LEN: usize = 64;
    const PREFIX: &str = "tacho-";
    let fixed_len = PREFIX.len() + suffix.len();
    let max_agent_len = MAX_LEN.saturating_sub(fixed_len).max(1);

    let mut sanitized = String::with_capacity(agent_id.len());
    for c in agent_id.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.len() > max_agent_len {
        sanitized.truncate(max_agent_len);
    }
    format!("{}{}{}", PREFIX, sanitized, suffix)
}

/// Parses a line from `/proc/<pid>/cgroup`.
/// Returns the cgroup path if it starts with "0::<path>" and the last path component
/// starts with "tacho-" and ends with ".scope".
pub fn parse_cgroup_line(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let path = trimmed.strip_prefix("0::")?;
    let last_component = path.rsplit('/').next().unwrap_or(path);
    if last_component.starts_with("tacho-") && last_component.ends_with(".scope") {
        Some(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unit_name_sanitization_and_length() {
        let name = unit_name("normal_agent-1", 1, 1234);
        assert_eq!(name, "tacho-normal_agent-1-r1-1234.scope");
        assert!(name.len() <= 64);

        let weird = unit_name("agent@sub/domain:foo*bar", 2, 9999);
        assert_eq!(weird, "tacho-agent_sub_domain_foo_bar-r2-9999.scope");
        assert!(weird.len() <= 64);

        let very_long = unit_name(
            "this_is_an_extremely_long_agent_name_that_definitely_exceeds_sixty_four_characters",
            42,
            123456,
        );
        assert!(very_long.len() <= 64);
        assert!(very_long.starts_with("tacho-"));
        assert!(very_long.ends_with("-r42-123456.scope"));
    }

    #[test]
    fn test_parse_cgroup_line() {
        let line =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tacho-worker-r1-5432.scope";
        assert_eq!(
            parse_cgroup_line(line),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/tacho-worker-r1-5432.scope")
        );

        // Non-matching prefix
        assert_eq!(parse_cgroup_line("1:name=systemd:/init.scope"), None);

        // Non-tacho scope
        let non_tacho =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/gnome-terminal.scope";
        assert_eq!(parse_cgroup_line(non_tacho), None);

        // Not a scope
        let non_scope = "0::/user.slice/user-1000.slice/tacho-worker-r1-5432.service";
        assert_eq!(parse_cgroup_line(non_scope), None);
    }
}
