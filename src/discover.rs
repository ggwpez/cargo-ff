use crate::types::{Config, CrateUnit, Edition, Error, Result};
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
pub fn run(cfg: &Config, tx: Sender<CrateUnit>) -> Result<()> {
    let mut cmd = MetadataCommand::new();
    cmd.no_deps();
    if let Some(p) = &cfg.manifest_path {
        cmd.manifest_path(p);
    }
    let metadata = cmd.exec()?;

    let workspace_members: HashSet<&cargo_metadata::PackageId> =
        metadata.workspace_members.iter().collect();

    // Resolve which packages to format. Precedence (matches `cargo fmt`):
    //   1. `-p PKG` (cfg.packages) → format exactly those.
    //   2. `--all` → format every workspace member.
    //   3. otherwise → format the package implicitly selected by
    //      `--manifest-path` (or cwd). For a virtual workspace with no
    //      implicit package, fall back to `workspace.default-members`.
    let selected: HashSet<&cargo_metadata::PackageId> = if !cfg.packages.is_empty() {
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
    } else if cfg.all {
        workspace_members.clone()
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

        let edition = map_edition(pkg.edition);
        let manifest_dir: PathBuf = pkg
            .manifest_path
            .parent()
            .map(|p| p.to_path_buf().into())
            .ok_or_else(|| {
                Error::Io(std::io::Error::other(format!(
                    "manifest_path has no parent: {}",
                    pkg.manifest_path
                )))
            })?;

        let mut entry_points: Vec<PathBuf> = Vec::new();
        for tgt in &pkg.targets {
            let raw: PathBuf = tgt.src_path.clone().into();
            let canon = raw.canonicalize().unwrap_or(raw);
            if claimed.insert(canon.clone()) {
                entry_points.push(canon);
            }
        }

        if entry_points.is_empty() {
            continue;
        }

        let unit = CrateUnit {
            edition,
            manifest_dir,
            files: entry_points,
        };
        if tx.send(unit).is_err() {
            return Err(Error::SendClosed);
        }
    }

    Ok(())
}

fn map_edition(e: cargo_metadata::Edition) -> Edition {
    match e {
        cargo_metadata::Edition::E2015 => Edition::E2015,
        cargo_metadata::Edition::E2018 => Edition::E2018,
        cargo_metadata::Edition::E2021 => Edition::E2021,
        cargo_metadata::Edition::E2024 => Edition::E2024,
        _ => Edition::E2024,
    }
}
