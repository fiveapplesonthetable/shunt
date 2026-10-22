//! Configuration management for agent hooks (Claude Code, Gemini CLI, Jetski).
//!
//! Provides idempotent JSON merge and precise uninstallation while preserving
//! existing keys, values, and ordering. Refuses and explains on invalid JSON.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::cli::Provider;

/// Resolves target configuration file path based on provider and user/project scope.
pub fn resolve_config_path(provider: Provider, user: bool) -> io::Result<PathBuf> {
    let home = env::var("HOME").map(PathBuf::from).map_err(|_| {
        io::Error::new(io::ErrorKind::NotFound, "HOME environment variable not set")
    })?;

    let path = match provider {
        Provider::Claude => {
            if user {
                home.join(".claude/settings.json")
            } else {
                PathBuf::from(".claude/settings.json")
            }
        }
        Provider::Gemini => {
            if user {
                home.join(".gemini/hooks.json")
            } else {
                PathBuf::from(".gemini/hooks.json")
            }
        }
        Provider::Jetski => {
            if user {
                home.join(".gemini/config/hooks.json")
            } else {
                PathBuf::from("_agents/hooks.json")
            }
        }
    };

    Ok(path)
}

/// Helper to execute an operation under an exclusive flock on a sibling lock file.
fn with_config_lock<F, R>(path: &Path, f: F) -> Result<R, String>
where
    F: FnOnce() -> Result<R, String>,
{
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory '{}': {e}", parent.display()))?;
        }
    }

    let lock_path = match path.file_name() {
        Some(name) => {
            let mut s = name.to_os_string();
            s.push(".lock");
            path.with_file_name(s)
        }
        None => PathBuf::from(format!("{}.lock", path.display())),
    };

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file '{}': {e}", lock_path.display()))?;

    let res = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
    if res != 0 {
        return Err(format!(
            "Failed to acquire lock on '{}'",
            lock_path.display()
        ));
    }

    let result = f();

    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    result
}

/// Installs tachograph hooks into the specified provider's configuration file.
pub fn install_hooks(provider: Provider, user: bool) -> Result<(), String> {
    let mut path = resolve_config_path(provider, user)
        .map_err(|e| format!("Failed to resolve config path: {e}"))?;

    // Follow symlinks if present so we edit the real target file
    if path.is_symlink() {
        if let Ok(canon) = fs::canonicalize(&path) {
            path = canon;
        }
    }

    // Refuse if file exists and is read-only
    if path.exists() {
        if let Ok(meta) = fs::metadata(&path) {
            if meta.permissions().readonly() {
                return Err(format!(
                    "Configuration file '{}' is read-only. Refusing to modify.",
                    path.display()
                ));
            }
        }
    }

    with_config_lock(&path, || {
        let mut root_val = load_or_init_json(&path)?;
        merge_provider_hooks(provider, &mut root_val);
        save_json(&path, &root_val)
    })?;

    let provider_name = match provider {
        Provider::Claude => "Claude Code",
        Provider::Gemini => "Gemini CLI",
        Provider::Jetski => "Jetski",
    };

    println!(
        "Successfully installed tachograph hooks for {provider_name} in {}",
        path.display()
    );
    Ok(())
}

/// Uninstalls tachograph hooks from the specified provider's configuration file.
pub fn uninstall_hooks(provider: Provider, user: bool) -> Result<(), String> {
    let mut path = resolve_config_path(provider, user)
        .map_err(|e| format!("Failed to resolve config path: {e}"))?;

    // Follow symlinks if present so we edit the real target file
    if path.is_symlink() {
        if let Ok(canon) = fs::canonicalize(&path) {
            path = canon;
        }
    }

    // Refuse if file exists and is read-only
    if path.exists() {
        if let Ok(meta) = fs::metadata(&path) {
            if meta.permissions().readonly() {
                return Err(format!(
                    "Configuration file '{}' is read-only. Refusing to modify.",
                    path.display()
                ));
            }
        }
    }

    let modified = with_config_lock(&path, || {
        if !path.exists() {
            return Ok(false);
        }

        let mut root_val = load_or_init_json(&path)?;
        let modified = remove_provider_hooks(provider, &mut root_val);

        if modified {
            save_json(&path, &root_val)?;
        }
        Ok(modified)
    })?;

    let provider_name = match provider {
        Provider::Claude => "Claude Code",
        Provider::Gemini => "Gemini CLI",
        Provider::Jetski => "Jetski",
    };

    if modified {
        println!(
            "Successfully uninstalled tachograph hooks for {provider_name} from {}",
            path.display()
        );
    } else {
        println!(
            "No tachograph hooks found for {provider_name} in {}; nothing changed.",
            path.display()
        );
    }

    Ok(())
}

/// Loads a JSON configuration file, or initializes an empty JSON object if file is missing.
///
/// Refuses with an explanation if the file exists but contains invalid JSON.
pub fn load_or_init_json(path: &Path) -> Result<serde_json::Value, String> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }

    let content = fs::read_to_string(path).map_err(|e| {
        format!(
            "Failed to read configuration file '{}': {e}",
            path.display()
        )
    })?;

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }

    serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| {
        format!(
            "Invalid JSON in '{}': {e}. Refusing to modify malformed configuration file.",
            path.display()
        )
    })
}

/// Saves formatted JSON back to disk via atomic tempfile write + rename.
pub fn save_json(path: &Path, val: &serde_json::Value) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory '{}': {e}", parent.display()))?;
    }

    let formatted =
        serde_json::to_string_pretty(val).map_err(|e| format!("Failed to serialize JSON: {e}"))?;

    // Atomic write via tempfile in the same directory + rename
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.json".to_string());
    let pid = std::process::id();
    let thread_id = format!("{:?}", std::thread::current().id());
    let clean_tid: String = thread_id.chars().filter(|c| c.is_alphanumeric()).collect();
    let tmp_path = parent.join(format!(".{}.tmp.{}.{}", file_name, pid, clean_tid));

    {
        let mut tmp_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| {
                format!(
                    "Failed to create temporary file '{}': {e}",
                    tmp_path.display()
                )
            })?;

        tmp_file
            .write_all(format!("{formatted}\n").as_bytes())
            .map_err(|e| {
                format!(
                    "Failed to write temporary file '{}': {e}",
                    tmp_path.display()
                )
            })?;
        tmp_file.flush().map_err(|e| {
            format!(
                "Failed to flush temporary file '{}': {e}",
                tmp_path.display()
            )
        })?;
        tmp_file.sync_all().map_err(|e| {
            format!(
                "Failed to sync temporary file '{}': {e}",
                tmp_path.display()
            )
        })?;
    }

    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!(
            "Failed to atomically rename '{}' to '{}': {e}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

/// Idempotently merges tachograph hooks into the given JSON object.
pub fn merge_provider_hooks(provider: Provider, root: &mut serde_json::Value) {
    if !root.is_object() {
        *root = serde_json::json!({});
    }

    match provider {
        Provider::Claude => {
            let hooks_obj = ensure_object(root, "hooks");
            let events = [
                ("PreToolUse", Some("Edit|Write|Bash")),
                ("PostToolUse", Some("Edit|Write|Bash")),
                ("SessionStart", None),
                ("UserPromptSubmit", None),
                ("Stop", None),
                ("SubagentStop", None),
                ("PreCompact", None),
                ("SessionEnd", None),
            ];

            for (event, matcher) in events {
                merge_standard_hook(hooks_obj, event, matcher, "tacho hook claude");
            }
        }
        Provider::Gemini => {
            let hooks_obj = ensure_object(root, "hooks");
            let events = [
                ("BeforeTool", Some("*")),
                ("AfterTool", Some("*")),
                ("SessionStart", None),
                ("SessionEnd", None),
            ];

            for (event, matcher) in events {
                merge_standard_hook(hooks_obj, event, matcher, "tacho hook gemini");
            }
        }
        Provider::Jetski => {
            let tacho_obj = ensure_object(root, "tacho");
            let events = ["PreToolUse", "PostToolUse", "SessionStart", "Stop"];
            for event in events {
                let entry = serde_json::json!([{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "tacho hook jetski"
                    }]
                }]);
                tacho_obj
                    .as_object_mut()
                    .unwrap()
                    .insert(event.to_string(), entry);
            }
        }
    }
}

/// Removes tachograph hooks from the given JSON object. Returns true if modified.
pub fn remove_provider_hooks(provider: Provider, root: &mut serde_json::Value) -> bool {
    let mut modified = false;

    match provider {
        Provider::Claude => {
            if let Some(hooks_obj) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
                let mut empty_keys = Vec::new();
                for (event, val) in hooks_obj.iter_mut() {
                    if let Some(arr) = val.as_array_mut() {
                        let original_len = arr.len();
                        arr.retain(|item| !is_tacho_command(item, "tacho hook claude"));
                        if arr.len() != original_len {
                            modified = true;
                        }
                        if arr.is_empty() {
                            empty_keys.push(event.clone());
                        }
                    }
                }
                for k in empty_keys {
                    hooks_obj.remove(&k);
                }
                if hooks_obj.is_empty() {
                    root.as_object_mut().unwrap().remove("hooks");
                    modified = true;
                }
            }
        }
        Provider::Gemini => {
            if let Some(hooks_obj) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
                let mut empty_keys = Vec::new();
                for (event, val) in hooks_obj.iter_mut() {
                    if let Some(arr) = val.as_array_mut() {
                        let original_len = arr.len();
                        arr.retain(|item| !is_tacho_command(item, "tacho hook gemini"));
                        if arr.len() != original_len {
                            modified = true;
                        }
                        if arr.is_empty() {
                            empty_keys.push(event.clone());
                        }
                    }
                }
                for k in empty_keys {
                    hooks_obj.remove(&k);
                }
                if hooks_obj.is_empty() {
                    root.as_object_mut().unwrap().remove("hooks");
                    modified = true;
                }
            }
        }
        Provider::Jetski => {
            if let Some(obj) = root.as_object_mut() {
                if obj.remove("tacho").is_some() {
                    modified = true;
                }
            }
        }
    }

    modified
}

fn ensure_object<'a>(parent: &'a mut serde_json::Value, key: &str) -> &'a mut serde_json::Value {
    let obj = parent.as_object_mut().expect("parent must be an object");
    if !obj.contains_key(key) || !obj[key].is_object() {
        obj.insert(key.to_string(), serde_json::json!({}));
    }
    obj.get_mut(key).unwrap()
}

fn merge_standard_hook(
    hooks_obj: &mut serde_json::Value,
    event: &str,
    matcher: Option<&str>,
    command_str: &str,
) {
    let hooks_map = hooks_obj.as_object_mut().unwrap();
    if !hooks_map.contains_key(event) || !hooks_map[event].is_array() {
        hooks_map.insert(event.to_string(), serde_json::json!([]));
    }
    let arr = hooks_map.get_mut(event).unwrap().as_array_mut().unwrap();

    // Check if command already exists
    let already_present = arr.iter().any(|item| is_tacho_command(item, command_str));
    if !already_present {
        let mut entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command_str
            }]
        });
        if let Some(m) = matcher {
            entry
                .as_object_mut()
                .unwrap()
                .insert("matcher".to_string(), serde_json::json!(m));
        }
        arr.push(entry);
    }
}

fn is_tacho_command(item: &serde_json::Value, target_cmd: &str) -> bool {
    if let Some(hooks_list) = item.get("hooks").and_then(|v| v.as_array()) {
        for h in hooks_list {
            if let Some(cmd) = h.get("command").and_then(|v| v.as_str()) {
                if cmd == target_cmd || cmd.starts_with("tacho hook") {
                    return true;
                }
            }
        }
    }
    false
}
