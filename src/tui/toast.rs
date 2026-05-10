//! Toast notifications for the terminal chat UI.
//!
//! The provider is intentionally pure/stateful: extensions and core modules can
//! publish arbitrary toast content, while the renderer decides where and how it
//! appears on screen.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

pub(crate) use synaps_cli::toast::{ToastPosition, ToastX, ToastY};

/// Arbitrary toast content. Callers can provide a simple string, multiple lines,
/// or keep a toast resident forever by setting `ttl` to `None`.
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    id: String,
    pub(crate) title: Option<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) position: ToastPosition,
    created_at: Instant,
    ttl: Option<Duration>,
}

impl Toast {
    pub(crate) fn new(id: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: None,
            lines: vec![line.into()],
            position: ToastPosition::default(),
            created_at: Instant::now(),
            ttl: Some(Duration::from_secs(4)),
        }
    }

    pub(crate) fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub(crate) fn lines(mut self, lines: Vec<String>) -> Self {
        self.lines = lines;
        self
    }

    pub(crate) fn at(mut self, position: ToastPosition) -> Self {
        self.position = position;
        self
    }

    pub(crate) fn ttl(mut self, ttl: Option<Duration>) -> Self {
        self.ttl = ttl;
        self
    }

    fn is_expired(&self, now: Instant) -> bool {
        self.ttl.is_some_and(|ttl| now.duration_since(self.created_at) >= ttl)
    }

    #[allow(dead_code)]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

/// In-memory toast provider. Upserting by id lets async loaders update a single
/// on-screen notification instead of flooding the transcript.
#[derive(Debug, Default)]
pub(crate) struct ToastProvider {
    toasts: VecDeque<Toast>,
    max_visible: usize,
}

impl ToastProvider {
    pub(crate) fn new() -> Self {
        Self { toasts: VecDeque::new(), max_visible: 5 }
    }

    pub(crate) fn upsert(&mut self, toast: Toast) {
        if let Some(existing) = self.toasts.iter_mut().find(|t| t.id == toast.id) {
            *existing = toast;
            return;
        }
        self.toasts.push_back(toast);
        while self.toasts.len() > self.max_visible {
            self.toasts.pop_front();
        }
    }

    pub(crate) fn dismiss(&mut self, id: &str) {
        self.toasts.retain(|toast| toast.id != id);
    }

    pub(crate) fn tick(&mut self) -> bool {
        let before = self.toasts.len();
        let now = Instant::now();
        self.toasts.retain(|toast| !toast.is_expired(now));
        before != self.toasts.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub(crate) fn visible(&self) -> impl Iterator<Item = &Toast> {
        self.toasts.iter()
    }
}

/// Compute the rectangle for a toast with the given content size.
pub(crate) fn toast_rect(area: Rect, toast_width: u16, toast_height: u16, position: ToastPosition) -> Rect {
    let safe = super::lightbox::lightbox_safe_area(area);
    let width = toast_width.min(safe.width);
    let height = toast_height.min(safe.height);
    let x = match position.x {
        ToastX::Left => safe.x,
        ToastX::Center => safe.x + safe.width.saturating_sub(width) / 2,
        ToastX::Right => safe.x + safe.width.saturating_sub(width),
    };
    let y = match position.y {
        ToastY::Top => safe.y.saturating_add(1),
        ToastY::Middle => safe.y + safe.height.saturating_sub(height) / 2,
        ToastY::Bottom => safe.y + safe.height.saturating_sub(height),
    }.min(safe.y + safe.height.saturating_sub(height));
    Rect { x, y, width, height }
}

/// Return a stable anchor point for extension-controlled persistent overlays
/// like an any-buddy style pet. This gives extensions an obvious coordinate
/// system without coupling them to ratatui internals.
#[allow(dead_code)]
pub(crate) fn anchor_point(area: Rect, position: ToastPosition) -> Position {
    let rect = toast_rect(area, 1, 1, position);
    Position { x: rect.x, y: rect.y }
}

pub(crate) fn toast_lines(toast: &Toast) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(title) = &toast.title {
        lines.push(Line::from(title.clone()));
    }
    lines.extend(toast.lines.iter().cloned().map(Line::from));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_places_diagonal_positions_inside_safe_edges() {
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(toast_rect(area, 20, 5, ToastPosition::TOP_LEFT), Rect::new(2, 1, 20, 5));
        assert_eq!(toast_rect(area, 20, 5, ToastPosition::TOP_RIGHT), Rect::new(78, 1, 20, 5));
        assert_eq!(toast_rect(area, 20, 5, ToastPosition::BOTTOM_LEFT), Rect::new(2, 35, 20, 5));
        assert_eq!(toast_rect(area, 20, 5, ToastPosition::BOTTOM_RIGHT), Rect::new(78, 35, 20, 5));
    }

    #[test]
    fn rect_places_center_positions() {
        let area = Rect::new(0, 0, 101, 41);
        assert_eq!(toast_rect(area, 21, 5, ToastPosition::CENTER), Rect::new(40, 18, 21, 5));
        assert_eq!(toast_rect(area, 21, 5, ToastPosition::TOP_CENTER), Rect::new(40, 1, 21, 5));
        assert_eq!(toast_rect(area, 21, 5, ToastPosition::MIDDLE_LEFT), Rect::new(2, 18, 21, 5));
        assert_eq!(toast_rect(area, 21, 5, ToastPosition::MIDDLE_RIGHT), Rect::new(78, 18, 21, 5));
        assert_eq!(toast_rect(area, 21, 5, ToastPosition::BOTTOM_CENTER), Rect::new(40, 36, 21, 5));
    }

    #[test]
    fn default_toast_position_is_top_center_below_status_bar() {
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(ToastPosition::default(), ToastPosition::TOP_CENTER);
        assert_eq!(toast_rect(area, 20, 5, ToastPosition::default()), Rect::new(40, 1, 20, 5));
    }

    #[test]
    fn provider_upsert_replaces_existing_toast() {
        let mut provider = ToastProvider::new();
        provider.upsert(Toast::new("loader", "1/3 loading"));
        provider.upsert(Toast::new("loader", "2/3 loading"));
        let visible: Vec<_> = provider.visible().collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].lines, vec!["2/3 loading".to_string()]);
    }

    #[test]
    fn persistent_toast_survives_tick() {
        let mut provider = ToastProvider::new();
        provider.upsert(Toast::new("buddy", "(•ᴗ•)").ttl(None));
        assert!(!provider.tick());
        assert_eq!(provider.visible().count(), 1);
    }
}
