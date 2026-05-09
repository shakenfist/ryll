//! Build-time helper: capture the current git SHA so the
//! resulting binary can identify itself at runtime. The SHA
//! is exposed to the source via `env!("RYLL_GIT_SHA")`.
//!
//! Resolution order:
//!
//! 1. `RYLL_GIT_SHA` environment variable. The repo's Makefile
//!    sets this at make-time (because the build itself runs
//!    inside a devcontainer where the host's `.git/worktrees/...`
//!    pointer is not accessible). This is the load-bearing path
//!    for repo builds.
//! 2. `git rev-parse --short=8 HEAD` + dirty check, run from the
//!    crate's manifest directory. Works for native `cargo build`
//!    against a regular checkout (CI release runners, contributor
//!    machines without docker, the Makefile's macos-build target).
//! 3. Fallback `"unknown"`. Hit when building from a tarball
//!    without a `.git` directory, or when both git invocations
//!    fail for any reason.
//!
//! `cargo:rerun-if-changed` triggers force a rebuild when the
//! HEAD ref or working tree state changes, so the embedded SHA
//! stays accurate during iterative development.

use std::process::Command;

fn main() {
    let sha = git_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RYLL_GIT_SHA={}", sha);

    // Re-run the build script when the env var changes (Makefile
    // path) or when HEAD / index moves (native git path).
    println!("cargo:rerun-if-env-changed=RYLL_GIT_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
}

fn git_sha() -> Option<String> {
    if let Ok(s) = std::env::var("RYLL_GIT_SHA") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let short = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !short.status.success() {
        return None;
    }
    let mut sha = String::from_utf8(short.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }

    // `--porcelain` empty output = clean tree; anything = dirty.
    if let Ok(status) = Command::new("git").args(["status", "--porcelain"]).output() {
        if status.status.success() && !status.stdout.is_empty() {
            sha.push_str("-dirty");
        }
    }

    Some(sha)
}
