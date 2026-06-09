use crate::cache;
use crate::size;
use crate::types::{Config, CrateUnit, Edition, Error, Result, UnknownEdition};
use cargo_metadata::MetadataCommand;
use crossbeam_channel::Sender;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

mod warnings;

/// Run the producer.
///
/// For each workspace member we emit one [`CrateUnit`] containing the
/// crate's *target entry points* (one path per target — `src/lib.rs`,
/// `src/bin/foo.rs`, `tests/it.rs`, …). rustfmt itself walks the `mod`
/// tree from each entry point. This matches what `cargo fmt` does,
/// including handling of `#[path = "…"]` attributes and skipping of
/// files that aren't declared as `mod` (e.g. trybuild ui fixtures).
///
/// The producer thread owns the `Sender`; dropping it when that closure exits
/// signals end-of-stream to the coalescer once discovery finishes.
pub(crate) fn run(cfg: &Config, tx: &Sender<CrateUnit>) -> Result<Option<cache::Cache>> {
    let mut cmd = MetadataCommand::new();
    cmd.no_deps();
    if let Some(p) = &cfg.manifest_path {
        cmd.manifest_path(p);
    }
    let metadata = cmd.exec()?;
    let ws_root = metadata.workspace_root.as_std_path().to_path_buf();
    let ws_roots = WsRoots::new(&ws_root);

    let mut cache_opt = cfg
        .experimental_cache
        .then(|| cache::Cache::load(metadata.workspace_root.as_std_path()));

    let workspace_members: HashSet<&cargo_metadata::PackageId> =
        metadata.workspace_members.iter().collect();

    let at_root = at_workspace_root(cfg, metadata.workspace_root.as_std_path());
    let selected = select_packages(cfg, &metadata, &workspace_members, at_root)?;

    // Cross-crate dedup. Some workspaces have targets whose `src_path`
    // contains `..` segments reaching into another crate's tree (e.g.
    // polkadot's `malus`). After canonicalization those files would be
    // claimed by multiple crates, possibly with different editions.
    // First crate to claim a file wins. We also track the owner's
    // manifest path + edition so the collision warning can render both
    // claim sites with line numbers (rustc-style multi-span).
    let mut claimed: HashMap<PathBuf, ClaimSite> = HashMap::new();
    // Editions seen across selected crates. If more than one shows up,
    // we emit a single multi-edition warning at the end (off by default
    // — it's only an advisory). Common in long-lived workspaces with
    // crates pinned to old editions.
    let mut editions_seen: HashMap<Edition, String> = HashMap::new();
    // Per-crate rustfmt config governance: nearest rustfmt.toml (walking
    // up to the workspace root) for each selected crate. Lets us flag
    // crate-level configs that shadow the workspace one.
    let mut governed: HashMap<PathBuf, Vec<(String, Edition)>> = HashMap::new();
    // Crates whose Cargo.toml has no `edition` key — cargo defaults them
    // to 2015, which is almost always unintended in a modern workspace.
    let mut implicit_2015: Vec<String> = Vec::new();

    for pkg in &metadata.packages {
        if !selected.contains(&pkg.id) {
            continue;
        }

        let edition: Edition =
            pkg.edition
                .try_into()
                .map_err(|UnknownEdition(year)| Error::UnsupportedEdition {
                    edition: year,
                    package: pkg.name.to_string(),
                })?;
        editions_seen
            .entry(edition)
            .or_insert_with(|| pkg.name.to_string());
        let manifest_dir: PathBuf = pkg
            .manifest_path
            .parent()
            .map(|p| p.as_std_path().to_path_buf())
            .ok_or_else(|| {
                Error::Io(std::io::Error::other(format!(
                    "manifest_path has no parent: {}",
                    pkg.manifest_path
                )))
            })?;
        if cfg.warnings
            && let Some(cfg_file) = find_nearest_config(&manifest_dir, &ws_root)
        {
            governed
                .entry(cfg_file)
                .or_default()
                .push((pkg.name.to_string(), edition));
        }
        if cfg.warnings
            && edition == Edition::E2015
            && !cargo_toml_declares_edition(pkg.manifest_path.as_std_path())
        {
            implicit_2015.push(pkg.name.to_string());
        }
        let entry_points =
            collect_entry_points(pkg, edition, &mut claimed, cfg.warnings, &ws_roots);
        if entry_points.is_empty() {
            continue;
        }

        let size_bytes = if let Some(c) = cache_opt.as_mut() {
            let built = cache::build(&manifest_dir);
            if c.matches(&manifest_dir, &built.fingerprint) {
                // Cached fingerprint matches — skip dispatch entirely.
                continue;
            }
            c.stage(manifest_dir.clone(), built.fingerprint);
            built.size_bytes
        } else {
            size::estimate(&manifest_dir)
        };
        let unit = CrateUnit {
            edition,
            manifest_dir,
            files: entry_points,
            size_bytes,
        };
        if tx.send(unit).is_err() {
            return Err(Error::SendClosed);
        }
    }

    if cfg.warnings && editions_seen.len() > 1 {
        warnings::emit_multi_edition_warning(&editions_seen);
    }
    if cfg.warnings {
        warnings::emit_shadow_config_warning(&ws_roots, &governed);
        warnings::emit_config_edition_warning(&ws_roots, &governed);
        warnings::emit_implicit_edition_warning(&implicit_2015);
    }

    Ok(cache_opt)
}

/// Canonicalize each of `pkg`'s target entry-point paths and claim them. The
/// first crate to claim a file keeps it; a later claimant only triggers a
/// collision warning (when `warnings` is set). Returns the files this crate
/// owns — those not already claimed by an earlier crate.
fn collect_entry_points(
    pkg: &cargo_metadata::Package,
    edition: Edition,
    claimed: &mut HashMap<PathBuf, ClaimSite>,
    emit_warnings: bool,
    roots: &WsRoots,
) -> Vec<PathBuf> {
    let mut entry_points: Vec<PathBuf> = Vec::new();
    for tgt in &pkg.targets {
        let raw = tgt.src_path.as_std_path().to_path_buf();
        let canon = raw.canonicalize().unwrap_or(raw);
        let claim_site = ClaimSite {
            name: pkg.name.to_string(),
            edition,
            manifest_path: pkg.manifest_path.as_std_path().to_path_buf(),
        };
        match claimed.entry(canon.clone()) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(claim_site);
                entry_points.push(canon);
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                if emit_warnings {
                    warnings::emit_claim_collision_warning(roots, &canon, o.get(), &claim_site);
                }
            }
        }
    }
    entry_points
}

/// Resolve which workspace packages to format. Precedence (matches `cargo fmt`):
///   1. `--all` → format every workspace member. `-p` is ignored
///      (even unknown values), matching `cargo fmt --all -p foo`.
///   2. `-p PKG` (`cfg.packages`) → format exactly those; unknown
///      names error.
///   3. running at the workspace root → every workspace member.
///      `cargo fmt`'s quirk: when the effective manifest is the
///      workspace root's manifest, it ignores both `default-members`
///      and a root `[package]` and formats every member. Reproducing
///      this is necessary for byte-equivalence on workspaces like
///      reth (`default-members = ["bin/reth"]`) and bevy (root
///      package `bevy` plus 87 sub-crates).
///   4. otherwise → format the package implicitly selected by
///      `--manifest-path` (or cwd). For a virtual workspace with no
///      implicit package, fall back to `workspace.default-members`.
fn select_packages<'a>(
    cfg: &Config,
    metadata: &'a cargo_metadata::Metadata,
    workspace_members: &HashSet<&'a cargo_metadata::PackageId>,
    at_root: bool,
) -> Result<HashSet<&'a cargo_metadata::PackageId>> {
    Ok(if cfg.all {
        workspace_members.clone()
    } else if !cfg.packages.is_empty() {
        let member_names: HashSet<&str> = metadata
            .packages
            .iter()
            .filter(|p| workspace_members.contains(&p.id))
            .map(|p| p.name.as_str())
            .collect();
        let unknown: Vec<String> = cfg
            .packages
            .iter()
            .filter(|n| !member_names.contains(n.as_str()))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(Error::UnknownPackages(unknown));
        }
        let names: HashSet<&str> = cfg.packages.iter().map(String::as_str).collect();
        metadata
            .packages
            .iter()
            .filter(|p| workspace_members.contains(&p.id) && names.contains(p.name.as_str()))
            .map(|p| &p.id)
            .collect()
    } else if at_root {
        workspace_members.clone()
    } else if let Some(root) = metadata.root_package() {
        std::iter::once(&root.id).collect()
    } else {
        metadata
            .workspace_default_packages()
            .into_iter()
            .map(|p| &p.id)
            .collect()
    })
}

/// True when the effective manifest path (`--manifest-path` if given,
/// else the nearest `Cargo.toml` walking up from cwd) equals the
/// workspace's root `Cargo.toml`. Mirrors `cargo-fmt`'s `in_workspace_root`
/// flag, which it uses to expand the implicit selection to every member.
fn at_workspace_root(cfg: &Config, ws_root: &Path) -> bool {
    let Ok(ws_manifest) = ws_root.join("Cargo.toml").canonicalize() else {
        return false;
    };
    let effective = match &cfg.manifest_path {
        Some(p) => p.canonicalize().ok(),
        None => std::env::current_dir()
            .ok()
            .and_then(|cwd| find_manifest_upward(&cwd)),
    };
    effective.is_some_and(|m| m == ws_manifest)
}

fn find_manifest_upward(start: &Path) -> Option<PathBuf> {
    let mut p = start.canonicalize().ok()?;
    loop {
        let cand = p.join("Cargo.toml");
        if cand.is_file() {
            return cand.canonicalize().ok();
        }
        if !p.pop() {
            return None;
        }
    }
}

/// Walk up from `start` (inclusive) to `root` (inclusive) looking for a
/// rustfmt config. rustfmt resolves config per file by walking up from
/// the file's directory and uses the nearest one it finds. Lookup order
/// (`.rustfmt.toml` then `rustfmt.toml`) matches `cache::compute_tool_hash`.
fn find_nearest_config(start: &Path, root: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        for name in [".rustfmt.toml", "rustfmt.toml"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// True if `manifest_path` declares an `edition` key — an explicit value
/// or `edition.workspace = true`. When absent, cargo defaults to 2015.
/// On read failure we assume it's declared, to avoid a false warning.
/// Best-effort line scan, like the other Cargo.toml helpers here.
fn cargo_toml_declares_edition(manifest_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(manifest_path) else {
        return true;
    };
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("edition") else {
            return false;
        };
        let rest = rest.trim_start();
        rest.starts_with('=') || rest.starts_with('.')
    })
}

/// Where a crate declared a target — captured so the collision warning
/// can show both claim sites with line numbers in `Cargo.toml`.
struct ClaimSite {
    name: String,
    edition: Edition,
    manifest_path: PathBuf,
}

/// Workspace root in both raw and canonicalized form. Used by the warning
/// emitters to display paths relative to the workspace root instead of as long
/// absolutes. On macOS the two often differ (`/tmp` vs `/private/tmp`); cargo
/// hands us raw paths but we canonicalize the file targets ourselves, so we
/// strip against either.
///
/// Built once per `run` and threaded into the emitters — deliberately *not* a
/// process global, so repeated in-process `run` calls each see their own root.
pub(super) struct WsRoots {
    raw: PathBuf,
    canon: PathBuf,
}

impl WsRoots {
    fn new(raw: &Path) -> Self {
        Self {
            raw: raw.to_path_buf(),
            canon: raw.canonicalize().unwrap_or_else(|_| raw.to_path_buf()),
        }
    }

    /// The raw (non-canonicalized) workspace root, as cargo reported it.
    pub(super) fn raw(&self) -> &Path {
        &self.raw
    }

    /// Strip the workspace-root prefix from `p` if possible, falling back to
    /// the absolute path when neither the raw nor canonical root matches.
    pub(super) fn rel<'p>(&self, p: &'p Path) -> &'p Path {
        p.strip_prefix(&self.raw)
            .or_else(|_| p.strip_prefix(&self.canon))
            .unwrap_or(p)
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; `unwrap` is the idiomatic way to fail loudly.
    #![allow(clippy::unwrap_used)]

    use super::{WsRoots, cargo_toml_declares_edition, find_nearest_config};
    use std::path::PathBuf;

    fn unique_tmp(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()))
    }

    #[test]
    fn cargo_toml_declares_edition_detects_explicit_and_inherited_keys() {
        let dir = unique_tmp("ff-disc-edition");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let explicit = dir.join("explicit.toml");
        std::fs::write(&explicit, "[package]\nname = \"x\"\nedition = \"2021\"\n").unwrap();
        assert!(cargo_toml_declares_edition(&explicit));

        let inherited = dir.join("inherited.toml");
        std::fs::write(&inherited, "[package]\nedition.workspace = true\n").unwrap();
        assert!(cargo_toml_declares_edition(&inherited));

        let absent = dir.join("absent.toml");
        std::fs::write(&absent, "[package]\nname = \"x\"\n").unwrap();
        assert!(!cargo_toml_declares_edition(&absent));

        // An unreadable manifest is assumed to declare an edition, so we never
        // emit a false "implicit 2015" warning on a read error.
        assert!(cargo_toml_declares_edition(&dir.join("missing.toml")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_nearest_config_walks_up_and_prefers_the_closest() {
        let root = unique_tmp("ff-disc-config");
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("crates").join("a");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("rustfmt.toml"), "edition = \"2021\"\n").unwrap();

        // From a nested crate, the walk reaches the workspace-root config.
        assert_eq!(
            find_nearest_config(&nested, &root),
            Some(root.join("rustfmt.toml"))
        );

        // A crate-local config shadows the root one (closest wins).
        std::fs::write(nested.join(".rustfmt.toml"), "").unwrap();
        assert_eq!(
            find_nearest_config(&nested, &root),
            Some(nested.join(".rustfmt.toml"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ws_roots_rel_strips_the_workspace_prefix() {
        let roots = WsRoots::new(&PathBuf::from("/ws"));
        assert_eq!(
            roots.rel(&PathBuf::from("/ws/crates/a/Cargo.toml")),
            PathBuf::from("crates/a/Cargo.toml")
        );
        // A path outside the workspace is returned unchanged.
        assert_eq!(
            roots.rel(&PathBuf::from("/elsewhere/x")),
            PathBuf::from("/elsewhere/x")
        );
    }
}
