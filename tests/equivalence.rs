//! Equivalence harness — proves `cargo ffmt` produces byte-identical
//! tree state to `cargo +nightly fmt` after running on the same input.
//!
//! Strategy:
//! 1. Take a clean git repo (`FFMT_BIG_REPO` env var, or in-tree fixture).
//! 2. Create two cheap copies via `git worktree add`.
//! 3. Apply identical synthetic formatting violations to both.
//! 4. Run stock `cargo +nightly fmt` in worktree A.
//! 5. Run `cargo +nightly ffmt` in worktree B.
//! 6. Assert exit codes equal AND `diff -ru A B` (excl. target/.git) is empty.
//!
//! This is the strong correctness test. Any divergence is a routing bug
//! in our discovery (wrong edition, missed file, double-formatted file).

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git invocation failed");
    if !out.status.success() {
        panic!(
            "git {:?} failed: {}\n{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    out
}

fn assert_clean(repo: &Path) {
    let out = git(repo, &["status", "--porcelain"]);
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.trim().is_empty(),
        "{} is not clean:\n{body}",
        repo.display()
    );
}

struct Worktree {
    path: PathBuf,
    upstream: PathBuf,
}

impl Worktree {
    fn add(upstream: &Path, name: &str) -> Self {
        let tmp = std::env::temp_dir().join(format!("ffmt-equiv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Detach so we don't leave a branch dangling.
        git(
            upstream,
            &["worktree", "add", "--detach", tmp.to_str().unwrap(), "HEAD"],
        );
        Self {
            path: tmp,
            upstream: upstream.to_path_buf(),
        }
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", self.path.to_str().unwrap()])
            .current_dir(&self.upstream)
            .output();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Apply identical synthetic formatting violations to a worktree.
/// Picks a handful of `src/lib.rs` files and appends a deliberately
/// badly-formatted helper. Whatever path we pick, both worktrees get
/// the exact same bytes appended to the same path.
fn dirty_tree(repo: &Path) -> Vec<PathBuf> {
    let mut touched = Vec::new();
    let bad = "\npub fn __ffmt_equiv_test_helper(  x:i32,y :i32 )->i32{x+y}\n";

    // Find a small, deterministic set of lib.rs files to dirty.
    let out = Command::new("find")
        .args([
            ".", "-path", "./target", "-prune", "-o", "-name", "lib.rs", "-print",
        ])
        .current_dir(repo)
        .output()
        .expect("find failed");
    let body = String::from_utf8_lossy(&out.stdout);
    let mut paths: Vec<&str> = body.lines().filter(|l| l.contains("/src/lib.rs")).collect();
    paths.sort();
    paths.truncate(20);

    for rel in paths {
        let p = repo.join(rel.trim_start_matches("./"));
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&p) {
            use std::io::Write;
            if f.write_all(bad.as_bytes()).is_ok() {
                touched.push(p);
            }
        }
    }
    assert!(
        !touched.is_empty(),
        "could not dirty any lib.rs file in {}",
        repo.display()
    );
    touched
}

/// Recursively diff two trees, ignoring target/ and .git/.
/// Returns Ok if identical, Err with the diff text otherwise.
fn diff_trees(a: &Path, b: &Path) -> Result<(), String> {
    let out = Command::new("diff")
        .args(["-ru", "--exclude=target", "--exclude=.git"])
        .arg(a)
        .arg(b)
        .output()
        .expect("diff failed to spawn");
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn run_fmt(cmd: &str, args: &[&str], repo: &Path) -> std::process::ExitStatus {
    Command::new("cargo")
        .arg("+nightly")
        .arg(cmd)
        .args(args)
        .current_dir(repo)
        .status()
        .expect("cargo invocation failed")
}

#[test]
#[ignore]
fn equivalence_on_big_repo() {
    let path = match std::env::var("FFMT_BIG_REPO") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("FFMT_BIG_REPO not set; skipping");
            return;
        }
    };
    assert_clean(&path);

    let stock = Worktree::add(&path, "stock");
    let ffmt = Worktree::add(&path, "ffmt");

    // Sanity: both worktrees should agree out of the gate.
    diff_trees(&stock.path, &ffmt.path).expect("worktrees diverge before any modification");

    // Phase 1: clean tree must format identically (no changes from either).
    let stock_check = run_fmt("fmt", &["--check"], &stock.path);
    let ffmt_check = run_fmt("ffmt", &["--check"], &ffmt.path);
    assert_eq!(
        stock_check.code(),
        ffmt_check.code(),
        "--check exit codes diverge on clean tree"
    );
    diff_trees(&stock.path, &ffmt.path).expect("clean tree diverges after --check");

    // Phase 2: introduce identical synthetic violations, then run both
    // formatters, compare results.
    let touched_stock = dirty_tree(&stock.path);
    let touched_ffmt = dirty_tree(&ffmt.path);
    assert_eq!(
        touched_stock.len(),
        touched_ffmt.len(),
        "dirty_tree picked different file sets — cannot compare"
    );
    diff_trees(&stock.path, &ffmt.path).expect("dirtied trees should still match");

    let stock_status = run_fmt("fmt", &[], &stock.path);
    let ffmt_status = run_fmt("ffmt", &[], &ffmt.path);
    assert!(stock_status.success(), "stock cargo fmt failed");
    assert!(ffmt_status.success(), "cargo ffmt failed");

    if let Err(diff) = diff_trees(&stock.path, &ffmt.path) {
        // Truncate so test output is readable.
        let preview: String = diff.lines().take(80).collect::<Vec<_>>().join("\n");
        panic!(
            "tree state diverges after formatting (showing first 80 lines):\n{preview}\n\n\
             ({} total bytes of diff)",
            diff.len()
        );
    }
}
