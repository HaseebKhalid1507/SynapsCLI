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
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_absolute() && h != Path::new("/")),
        }
    }

    /// `p` relative to the cwd when under it, `~/…` when under home,
    /// otherwise unchanged. Only absolute, component-boundary matches count
    /// (`/a/bc` is not under `/a/b`).
    pub(crate) fn shorten<'a>(&self, p: &'a str) -> Cow<'a, str> {
        if !p.starts_with('/') {
            return Cow::Borrowed(p);
        }
        let under = |base: &Path| -> Option<&'a str> {
            let b = base.to_str()?.trim_end_matches('/');
            if b.is_empty() {
                return None;
            }
            let rest = p.strip_prefix(b)?;
            if rest.is_empty() {
                Some("")
            } else {
                rest.strip_prefix('/')
            }
        };
        if let Some(rest) = self.cwd.as_deref().and_then(under) {
            return Cow::Borrowed(if rest.is_empty() { "." } else { rest });
        }
        if let Some(rest) = self.home.as_deref().and_then(under) {
            return Cow::Owned(if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rest}")
            });
        }
        Cow::Borrowed(p)
    }
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
