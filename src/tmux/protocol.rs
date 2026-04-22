//! Control mode protocol parser for tmux notifications and command responses.

/// Events received from tmux control mode.
#[derive(Debug, Clone)]
pub enum TmuxEvent {
    /// %begin <time> <command_num> <flags>
    Begin { time: u64, command_num: u64, flags: u32 },
    /// %end <time> <command_num> <flags>
    End { time: u64, command_num: u64, flags: u32 },
    /// %error <time> <command_num> <flags>
    Error { time: u64, command_num: u64, flags: u32 },
    /// %output %<pane_id> <data>
    Output { pane_id: String, data: String },
    /// %window-add @<window_id>
    WindowAdd { window_id: String },
    /// %window-close @<window_id>
    WindowClose { window_id: String },
    /// %window-renamed @<window_id> <name>
    WindowRenamed { window_id: String, name: String },
    /// %session-changed $<session_id> <name>
    SessionChanged { session_id: String, name: String },
    /// %layout-change @<window_id> <layout_string>
    LayoutChange { window_id: String, layout: String },
    /// %pane-mode-changed %<pane_id>
    PaneModeChanged { pane_id: String },
    /// %pane-exited %<pane_id>
    PaneExited { pane_id: String },
    /// Non-notification data (command response lines between %begin/%end)
    Data(String),
}

impl TmuxEvent {
    /// Parse a single line from tmux control mode output.
    pub fn parse(line: &str) -> Option<TmuxEvent> {
        if line.starts_with("%begin ") {
            return Self::parse_triple(line, "%begin ").map(|(t, n, f)| TmuxEvent::Begin {
                time: t, command_num: n, flags: f,
            });
        }
        if line.starts_with("%end ") {
            return Self::parse_triple(line, "%end ").map(|(t, n, f)| TmuxEvent::End {
                time: t, command_num: n, flags: f,
            });
        }
        if line.starts_with("%error ") {
            return Self::parse_triple(line, "%error ").map(|(t, n, f)| TmuxEvent::Error {
                time: t, command_num: n, flags: f,
            });
        }
        if let Some(rest) = line.strip_prefix("%output ") {
            let (pane_id, data) = rest.split_once(' ').unwrap_or((rest, ""));
            return Some(TmuxEvent::Output {
                pane_id: pane_id.to_string(),
                data: data.to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%window-add ") {
            return Some(TmuxEvent::WindowAdd { window_id: rest.trim().to_string() });
        }
        if let Some(rest) = line.strip_prefix("%window-close ") {
            return Some(TmuxEvent::WindowClose { window_id: rest.trim().to_string() });
        }
        if let Some(rest) = line.strip_prefix("%window-renamed ") {
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            return Some(TmuxEvent::WindowRenamed {
                window_id: id.to_string(),
                name: name.to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%session-changed ") {
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            return Some(TmuxEvent::SessionChanged {
                session_id: id.to_string(),
                name: name.to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%layout-change ") {
            let (id, layout) = rest.split_once(' ').unwrap_or((rest, ""));
            return Some(TmuxEvent::LayoutChange {
                window_id: id.to_string(),
                layout: layout.to_string(),
            });
        }
        if let Some(rest) = line.strip_prefix("%pane-mode-changed ") {
            return Some(TmuxEvent::PaneModeChanged { pane_id: rest.trim().to_string() });
        }
        if let Some(rest) = line.strip_prefix("%pane-exited ") {
            return Some(TmuxEvent::PaneExited { pane_id: rest.trim().to_string() });
        }
        // Non-notification line = data (command response body)
        Some(TmuxEvent::Data(line.to_string()))
    }

    fn parse_triple(line: &str, prefix: &str) -> Option<(u64, u64, u32)> {
        let rest = line.strip_prefix(prefix)?;
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() >= 3 {
            let time = parts[0].parse().ok()?;
            let num = parts[1].parse().ok()?;
            let flags = parts[2].parse().ok()?;
            Some((time, num, flags))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_output_notification() {
        let line = "%output %0 hello world";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::Output { ref pane_id, ref data })
            if pane_id == "%0" && data == "hello world"));
    }

    #[test]
    fn test_parse_window_add_notification() {
        let line = "%window-add @5";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::WindowAdd { ref window_id })
            if window_id == "@5"));
    }

    #[test]
    fn test_parse_layout_change() {
        let line = "%layout-change @0 ab12,200x50,0,0{100x50,0,0,0,99x50,101,0,1}";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::LayoutChange { ref window_id, .. })
            if window_id == "@0"));
    }

    #[test]
    fn test_parse_begin_end() {
        let begin = "%begin 1234567890 42 1";
        let event = TmuxEvent::parse(begin);
        assert!(matches!(event, Some(TmuxEvent::Begin { command_num: 42, .. })));

        let end = "%end 1234567890 42 1";
        let event = TmuxEvent::parse(end);
        assert!(matches!(event, Some(TmuxEvent::End { command_num: 42, .. })));
    }

    #[test]
    fn test_parse_unknown_line() {
        let line = "some random output";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::Data(_))));
    }

    #[test]
    fn test_parse_pane_exited() {
        let line = "%pane-exited %3";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::PaneExited { ref pane_id })
            if pane_id == "%3"));
    }

    #[test]
    fn test_parse_session_changed() {
        let line = "%session-changed $1 my-session";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::SessionChanged { ref session_id, ref name })
            if session_id == "$1" && name == "my-session"));
    }

    #[test]
    fn test_parse_error() {
        let line = "%error 1234567890 5 0";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::Error { command_num: 5, .. })));
    }

    #[test]
    fn test_parse_window_close() {
        let line = "%window-close @3";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::WindowClose { ref window_id })
            if window_id == "@3"));
    }

    #[test]
    fn test_parse_window_renamed() {
        let line = "%window-renamed @2 my-window";
        let event = TmuxEvent::parse(line);
        assert!(matches!(event, Some(TmuxEvent::WindowRenamed { ref window_id, ref name })
            if window_id == "@2" && name == "my-window"));
    }
}
