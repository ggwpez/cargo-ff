//! Experimental skip cache. Records `manifest_dir → {*.rs file → mtime_ns}`
//! across runs; on subsequent runs, crates whose fingerprint is unchanged
//! are not dispatched to rustfmt at all. Works for both `--check` and
//! write mode — in write mode, the run that actually rewrites a file
//! invalidates that crate (mtimes shift), so the next run re-dispatches
//! once and then re-caches.
//!
//! Soundness rests on the assumption that every file rustfmt would
//! format for a crate lives under that crate's `manifest_dir` (excluding
//! `target/`). This holds for typical workspaces but breaks for
//! cross-crate `#[path = "../other/foo.rs"]` constructs (the polkadot
//! `malus` case). Caller opts in via `--experimental-cache`.
//!
//! Salting: rustfmt --version output and workspace-root rustfmt config
//! files are hashed into a header; any mismatch on load → cache empty.

use crate::size;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

const VERSION_TAG: &str = "ff-cache-v3";

/// Sorted file → mtime (ns since epoch). `BTreeMap` so equality is structural
/// and serialization is deterministic.
pub(crate) type Fingerprint = BTreeMap<PathBuf, u128>;

pub(crate) struct Cache {
    path: Option<PathBuf>,
    tool_hash: u64,
    retained: HashMap<PathBuf, Fingerprint>,
    pending: HashMap<PathBuf, Fingerprint>,
}

impl Cache {
    pub fn load(workspace_root: &Path) -> Self {
        let path = cache_path();
        let tool_hash = compute_tool_hash(workspace_root);
        let retained = path
            .as_deref()
            .and_then(|p| read_entries(p, tool_hash).ok())
            .unwrap_or_default();
        Self {
            path,
            tool_hash,
            retained,
            pending: HashMap::new(),
        }
    }

    pub fn matches(&self, manifest_dir: &Path, current: &Fingerprint) -> bool {
        self.retained
            .get(manifest_dir)
            .is_some_and(|fp| fp == current)
    }

    pub fn stage(&mut self, manifest_dir: PathBuf, fingerprint: Fingerprint) {
        self.pending.insert(manifest_dir, fingerprint);
    }

    pub fn invalidate(&mut self, manifest_dir: &Path) {
        self.retained.remove(manifest_dir);
        self.pending.remove(manifest_dir);
    }

    pub fn commit_and_save(mut self) -> io::Result<()> {
        for (k, v) in self.pending.drain() {
            self.retained.insert(k, v);
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tsv.tmp");
        {
            let f = fs::File::create(&tmp)?;
            let mut w = BufWriter::new(f);
            writeln!(w, "{}\t{:016x}", VERSION_TAG, self.tool_hash)?;
            for (dir, files) in &self.retained {
                let ds = dir.to_string_lossy();
                if ds.contains('\t') || ds.contains('\n') {
                    continue;
                }
                for (file, mtime) in files {
                    let fs_ = file.to_string_lossy();
                    if fs_.contains('\t') || fs_.contains('\n') {
                        continue;
                    }
                    writeln!(w, "{ds}\t{fs_}\t{mtime}")?;
                }
            }
        }
        fs::rename(tmp, path)
    }
}

/// A crate's freshly-walked state: the per-file mtime [`Fingerprint`] plus the
/// `*.rs` byte total used as the work-size proxy.
pub(crate) struct Build {
    pub fingerprint: Fingerprint,
    pub size_bytes: u64,
}

/// Walk all `*.rs` files under `manifest_dir` (excluding `target/`), returning
/// the per-file mtime fingerprint plus the byte total clamped to
/// [`size::HUGE_CUTOFF_BYTES`]. Reading the cutoff here (rather than taking it
/// as a parameter) keeps the cached and uncached size proxies — `cache::build`
/// and `size::estimate` — clamped to the *same* value, which is what makes the
/// coalescer's `>= solo_threshold` comparison exact for both paths.
pub(crate) fn build(manifest_dir: &Path) -> Build {
    let mut fingerprint = BTreeMap::new();
    let mut total: u64 = 0;
    let walker = walkdir::WalkDir::new(manifest_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target");
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        total = total.saturating_add(meta.len());
        if let Ok(mt) = meta.modified()
            && let Ok(d) = mt.duration_since(UNIX_EPOCH)
        {
            fingerprint.insert(entry.path().to_path_buf(), d.as_nanos());
        }
    }
    Build {
        fingerprint,
        size_bytes: total.min(size::HUGE_CUTOFF_BYTES),
    }
}

fn read_entries(path: &Path, expected: u64) -> io::Result<HashMap<PathBuf, Fingerprint>> {
    let f = fs::File::open(path)?;
    let mut lines = BufReader::new(f).lines();
    let header = lines.next().transpose()?.unwrap_or_default();
    let mut hp = header.split('\t');
    let (Some(tag), Some(hash)) = (hp.next(), hp.next()) else {
        return Ok(HashMap::new());
    };
    if tag != VERSION_TAG {
        return Ok(HashMap::new());
    }
    let Ok(stored) = u64::from_str_radix(hash, 16) else {
        return Ok(HashMap::new());
    };
    if stored != expected {
        return Ok(HashMap::new());
    }

    let mut out: HashMap<PathBuf, Fingerprint> = HashMap::new();
    for line in lines {
        let line = line?;
        let mut parts = line.splitn(3, '\t');
        let (Some(dir), Some(file), Some(mtime)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(mtime) = mtime.parse() else { continue };
        out.entry(PathBuf::from(dir))
            .or_default()
            .insert(PathBuf::from(file), mtime);
    }
    Ok(out)
}

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".cache");
    p.push("cargo-ff");
    p.push("fingerprints.tsv");
    Some(p)
}

/// FNV-1a 64-bit. Deterministic across runs (unlike `DefaultHasher`).
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn compute_tool_hash(workspace_root: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    if let Ok(out) = Command::new("rustfmt").arg("--version").output() {
        h = fnv1a(h, &out.stdout);
    }
    for name in [".rustfmt.toml", "rustfmt.toml"] {
        if let Ok(bytes) = fs::read(workspace_root.join(name)) {
            h = fnv1a(h, b"\x00");
            h = fnv1a(h, name.as_bytes());
            h = fnv1a(h, b"\x00");
            h = fnv1a(h, &bytes);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; `unwrap` is the idiomatic way to fail loudly.
    #![allow(clippy::unwrap_used)]

    use super::{Build, Cache, Fingerprint, build, read_entries};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn unique_tmp(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()))
    }

    #[test]
    fn build_fingerprints_only_rs_files_and_sums_their_bytes() {
        let dir = unique_tmp("ff-cache-build");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn a() {}\n").unwrap(); // 10 bytes
        std::fs::write(dir.join("b.rs"), b"fn b() {}\n").unwrap(); // 10 bytes
        std::fs::write(dir.join("README.md"), b"ignored").unwrap();

        let Build {
            fingerprint,
            size_bytes,
        } = build(&dir);
        assert_eq!(fingerprint.len(), 2, "only the two .rs files are fingerprinted");
        assert_eq!(size_bytes, 20, "size proxy sums the .rs bytes, not the .md file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips_and_salt_guards_against_tool_changes() {
        let dir = unique_tmp("ff-cache-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tsv = dir.join("fingerprints.tsv");

        let manifest = PathBuf::from("/ws/crate-a");
        let mut fingerprint = Fingerprint::new();
        fingerprint.insert(PathBuf::from("/ws/crate-a/src/lib.rs"), 42u128);

        let mut cache = Cache {
            path: Some(tsv.clone()),
            tool_hash: 0xdead_beef,
            retained: HashMap::new(),
            pending: HashMap::new(),
        };
        cache.stage(manifest.clone(), fingerprint.clone());
        cache.commit_and_save().unwrap();

        // Same tool hash → the staged entry comes back byte-identical.
        let loaded = read_entries(&tsv, 0xdead_beef).unwrap();
        assert_eq!(loaded.get(&manifest), Some(&fingerprint));

        // Different tool hash (e.g. rustfmt upgraded) → salt mismatch wipes it.
        let stale = read_entries(&tsv, 0x0bad_f00d).unwrap();
        assert!(stale.is_empty(), "a tool-hash mismatch must invalidate the cache");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
