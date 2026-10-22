//! Command classification into high-level categories (Build, Test, Push, etc.).
//!
//! Maps an argument vector (or command line) to a `CommandClass` enum value.
//! For compound shell pipelines (e.g. `cd dir && cargo test`), the classifier
//! evaluates each pipeline segment and picks the highest-ranked action according
//! to a fixed precedence order.

pub use tacho_proto::tacho::CommandClass;

/// Precedence rank for command classes when resolving compound shell lines.
/// Higher rank wins in compound expressions.
fn rank(class: CommandClass) -> u32 {
    match class {
        CommandClass::Test => 12,
        CommandClass::Build => 11,
        CommandClass::Push => 10,
        CommandClass::Commit => 9,
        CommandClass::Rebase => 8,
        CommandClass::Fetch => 7,
        CommandClass::Package => 6,
        CommandClass::Install => 5,
        CommandClass::Lint => 4,
        CommandClass::Format => 3,
        CommandClass::Exec => 2,
        CommandClass::Command => 1,
        CommandClass::Unspecified => 0,
    }
}

/// Classifies a command given as an array of argument words.
pub fn classify<T: AsRef<str>>(argv: &[T]) -> CommandClass {
    if argv.is_empty() {
        return CommandClass::Command;
    }

    // Tokenize words
    let tokens: Vec<String> = if argv.len() == 1 {
        tokenize_shell(argv[0].as_ref())
    } else {
        argv.iter().map(|s| s.as_ref().to_string()).collect()
    };

    if tokens.is_empty() {
        return CommandClass::Command;
    }

    // Split compound shell lines by operators: &&, ||, ;, |, |&
    let mut best_class = CommandClass::Command;
    let mut current_segment: Vec<&str> = Vec::new();

    for word in &tokens {
        let w = word.as_str();
        if w == "&&" || w == "||" || w == ";" || w == "|" || w == "|&" {
            if !current_segment.is_empty() {
                let seg_class = classify_simple_segment(&current_segment);
                if rank(seg_class) > rank(best_class) {
                    best_class = seg_class;
                }
                current_segment.clear();
            }
        } else {
            current_segment.push(w);
        }
    }

    if !current_segment.is_empty() {
        let seg_class = classify_simple_segment(&current_segment);
        if rank(seg_class) > rank(best_class) {
            best_class = seg_class;
        }
    }

    best_class
}

/// Tokenizes a shell line respecting single and double quotes.
fn tokenize_shell(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
            continue;
        }

        if in_double {
            if c == '"' {
                in_double = false;
            } else if c == '\\' {
                if let Some(&next_c) = chars.peek() {
                    if next_c == '"' || next_c == '\\' || next_c == '$' || next_c == '`' {
                        current.push(chars.next().unwrap());
                        continue;
                    }
                }
                current.push(c);
            } else {
                current.push(c);
            }
            continue;
        }

        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => {
                if let Some(next_c) = chars.next() {
                    current.push(next_c);
                }
            }
            ' ' | '\t' | '\r' | '\n' => {
                if !current.is_empty() {
                    tokens.push(current);
                    current = String::new();
                }
            }
            '&' => {
                if !current.is_empty() {
                    tokens.push(current);
                    current = String::new();
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push("&&".to_string());
                } else {
                    tokens.push("&".to_string());
                }
            }
            '|' => {
                if !current.is_empty() {
                    tokens.push(current);
                    current = String::new();
                }
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push("||".to_string());
                } else if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push("|&".to_string());
                } else {
                    tokens.push("|".to_string());
                }
            }
            ';' => {
                if !current.is_empty() {
                    tokens.push(current);
                    current = String::new();
                }
                tokens.push(";".to_string());
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Classifies a single command segment without compound shell operators.
fn classify_simple_segment<'a>(mut words: &'a [&'a str]) -> CommandClass {
    // 1. Strip leading environment variables (e.g. `RUST_LOG=debug`, `CC=clang`)
    while !words.is_empty() && is_env_var(words[0]) {
        words = &words[1..];
    }
    if words.is_empty() {
        return CommandClass::Command;
    }

    // 2. Iteratively peel wrapper commands and their flags
    loop {
        if words.is_empty() {
            return CommandClass::Command;
        }
        let program = program_name(words[0]);

        match program {
            "sudo" => {
                words = peel_sudo_flags(&words[1..]);
            }
            "env" => {
                words = peel_env_flags(&words[1..]);
            }
            "time" => {
                words = peel_time_flags(&words[1..]);
            }
            "nice" => {
                words = peel_nice_flags(&words[1..]);
            }
            "ionice" => {
                words = peel_ionice_flags(&words[1..]);
            }
            "nohup" => {
                words = &words[1..];
            }
            "timeout" => {
                words = peel_timeout_flags(&words[1..]);
            }
            "stdbuf" => {
                words = peel_stdbuf_flags(&words[1..]);
            }
            "xargs" => {
                words = peel_xargs_flags(&words[1..]);
            }
            "npx" | "bunx" => {
                words = peel_npx_flags(&words[1..]);
            }
            "pnpm" if words.get(1) == Some(&"dlx") => {
                words = peel_npx_flags(&words[2..]);
            }
            "yarn" if words.get(1) == Some(&"dlx") => {
                words = peel_npx_flags(&words[2..]);
            }
            "uv" if words.get(1) == Some(&"run") => {
                words = peel_uv_run_flags(&words[2..]);
            }
            "poetry" if words.get(1) == Some(&"run") => {
                words = peel_simple_runner_flags(&words[2..]);
            }
            "pipenv" if words.get(1) == Some(&"run") => {
                words = peel_simple_runner_flags(&words[2..]);
            }
            "bundle" if words.get(1) == Some(&"exec") => {
                words = peel_simple_runner_flags(&words[2..]);
            }
            "python" | "python3" if words.get(1) == Some(&"-m") && words.len() >= 3 => {
                words = &words[2..];
            }
            "sh" | "bash" | "zsh"
                if words.len() >= 3 && (words[1] == "-c" || words[1] == "-lc") =>
            {
                let inner_cmd = words[2..].join(" ");
                let inner_tokens = tokenize_shell(&inner_cmd);
                let inner_refs: Vec<&str> = inner_tokens.iter().map(|s| s.as_str()).collect();
                return classify(&inner_refs);
            }
            _ => break,
        }
    }

    if words.is_empty() {
        return CommandClass::Command;
    }

    let program = program_name(words[0]);

    // 3. Match specific tool invocations
    match program {
        // Rust / Cargo
        "cargo" => {
            let sub = words[1..]
                .iter()
                .copied()
                .find(|&w| !w.starts_with('-') && !w.starts_with('+'))
                .unwrap_or("");
            match sub {
                "test" | "nextest" | "t" => CommandClass::Test,
                "build" | "b" | "check" | "c" => CommandClass::Build,
                "clippy" => CommandClass::Lint,
                "fmt" => CommandClass::Format,
                "install" => CommandClass::Install,
                "package" | "publish" => CommandClass::Package,
                "run" | "r" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "rustc" => CommandClass::Build,
        "rustfmt" => CommandClass::Format,

        // Git / VCS
        "git" => {
            let rem = peel_git_options(&words[1..]);
            let sub = find_first_non_flag(rem).unwrap_or("");
            match sub {
                "push" => CommandClass::Push,
                "commit" => CommandClass::Commit,
                "rebase" | "merge" | "cherry-pick" | "revert" | "absorb" => CommandClass::Rebase,
                "fetch" | "pull" => CommandClass::Fetch,
                _ => CommandClass::Command,
            }
        }
        "gh" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub1 = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match (sub1, sub2) {
                ("pr", "create" | "merge" | "edit" | "ready") | ("stack", "submit" | "push") => {
                    CommandClass::Push
                }
                ("pr", "sync") | ("stack", "sync") => CommandClass::Rebase,
                ("pr", "checks") | ("run", "watch" | "view") => CommandClass::Test,
                ("pr", "checkout" | "view" | "list" | "status") | ("repo", "clone" | "sync") => {
                    CommandClass::Fetch
                }
                _ => CommandClass::Command,
            }
        }
        "gt" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "submit" => CommandClass::Push,
                "restack" | "sync" => CommandClass::Rebase,
                _ => CommandClass::Command,
            }
        }
        "jj" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "git" if sub2 == "push" => CommandClass::Push,
                "git" if sub2 == "fetch" => CommandClass::Fetch,
                "commit" | "describe" => CommandClass::Commit,
                "rebase" | "squash" => CommandClass::Rebase,
                _ => CommandClass::Command,
            }
        }
        "hg" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "push" => CommandClass::Push,
                "commit" => CommandClass::Commit,
                "rebase" => CommandClass::Rebase,
                "pull" => CommandClass::Fetch,
                _ => CommandClass::Command,
            }
        }

        // Node / JavaScript / TypeScript
        "npm" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "test" | "t" => CommandClass::Test,
                "run" if is_test_word(sub2) => CommandClass::Test,
                "run" if is_build_word(sub2) => CommandClass::Build,
                "run" if is_lint_word(sub2) => CommandClass::Lint,
                "run" if is_format_word(sub2) => CommandClass::Format,
                "build" => CommandClass::Build,
                "lint" => CommandClass::Lint,
                "format" => CommandClass::Format,
                "install" | "i" | "ci" => CommandClass::Install,
                "pack" => CommandClass::Package,
                "start" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "yarn" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "test" => CommandClass::Test,
                "run" if is_test_word(sub2) => CommandClass::Test,
                "run" if is_build_word(sub2) => CommandClass::Build,
                "run" if is_lint_word(sub2) => CommandClass::Lint,
                "run" if is_format_word(sub2) => CommandClass::Format,
                "build" => CommandClass::Build,
                "lint" => CommandClass::Lint,
                "format" => CommandClass::Format,
                "add" | "install" => CommandClass::Install,
                "start" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "pnpm" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "test" | "t" => CommandClass::Test,
                "run" if is_test_word(sub2) => CommandClass::Test,
                "run" if is_build_word(sub2) => CommandClass::Build,
                "run" if is_lint_word(sub2) => CommandClass::Lint,
                "run" if is_format_word(sub2) => CommandClass::Format,
                "build" => CommandClass::Build,
                "lint" => CommandClass::Lint,
                "format" => CommandClass::Format,
                "add" | "install" | "i" => CommandClass::Install,
                "start" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "bun" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "test" => CommandClass::Test,
                "build" => CommandClass::Build,
                "add" | "install" | "i" => CommandClass::Install,
                "run" if is_test_word(sub2) => CommandClass::Test,
                "run" if is_build_word(sub2) => CommandClass::Build,
                _ => CommandClass::Command,
            }
        }
        "jest" | "vitest" | "mocha" | "cypress" | "playwright" | "rspec" => CommandClass::Test,
        "rake" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            if is_test_word(sub) || sub.contains("spec") {
                CommandClass::Test
            } else if is_build_word(sub) {
                CommandClass::Build
            } else {
                CommandClass::Command
            }
        }
        "tsc" => CommandClass::Build,
        "eslint" | "oxlint" | "biome" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            if is_format_word(sub) {
                CommandClass::Format
            } else {
                CommandClass::Lint
            }
        }
        "prettier" => CommandClass::Format,

        // Python
        "pytest" | "nosetests" | "tox" | "unittest" => CommandClass::Test,
        "python" | "python3" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            if sub.ends_with(".py") {
                CommandClass::Exec
            } else {
                CommandClass::Command
            }
        }
        "pip" | "pip3" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "install" => CommandClass::Install,
                _ => CommandClass::Command,
            }
        }
        "poetry" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            match sub {
                "install" | "add" => CommandClass::Install,
                "build" => CommandClass::Package,
                _ => CommandClass::Command,
            }
        }
        "uv" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "pip" if sub2 == "install" => CommandClass::Install,
                "add" | "sync" => CommandClass::Install,
                _ => CommandClass::Command,
            }
        }
        "pipenv" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "install" => CommandClass::Install,
                _ => CommandClass::Command,
            }
        }
        "ruff" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "check" => CommandClass::Lint,
                "format" => CommandClass::Format,
                _ => CommandClass::Lint,
            }
        }
        "black" | "isort" | "autopep8" => CommandClass::Format,
        "flake8" | "pylint" | "mypy" | "pyright" => CommandClass::Lint,

        // Go
        "go" => {
            let non_flags = collect_non_flags(&words[1..]);
            let sub = non_flags.first().copied().unwrap_or("");
            let sub2 = non_flags.get(1).copied().unwrap_or("");
            match sub {
                "test" => CommandClass::Test,
                "build" => CommandClass::Build,
                "vet" => CommandClass::Lint,
                "fmt" => CommandClass::Format,
                "get" | "install" => CommandClass::Install,
                "mod" if sub2 == "download" || sub2 == "tidy" || sub2 == "vendor" => {
                    CommandClass::Install
                }
                "run" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "gofmt" => CommandClass::Format,
        "golangci-lint" | "staticcheck" => CommandClass::Lint,

        // JVM: Gradle & Maven
        "gradle" | "gradlew" => classify_gradle_tasks(&words[1..]),
        "mvn" | "mvnw" => classify_mvn_goals(&words[1..]),

        // C / C++ / Native Build systems
        "make" => {
            let targets = collect_non_flags(&words[1..]);
            if targets.iter().any(|&t| is_test_word(t)) {
                CommandClass::Test
            } else if targets.iter().any(|&t| is_lint_word(t)) {
                CommandClass::Lint
            } else if targets.iter().any(|&t| is_format_word(t)) {
                CommandClass::Format
            } else if targets.contains(&"install") {
                CommandClass::Install
            } else if targets.contains(&"clean") && targets.len() == 1 {
                CommandClass::Command
            } else {
                CommandClass::Build
            }
        }
        "ninja" => {
            let targets = collect_non_flags(&words[1..]);
            if targets.iter().any(|&t| is_test_word(t)) {
                CommandClass::Test
            } else {
                CommandClass::Build
            }
        }
        "dotnet" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "test" | "vstest" => CommandClass::Test,
                "build" | "clean" => CommandClass::Build,
                "format" => CommandClass::Format,
                "publish" | "pack" => CommandClass::Package,
                "restore" => CommandClass::Install,
                "run" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "just" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            if is_test_word(sub) {
                CommandClass::Test
            } else if is_build_word(sub) {
                CommandClass::Build
            } else if is_lint_word(sub) {
                CommandClass::Lint
            } else if is_format_word(sub) {
                CommandClass::Format
            } else {
                CommandClass::Command
            }
        }
        "gcc" | "g++" | "clang" | "clang++" => CommandClass::Build,
        "cmake" => {
            if words.contains(&"--build") {
                CommandClass::Build
            } else if words.contains(&"--install") {
                CommandClass::Install
            } else {
                CommandClass::Command
            }
        }
        "ctest" => CommandClass::Test,
        "clang-format" => CommandClass::Format,
        "clang-tidy" | "cppcheck" => CommandClass::Lint,

        // Bazel / Blaze
        "bazel" | "blaze" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "test" | "coverage" => CommandClass::Test,
                "build" => CommandClass::Build,
                "run" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }

        // Packaging & Containers
        "docker" | "podman" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "build" => CommandClass::Package,
                "run" => CommandClass::Exec,
                _ => CommandClass::Command,
            }
        }
        "tar" | "zip" => CommandClass::Package,

        // System package managers
        "apt" | "apt-get" | "brew" | "yum" | "dnf" | "apk" | "pacman" => {
            let sub = find_first_non_flag(&words[1..]).unwrap_or("");
            match sub {
                "install" | "add" => CommandClass::Install,
                _ => CommandClass::Command,
            }
        }

        // Shell linters
        "shellcheck" | "hadolint" | "markdownlint" | "yamllint" => CommandClass::Lint,

        _ => {
            if program.starts_with("./") || program.contains('/') {
                CommandClass::Exec
            } else {
                CommandClass::Command
            }
        }
    }
}

/// Extracts the base executable name from a path.
#[inline]
fn program_name(raw: &str) -> &str {
    raw.rsplit('/').next().unwrap_or(raw)
}

/// Peels git top-level options (-C <dir>, -c <k=v>, --git-dir <dir>, etc.).
fn peel_git_options<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if (w == "-C" || w == "-c" || w == "--git-dir" || w == "--work-tree" || w == "--namespace")
            && words.len() >= 2
        {
            words = &words[2..];
        } else if w.starts_with("-C")
            || w.starts_with("-c")
            || w.starts_with("--git-dir=")
            || w.starts_with("--work-tree=")
            || w.starts_with("--namespace=")
            || w.starts_with("--exec-path")
            || w == "--bare"
            || w == "--no-pager"
            || w == "-p"
            || w == "--literal-pathspecs"
            || w == "--no-replace-objects"
        {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `sudo`.
fn peel_sudo_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-u" || w == "-g" || w == "-p" || w == "-C" || w == "-U" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags and variable assignments from `env`.
fn peel_env_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-u" || w == "-C" || w == "-S" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') || is_env_var(w) {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `time`.
fn peel_time_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-f" || w == "-o" || w == "--format" || w == "--output" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `nice`.
fn peel_nice_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-n" || w == "--adjustment" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `ionice`.
fn peel_ionice_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-c" || w == "-n" || w == "-p" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags and duration from `timeout`.
fn peel_timeout_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    // 1. Skip flags
    while !words.is_empty() {
        let w = words[0];
        if w == "-s" || w == "-k" || w == "--signal" || w == "--kill-after" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    // 2. Next argument is DURATION
    if !words.is_empty() {
        words = &words[1..];
    }
    words
}

/// Peels flags from `stdbuf`.
fn peel_stdbuf_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if (w == "-i" || w == "-o" || w == "-e") && words.len() > 1 {
            words = &words[2..];
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `xargs`.
fn peel_xargs_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-n"
            || w == "-I"
            || w == "-L"
            || w == "-P"
            || w == "-d"
            || w == "-E"
            || w == "-a"
            || w == "--max-args"
            || w == "--replace"
            || w == "--max-lines"
            || w == "--max-procs"
        {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `npx` / `bunx`.
fn peel_npx_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-p" || w == "--package" || w == "-c" {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels flags from `uv run`.
fn peel_uv_run_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() {
        let w = words[0];
        if w == "-p"
            || w == "--python"
            || w == "--package"
            || w == "--extra"
            || w == "--with"
            || w == "--with-requirements"
        {
            words = if words.len() > 1 {
                &words[2..]
            } else {
                &words[1..]
            };
        } else if w.starts_with('-') {
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// Peels generic runner flags (e.g. `poetry run`, `pipenv run`, `bundle exec`).
fn peel_simple_runner_flags<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while !words.is_empty() && words[0].starts_with('-') {
        words = &words[1..];
    }
    words
}

/// Finds the first argument that does not start with `-`.
fn find_first_non_flag<'a>(words: &'a [&'a str]) -> Option<&'a str> {
    words.iter().copied().find(|&w| !w.starts_with('-'))
}

/// Collects all arguments that do not start with `-`.
fn collect_non_flags<'a>(words: &'a [&'a str]) -> Vec<&'a str> {
    words
        .iter()
        .copied()
        .filter(|&w| !w.starts_with('-'))
        .collect()
}

/// Classifies Gradle tasks into `CommandClass`.
fn classify_gradle_tasks(words: &[&str]) -> CommandClass {
    let tasks = collect_non_flags(words);
    if tasks.iter().any(|&t| is_lint_word(t)) {
        CommandClass::Lint
    } else if tasks
        .iter()
        .any(|&t| is_format_word(t) || t.contains("spotless"))
    {
        CommandClass::Format
    } else if tasks.iter().any(|&t| is_test_word(t)) {
        CommandClass::Test
    } else if tasks
        .iter()
        .any(|&t| t.contains("publish") || t.contains("distZip") || t.contains("distTar"))
    {
        CommandClass::Package
    } else {
        CommandClass::Build
    }
}

/// Classifies Maven goals into `CommandClass`.
fn classify_mvn_goals(words: &[&str]) -> CommandClass {
    let goals = collect_non_flags(words);
    if goals.iter().any(|&g| g.contains("test") || g == "verify") {
        CommandClass::Test
    } else if goals
        .iter()
        .any(|&g| g.contains("checkstyle") || g.contains("spotbugs") || g.contains("sonar"))
    {
        CommandClass::Lint
    } else if goals
        .iter()
        .any(|&g| g.contains("spotless:apply") || g.contains("fmt:format"))
    {
        CommandClass::Format
    } else if goals.iter().any(|&g| g == "package" || g == "deploy") {
        CommandClass::Package
    } else if goals.contains(&"install") {
        CommandClass::Install
    } else {
        CommandClass::Build
    }
}

#[inline]
fn is_env_var(s: &str) -> bool {
    let mut parts = s.splitn(2, '=');
    if let (Some(k), Some(_)) = (parts.next(), parts.next()) {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

#[inline]
fn is_test_word(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("test") || lower == "check" || lower.contains("check")
}

#[inline]
fn is_build_word(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("build") || lower.contains("compile") || lower.contains("assemble")
}

#[inline]
fn is_lint_word(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("lint")
        || lower == "clippy"
        || lower.contains("checkstyle")
        || lower.contains("detekt")
}

#[inline]
fn is_format_word(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("fmt") || lower.contains("format") || lower.contains("spotless")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_table() {
        let table: &[(&str, CommandClass)] = &[
            // 1. Rust / Cargo (25 commands)
            ("cargo build", CommandClass::Build),
            ("cargo build --release -p my-crate", CommandClass::Build),
            ("cargo check", CommandClass::Build),
            ("cargo check --all-targets", CommandClass::Build),
            ("cargo b", CommandClass::Build),
            ("cargo c", CommandClass::Build),
            ("cargo test", CommandClass::Test),
            ("cargo test -p core -- --nocapture", CommandClass::Test),
            ("cargo nextest run", CommandClass::Test),
            ("cargo t", CommandClass::Test),
            ("cargo clippy", CommandClass::Lint),
            ("cargo clippy -- -D warnings", CommandClass::Lint),
            ("cargo fmt", CommandClass::Format),
            ("cargo fmt --check", CommandClass::Format),
            ("cargo install cargo-watch", CommandClass::Install),
            ("cargo package", CommandClass::Package),
            ("cargo publish", CommandClass::Package),
            ("cargo run --bin app", CommandClass::Exec),
            ("cargo r", CommandClass::Exec),
            ("cargo +nightly test", CommandClass::Test),
            ("cargo +beta check", CommandClass::Build),
            ("cargo --quiet test", CommandClass::Test),
            ("cargo -q build", CommandClass::Build),
            ("rustc main.rs -o main", CommandClass::Build),
            ("rustfmt src/lib.rs", CommandClass::Format),
            // 2. Git / VCS (20 commands)
            ("git push origin main", CommandClass::Push),
            ("git push --force-with-lease", CommandClass::Push),
            ("git commit -m 'Initial commit'", CommandClass::Commit),
            ("git commit -am 'fix bug'", CommandClass::Commit),
            ("git rebase -i HEAD~3", CommandClass::Rebase),
            ("git rebase origin/main", CommandClass::Rebase),
            ("git merge origin/main", CommandClass::Rebase),
            ("git cherry-pick abc1234", CommandClass::Rebase),
            ("git revert HEAD", CommandClass::Rebase),
            ("git fetch --all", CommandClass::Fetch),
            ("git pull origin main", CommandClass::Fetch),
            ("git pull -r", CommandClass::Fetch),
            ("git checkout -b feature", CommandClass::Command),
            ("git status", CommandClass::Command),
            ("git diff", CommandClass::Command),
            ("git log -n 10", CommandClass::Command),
            ("jj git push", CommandClass::Push),
            ("jj git fetch", CommandClass::Fetch),
            ("jj commit -m 'new change'", CommandClass::Commit),
            ("jj describe -m 'title'", CommandClass::Commit),
            ("jj rebase -d main", CommandClass::Rebase),
            ("jj squash", CommandClass::Rebase),
            ("hg push", CommandClass::Push),
            ("hg commit -m 'wip'", CommandClass::Commit),
            ("hg rebase", CommandClass::Rebase),
            ("hg pull -u", CommandClass::Fetch),
            ("git -C /tmp/repo rebase origin/main", CommandClass::Rebase),
            (
                "git -c user.name=Bot commit -m 'bot commit'",
                CommandClass::Commit,
            ),
            ("git --git-dir=/repo/.git push", CommandClass::Push),
            (
                "git -C /path/to/repo cherry-pick abc1234",
                CommandClass::Rebase,
            ),
            ("git -C /path/to/repo fetch", CommandClass::Fetch),
            ("git absorb", CommandClass::Rebase),
            ("gh pr create --fill", CommandClass::Push),
            ("gh pr merge --auto", CommandClass::Push),
            ("gh pr edit --title 'new title'", CommandClass::Push),
            ("gh pr ready", CommandClass::Push),
            ("gh stack submit", CommandClass::Push),
            ("gh stack push", CommandClass::Push),
            ("gt submit", CommandClass::Push),
            ("gh pr sync", CommandClass::Rebase),
            ("gh stack sync", CommandClass::Rebase),
            ("gt restack", CommandClass::Rebase),
            ("gt sync", CommandClass::Rebase),
            ("gh pr checks", CommandClass::Test),
            ("gh run watch", CommandClass::Test),
            ("gh run view", CommandClass::Test),
            ("gh pr checkout 123", CommandClass::Fetch),
            ("gh pr view", CommandClass::Fetch),
            ("gh pr list", CommandClass::Fetch),
            ("gh pr status", CommandClass::Fetch),
            ("gh repo clone owner/repo", CommandClass::Fetch),
            ("gh repo sync", CommandClass::Fetch),
            // 3. Node / JavaScript / TypeScript (25 commands)
            ("npm test", CommandClass::Test),
            ("npm t", CommandClass::Test),
            ("npm run test", CommandClass::Test),
            ("npm run test:unit", CommandClass::Test),
            ("npm run check", CommandClass::Test),
            ("npm run build", CommandClass::Build),
            ("npm run compile", CommandClass::Build),
            ("npm run lint", CommandClass::Lint),
            ("npm run lint:fix", CommandClass::Lint),
            ("npm run format", CommandClass::Format),
            ("npm install react", CommandClass::Install),
            ("npm i -D typescript", CommandClass::Install),
            ("npm ci", CommandClass::Install),
            ("npm pack", CommandClass::Package),
            ("npm start", CommandClass::Exec),
            ("yarn test", CommandClass::Test),
            ("yarn build", CommandClass::Build),
            ("yarn lint", CommandClass::Lint),
            ("yarn format", CommandClass::Format),
            ("yarn add lodash", CommandClass::Install),
            ("yarn start", CommandClass::Exec),
            ("pnpm test", CommandClass::Test),
            ("pnpm t", CommandClass::Test),
            ("pnpm build", CommandClass::Build),
            ("pnpm lint", CommandClass::Lint),
            ("pnpm format", CommandClass::Format),
            ("pnpm add express", CommandClass::Install),
            ("pnpm start", CommandClass::Exec),
            ("bun test", CommandClass::Test),
            ("bun build", CommandClass::Build),
            ("bun install", CommandClass::Install),
            ("bun run test", CommandClass::Test),
            ("bun run build", CommandClass::Build),
            ("jest --coverage", CommandClass::Test),
            ("vitest run", CommandClass::Test),
            ("mocha tests/", CommandClass::Test),
            ("cypress run", CommandClass::Test),
            ("playwright test", CommandClass::Test),
            ("tsc --noEmit", CommandClass::Build),
            ("eslint . --ext .ts", CommandClass::Lint),
            ("oxlint src/", CommandClass::Lint),
            ("prettier --write .", CommandClass::Format),
            // 4. Python (20 commands)
            ("pytest tests/", CommandClass::Test),
            ("pytest -v -k 'test_unit'", CommandClass::Test),
            ("nosetests", CommandClass::Test),
            ("tox", CommandClass::Test),
            ("python3 -m pytest", CommandClass::Test),
            ("python -m unittest discover", CommandClass::Test),
            (
                "python -m pip install -r requirements.txt",
                CommandClass::Install,
            ),
            ("pip install torch", CommandClass::Install),
            ("pip3 install -e .", CommandClass::Install),
            ("poetry install", CommandClass::Install),
            ("poetry run pytest", CommandClass::Test),
            ("poetry build", CommandClass::Package),
            ("uv pip install flask", CommandClass::Install),
            ("uv add requests", CommandClass::Install),
            ("uv sync", CommandClass::Install),
            ("uv run pytest", CommandClass::Test),
            ("uv run --frozen pytest", CommandClass::Test),
            ("uv run python -m unittest", CommandClass::Test),
            ("ruff check .", CommandClass::Lint),
            ("ruff format .", CommandClass::Format),
            ("black src/", CommandClass::Format),
            ("isort .", CommandClass::Format),
            ("flake8 .", CommandClass::Lint),
            ("mypy src/", CommandClass::Lint),
            ("pyright src/", CommandClass::Lint),
            ("python main.py", CommandClass::Exec),
            // 5. Go (12 commands)
            ("go test ./...", CommandClass::Test),
            ("go test -v -race .", CommandClass::Test),
            ("go build -o server ./cmd/server", CommandClass::Build),
            ("go vet ./...", CommandClass::Lint),
            ("go fmt ./...", CommandClass::Format),
            ("gofmt -w .", CommandClass::Format),
            ("golangci-lint run", CommandClass::Lint),
            ("staticcheck ./...", CommandClass::Lint),
            ("go get github.com/gin-gonic/gin", CommandClass::Install),
            ("go mod download", CommandClass::Install),
            ("go mod tidy", CommandClass::Install),
            ("go run main.go", CommandClass::Exec),
            // 6. JVM Tools: Gradle & Maven (15 commands)
            ("gradle test", CommandClass::Test),
            ("gradle testDebugUnitTest", CommandClass::Test),
            ("gradle check", CommandClass::Test),
            ("gradle build", CommandClass::Build),
            ("./gradlew test", CommandClass::Test),
            ("./gradlew check", CommandClass::Test),
            ("./gradlew build", CommandClass::Build),
            ("./gradlew assembleRelease", CommandClass::Build),
            ("./gradlew ktlintCheck", CommandClass::Lint),
            ("./gradlew spotlessApply", CommandClass::Format),
            ("mvn test", CommandClass::Test),
            ("./mvnw test", CommandClass::Test),
            ("mvn verify", CommandClass::Test),
            ("mvn clean compile", CommandClass::Build),
            ("mvn compile", CommandClass::Build),
            ("mvn package", CommandClass::Package),
            ("mvn install", CommandClass::Install),
            ("mvn spotless:apply", CommandClass::Format),
            ("mvn checkstyle:check", CommandClass::Lint),
            // 7. C/C++ & Native Build (15 commands)
            ("make -j8", CommandClass::Build),
            ("make all", CommandClass::Build),
            ("make test", CommandClass::Test),
            ("make check", CommandClass::Test),
            ("make install", CommandClass::Install),
            ("make clean", CommandClass::Command),
            ("ninja -C out/release", CommandClass::Build),
            ("ninja test", CommandClass::Test),
            ("ninja check", CommandClass::Test),
            ("cmake --build build", CommandClass::Build),
            ("cmake --install build", CommandClass::Install),
            ("ctest --verbose", CommandClass::Test),
            ("gcc -O2 main.c", CommandClass::Build),
            ("clang++ -std=c++20 app.cc", CommandClass::Build),
            ("clang-format -i src/foo.cc", CommandClass::Format),
            ("clang-tidy src/foo.cc", CommandClass::Lint),
            // 8. Bazel / Blaze (8 commands)
            ("bazel test //...", CommandClass::Test),
            ("bazel build //src:main", CommandClass::Build),
            ("bazel run //cmd:tool", CommandClass::Exec),
            ("bazel --output_base=/tmp test //...", CommandClass::Test),
            ("bazel --output_base=/tmp build //...", CommandClass::Build),
            ("blaze test //my/pkg:all", CommandClass::Test),
            ("blaze build //my/pkg:binary", CommandClass::Build),
            ("blaze --c=dbg test //...", CommandClass::Test),
            // 9. Containers & System Packages (8 commands)
            ("docker build -t app:latest .", CommandClass::Package),
            ("docker run -it ubuntu bash", CommandClass::Exec),
            ("podman build -t image .", CommandClass::Package),
            ("podman run --rm alpine", CommandClass::Exec),
            ("apt-get install -y build-essential", CommandClass::Install),
            ("brew install ripgrep", CommandClass::Install),
            ("apk add curl", CommandClass::Install),
            ("tar -czf archive.tar.gz src/", CommandClass::Package),
            // 10. Shell linters & utilities (8 commands)
            ("shellcheck script.sh", CommandClass::Lint),
            ("hadolint Dockerfile", CommandClass::Lint),
            ("markdownlint README.md", CommandClass::Lint),
            ("yamllint .", CommandClass::Lint),
            ("ls -la", CommandClass::Command),
            ("cat file.txt", CommandClass::Command),
            ("mkdir -p /tmp/test", CommandClass::Command),
            ("rm -rf target", CommandClass::Command),
            ("pwd", CommandClass::Command),
            ("echo 'hello'", CommandClass::Command),
            // 11. Wrappers & Prefixes (25 commands)
            ("time -v cargo test", CommandClass::Test),
            ("time -p pytest", CommandClass::Test),
            ("time make all", CommandClass::Build),
            ("nice -n 19 pytest", CommandClass::Test),
            ("nice -19 cargo build", CommandClass::Build),
            ("ionice -c 3 cargo test", CommandClass::Test),
            ("env -i FOO=BAR cargo test", CommandClass::Test),
            ("env -i CC=clang make", CommandClass::Build),
            ("sudo cargo build", CommandClass::Build),
            ("sudo -u root cargo test", CommandClass::Test),
            ("sudo pytest", CommandClass::Test),
            ("xargs -n 1 cargo test", CommandClass::Test),
            ("xargs -I {} cargo test", CommandClass::Test),
            ("timeout 10s cargo test", CommandClass::Test),
            ("timeout -s 9 30 pytest", CommandClass::Test),
            ("stdbuf -oL cargo test", CommandClass::Test),
            ("nohup make all", CommandClass::Build),
            ("npx vitest", CommandClass::Test),
            ("npx jest", CommandClass::Test),
            ("npx -p typescript tsc", CommandClass::Build),
            ("npx --yes prettier --write .", CommandClass::Format),
            ("bunx vitest", CommandClass::Test),
            ("pnpm dlx jest", CommandClass::Test),
            ("yarn dlx eslint", CommandClass::Lint),
            ("bundle exec rspec", CommandClass::Test),
            ("bundle exec rake test", CommandClass::Test),
            ("pipenv run pytest", CommandClass::Test),
            ("RUST_BACKTRACE=1 cargo test", CommandClass::Test),
            ("FOO=bar BAZ=qux python -m pytest", CommandClass::Test),
            // 12. Compound shell lines & pipelines (15 commands)
            ("cd dir && cargo test", CommandClass::Test),
            ("cargo build && cargo test", CommandClass::Test),
            ("git add . && git commit -m 'wip'", CommandClass::Commit),
            (
                "git commit -m 'ready' && git push origin main",
                CommandClass::Push,
            ),
            (
                "npm run lint && npm run build && npm test",
                CommandClass::Test,
            ),
            (
                "cd frontend && pnpm install && pnpm build",
                CommandClass::Build,
            ),
            (
                "git fetch origin && git rebase origin/main",
                CommandClass::Rebase,
            ),
            ("sh -c 'cargo test'", CommandClass::Test),
            ("bash -c 'pytest tests/'", CommandClass::Test),
            ("zsh -lc 'make test'", CommandClass::Test),
            (
                "mkdir -p build && cd build && cmake .. && make -j4",
                CommandClass::Build,
            ),
            ("cargo test | tee test.log", CommandClass::Test),
            ("pytest | cat", CommandClass::Test),
            ("cargo check || cargo build", CommandClass::Build),
            ("cd repo; npm test", CommandClass::Test),
            ("dotnet test", CommandClass::Test),
            ("dotnet build", CommandClass::Build),
            ("just test", CommandClass::Test),
            ("just build", CommandClass::Build),
        ];

        assert!(
            table.len() >= 150,
            "Must test at least 150 commands, got {}",
            table.len()
        );

        for &(cmd, expected) in table {
            let actual = classify(&[cmd]);
            assert_eq!(
                actual, expected,
                "Command {:?} classified as {:?}, expected {:?}",
                cmd, actual, expected
            );
        }
    }
}
