use crate::cache;
use crate::size;
use crate::types::{Config, CrateUnit, Edition, Error, Result, UnknownEdition};
use cargo_metadata::MetadataCommand;
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::path::PathBuf;

/// Run the producer.
///
/// For each workspace member we emit one [`CrateUnit`] containing the
/// crate's *target entry points* (one path per target — `src/lib.rs`,
/// `src/bin/foo.rs`, `tests/it.rs`, …). rustfmt itself walks the `mod`
/// tree from each entry point. This matches what `cargo fmt` does,
/// including handling of `#[path = "…"]` attributes and skipping of
/// files that aren't declared as `mod` (e.g. trybuild ui fixtures).
pub(crate) fn run(cfg: &Config, tx: Sender<CrateUnit>) -> Result<Option<cache::Cache>> {
    let mut cmd = MetadataCommand::new();
    cmd.no_deps();
    if let Some(p) = &cfg.manifest_path {
        cmd.manifest_path(p);
    }
    let metadata = cmd.exec()?;

    let mut cache_opt = cfg
        .experimental_cache
        .then(|| cache::Cache::load(metadata.workspace_root.as_std_path()));

    let workspace_members: HashSet<&cargo_metadata::PackageId> =
        metadata.workspace_members.iter().collect();

    // Resolve which packages to format. Precedence (matches `cargo fmt`):
    //   1. `--all` → format every workspace member. `-p` is ignored
    //      (even unknown values), matching `cargo fmt --all -p foo`.
    //   2. `-p PKG` (cfg.packages) → format exactly those; unknown
    //      names error.
    //   3. otherwise → format the package implicitly selected by
    //      `--manifest-path` (or cwd). For a virtual workspace with no
    //      implicit package, fall back to `workspace.default-members`.
    let selected: HashSet<&cargo_metadata::PackageId> = if cfg.all {
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
    } else if let Some(root) = metadata.root_package() {
        std::iter::once(&root.id).collect()
    } else {
        metadata
            .workspace_default_packages()
            .into_iter()
            .map(|p| &p.id)
            .collect()
    };

    // Cross-crate dedup. Some workspaces have targets whose `src_path`
    // contains `..` segments reaching into another crate's tree (e.g.
    // polkadot's `malus`). After canonicalization those files would be
    // claimed by multiple crates, possibly with different editions.
    // First crate to claim a file wins.
    let mut claimed: HashSet<PathBuf> = HashSet::new();

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

        let mut entry_points: Vec<PathBuf> = Vec::new();
        for tgt in &pkg.targets {
            let raw = tgt.src_path.as_std_path().to_path_buf();
            let canon = raw.canonicalize().unwrap_or(raw);
            if claimed.insert(canon.clone()) {
                entry_points.push(canon);
            }
        }

        if entry_points.is_empty() {
            continue;
        }

        let size_bytes = if let Some(c) = cache_opt.as_mut() {
            let (fp, bytes) = cache::build(&manifest_dir, size::HUGE_CUTOFF_BYTES);
            if c.matches(&manifest_dir, &fp) {
                // Cached fingerprint matches — skip dispatch entirely.
                continue;
            }
            c.stage(manifest_dir.clone(), fp);
            bytes
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

    Ok(cache_opt)
}
