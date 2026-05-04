use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    Terminal,
};

#[cfg(test)]
mod crossterm {
    pub mod cursor {
        #[allow(dead_code)]
        pub struct MoveTo(pub u16, pub u16);
    }
    pub mod style {
        #[allow(dead_code)]
        pub struct Print<T>(pub T);
    }
    #[allow(dead_code)]
    pub trait QueueableCommand: std::io::Write + Sized {
        fn queue(&mut self, _command: impl Sized) -> std::io::Result<&mut Self> {
            Ok(self)
        }
    }
    impl<T: std::io::Write> QueueableCommand for T {}
}

#[path = "../src/chatui/viewport.rs"]
mod viewport;

fn ch(buf: &Buffer, x: u16, y: u16) -> &str {
    buf.cell((x, y)).unwrap().symbol()
}

#[test]
fn edge_scrub_positions_include_both_terminal_edge_columns() {
    let positions = viewport::edge_scrub_positions(Rect::new(0, 0, 4, 3));
    assert_eq!(positions, vec![(0, 0), (3, 0), (0, 1), (3, 1), (0, 2), (3, 2)]);
}

#[test]
fn edge_scrub_positions_do_not_duplicate_single_column_terminal() {
    let positions = viewport::edge_scrub_positions(Rect::new(5, 7, 1, 2));
    assert_eq!(positions, vec![(5, 7), (5, 8)]);
}

#[test]
fn edge_scrub_area_respects_dynamic_bottom_protection() {
    let area = viewport::edge_scrub_area(Rect::new(0, 0, 100, 30), 13)
        .expect("room remains for message-area scrub");

    assert_eq!(area, Rect::new(0, 2, 100, 15));
    assert_eq!(area.y + area.height, 17, "protected rows begin at y=17");
}

#[test]
fn edge_scrub_area_disables_when_bottom_ui_consumes_terminal() {
    assert_eq!(viewport::edge_scrub_area(Rect::new(0, 0, 80, 12), 10), None);
}

#[test]
fn scrub_edge_columns_resets_previous_frame_model_so_blank_edges_are_redrawn() {
    let backend = TestBackend::new(4, 2);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let buf = frame.buffer_mut();
            buf.cell_mut((0, 0)).unwrap().set_symbol("L");
            buf.cell_mut((3, 0)).unwrap().set_symbol("R");
            buf.cell_mut((0, 1)).unwrap().set_symbol("l");
            buf.cell_mut((3, 1)).unwrap().set_symbol("r");
        })
        .unwrap();

    // Simulate ratatui's back buffer believing the next edge state is blank,
    // which would normally make the diff skip writing physical blanks.
    viewport::scrub_terminal_edges(&mut terminal, Style::default()).unwrap();

    let completed = terminal
        .draw(|frame| {
            frame.buffer_mut().reset();
            frame.buffer_mut().cell_mut((1, 0)).unwrap().set_symbol("x");
        })
        .unwrap();

    assert_eq!(ch(completed.buffer, 0, 0), " ");
    assert_eq!(ch(completed.buffer, 3, 0), " ");
    assert_eq!(ch(completed.buffer, 0, 1), " ");
    assert_eq!(ch(completed.buffer, 3, 1), " ");
}

#[test]
fn render_scrolled_lines_clears_left_and_right_edges_from_previous_frame() {
    let area = Rect::new(0, 0, 10, 3);
    let style = Style::default().bg(Color::Black);
    let mut buf = Buffer::filled(area, ratatui::buffer::Cell::new(" "));

    let previous = vec![
        Line::from(Span::raw("R")),
        Line::from(Span::raw("middle")),
        Line::from(Span::raw("last-edgeX")),
    ];
    viewport::render_scrolled_lines(&mut buf, area, &previous, style);
    assert_eq!(ch(&buf, 0, 0), "R");
    assert_eq!(ch(&buf, 9, 2), "X");

    let next = vec![
        Line::from(Span::raw("middle")),
        Line::from(Span::raw("last-edgeX")),
        Line::from(Span::raw("done")),
    ];
    viewport::render_scrolled_lines(&mut buf, area, &next, style);

    assert_eq!(ch(&buf, 0, 0), "m");
    assert_eq!(ch(&buf, 0, 1), "l");
    assert_eq!(ch(&buf, 0, 2), "d");
    assert_eq!(ch(&buf, 9, 2), " ", "right-edge glyphs from the previous frame must be cleared");
}
