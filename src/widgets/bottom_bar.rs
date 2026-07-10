//! The bottom tab strip.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

/// One styled block per tab over a solid bar background; the selected tab is
/// highlighted. Render into a one-row area.
pub struct BottomBar<'a> {
    pub labels: Vec<&'a str>,
    pub selected: usize,
}

impl Widget for BottomBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::default().bg(Color::DarkGray));
        let mut spans: Vec<Span> = Vec::new();
        for (i, label) in self.labels.iter().enumerate() {
            let label = format!(" {label} ");
            if i == self.selected {
                spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    label,
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
            }
        }
        buf.set_line(area.x, area.y, &Line::from(spans), area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::testing::assert_rows;

    #[test]
    fn renders_labels_with_padding() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        BottomBar {
            labels: vec!["one", "two"],
            selected: 1,
        }
        .render(area, &mut buf);
        assert_rows(&buf, &[" one  two"]);
    }

    #[test]
    fn clips_to_area_width() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        BottomBar {
            labels: vec!["averylongname"],
            selected: 0,
        }
        .render(area, &mut buf);
        assert_rows(&buf, &[" avery"]);
    }
}
