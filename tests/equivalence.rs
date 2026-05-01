//! Equivalence harness — proves `cargo ff` produces byte-identical
//! tree state to `cargo +nightly fmt` after running on the same input.
//!
//! Strategy:
//! 1. Take a clean git repo (`FF_BIG_REPO` env var, or in-tree fixture).
//! 2. Create two cheap copies via `git worktree add`.
//! 3. Apply identical synthetic formatting violations to both.
//! 4. Run stock `cargo +nightly fmt` in worktree A.
//! 5. Run `cargo +nightly ff` in worktree B.
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
        let tmp = std::env::temp_dir().join(format!("ff-equiv-{name}-{}", std::process::id()));
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
    let bad = "\npub fn __ff_equiv_test_helper(  x:i32,y :i32 )->i32{x+y}\n";

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

/// `toolchain` is the rustup channel string (`"nightly"`, `"stable"`,
/// `"1.92"`, …) or `None` to use whatever rustup picks by default.
fn run_fmt(
    toolchain: Option<&str>,
    cmd: &str,
    args: &[&str],
    repo: &Path,
) -> std::process::ExitStatus {
    let mut c = Command::new("cargo");
    if let Some(t) = toolchain {
        c.arg(format!("+{t}"));
    }
    c.arg(cmd)
        .args(args)
        .current_dir(repo)
        .status()
        .expect("cargo invocation failed")
}

/// One scenario in the equivalence matrix: stock and ff are invoked
/// with different arg lists, but expected to produce byte-identical
/// tree state. Used to prove that knobs like `--ff-batch-size` are
/// purely internal — they don't leak into rustfmt's output.
struct Scenario {
    label: &'static str,
    stock_args: &'static [&'static str],
    ff_args: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    // Default invocation: workspace-root selection, default batch size.
    Scenario {
        label: "default",
        stock_args: &[],
        ff_args: &[],
    },
    // Explicit --all: should match default at workspace root.
    Scenario {
        label: "all",
        stock_args: &["--all"],
        ff_args: &["--all"],
    },
    // Per-crate dispatch (degenerate batching). Output must be invariant.
    Scenario {
        label: "bs1",
        stock_args: &[],
        ff_args: &["--ff-batch-size", "1"],
    },
    // Large batch (many crates per rustfmt). Stresses cross-crate batching.
    Scenario {
        label: "bs24",
        stock_args: &[],
        ff_args: &["--ff-batch-size", "24"],
    },
];

fn run_scenario(repo: &Path, toolchain: Option<&str>, tag: &str, s: &Scenario) {
    let stock = Worktree::add(repo, &format!("stock-{tag}-{}", s.label));
    let ff = Worktree::add(repo, &format!("ff-{tag}-{}", s.label));
    diff_trees(&stock.path, &ff.path).expect("worktrees diverge before any modification");

    let touched_stock = dirty_tree(&stock.path);
    let touched_ff = dirty_tree(&ff.path);
    assert_eq!(
        touched_stock.len(),
        touched_ff.len(),
        "[{tag}/{}] dirty_tree picked different file sets",
        s.label
    );
    diff_trees(&stock.path, &ff.path)
        .unwrap_or_else(|_| panic!("[{tag}/{}] dirtied trees should match", s.label));

    let stock_status = run_fmt(toolchain, "fmt", s.stock_args, &stock.path);
    let ff_status = run_fmt(toolchain, "ff", s.ff_args, &ff.path);
    assert!(
        stock_status.success(),
        "[{tag}/{}] stock cargo fmt failed",
        s.label
    );
    assert!(ff_status.success(), "[{tag}/{}] cargo ff failed", s.label);

    if let Err(diff) = diff_trees(&stock.path, &ff.path) {
        let preview: String = diff.lines().take(80).collect::<Vec<_>>().join("\n");
        panic!(
            "[{tag}/{}] tree state diverges after formatting (first 80 lines):\n{preview}\n\n\
             ({} total bytes of diff)",
            s.label,
            diff.len()
        );
    }
}

/// Full thorough equivalence matrix on a single (repo, toolchain) pair:
/// clean-tree --check, then every [`SCENARIOS`] variant with synthetic
/// dirtying. Each scenario gets its own worktree pair so failures are
/// localized.
fn run_equivalence(repo: &Path, toolchain: Option<&str>, tag: &str) {
    // ── --check path ──
    //   1. stock and ff exit with the same code on a clean tree.
    //   2. neither writes anything: every fingerprint field
    //      (content + mtime + size + mode + ino) is preserved across
    //      the --check call. --check is supposed to be read-only.
    let stock = Worktree::add(repo, &format!("stock-{tag}-check"));
    let ff = Worktree::add(repo, &format!("ff-{tag}-check"));
    diff_trees(&stock.path, &ff.path).expect("worktrees diverge before any modification");

    let stock_before = snapshot_fingerprints(&stock.path);
    let ff_before = snapshot_fingerprints(&ff.path);
    let stock_check = run_fmt(toolchain, "fmt", &["--check"], &stock.path);
    let ff_check = run_fmt(toolchain, "ff", &["--check"], &ff.path);
    assert_eq!(
        stock_check.code(),
        ff_check.code(),
        "[{tag}/check] exit codes diverge"
    );
    let stock_after = snapshot_fingerprints(&stock.path);
    let ff_after = snapshot_fingerprints(&ff.path);
    assert_fingerprints_unchanged(&stock_before, &stock_after, tag, "fmt --check");
    assert_fingerprints_unchanged(&ff_before, &ff_after, tag, "ff --check");
    drop(stock);
    drop(ff);

    // ── fix path on a clean tree ──
    // Both must succeed. Per-worktree invariant: any file whose content
    // wasn't changed must have all metadata preserved too (rustfmt's
    // "only write on diff" contract). Cross-worktree mtime/ino equality
    // is impossible — files are created at different wall-clock times.
    assert_no_spurious_rewrites(repo, toolchain, tag, "fmt");
    assert_no_spurious_rewrites(repo, toolchain, tag, "ff");

    // ── fix path under deliberate dirtying ──
    // Each scenario dirties identically in a fresh stock+ff worktree
    // pair, runs both formatters, and asserts byte-identical tree
    // content cross-worktree.
    for s in SCENARIOS {
        run_scenario(repo, toolchain, tag, s);
    }
}

/// Strict equality of every fingerprint field (incl. mtime/ino) for
/// every file. Used after `--check` to prove nothing was touched.
fn assert_fingerprints_unchanged(
    before: &std::collections::BTreeMap<PathBuf, FileFingerprint>,
    after: &std::collections::BTreeMap<PathBuf, FileFingerprint>,
    tag: &str,
    op: &str,
) {
    let mut diffs: Vec<&PathBuf> = before
        .iter()
        .filter(|(p, fp)| after.get(*p) != Some(fp))
        .map(|(p, _)| p)
        .collect();
    diffs.sort();
    assert!(
        diffs.is_empty(),
        "[{tag}/{op}] {} *.rs file(s) drifted. {op} must not touch the filesystem (first 5: {:?}).",
        diffs.len(),
        diffs.iter().take(5).collect::<Vec<_>>(),
    );
    assert_eq!(
        before.len(),
        after.len(),
        "[{tag}/{op}] file count changed during {op}",
    );
}

/// Per-file fingerprint snapshotted before/after a clean-tree run to
/// prove cargo-ff (and stock cargo-fmt) leave already-formatted files
/// completely untouched at the filesystem level — not just byte-equal.
#[derive(PartialEq, Eq, Clone)]
struct FileFingerprint {
    /// 64-bit hash of file bytes. Cheap and effectively collision-free
    /// at the per-file scale we deal with here. Catches content
    /// rewrites even when length is preserved.
    content: u64,
    mtime: std::time::SystemTime,
    size: u64,
    /// Unix mode bits. A spurious chmod would show up here.
    mode: u32,
    /// Inode. Changes if rustfmt did a write-temp-then-rename swap,
    /// even if content + size + mode all match.
    ino: u64,
}

fn fingerprint(path: &Path) -> Option<FileFingerprint> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::MetadataExt;
    let bytes = std::fs::read(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let m = std::fs::metadata(path).ok()?;
    Some(FileFingerprint {
        content: h.finish(),
        mtime: m.modified().ok()?,
        size: m.len(),
        mode: m.mode(),
        ino: m.ino(),
    })
}

/// Invariant: if a file's content didn't change, neither did any of
/// its filesystem metadata (mtime, size, mode, ino). rustfmt's contract
/// is to only write a file when content actually differs; this test
/// verifies cargo-ff doesn't break that contract by triggering spurious
/// rewrites. Looser than "no drift at all" so it stays valid even when
/// the upstream tree isn't fmt-clean against the local toolchain (the
/// content-changed files are expected to drift and ignored).
fn assert_no_spurious_rewrites(repo: &Path, toolchain: Option<&str>, tag: &str, cmd: &str) {
    let wt = Worktree::add(repo, &format!("{cmd}-{tag}-fp"));
    let before = snapshot_fingerprints(&wt.path);
    let status = run_fmt(toolchain, cmd, &[], &wt.path);
    assert!(
        status.success(),
        "[{tag}/fp/{cmd}] cargo {cmd} failed on clean tree"
    );
    let after = snapshot_fingerprints(&wt.path);

    let mut spurious: Vec<&PathBuf> = Vec::new();
    for (p, before_fp) in &before {
        let Some(after_fp) = after.get(p) else {
            continue; // deleted file — not our concern here
        };
        // Content unchanged but other metadata changed → rustfmt
        // touched the file unnecessarily.
        if after_fp.content == before_fp.content && after_fp != before_fp {
            spurious.push(p);
        }
    }
    spurious.sort();
    assert!(
        spurious.is_empty(),
        "[{tag}/fp/{cmd}] {} *.rs file(s) had metadata changed despite identical content \
         (first 5: {:?}). cargo {cmd} touched files that didn't need rewriting.",
        spurious.len(),
        spurious.iter().take(5).collect::<Vec<_>>(),
    );
}

fn snapshot_fingerprints(repo: &Path) -> std::collections::BTreeMap<PathBuf, FileFingerprint> {
    let mut out = std::collections::BTreeMap::new();
    let walk = Command::new("find")
        .args([
            ".", "-path", "./target", "-prune", "-o", "-name", "*.rs", "-print",
        ])
        .current_dir(repo)
        .output()
        .expect("find failed");
    for rel in String::from_utf8_lossy(&walk.stdout).lines() {
        if !rel.ends_with(".rs") {
            continue;
        }
        let p = repo.join(rel.trim_start_matches("./"));
        if let Some(fp) = fingerprint(&p) {
            out.insert(p, fp);
        }
    }
    out
}

#[test]
#[ignore]
fn equivalence_on_big_repo() {
    let path = match std::env::var("FF_BIG_REPO") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("FF_BIG_REPO not set; skipping");
            return;
        }
    };
    assert_clean(&path);
    let toolchain = std::env::var("FF_TOOLCHAIN").ok();
    let tag = toolchain.as_deref().unwrap_or("default");
    run_equivalence(&path, toolchain.as_deref(), tag);
}

/// Equivalence on cargo-ffmt itself, with whatever rustup picks by
/// default (typically stable). Catches divergences that only show up
/// on stable rustfmt — silently-dropped unstable options, edition
/// handling, etc.
#[test]
#[ignore]
fn equivalence_on_self_default() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    run_equivalence(&path, None, "default");
}

#[test]
#[ignore]
fn equivalence_on_self_nightly() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    run_equivalence(&path, Some("nightly"), "nightly");
}
