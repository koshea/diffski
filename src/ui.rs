//! Rendering: the two-pane layout (file list + diff), scrollbar, footer, and the
//! help overlay. Also records pane rectangles into `App` for mouse hit-testing.

use crate::app::{App, Mode};
use crate::git::ChangeKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

/// Rounded, subtle panel borders.
fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [left, right] = Layout::horizontal([
        Constraint::Percentage(app.split_pct),
        Constraint::Percentage(100 - app.split_pct),
    ])
    .areas(main);

    // Record geometry for the next round of mouse hit-testing, and the diff pane
    // size so the (separately driven) build renders/wraps at the right width.
    app.area_main = main;
    app.area_left = left;
    app.area_right = right;
    app.diff_width = right.width.saturating_sub(2);
    app.diff_viewport_height = right.height.saturating_sub(2);

    draw_file_list(f, app, left);
    draw_diff(f, app, right);
    draw_footer(f, app, footer);

    if app.show_help {
        draw_help(f, main);
    }
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect) {
    // The file you're looking at is no longer "recently changed".
    if let Some(path) = app.current_entry().map(|e| e.path.clone()) {
        app.mark_viewed(&path);
    }

    let (mut total_add, mut total_del) = (0u32, 0u32);
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&i| {
            let entry = &app.files[i];
            let (marker, color) = marker_style(entry.kind);
            let mut spans = vec![
                Span::styled(format!("{marker} "), Style::default().fg(color)),
                Span::raw(entry.path.clone()),
            ];
            // Per-file +/- line counts.
            if let Some(a) = entry.added.filter(|&a| a > 0) {
                total_add += a;
                spans.push(Span::styled(
                    format!(" +{a}"),
                    Style::default().fg(Color::Green),
                ));
            }
            if let Some(d) = entry.removed.filter(|&d| d > 0) {
                total_del += d;
                spans.push(Span::styled(
                    format!(" -{d}"),
                    Style::default().fg(Color::Red),
                ));
            }
            // Cue for files changed on disk that you haven't looked at yet.
            if app.recently_changed.contains(&entry.path) {
                spans.push(Span::styled(" ●", Style::default().fg(Color::Cyan)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    // Changeset summary in the panel title: N files · +added -removed.
    let mut title = vec![Span::raw(format!(
        " changed files ({}) ",
        app.filtered.len()
    ))];
    if total_add > 0 {
        title.push(Span::styled(
            format!("+{total_add} "),
            Style::default().fg(Color::Green),
        ));
    }
    if total_del > 0 {
        title.push(Span::styled(
            format!("-{total_del} "),
            Style::default().fg(Color::Red),
        ));
    }

    let list = List::new(items)
        .block(panel(Line::from(title)))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▶ ");

    if app.filtered.is_empty() {
        app.list_state.select(None);
    } else {
        app.list_state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
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
    let block = panel(title);

    if app.files.is_empty() {
        return render_placeholder(f, area, block, "✨ clean working tree — nothing changed");
    }
    if app.filtered.is_empty() {
        return render_placeholder(f, area, block, "no files match the current search");
    }
    if app.combined.lines.is_empty() {
        return render_placeholder(f, area, block, "rendering diffs…");
    }

    // Render just the visible window, applying the selection highlight, so we
    // never clone the whole combined diff and can style selected cells.
    let inner = block.inner(area);
    let total = app.combined.lines.len();
    let start = (app.diff_scroll as usize).min(total.saturating_sub(1));
    let end = (start + inner.height as usize).min(total);
    let sel = app.selection.map(|s| s.ordered());

    let mut visible: Vec<Line> = Vec::with_capacity(end - start);
    for (li, line) in app.combined.lines[start..end].iter().enumerate() {
        let li = start + li;
        match sel.and_then(|s| selection_span(s, li, line_len(line))) {
            Some((cs, ce)) if ce > cs => visible.push(highlight_range(line, cs, ce)),
            _ => visible.push(line.clone()),
        }
    }

    f.render_widget(Paragraph::new(Text::from(visible)).block(block), area);

    // Scrollbar on the diff pane's right edge when there's more than one screen.
    if total > inner.height as usize {
        let mut sb = ScrollbarState::new(total.saturating_sub(inner.height as usize))
            .position(app.diff_scroll as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        f.render_stateful_widget(scrollbar, area.inner(Margin::new(0, 1)), &mut sb);
    }
}

fn render_placeholder(f: &mut Frame, area: Rect, block: Block, msg: &str) {
    let p = Paragraph::new(msg)
        .block(block)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let line = match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled("search: ", Style::default().fg(Color::Yellow)),
            Span::raw(app.search.clone()),
            Span::styled("▏", dim),
            Span::styled("   (Enter to accept · Esc to clear)", dim),
        ]),
        Mode::Normal if !app.status.is_empty() => Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Green),
        )),
        Mode::Normal => {
            let follow = if app.follow { "  ·  follow" } else { "" };
            let info = format!(
                "diff: {}  ·  sort: {} {}{}",
                app.base_label,
                app.sort_field.label(),
                if app.sort_desc { "▼" } else { "▲" },
                follow,
            );
            Line::from(vec![
                Span::styled(
                    "↑/↓ file  scroll  s sort  r rev  t theme  b base  f follow  ? help  q quit",
                    dim,
                ),
                Span::raw("   "),
                Span::styled(info, Style::default().fg(Color::Cyan)),
            ])
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("↑ / ↓  (j / k)", "jump to previous / next file"),
        ("PgUp / PgDn  (Space)", "scroll the combined diff"),
        ("Home / End  (g / G)", "jump to top / bottom"),
        ("mouse wheel", "scroll diff · over the list, change file"),
        ("click", "select a file / place the cursor"),
        ("drag in diff", "select text — copies on release"),
        ("y", "copy the current selection"),
        ("drag the divider", "resize the panes"),
        ("s / r", "sort field / reverse direction"),
        ("t / T", "cycle syntax theme forward / back"),
        ("b", "toggle working ⇄ base-branch diff"),
        ("f", "follow — jump to files as they change"),
        ("/", "search filenames"),
        ("q / Esc", "quit"),
    ];

    let mut lines = vec![Line::from("")];
    for (k, d) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), Style::default().fg(Color::Cyan)),
            Span::raw(*d),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default().fg(Color::DarkGray),
    )));

    let w = 62.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(panel(" help ").border_style(Style::default().fg(Color::Cyan))),
        rect,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn line_len(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// The `[start, end)` column range of `line` `li` that falls inside selection
/// `((l0,c0),(l1,c1))`, or `None` if the line isn't selected.
fn selection_span(
    sel: ((usize, usize), (usize, usize)),
    li: usize,
    len: usize,
) -> Option<(usize, usize)> {
    let ((l0, c0), (l1, c1)) = sel;
    if li < l0 || li > l1 {
        return None;
    }
    let cs = if li == l0 { c0 } else { 0 };
    let ce = if li == l1 { c1 } else { len };
    Some((cs.min(len), ce.min(len)))
}

/// Clone `line`, adding a reversed-video highlight to chars in `[cs, ce)`.
fn highlight_range(line: &Line<'static>, cs: usize, ce: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let chars: Vec<char> = span.content.chars().collect();
        let n = chars.len();
        let (s0, s1) = (col, col + n);
        let os = cs.max(s0);
        let oe = ce.min(s1);
        if oe <= os {
            out.push(span.clone());
        } else {
            let a = os - s0;
            let b = oe - s0;
            if a > 0 {
                out.push(Span::styled(
                    chars[..a].iter().collect::<String>(),
                    span.style,
                ));
            }
            out.push(Span::styled(
                chars[a..b].iter().collect::<String>(),
                span.style.add_modifier(Modifier::REVERSED),
            ));
            if b < n {
                out.push(Span::styled(
                    chars[b..].iter().collect::<String>(),
                    span.style,
                ));
            }
        }
        col = s1;
    }
    Line::from(out)
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
