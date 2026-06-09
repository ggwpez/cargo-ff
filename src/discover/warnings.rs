//! Advisory diagnostics emitted during discovery (off by default, behind
//! `--ff-warnings`). All output is rustc-flavored and goes to stderr; none of
//! it affects formatting. Split out from the core discovery logic so the
//! routing code in `super` stays focused on *what* to format, not *how* to
//! complain about it.

use super::{ClaimSite, WsRoots};
use crate::types::Edition;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) fn emit_claim_collision_warning(
    roots: &WsRoots,
    canon: &Path,
    first: &ClaimSite,
    second: &ClaimSite,
) {
    use std::fmt::Write as _;
    let p = crate::style::palette();
    let (w, wr) = (p.warning.render(), p.warning.render_reset());
    let (f, fr) = (p.frame.render(), p.frame.render_reset());
    let (n, nr) = (p.note.render(), p.note.render_reset());

    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "{w}warning{wr}: file `{}` claimed by multiple crates",
        roots.rel(canon).display()
    );
    // Primary span: the first crate's Cargo.toml entry. Caret label
    // names the owning crate so readers know which side wins.
    render_claim_span(
        roots,
        &mut buf,
        canon,
        first,
        &format!("first claim (`{}`)", first.name),
    );
    // Secondary span as a `note:` — rustc convention for additional
    // related code locations.
    let _ = writeln!(buf, "{n}note{nr}: also claimed here (`{}`)", second.name);
    render_claim_span(roots, &mut buf, canon, second, "");
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

/// Render one claim site as a span with `file:line:col` header, source
/// line, and caret-row pointer. `caret_label` is appended after the
/// carets when non-empty (rustc's "label" style). Falls back to a
/// file-only line when the source line can't be located.
fn render_claim_span(
    roots: &WsRoots,
    buf: &mut String,
    canon: &Path,
    site: &ClaimSite,
    caret_label: &str,
) {
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
            roots.rel(&site.manifest_path).display()
        );
        let _ = writeln!(buf, " {blank} {f}|{fr}");
        let _ = writeln!(buf, " {f}{line_no} |{fr} {body}");
        let carets = "^".repeat(body.len());
        let label = if caret_label.is_empty() {
            String::new()
        } else {
            format!(" {caret_label}")
        };
        let _ = writeln!(buf, " {blank} {f}| {carets}{label}{fr}");
    } else {
        let _ = writeln!(buf, "  {f}-->{fr} {}", roots.rel(&site.manifest_path).display());
    }
}

pub(super) fn emit_multi_edition_warning(seen: &HashMap<Edition, String>) {
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

/// A `rustfmt.toml` sitting below the workspace root silently overrides
/// the workspace config for its subtree (rustfmt resolves config per
/// file). Surface those so a stray sub-config isn't mistaken for the
/// workspace one. No-op when nothing below the root claims a crate.
pub(super) fn emit_shadow_config_warning(
    roots: &WsRoots,
    governed: &HashMap<PathBuf, Vec<(String, Edition)>>,
) {
    use std::fmt::Write as _;
    let mut shadows: Vec<(&PathBuf, &Vec<(String, Edition)>)> = governed
        .iter()
        .filter(|(cfg, _)| cfg.parent() != Some(roots.raw()))
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
        let _ = writeln!(
            buf,
            "   {n}note{nr}: `{}` governs {joined}",
            roots.rel(cfg).display()
        );
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
pub(super) fn emit_config_edition_warning(
    roots: &WsRoots,
    governed: &HashMap<PathBuf, Vec<(String, Edition)>>,
) {
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
            roots.rel(cfg).display()
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
pub(super) fn emit_implicit_edition_warning(names: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::{extract_path_string, extract_toml_edition};

    #[test]
    fn extract_path_string_reads_quoted_value() {
        assert_eq!(extract_path_string(r#"path = "src/lib.rs""#), Some("src/lib.rs"));
        assert_eq!(extract_path_string(r#"  path="foo.rs""#), Some("foo.rs"));
    }

    #[test]
    fn extract_path_string_ignores_non_path_keys() {
        assert_eq!(extract_path_string(r#"name = "foo""#), None);
        // "path" must be the whole key, not a prefix of one.
        assert_eq!(extract_path_string(r#"pathological = "x""#), None);
        assert_eq!(extract_path_string(r#"# path = "x""#), None);
    }

    #[test]
    fn extract_toml_edition_finds_literal() {
        assert_eq!(extract_toml_edition("edition = \"2021\"\n"), Some("2021".to_string()));
        assert_eq!(
            extract_toml_edition("[package]\nedition=\"2018\"\n"),
            Some("2018".to_string())
        );
    }

    #[test]
    fn extract_toml_edition_skips_inherited_and_absent() {
        // `edition.workspace = true` has no inline literal to extract.
        assert_eq!(extract_toml_edition("edition.workspace = true"), None);
        assert_eq!(extract_toml_edition("name = \"x\"\n"), None);
    }
}
