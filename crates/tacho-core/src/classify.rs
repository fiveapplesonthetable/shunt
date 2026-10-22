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

    // If argv has a single element that contains shell operators, split by shell words
    let words: Vec<&str> = if argv.len() == 1 {
        argv[0].as_ref().split_whitespace().collect()
    } else {
        argv.iter().map(|s| s.as_ref()).collect()
    };

    if words.is_empty() {
        return CommandClass::Command;
    }

    // Split compound shell lines by operators: &&, ||, ;, |, |&
    let mut best_class = CommandClass::Command;
    let mut current_segment = Vec::new();

    for &word in &words {
        if word == "&&" || word == "||" || word == ";" || word == "|" || word == "|&" {
            if !current_segment.is_empty() {
                let seg_class = classify_simple_segment(&current_segment);
                if rank(seg_class) > rank(best_class) {
                    best_class = seg_class;
                }
                current_segment.clear();
            }
        } else {
            current_segment.push(word);
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

/// Classifies a single command segment without compound shell operators.
fn classify_simple_segment(words: &[&str]) -> CommandClass {
    // 1. Strip leading environment variables (e.g. `RUST_LOG=debug`, `CC=clang`)
    let mut i = 0;
    while i < words.len() && is_env_var(words[i]) {
        i += 1;
    }
    let words = &words[i..];
    if words.is_empty() {
        return CommandClass::Command;
    }

    // 2. Strip wrapper commands (sudo, env, time, nice, nohup, xargs)
    let program = match words[0].rsplit('/').next() {
        Some(p) => p,
        None => words[0],
    };

    match program {
        "sudo" | "env" | "time" | "nice" | "nohup" | "xargs" => {
            if words.len() > 1 {
                return classify_simple_segment(&words[1..]);
            }
        }
        "sh" | "bash" | "zsh" if words.len() >= 3 && (words[1] == "-c" || words[1] == "-lc") => {
            // Check for `sh -c "cargo test"` or `sh -c 'cargo test'`
            let inner_cmd = words[2..].join(" ");
            let inner_trimmed = inner_cmd.trim_matches(|c| c == '\'' || c == '"');
            let inner: Vec<&str> = inner_trimmed.split_whitespace().collect();
            return classify(&inner);
        }
        _ => {}
    }

    // 3. Match specific tool invocations
    let sub = words.get(1).copied().unwrap_or("");
    let sub2 = words.get(2).copied().unwrap_or("");

    match program {
        // Rust / Cargo
        "cargo" => match sub {
            "test" | "nextest" | "t" => CommandClass::Test,
            "build" | "b" | "check" | "c" => CommandClass::Build,
            "clippy" => CommandClass::Lint,
            "fmt" => CommandClass::Format,
            "install" => CommandClass::Install,
            "package" | "publish" => CommandClass::Package,
            "run" | "r" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "rustc" => CommandClass::Build,
        "rustfmt" => CommandClass::Format,

        // Git / VCS
        "git" => match sub {
            "push" => CommandClass::Push,
            "commit" => CommandClass::Commit,
            "rebase" => CommandClass::Rebase,
            "fetch" | "pull" => CommandClass::Fetch,
            _ => CommandClass::Command,
        },
        "jj" => match sub {
            "git" if sub2 == "push" => CommandClass::Push,
            "git" if sub2 == "fetch" => CommandClass::Fetch,
            "commit" | "describe" => CommandClass::Commit,
            "rebase" | "squash" => CommandClass::Rebase,
            _ => CommandClass::Command,
        },
        "hg" => match sub {
            "push" => CommandClass::Push,
            "commit" => CommandClass::Commit,
            "rebase" => CommandClass::Rebase,
            "pull" => CommandClass::Fetch,
            _ => CommandClass::Command,
        },

        // Node / JavaScript / TypeScript
        "npm" => match sub {
            "test" | "t" => CommandClass::Test,
            "run" if is_test_word(sub2) => CommandClass::Test,
            "run" if is_build_word(sub2) => CommandClass::Build,
            "run" if is_lint_word(sub2) => CommandClass::Lint,
            "run" if is_format_word(sub2) => CommandClass::Format,
            "install" | "i" | "ci" => CommandClass::Install,
            "pack" => CommandClass::Package,
            "start" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "yarn" => match sub {
            "test" => CommandClass::Test,
            "build" => CommandClass::Build,
            "lint" => CommandClass::Lint,
            "format" => CommandClass::Format,
            "add" | "install" => CommandClass::Install,
            "start" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "pnpm" => match sub {
            "test" | "t" => CommandClass::Test,
            "build" => CommandClass::Build,
            "lint" => CommandClass::Lint,
            "format" => CommandClass::Format,
            "add" | "install" | "i" => CommandClass::Install,
            "start" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "bun" => match sub {
            "test" => CommandClass::Test,
            "build" => CommandClass::Build,
            "add" | "install" | "i" => CommandClass::Install,
            "run" if is_test_word(sub2) => CommandClass::Test,
            "run" if is_build_word(sub2) => CommandClass::Build,
            _ => CommandClass::Command,
        },
        "jest" | "vitest" | "mocha" | "cypress" | "playwright" => CommandClass::Test,
        "tsc" => CommandClass::Build,
        "eslint" | "oxlint" | "biome" if sub == "lint" || sub == "check" => CommandClass::Lint,
        "eslint" | "oxlint" => CommandClass::Lint,
        "prettier" => CommandClass::Format,

        // Python
        "pytest" | "nosetests" | "tox" => CommandClass::Test,
        "python" | "python3" => {
            if sub == "-m" {
                match sub2 {
                    "pytest" | "unittest" => CommandClass::Test,
                    "pip" => CommandClass::Install,
                    "build" | "flit" => CommandClass::Package,
                    "ruff" => CommandClass::Lint,
                    _ => CommandClass::Exec,
                }
            } else if sub.ends_with(".py") {
                CommandClass::Exec
            } else {
                CommandClass::Command
            }
        }
        "pip" | "pip3" => match sub {
            "install" => CommandClass::Install,
            _ => CommandClass::Command,
        },
        "poetry" => match sub {
            "run" if is_test_word(sub2) => CommandClass::Test,
            "run" if is_build_word(sub2) => CommandClass::Build,
            "install" | "add" => CommandClass::Install,
            "build" => CommandClass::Package,
            _ => CommandClass::Command,
        },
        "uv" => match sub {
            "pip" if sub2 == "install" => CommandClass::Install,
            "add" | "sync" => CommandClass::Install,
            "run" if is_test_word(sub2) => CommandClass::Test,
            "run" if is_build_word(sub2) => CommandClass::Build,
            _ => CommandClass::Command,
        },
        "ruff" => match sub {
            "check" => CommandClass::Lint,
            "format" => CommandClass::Format,
            _ => CommandClass::Lint,
        },
        "black" | "isort" | "autopep8" => CommandClass::Format,
        "flake8" | "pylint" | "mypy" | "pyright" => CommandClass::Lint,

        // Go
        "go" => match sub {
            "test" => CommandClass::Test,
            "build" => CommandClass::Build,
            "vet" => CommandClass::Lint,
            "fmt" => CommandClass::Format,
            "get" | "install" => CommandClass::Install,
            "mod" if sub2 == "download" || sub2 == "tidy" => CommandClass::Install,
            "run" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "gofmt" => CommandClass::Format,
        "golangci-lint" | "staticcheck" => CommandClass::Lint,

        // C / C++ / Build systems
        "make" | "ninja" => CommandClass::Build,
        "gcc" | "g++" | "clang" | "clang++" => CommandClass::Build,
        "cmake" => {
            if words.contains(&"--build") {
                CommandClass::Build
            } else {
                CommandClass::Command
            }
        }
        "ctest" => CommandClass::Test,
        "clang-format" => CommandClass::Format,
        "clang-tidy" | "cppcheck" => CommandClass::Lint,

        // Google / Bazel / Blaze
        "bazel" | "blaze" => match sub {
            "test" => CommandClass::Test,
            "build" => CommandClass::Build,
            "run" => CommandClass::Exec,
            _ => CommandClass::Command,
        },

        // Packaging & Containers
        "docker" | "podman" => match sub {
            "build" => CommandClass::Package,
            "run" => CommandClass::Exec,
            _ => CommandClass::Command,
        },
        "tar" | "zip" => CommandClass::Package,

        // System packages
        "apt" | "apt-get" | "brew" | "yum" | "dnf" | "apk" => match sub {
            "install" | "add" => CommandClass::Install,
            _ => CommandClass::Command,
        },

        // Shell linters
        "shellcheck" | "hadolint" | "markdownlint" | "yamllint" => CommandClass::Lint,

        _ => {
            if program.starts_with("./") {
                CommandClass::Exec
            } else {
                CommandClass::Command
            }
        }
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
    s.contains("test") || s == "check"
}

#[inline]
fn is_build_word(s: &str) -> bool {
    s.contains("build") || s == "compile"
}

#[inline]
fn is_lint_word(s: &str) -> bool {
    s.contains("lint") || s == "clippy"
}

#[inline]
fn is_format_word(s: &str) -> bool {
    s.contains("fmt") || s.contains("format")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_table() {
        let table: &[(&str, CommandClass)] = &[
            // 1. Rust / Cargo
            ("cargo build", CommandClass::Build),
            ("cargo build --release -p my-crate", CommandClass::Build),
            ("cargo check", CommandClass::Build),
            ("cargo check --all-targets", CommandClass::Build),
            ("cargo b", CommandClass::Build),
            ("cargo c", CommandClass::Build),
            ("cargo test", CommandClass::Test),
            ("cargo test -p core -- --nocapture", CommandClass::Test),
            ("cargo nextest run", CommandClass::Test),
            ("cargo clippy", CommandClass::Lint),
            ("cargo clippy -- -D warnings", CommandClass::Lint),
            ("cargo fmt", CommandClass::Format),
            ("cargo fmt --check", CommandClass::Format),
            ("cargo install cargo-watch", CommandClass::Install),
            ("cargo package", CommandClass::Package),
            ("cargo publish", CommandClass::Package),
            ("cargo run --bin app", CommandClass::Exec),
            ("rustc main.rs -o main", CommandClass::Build),
            ("rustfmt src/lib.rs", CommandClass::Format),
            // 2. Git / VCS
            ("git push origin main", CommandClass::Push),
            ("git push --force-with-lease", CommandClass::Push),
            ("git commit -m 'Initial commit'", CommandClass::Commit),
            ("git commit -am 'fix bug'", CommandClass::Commit),
            ("git rebase -i HEAD~3", CommandClass::Rebase),
            ("git rebase origin/main", CommandClass::Rebase),
            ("git fetch --all", CommandClass::Fetch),
            ("git pull origin main", CommandClass::Fetch),
            ("jj git push", CommandClass::Push),
            ("jj git fetch", CommandClass::Fetch),
            ("jj commit -m 'new change'", CommandClass::Commit),
            ("jj describe -m 'title'", CommandClass::Commit),
            ("jj rebase -d main", CommandClass::Rebase),
            ("hg push", CommandClass::Push),
            ("hg commit -m 'wip'", CommandClass::Commit),
            ("hg pull -u", CommandClass::Fetch),
            // 3. Node / JavaScript
            ("npm test", CommandClass::Test),
            ("npm t", CommandClass::Test),
            ("npm run test", CommandClass::Test),
            ("npm run build", CommandClass::Build),
            ("npm run lint", CommandClass::Lint),
            ("npm run format", CommandClass::Format),
            ("npm install react", CommandClass::Install),
            ("npm i -D typescript", CommandClass::Install),
            ("npm ci", CommandClass::Install),
            ("npm pack", CommandClass::Package),
            ("npm start", CommandClass::Exec),
            ("yarn test", CommandClass::Test),
            ("yarn build", CommandClass::Build),
            ("yarn lint", CommandClass::Lint),
            ("yarn add lodash", CommandClass::Install),
            ("pnpm test", CommandClass::Test),
            ("pnpm build", CommandClass::Build),
            ("pnpm add express", CommandClass::Install),
            ("bun test", CommandClass::Test),
            ("bun install", CommandClass::Install),
            ("jest --coverage", CommandClass::Test),
            ("vitest run", CommandClass::Test),
            ("tsc --noEmit", CommandClass::Build),
            ("eslint . --ext .ts", CommandClass::Lint),
            ("prettier --write .", CommandClass::Format),
            // 4. Python
            ("pytest tests/", CommandClass::Test),
            ("pytest -v -k 'test_unit'", CommandClass::Test),
            ("python3 -m pytest", CommandClass::Test),
            ("python -m unittest discover", CommandClass::Test),
            (
                "python -m pip install -r requirements.txt",
                CommandClass::Install,
            ),
            ("pip install torch", CommandClass::Install),
            ("poetry install", CommandClass::Install),
            ("poetry run pytest", CommandClass::Test),
            ("uv pip install flask", CommandClass::Install),
            ("uv run pytest", CommandClass::Test),
            ("ruff check .", CommandClass::Lint),
            ("ruff format .", CommandClass::Format),
            ("black src/", CommandClass::Format),
            ("flake8 .", CommandClass::Lint),
            ("mypy src/", CommandClass::Lint),
            ("python main.py", CommandClass::Exec),
            // 5. Go
            ("go test ./...", CommandClass::Test),
            ("go test -v -race .", CommandClass::Test),
            ("go build -o server ./cmd/server", CommandClass::Build),
            ("go vet ./...", CommandClass::Lint),
            ("go fmt ./...", CommandClass::Format),
            ("gofmt -w .", CommandClass::Format),
            ("golangci-lint run", CommandClass::Lint),
            ("go get github.com/gin-gonic/gin", CommandClass::Install),
            ("go mod tidy", CommandClass::Install),
            ("go run main.go", CommandClass::Exec),
            // 6. C/C++ & Native Build
            ("make -j8", CommandClass::Build),
            ("make all", CommandClass::Build),
            ("ninja -C out/release", CommandClass::Build),
            ("cmake --build build", CommandClass::Build),
            ("ctest --verbose", CommandClass::Test),
            ("gcc -O2 main.c", CommandClass::Build),
            ("clang++ -std=c++20 app.cc", CommandClass::Build),
            ("clang-format -i src/foo.cc", CommandClass::Format),
            ("clang-tidy src/foo.cc", CommandClass::Lint),
            // 7. Bazel / Blaze
            ("bazel test //...", CommandClass::Test),
            ("bazel build //src:main", CommandClass::Build),
            ("blaze test //my/pkg:all", CommandClass::Test),
            ("blaze build //my/pkg:binary", CommandClass::Build),
            // 8. Containers & System
            ("docker build -t app:latest .", CommandClass::Package),
            ("podman build -t image .", CommandClass::Package),
            ("apt-get install -y build-essential", CommandClass::Install),
            ("brew install ripgrep", CommandClass::Install),
            ("tar -czf archive.tar.gz src/", CommandClass::Package),
            ("shellcheck script.sh", CommandClass::Lint),
            // 9. Standard commands
            ("ls -la", CommandClass::Command),
            ("cat file.txt", CommandClass::Command),
            ("mkdir -p /tmp/test", CommandClass::Command),
            ("rm -rf target", CommandClass::Command),
            ("pwd", CommandClass::Command),
            ("echo 'hello'", CommandClass::Command),
            // 10. Wrappers & Compound Shell Lines
            ("sudo cargo build", CommandClass::Build),
            ("RUST_BACKTRACE=1 cargo test", CommandClass::Test),
            ("FOO=bar BAZ=qux python -m pytest", CommandClass::Test),
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
            (
                "mkdir -p build && cd build && cmake .. && make -j4",
                CommandClass::Build,
            ),
        ];

        assert!(
            table.len() >= 60,
            "Must test at least 60 commands, got {}",
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
