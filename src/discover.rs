use crate::cache;
use crate::size;
use crate::types::{Config, CrateUnit, Edition, Error, Result, UnknownEdition};
use cargo_metadata::MetadataCommand;
use crossbeam_channel::Sender;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    let ws_root = metadata.workspace_root.as_std_path().to_path_buf();
    set_workspace_root(&ws_root);

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
    //   3. running at the workspace root → every workspace member.
    //      `cargo fmt`'s quirk: when the effective manifest is the
    //      workspace root's manifest, it ignores both `default-members`
    //      and a root `[package]` and formats every member. Reproducing
    //      this is necessary for byte-equivalence on workspaces like
    //      reth (`default-members = ["bin/reth"]`) and bevy (root
    //      package `bevy` plus 87 sub-crates).
    //   4. otherwise → format the package implicitly selected by
    //      `--manifest-path` (or cwd). For a virtual workspace with no
    //      implicit package, fall back to `workspace.default-members`.
    let at_root = at_workspace_root(cfg, metadata.workspace_root.as_std_path());
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
    };

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
        let mut entry_points: Vec<PathBuf> = Vec::new();
        for tgt in &pkg.targets {
            let raw = tgt.src_path.as_std_path().to_path_buf();
            let canon = raw.canonicalize().unwrap_or(raw);
            let claimer = ClaimSite {
                name: pkg.name.to_string(),
                edition,
                manifest_path: pkg.manifest_path.as_std_path().to_path_buf(),
            };
            match claimed.entry(canon.clone()) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(claimer);
                    entry_points.push(canon);
                }
                std::collections::hash_map::Entry::Occupied(o) => {
                    if cfg.warnings {
                        emit_claim_collision_warning(&canon, o.get(), &claimer);
                    }
                }
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

    if cfg.warnings && editions_seen.len() > 1 {
        emit_multi_edition_warning(&editions_seen);
    }
    if cfg.warnings {
        emit_shadow_config_warning(&governed, &ws_root);
        emit_config_edition_warning(&governed);
        emit_implicit_edition_warning(&implicit_2015);
    }

    Ok(cache_opt)
}

/// True when the effective manifest path (`--manifest-path` if given,
/// else the nearest `Cargo.toml` walking up from cwd) equals the
/// workspace's root `Cargo.toml`. Mirrors `cargo-fmt`'s `in_workspace_root`
/// flag, which it uses to expand the implicit selection to every member.
fn at_workspace_root(cfg: &Config, ws_root: &Path) -> bool {
    let ws_manifest = match ws_root.join("Cargo.toml").canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let effective = match &cfg.manifest_path {
        Some(p) => p.canonicalize().ok(),
        None => std::env::current_dir()
            .ok()
            .and_then(|cwd| find_manifest_upward(&cwd)),
    };
    effective.map(|m| m == ws_manifest).unwrap_or(false)
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

/// Where a crate declared a target — captured so the collision warning
/// can show both claim sites with line numbers in `Cargo.toml`.
struct ClaimSite {
    name: String,
    edition: Edition,
    manifest_path: PathBuf,
}

/// Workspace root captured at the start of `discover::run`, in both
/// raw and canonicalized form. Used by warning emitters to display
/// paths relative to the workspace root instead of as long absolutes.
/// On macOS the two often differ (`/tmp` vs `/private/tmp`); cargo
/// hands us raw paths but we canonicalize the file targets ourselves,
/// so we strip against either.
struct WsRoots {
    raw: PathBuf,
    canon: PathBuf,
}

static WS_ROOTS: OnceLock<WsRoots> = OnceLock::new();

fn set_workspace_root(raw: &Path) {
    let _ = WS_ROOTS.set(WsRoots {
        raw: raw.to_path_buf(),
        canon: raw.canonicalize().unwrap_or_else(|_| raw.to_path_buf()),
    });
}

/// Strip the workspace-root prefix from `p` if possible, falling back
/// to the absolute path when nothing matches.
fn rel(p: &Path) -> &Path {
    let Some(roots) = WS_ROOTS.get() else {
        return p;
    };
    p.strip_prefix(&roots.raw)
        .or_else(|_| p.strip_prefix(&roots.canon))
        .unwrap_or(p)
}

fn emit_claim_collision_warning(canon: &Path, first: &ClaimSite, second: &ClaimSite) {
    use std::fmt::Write as _;
    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (f, fr) = (p.frame.render(), p.frame.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());

    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "{w}warning{wr}: file `{}` claimed by multiple crates",
        rel(canon).display()
    );
    // Primary span: the first crate's Cargo.toml entry. Caret label
    // names the owning crate so readers know which side wins.
    render_claim_span(
        &mut buf,
        canon,
        first,
        &format!("first claim (`{}`)", first.name),
    );
    // Secondary span as a `note:` — rustc convention for additional
    // related code locations.
    let _ = writeln!(buf, "{n}note{nr}: also claimed here (`{}`)", second.name);
    render_claim_span(&mut buf, canon, second, "");
    if first.edition != second.edition {
        let _ = writeln!(
            buf,
            "   {f}={fr} {n}note{nr}: editions differ — using `{}`'s {} over `{}`'s {}",
            first.name,
            first.edition.as_str(),
            second.name,
            second.edition.as_str()
        );
    }
    buf.push('\n');
    let _ = std::io::stderr().write_all(buf.as_bytes());
}

/// Render one claim site as a span with file:line:col header, source
/// line, and caret-row pointer. `caret_label` is appended after the
/// carets when non-empty (rustc's "label" style). Falls back to a
/// file-only line when the source line can't be located.
fn render_claim_span(buf: &mut String, canon: &Path, site: &ClaimSite, caret_label: &str) {
    use std::fmt::Write as _;
    let p = crate::style::palette();
    let (f, fr) = (p.frame.render(), p.frame.render_reset());

    if let Some((line_no, line_text)) = find_target_path_line(&site.manifest_path, canon) {
        let pad = line_no.to_string().len();
        let blank = " ".repeat(pad);
        let body = line_text.trim_end();
        let _ = writeln!(
            buf,
            " {blank}{f}-->{fr} {}:{line_no}:1",
            rel(&site.manifest_path).display()
        );
        let _ = writeln!(buf, " {blank} {f}|{fr}");
        let _ = writeln!(buf, " {f}{line_no} |{fr} {body}");
        if caret_label.is_empty() {
            let _ = writeln!(buf, " {blank} {f}| {}{fr}", "^".repeat(body.len()));
        } else {
            let _ = writeln!(
                buf,
                " {blank} {f}| {} {caret_label}{fr}",
                "^".repeat(body.len())
            );
        }
    } else {
        let _ = writeln!(buf, "  {f}-->{fr} {}", rel(&site.manifest_path).display());
    }
}

fn emit_multi_edition_warning(seen: &HashMap<Edition, String>) {
    use std::fmt::Write as _;
    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());
    let mut summary: Vec<(Edition, &String)> = seen.iter().map(|(e, n)| (*e, n)).collect();
    summary.sort_by_key(|(e, _)| e.as_str());
    let parts: Vec<String> = summary
        .iter()
        .map(|(e, n)| format!("{} (e.g. `{n}`)", e.as_str()))
        .collect();
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "{w}warning{wr}: workspace mixes {} editions",
        summary.len()
    );
    let _ = writeln!(buf, "   {n}note{nr}: {}", parts.join(", "));
    let _ = writeln!(
        buf,
        "   {n}note{nr}: rustfmt parses each crate per its own edition, so reserved-keyword identifiers may format differently across the boundary"
    );
    buf.push('\n');
    let _ = std::io::stderr().write_all(buf.as_bytes());
}

/// Scan a Cargo.toml line-by-line for a `path = "..."` whose value
/// resolves to `target_canon`. Best-effort diagnostic helper — not a
/// real TOML parser, just close enough for cargo target tables.
fn find_target_path_line(manifest_path: &Path, target_canon: &Path) -> Option<(usize, String)> {
    let manifest_dir = manifest_path.parent()?;
    let content = std::fs::read_to_string(manifest_path).ok()?;
    for (idx, line) in content.lines().enumerate() {
        let Some(quoted) = extract_path_string(line) else {
            continue;
        };
        let resolved = manifest_dir.join(quoted).canonicalize().ok();
        if resolved.as_deref() == Some(target_canon) {
            return Some((idx + 1, line.to_string()));
        }
    }
    None
}

fn extract_path_string(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("path")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
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

/// A `rustfmt.toml` sitting below the workspace root silently overrides
/// the workspace config for its subtree (rustfmt resolves config per
/// file). Surface those so a stray sub-config isn't mistaken for the
/// workspace one. No-op when nothing below the root claims a crate.
fn emit_shadow_config_warning(governed: &HashMap<PathBuf, Vec<(String, Edition)>>, ws_root: &Path) {
    use std::fmt::Write as _;
    let mut shadows: Vec<(&PathBuf, &Vec<(String, Edition)>)> = governed
        .iter()
        .filter(|(cfg, _)| cfg.parent() != Some(ws_root))
        .collect();
    if shadows.is_empty() {
        return;
    }
    shadows.sort_by(|a, b| a.0.cmp(b.0));

    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());
    let n_files = shadows.len();
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "{w}warning{wr}: {n_files} nested rustfmt.toml file{} shadow{} the workspace config",
        if n_files == 1 { "" } else { "s" },
        if n_files == 1 { "s" } else { "" },
    );
    for (cfg, crates) in &shadows {
        let mut names: Vec<&str> = crates.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        let joined = names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(buf, "   {n}note{nr}: `{}` governs {joined}", rel(cfg).display());
    }
    let _ = writeln!(
        buf,
        "   {n}note{nr}: rustfmt resolves config per file (walking up from each path), so these crates ignore the workspace-root rustfmt.toml"
    );
    buf.push('\n');
    let _ = std::io::stderr().write_all(buf.as_bytes());
}

/// `cargo ff` always passes `--edition` (from each crate's Cargo.toml,
/// see `exec::format_batch`), which overrides any `edition` in a
/// rustfmt.toml. That key is silently ineffective here but still applies
/// to a bare `rustfmt` run, so the two can disagree. Warn per config
/// whose declared edition differs from a crate it governs.
fn emit_config_edition_warning(governed: &HashMap<PathBuf, Vec<(String, Edition)>>) {
    use std::fmt::Write as _;
    let mut configs: Vec<(&PathBuf, &Vec<(String, Edition)>)> = governed.iter().collect();
    configs.sort_by(|a, b| a.0.cmp(b.0));

    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());

    let mut buf = String::new();
    for (cfg, crates) in configs {
        let Some(cfg_edition) = std::fs::read_to_string(cfg)
            .ok()
            .and_then(|c| extract_toml_edition(&c))
        else {
            continue;
        };
        // Representative crate whose Cargo.toml edition differs — that's
        // the one `--edition` will silently win over.
        let mut mismatched: Vec<&(String, Edition)> = crates
            .iter()
            .filter(|(_, e)| e.as_str() != cfg_edition)
            .collect();
        if mismatched.is_empty() {
            continue;
        }
        mismatched.sort_by(|a, b| a.0.cmp(&b.0));
        let (name, ed) = mismatched[0];
        let _ = writeln!(
            buf,
            "{w}warning{wr}: `{}` sets `edition = \"{cfg_edition}\"`, which cargo ff overrides",
            rel(cfg).display()
        );
        let _ = writeln!(
            buf,
            "   {n}note{nr}: cargo ff passes `--edition` from each crate's Cargo.toml (e.g. `{name}` is edition {})",
            ed.as_str()
        );
        let _ = writeln!(
            buf,
            "   {n}note{nr}: the rustfmt.toml `edition` still applies to a bare `rustfmt` run, so output can diverge from cargo ff / cargo fmt"
        );
        buf.push('\n');
    }
    if !buf.is_empty() {
        let _ = std::io::stderr().write_all(buf.as_bytes());
    }
}

/// Crates with no `edition` key default to 2015 — almost always a
/// mistake in a modern workspace, and 2015 formats differently (e.g.
/// `dyn`/`async`/`try` aren't reserved). Aggregate them into one warning.
fn emit_implicit_edition_warning(names: &[String]) {
    use std::fmt::Write as _;
    if names.is_empty() {
        return;
    }
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort_unstable();

    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());
    let (h, hr) = (p.help.render(), p.help.render_reset());

    let count = names.len();
    let joined = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "{w}warning{wr}: {count} crate{} default{} to edition 2015 (no `edition` in Cargo.toml)",
        if count == 1 { "" } else { "s" },
        if count == 1 { "s" } else { "" },
    );
    let _ = writeln!(buf, "   {n}note{nr}: {joined}");
    let _ = writeln!(
        buf,
        "   {h}help{hr}: add `edition = \"2021\"` (or another) to each Cargo.toml — 2015 formats differently from later editions"
    );
    buf.push('\n');
    let _ = std::io::stderr().write_all(buf.as_bytes());
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

/// Best-effort scan for a top-level `edition = "..."` in a rustfmt.toml.
/// Not a real TOML parser — rustfmt.toml is flat, so a line scan suffices.
fn extract_toml_edition(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("edition") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        return Some(rest[..end].to_string());
    }
    None
}
