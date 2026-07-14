//! Rendering: the two-pane layout (file list + diff) and the footer/help line.

use crate::app::{App, Mode};
use crate::git::ChangeKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(main);

    // Record the pane size so the (separately driven) build renders at the right
    // width. The heavy build runs in the event loop, not here, so the first
    // frame — the file list — paints immediately.
    app.diff_width = right.width.saturating_sub(2);
    app.diff_viewport_height = right.height.saturating_sub(2);

    draw_file_list(f, app, left);
    draw_diff(f, app, right);
    draw_footer(f, app, footer);
}

fn draw_file_list(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::bordered().title(format!(" changed files ({}) ", app.filtered.len()));

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&i| {
            let entry = &app.files[i];
            let (marker, color) = marker_style(entry.kind);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(color)),
                Span::raw(entry.path.clone()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !app.filtered.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    // Title shows the file at the top of the viewport and a scroll indicator, so
    // you always know where you are in the combined diff.
    let title = match app.current_entry() {
        Some(e) => format!(
            " {} {}   [{} / {}] ",
            e.kind.marker(),
            e.path,
            app.diff_scroll,
            app.combined.lines.len(),
        ),
        None => " diff ".to_string(),
    };
    let block = Block::bordered().title(title);

    // Empty states.
    if app.files.is_empty() {
        return render_placeholder(
            f,
            area,
            block,
            "✨ clean working tree — nothing changed since HEAD",
        );
    }
    if app.filtered.is_empty() {
        return render_placeholder(f, area, block, "no files match the current search");
    }
    if app.combined.lines.is_empty() {
        return render_placeholder(f, area, block, "rendering diffs…");
    }

    let paragraph = Paragraph::new(app.combined.clone())
        .block(block)
        .scroll((app.diff_scroll, 0));
    f.render_widget(paragraph, area);
}

fn render_placeholder(f: &mut Frame, area: ratatui::layout::Rect, block: Block, msg: &str) {
    let p = Paragraph::new(msg)
        .block(block)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let line = match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled("search: ", Style::default().fg(Color::Yellow)),
            Span::raw(app.search.clone()),
            Span::styled("▏", dim),
            Span::styled("   (Enter to accept · Esc to clear)", dim),
        ]),
        Mode::Normal => {
            if !app.status.is_empty() {
                Line::from(Span::styled(
                    app.status.clone(),
                    Style::default().fg(Color::Red),
                ))
            } else {
                let info = format!(
                    "diff: {}  ·  sort: {} {}",
                    app.base_label,
                    app.sort_field.label(),
                    if app.sort_desc { "▼" } else { "▲" },
                );
                Line::from(vec![
                    Span::styled(
                        "↑/↓ file  PgUp/PgDn scroll  s sort  r rev  t theme  b base  / search  q quit",
                        dim,
                    ),
                    Span::raw("   "),
                    Span::styled(info, Style::default().fg(Color::Cyan)),
                ])
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn marker_style(kind: ChangeKind) -> (char, Color) {
    match kind {
        ChangeKind::Modified => ('M', Color::Yellow),
        ChangeKind::Added => ('A', Color::Green),
        ChangeKind::Deleted => ('D', Color::Red),
        ChangeKind::Renamed => ('R', Color::Cyan),
        ChangeKind::Untracked => ('?', Color::Blue),
    }
}
