use ratatui::layout::Rect;

/// Terminal-safe inset for overlays/lightboxes. Some terminals fail to repaint
/// the far left/right columns reliably; default overlays therefore avoid two
/// columns on each horizontal edge.
pub(crate) const LIGHTBOX_EDGE_INSET: u16 = 2;

pub(crate) fn lightbox_safe_area(area: Rect) -> Rect {
    if area.width <= LIGHTBOX_EDGE_INSET.saturating_mul(2) {
        area
    } else {
        Rect {
            x: area.x + LIGHTBOX_EDGE_INSET,
            y: area.y,
            width: area.width - LIGHTBOX_EDGE_INSET.saturating_mul(2),
            height: area.height,
        }
    }
}

pub(crate) fn centered_lightbox_rect(area: Rect, width: u16, height: u16) -> Rect {
    let safe = lightbox_safe_area(area);
    let width = width.min(safe.width);
    let height = height.min(safe.height);
    Rect {
        x: safe.x + safe.width.saturating_sub(width) / 2,
        y: safe.y + safe.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_area_moves_edges_two_cells_toward_middle() {
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(lightbox_safe_area(area), Rect::new(2, 0, 96, 40));
    }

    #[test]
    fn centered_lightbox_never_uses_unreliable_edge_columns_when_full_width() {
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(centered_lightbox_rect(area, 100, 20), Rect::new(2, 10, 96, 20));
    }

    #[test]
    fn tiny_area_is_left_unchanged() {
        let area = Rect::new(0, 0, 2, 4);
        assert_eq!(lightbox_safe_area(area), area);
        assert_eq!(centered_lightbox_rect(area, 2, 2), Rect::new(0, 1, 2, 2));
    }
}
