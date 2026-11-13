//! Display-only path shortening for tool cards (grep / find output).
//!
//! Tools resolve paths against the session cwd, so their output is full of
//! `/home/<user>/project/...`. Cards show paths under the session cwd
//! relative to it (the header already says where that is), other paths
//! under the home directory as `~/...`. The tool result content is never
//! rewritten — only the rendered row.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Where card paths are shortened from: the session cwd and `$HOME`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PathBase {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) home: Option<PathBuf>,
}

impl PathBase {
    pub(crate) fn new(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd,
            home: home_dir_var()
                .map(PathBuf::from)
                .filter(|h| h.is_absolute() && h != Path::new("/")),
        }
    }

    /// `p` relative to the cwd when under it, `~/…` when under home,
    /// otherwise unchanged. Only absolute, component-boundary matches count
    /// (`/a/bc` is not under `/a/b`). On Windows `C:\…` paths count too,
    /// matched case-insensitively with `\` and `/` alike.
    pub(crate) fn shorten<'a>(&self, p: &'a str) -> Cow<'a, str> {
        shorten_in(p, self.cwd.as_deref(), self.home.as_deref(), cfg!(windows))
    }
}

/// The home directory variable: `USERPROFILE` on Windows (`HOME` when a
/// Unix-style shell set only that), `HOME` elsewhere.
fn home_dir_var() -> Option<std::ffi::OsString> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }
}

fn is_absolute(p: &str, windows: bool) -> bool {
    if !windows {
        return p.starts_with('/');
    }
    matches!(p.as_bytes(), [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic())
        || p.starts_with('/')
        || p.starts_with("\\\\")
}

/// [`PathBase::shorten`] with the platform as a parameter, so both rule sets
/// are tested everywhere.
fn shorten_in<'a>(p: &'a str, cwd: Option<&Path>, home: Option<&Path>, windows: bool) -> Cow<'a, str> {
    if !is_absolute(p, windows) {
        return Cow::Borrowed(p);
    }
    let is_sep = move |c: char| c == '/' || (windows && c == '\\');
    // Same byte length as the input: `\` → `/` and ASCII lowercase only.
    let norm = |s: &str| if windows { s.replace('\\', "/").to_ascii_lowercase() } else { s.to_string() };
    let np = norm(p);
    // (rest, the separator after the base) when `p` is under `base`.
    let under = |base: &Path| -> Option<(&'a str, char)> {
        let nb = norm(base.to_str()?);
        let nb = nb.trim_end_matches('/');
        if nb.is_empty() || !np.starts_with(nb) {
            return None;
        }
        let rest = &p[nb.len()..];
        let Some(sep) = rest.chars().next() else {
            return Some(("", '/'));
        };
        let mut rest = rest.strip_prefix(is_sep)?;
        // `<cwd>/./x` (a tool given the path `.`) reads as `x`.
        while let Some(r) = rest.strip_prefix('.').and_then(|r| r.strip_prefix(is_sep)) {
            rest = r;
        }
        Some((rest, sep))
    };
    if let Some((rest, _)) = cwd.and_then(under) {
        return Cow::Borrowed(if rest.is_empty() { "." } else { rest });
    }
    if let Some((rest, sep)) = home.and_then(under) {
        return Cow::Owned(if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{sep}{rest}")
        });
    }
    Cow::Borrowed(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBase {
        PathBase {
            cwd: Some("/home/u/Projects/synaps".into()),
            home: Some("/home/u".into()),
        }
    }

    /// Windows card paths: drive paths under the session folder are relative,
    /// matched case-insensitively with either separator; under the user
    /// profile `~\…`; the `\./` a tool given `.` produces is dropped.
    #[test]
    fn windows_paths_shorten_under_cwd_then_profile() {
        let cwd = Path::new(r"C:\Users\Haseeb\smoke-proj");
        let home = Path::new(r"C:\Users\Haseeb");
        let s = |p| shorten_in(p, Some(cwd), Some(home), true).into_owned();
        assert_eq!(s(r"C:\Users\Haseeb\smoke-proj\./README.md"), "README.md");
        assert_eq!(s(r"C:\Users\Haseeb\smoke-proj\src\calc.ps1"), r"src\calc.ps1");
        assert_eq!(s("c:/users/haseeb/smoke-proj/docs/notes.md"), "docs/notes.md");
        assert_eq!(s(r"C:\Users\Haseeb\smoke-proj"), ".");
        assert_eq!(s(r"C:\Users\Haseeb\smoke-projx\a"), r"~\smoke-projx\a");
        assert_eq!(s(r"C:\Users\Haseeb\AppData\Local\Temp\x.txt"), r"~\AppData\Local\Temp\x.txt");
        assert_eq!(s(r"D:\other\x"), r"D:\other\x");
        assert_eq!(s(r"src\rel.ps1"), r"src\rel.ps1");
        // The Unix rules never treat a drive path as absolute.
        assert_eq!(shorten_in(r"C:\Users\Haseeb\a", Some(cwd), Some(home), false), r"C:\Users\Haseeb\a");
    }

    #[test]
    fn a_dot_component_after_the_cwd_is_dropped() {
        let b = base();
        assert_eq!(b.shorten("/home/u/Projects/synaps/./src/main.rs"), "src/main.rs");
        assert_eq!(b.shorten("/home/u/Projects/synaps/."), ".");
        assert_eq!(b.shorten("/home/u/Projects/synaps/.hidden"), ".hidden");
    }

    #[test]
    fn shortens_under_cwd_then_home() {
        let b = base();
        assert_eq!(
            b.shorten("/home/u/Projects/synaps/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(b.shorten("/home/u/Projects/synaps"), ".");
        assert_eq!(
            b.shorten("/home/u/Projects/synapsx/a"),
            "~/Projects/synapsx/a"
        );
        assert_eq!(b.shorten("/home/u/.cargo/x"), "~/.cargo/x");
        assert_eq!(b.shorten("/etc/hosts"), "/etc/hosts");
        assert_eq!(b.shorten("src/rel.rs"), "src/rel.rs");
        assert_eq!(PathBase::default().shorten("/home/u/a"), "/home/u/a");
    }

    fn card_rows(tool: &str, result: &str, width: usize) -> Vec<String> {
        use crate::tui::transcript::{ChatMessage, RenderCtx, TranscriptStore};
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        store.set_path_base(base());
        store.push_msg(ChatMessage::ToolUse {
            tool_id: "g1".into(),
            tool_name: tool.into(),
            input: r#"{"path":"src","pattern":"fn main"}"#.into(),
        });
        store.push_msg(ChatMessage::ToolResult {
            tool_id: "g1".into(),
            content: result.into(),
            elapsed_ms: Some(9),
        });
        let ctx = RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "agent",
        };
        let rows: Vec<String> = store
            .render_message_lines(1, width, &ctx)
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // The model-facing content is untouched.
        match &store.messages()[1].msg {
            ChatMessage::ToolResult { content, .. } => assert_eq!(content, result),
            _ => unreachable!(),
        }
        rows
    }

    #[test]
    fn grep_card_shows_cwd_relative_paths_and_ellipsis() {
        let result = "/home/u/Projects/synaps/src/bin/hidden/main.rs:25:fn main() -> io::Result<()> {\n\
                      /home/u/Projects/synaps/src/cmd/login.rs:466:fn main_app_args(profile: Option<String>) -> Vec<String> { todo!() }\n\
                      /opt/elsewhere/main.rs:1:fn main() {}";
        let rows = card_rows("grep", result, 70);
        let all = rows.join("\n");
        assert!(!all.contains("/home/u/Projects/synaps/"), "{all}");
        assert!(all.contains("src/bin/hidden/main.rs:25:fn main()"), "{all}");
        assert!(
            all.contains("/opt/elsewhere/main.rs:1:"),
            "outside cwd kept: {all}"
        );
        let long = rows
            .iter()
            .find(|r| r.contains("src/cmd/login.rs:466:"))
            .expect("login row");
        assert!(
            long.trim_end().ends_with('\u{2026}'),
            "clipped row ends in an ellipsis: {long:?}"
        );
    }

    #[test]
    fn find_card_shows_cwd_relative_paths() {
        let result =
            "/home/u/Projects/synaps/src/main.rs\n/home/u/Projects/synaps/src/cmd/login.rs";
        let all = card_rows("find", result, 80).join("\n");
        assert!(!all.contains("/home/u"), "{all}");
        assert!(all.contains("src/cmd/login.rs"), "{all}");
    }
}
