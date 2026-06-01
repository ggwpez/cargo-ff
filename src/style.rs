//! Tiny styling layer for rustc-flavored warnings on stderr. Off when
//! stderr is not a tty, when `NO_COLOR` is set, or when output is being
//! captured (CI logs etc.). `CLICOLOR_FORCE=1` overrides and forces on.

use anstyle::{AnsiColor, Style};
use std::sync::OnceLock;

/// Pre-configured styles matching rustc's diagnostic colors: bold
/// yellow `warning:`, cyan frame (`-->`, `=`), bold cyan `note:` /
/// `help:`.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub warning: Style,
    pub frame: Style,
    pub note: Style,
    pub help: Style,
}

impl Palette {
    fn colored() -> Self {
        let cyan = Style::new().fg_color(Some(AnsiColor::Cyan.into()));
        Self {
            warning: Style::new().bold().fg_color(Some(AnsiColor::Yellow.into())),
            frame: cyan,
            note: cyan.bold(),
            help: cyan.bold(),
        }
    }

    fn plain() -> Self {
        Self {
            warning: Style::new(),
            frame: Style::new(),
            note: Style::new(),
            help: Style::new(),
        }
    }
}

pub(crate) fn palette() -> &'static Palette {
    static CACHE: OnceLock<Palette> = OnceLock::new();
    CACHE.get_or_init(|| {
        if should_color() {
            Palette::colored()
        } else {
            Palette::plain()
        }
    })
}

/// `CLICOLOR_FORCE=1` (any non-`"0"` value) wins — useful when piping
/// to a pager or CI log viewer that supports color. Otherwise
/// `NO_COLOR` (any value) disables, matching the cross-tool convention.
/// Falls back to stderr-is-tty as the default heuristic.
fn should_color() -> bool {
    use std::io::IsTerminal;
    if let Some(v) = std::env::var_os("CLICOLOR_FORCE")
        && v != "0"
    {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stderr().is_terminal()
}
