//! Rendering: the two-pane layout (file list + diff), scrollbar, footer, and the
//! help overlay. Also records pane rectangles into `App` for mouse hit-testing.

use crate::app::{App, HistoryView, Hover, Mode};
use crate::git::{ChangeKind, KIND_ORDER};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let (left, divider, right) = if app.sidebar_collapsed {
        (Rect::default(), Rect::default(), main)
    } else {
        let [left, divider, right] = Layout::horizontal([
            Constraint::Percentage(app.split_pct),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(main);
        (left, divider, right)
    };

    // Record geometry for mouse hit-testing, and the diff pane size so the
    // (separately driven) build renders/wraps at the right width. Each pane's
    // first row is its header; the diff pane's last column is reserved for
    // the scrollbar rail.
    app.area_main = main;
    app.area_left = left;
    app.area_right = right;
    app.diff_width = right.width.saturating_sub(1);
    app.diff_viewport_height = right.height.saturating_sub(1);

    if app.sidebar_collapsed {
        app.active_path_overflow = false;
    } else {
        draw_file_list(f, app, left);
        draw_divider(f, divider);
    }
    draw_diff(f, app, right);
    draw_footer(f, app, footer);

    if app.show_help {
        draw_help(f, main);
    }
}

/// The thin vertical separator between the panes (also the drag target).
fn draw_divider(f: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let buf = f.buffer_mut();
    for y in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, y)) {
            cell.set_symbol("│");
            cell.set_style(dim);
        }
    }
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect) {
    // The file you're looking at is no longer "recently changed".
    if let Some(path) = app.current_entry().map(|e| e.path.clone()) {
        app.mark_viewed(&path);
    }
    // Restart the marquee when the selection changes.
    if app.selected != app.marquee_sel {
        app.marquee_sel = app.selected;
        app.marquee_reset();
    }

    // Width available for a row's content (minus the 2-col highlight symbol
    // that the List reserves on every row).
    let row_w = (area.width as usize).saturating_sub(2);
    let hover = app.hover_target();
    let offset = app.marquee_offset;

    let (mut total_add, mut total_del) = (0u32, 0u32);
    let mut active_overflow = false;
    let mut items: Vec<ListItem> = Vec::with_capacity(app.filtered.len());

    // Content-search hits per visible file (indexes parallel `filtered`).
    let mut match_counts = vec![0usize; app.filtered.len()];
    for m in &app.matches {
        if let Some(c) = match_counts.get_mut(m.file_pos) {
            *c += 1;
        }
    }

    for (row, &i) in app.filtered.iter().enumerate() {
        let entry = &app.files[i];
        let (marker, color) = marker_style(entry.kind);

        // Right-hand side: +/- stats and the recently-changed cue. Measure their
        // width so the (truncated) path takes exactly the remaining space.
        let mut tail_spans: Vec<Span> = Vec::new();
        let mut tail_w = 0usize;
        if let Some(a) = entry.added.filter(|&a| a > 0) {
            total_add += a;
            let s = format!(" +{a}");
            tail_w += s.chars().count();
            tail_spans.push(Span::styled(s, Style::default().fg(Color::Green)));
        }
        if let Some(d) = entry.removed.filter(|&d| d > 0) {
            total_del += d;
            let s = format!(" -{d}");
            tail_w += s.chars().count();
            tail_spans.push(Span::styled(s, Style::default().fg(Color::Red)));
        }
        if app.recently_changed.contains(&entry.path) {
            tail_w += 2; // " ●"
            tail_spans.push(Span::styled(" ●", Style::default().fg(Color::Cyan)));
        }
        if let Some(&n) = match_counts.get(row).filter(|&&n| n > 0) {
            let s = format!(" ×{n}");
            tail_w += s.chars().count();
            tail_spans.push(Span::styled(s, Style::default().fg(Color::Yellow)));
        }

        let path_avail = row_w.saturating_sub(2 + tail_w); // 2 = "M " marker
        let full_len = entry.path.chars().count();
        let path = if row == app.selected && full_len > path_avail && path_avail > 4 {
            // Marquee the selected file's overflowing name so it can be read.
            active_overflow = true;
            marquee(&entry.path, path_avail, offset)
        } else {
            truncate_middle(&entry.path, path_avail)
        };

        let mut spans = vec![
            Span::styled(format!("{marker} "), Style::default().fg(color)),
            Span::raw(path),
        ];
        spans.extend(tail_spans);
        let line_style = maybe_underline(Style::default(), hover == Some(Hover::FileRow(row)));
        items.push(ListItem::new(Line::from(spans).style(line_style)));
    }
    app.active_path_overflow = active_overflow;

    // Flat header: CHANGES (N) + optional stats on the left; sort field +
    // direction on the right (never dropped — the stats give way first if
    // both can't fit).
    if area.height == 0 {
        return;
    }
    let left_base = format!(" CHANGES ({})", app.filtered.len());
    let right = app.sort_field.indicator(app.sort_desc);

    // The pane's true last column doubles as part of the divider's ±1 mouse
    // grab tolerance (see `App::near_divider`), so a click there can never
    // reach this row — reserve it rather than rendering anything unclickable.
    let avail = area.width.saturating_sub(1) as usize;
    let right_w = right.chars().count();
    let base_w = left_base.chars().count();
    let stats_w = (if total_add > 0 {
        format!(" +{total_add}").chars().count()
    } else {
        0
    }) + (if total_del > 0 {
        format!(" -{total_del}").chars().count()
    } else {
        0
    });
    let show_stats = base_w + stats_w + right_w <= avail;
    let left_w = base_w + if show_stats { stats_w } else { 0 };
    let pad = avail.saturating_sub(left_w + right_w);

    let mut header = vec![Span::styled(
        left_base,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )];
    if show_stats {
        if total_add > 0 {
            header.push(Span::styled(
                format!(" +{total_add}"),
                Style::default().fg(Color::Green),
            ));
        }
        if total_del > 0 {
            header.push(Span::styled(
                format!(" -{total_del}"),
                Style::default().fg(Color::Red),
            ));
        }
    }
    header.push(Span::raw(" ".repeat(pad)));
    let dim = Style::default().fg(Color::DarkGray);
    header.push(Span::styled(
        app.sort_field.label(),
        maybe_underline(dim, hover == Some(Hover::SortLabel)),
    ));
    header.push(Span::styled(" ", dim));
    let arrow = if app.sort_desc { "▼" } else { "▲" };
    header.push(Span::styled(
        arrow,
        maybe_underline(dim, hover == Some(Hover::SortArrow)),
    ));

    f.render_widget(
        Paragraph::new(Line::from(header)),
        Rect { height: 1, ..area },
    );

    // Row 1: kind chips — always visible and clickable, mirrors the `c`-mode
    // pills so click and keyboard toggling stay in sync.
    let mut kind_chips = vec![Span::raw(" ")];
    for kind in KIND_ORDER {
        let (marker, color) = marker_style(kind);
        let style = if app.kind_filter.contains(&kind) {
            Style::default()
                .fg(color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let style = maybe_underline(style, hover == Some(Hover::KindChip(kind)));
        kind_chips.push(Span::styled(format!("[{marker}]"), style));
    }
    f.render_widget(
        Paragraph::new(Line::from(kind_chips)),
        Rect {
            y: area.y + 1,
            height: 1,
            ..area
        },
    );

    // Row 2: file-type chips — dynamic per changeset. Extensions come from
    // files already narrowed by every other filter (never by this one), so
    // deselecting a type never hides its own chip. Overflow simply clips at
    // the pane edge.
    let mut type_chips = vec![Span::raw(" ")];
    for ext in &app.file_types {
        let style = if app.file_type_filter.contains(ext) {
            Style::default()
                .fg(chip_color(ext))
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let style = maybe_underline(style, hover == Some(Hover::FileType(ext.clone())));
        type_chips.push(Span::styled(format!("[.{ext}]"), style));
        type_chips.push(Span::raw(" "));
    }
    f.render_widget(
        Paragraph::new(Line::from(type_chips)),
        Rect {
            y: area.y + 2,
            height: 1,
            ..area
        },
    );

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▶ ");

    if app.filtered.is_empty() {
        app.list_state.select(None);
    } else {
        app.list_state.select(Some(app.selected));
    }
    let list_area = Rect {
        x: area.x,
        y: area.y + 3,
        width: area.width,
        height: area.height.saturating_sub(3),
    };
    f.render_stateful_widget(list, list_area, &mut app.list_state);
}

/// Header while browsing history: marker, historical path, short hash,
/// subject, optional red `pre-branch` pill, then commit position and scroll
/// position (mirroring the live header's `[line/total]`).
fn history_header(h: &HistoryView, width: u16) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let entry = &h.entries[h.pos];
    let suffix = format!(
        " ({}/{}) [{}/{}]",
        h.pos + 1,
        h.entries.len(),
        h.scroll,
        h.text.lines.len()
    );
    let (path_avail, hash_shown, subject_avail) = history_header_widths(
        width.saturating_sub(1) as usize,
        entry.path_at_commit.chars().count(),
        entry.short_hash.chars().count(),
        suffix.chars().count(),
        entry.pre_branch,
    );
    let mut spans = vec![
        Span::styled(" ⟲ ", Style::default().fg(Color::Cyan)),
        Span::styled(truncate_middle(&entry.path_at_commit, path_avail), dim),
    ];
    if hash_shown {
        spans.push(Span::styled(
            format!(" · {}", entry.short_hash),
            Style::default().fg(Color::Cyan),
        ));
    }
    if subject_avail > 0 {
        spans.push(Span::styled(
            format!(" {}", truncate_end(&entry.subject, subject_avail)),
            Style::default().fg(Color::Gray),
        ));
    }
    if entry.pre_branch {
        spans.push(Span::styled(
            " pre-branch ",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::REVERSED),
        ));
    }
    spans.push(Span::styled(suffix, dim));
    Line::from(spans)
}

fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    // Flat header: change marker, path, and scroll position.
    let dim = Style::default().fg(Color::DarkGray);
    let header = match &app.history {
        Some(h) => history_header(h, area.width),
        None => match app.current_entry() {
            Some(e) => {
                let (marker, color) = marker_style(e.kind);
                let position = window_position(app.diff_scroll as usize, app.view_window());
                let behind = app
                    .behind
                    .filter(|&n| n > 0)
                    .map(|n| format!(" ↓{n}"))
                    .unwrap_or_default();

                // Columns left for the path + base label, after the " M " marker
                // prefix and the never-truncated behind/position suffix (and the
                // rail column, already excluded from `area.width` — see `draw()`).
                let fixed_w = 3 + behind.chars().count() + position.chars().count();
                let total = (area.width.saturating_sub(1) as usize).saturating_sub(fixed_w);
                let (path_avail, base_avail) =
                    split_diff_header_width(total, app.base_label.chars().count());
                let base_label = truncate_middle(&app.base_label, base_avail);

                let mut spans = vec![
                    Span::styled(format!(" {marker} "), Style::default().fg(color)),
                    Span::styled(truncate_middle(&e.path, path_avail), dim),
                ];
                if !base_label.is_empty() {
                    spans.push(Span::styled(format!("   {base_label}"), dim));
                }
                spans.push(Span::styled(behind, Style::default().fg(Color::Yellow)));
                spans.push(Span::styled(position, dim));
                Line::from(spans)
            }
            None => Line::from(Span::styled(" DIFF", dim)),
        },
    };
    f.render_widget(Paragraph::new(header), Rect { height: 1, ..area });

    // Content area: below the header, left of the reserved rail column.
    let inner = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width.saturating_sub(1),
        height: area.height.saturating_sub(1),
    };

    if app.history.is_none() {
        if app.files.is_empty() {
            return render_placeholder(f, inner, "✨ clean working tree — nothing changed");
        }
        if app.filtered.is_empty() {
            return render_placeholder(f, inner, "no files match the current filters");
        }
        if app.combined.lines.is_empty() {
            return render_placeholder(f, inner, "rendering diffs…");
        }
    }

    // Render just the visible window, applying the selection highlight, so we
    // never clone the whole combined diff and can style selected cells.
    let (text, scroll) = match &app.history {
        Some(h) => (&h.text, h.scroll),
        None => (&app.combined, app.diff_scroll),
    };
    let (win_start, win_end) = match &app.history {
        Some(h) => (0, h.text.lines.len()),
        None => app.view_window(),
    };
    let win_len = win_end - win_start;
    let start = (scroll as usize)
        .max(win_start)
        .min(win_end.saturating_sub(1));
    let end = (start + inner.height as usize).min(win_end);
    let sel = app.selection.map(|s| s.ordered());

    // Content-search hits inside the visible window: (row, cols, is_current).
    // Inert while browsing history — matches index the live combined diff.
    let visible_matches: Vec<(usize, (usize, usize), bool)> = if app.history.is_some() {
        Vec::new()
    } else {
        app.matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.aligned && m.row >= start && m.row < end && m.cols.1 > m.cols.0)
            .map(|(i, m)| (m.row, m.cols, i == app.current_match))
            .collect()
    };

    let mut visible: Vec<Line> = Vec::with_capacity(end - start);
    for (li, line) in text.lines[start..end].iter().enumerate() {
        let li = start + li;
        let mut styled = line.clone();
        for &(row, (cs, ce), is_current) in &visible_matches {
            if row != li {
                continue;
            }
            styled = if is_current {
                restyle_range(&styled, cs, ce, |s| {
                    s.add_modifier(Modifier::REVERSED | Modifier::BOLD)
                })
            } else {
                restyle_range(&styled, cs, ce, |s| s.bg(Color::Yellow).fg(Color::Black))
            };
        }
        // Selection highlight applies on top of match highlights.
        match sel.and_then(|s| selection_span(s, li, line_len(&styled))) {
            Some((cs, ce)) if ce > cs => visible.push(restyle_range(&styled, cs, ce, |s| {
                s.add_modifier(Modifier::REVERSED)
            })),
            _ => visible.push(styled),
        }
    }

    f.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Scrollbar in the reserved rail column when there's more than one screen.
    if win_len > inner.height as usize {
        let rail = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: inner.y,
            width: 1,
            height: inner.height,
        };
        let mut sb = ScrollbarState::new(win_len.saturating_sub(inner.height as usize))
            .position((scroll as usize).saturating_sub(win_start));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        f.render_stateful_widget(scrollbar, rail, &mut sb);

        // Overlay content-search match markers on the rail: yellow marks per
        // match, the current match drawn last (cyan) so it wins its cell.
        if app.history.is_none()
            && app.is_content_search()
            && !app.matches.is_empty()
            && rail.height > 0
        {
            let buf = f.buffer_mut();
            for (i, m) in app.matches.iter().enumerate() {
                if !m.aligned || i == app.current_match || m.row < win_start || m.row >= win_end {
                    continue;
                }
                let y = rail.y + rail_y(m.row - win_start, win_len, rail.height);
                if let Some(cell) = buf.cell_mut((rail.x, y)) {
                    cell.set_symbol("▪");
                    cell.set_style(Style::default().fg(Color::Yellow));
                }
            }
            if let Some(m) = app
                .matches
                .get(app.current_match)
                .filter(|m| m.aligned && m.row >= win_start && m.row < win_end)
            {
                let y = rail.y + rail_y(m.row - win_start, win_len, rail.height);
                if let Some(cell) = buf.cell_mut((rail.x, y)) {
                    cell.set_symbol("█");
                    cell.set_style(Style::default().fg(Color::Cyan));
                }
            }
        }
    }
}

fn render_placeholder(f: &mut Frame, area: Rect, msg: &str) {
    f.render_widget(
        Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let line = match app.mode {
        Mode::Search => {
            let mut spans = vec![
                Span::styled(
                    format!("search[{}]: ", app.search_scope.glyph()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(app.search.clone()),
                Span::styled("▏", dim),
            ];
            if app.is_content_search() {
                let n = app.matches.len();
                spans.push(Span::styled(
                    format!(" — {n} match{}", if n == 1 { "" } else { "es" }),
                    Style::default().fg(Color::Yellow),
                ));
            }
            spans.push(Span::styled(
                "   (Tab scope · file: names · Enter accept · Esc clear)",
                dim,
            ));
            Line::from(spans)
        }
        Mode::Normal if !app.status.is_empty() => {
            let mut spans = vec![Span::styled(
                app.status.clone(),
                Style::default().fg(Color::Green),
            )];
            if app.update_ready {
                spans.push(Span::styled(
                    "   ⟳ restart to apply",
                    Style::default().fg(Color::Magenta),
                ));
            }
            Line::from(spans)
        }
        Mode::KindFilter => {
            let mut spans = vec![Span::styled("kinds: ", Style::default().fg(Color::Yellow))];
            for kind in KIND_ORDER {
                let (marker, color) = marker_style(kind);
                let style = if app.kind_filter.contains(&kind) {
                    Style::default()
                        .fg(color)
                        .add_modifier(Modifier::REVERSED | Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!("[{marker}]"), style));
            }
            spans.push(Span::styled("   (a/m/d/r/u toggle · Enter/Esc done)", dim));
            Line::from(spans)
        }
        Mode::Normal => {
            let mut extra = String::new();
            if app.history.is_some() {
                extra.push_str(" · history (←/→ walk · Esc live)");
            }
            if app.follow {
                extra.push_str(" · follow");
            }
            if !app.kind_filter.is_empty() {
                extra.push_str(" · kinds: ");
                for kind in KIND_ORDER {
                    if app.kind_filter.contains(&kind) {
                        extra.push(kind.marker());
                    }
                }
            }
            if app.history.is_none() && app.is_content_search() {
                if app.matches.is_empty() {
                    extra.push_str(" · match 0/0");
                } else {
                    extra.push_str(&format!(
                        " · match {}/{}",
                        app.current_match + 1,
                        app.matches.len()
                    ));
                }
            }
            let mut spans = vec![Span::styled(
                "c kinds · b base · / search · Tab collapse · ? help · q quit",
                dim,
            )];
            if !extra.is_empty() {
                spans.push(Span::styled(extra, Style::default().fg(Color::Cyan)));
            }
            if app.update_ready {
                spans.push(Span::styled(
                    " · ⟳ restart to apply",
                    Style::default().fg(Color::Magenta),
                ));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("↑ / ↓  (j / k)", "jump to previous / next file"),
        ("PgUp / PgDn  (Space)", "scroll the combined diff"),
        ("Home / End  (g / G)", "jump to top / bottom"),
        ("← / →", "walk the selected file's commit history"),
        ("mouse wheel", "scroll diff · over the list, change file"),
        ("click", "select a file / place the cursor"),
        ("drag in diff", "select text — copies on release"),
        ("y", "copy the current selection"),
        ("drag the divider", "resize the panes"),
        ("s / r", "sort field / reverse direction"),
        ("click field / ▲▼", "cycle sort field / reverse direction"),
        ("t / T", "cycle syntax theme forward / back"),
        ("b", "toggle working ⇄ base-branch diff"),
        ("c", "filter by change type (a/m/d/r/u)"),
        ("click a chip", "toggle a kind or file-type filter"),
        ("Tab", "collapse / expand the file list"),
        ("f", "follow — focus each file as it changes"),
        ("/", "search inside changes · file: filters names"),
        ("n / N", "next / previous match (↓/↑ while typing)"),
        ("q / Esc", "quit · Esc first exits history"),
    ];

    let mut lines: Vec<Line> = Vec::new();
    for (k, d) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), Style::default().fg(Color::Cyan)),
            Span::raw(*d),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default().fg(Color::DarkGray),
    )));

    let w = 62.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Plain)
                .title(" help ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
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

/// Clone `line`, restyling chars in `[cs, ce)` via `restyle`.
fn restyle_range(
    line: &Line<'static>,
    cs: usize,
    ce: usize,
    restyle: impl Fn(Style) -> Style,
) -> Line<'static> {
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
                restyle(span.style),
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

/// Shorten `s` to `max` columns with a middle ellipsis, keeping more of the tail
/// (the filename) than the head, since paths tend to share a long prefix.
fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    if max == 0 {
        return String::new();
    }
    if n <= max {
        return s.to_string();
    }
    if max <= 3 {
        // Too tight for an ellipsis; show the tail (the distinguishing end).
        return chars[n - max..].iter().collect();
    }
    let keep = max - 1; // room for the ellipsis
    let head = keep / 3;
    let tail = keep - head;
    let mut out = String::with_capacity(max);
    out.extend(&chars[..head]);
    out.push('…');
    out.extend(&chars[n - tail..]);
    out
}

/// Scroll-position suffix for the diff header, relative to the visible
/// window (the whole diff, or the focused file's section in follow mode).
fn window_position(scroll: usize, win: (usize, usize)) -> String {
    format!(
        " [{}/{}]",
        scroll.saturating_sub(win.0),
        win.1.saturating_sub(win.0)
    )
}

/// A horizontally scrolling window of `s` (with a separator), `width` columns
/// wide, advanced by `offset`. Used for the selected file's overflowing name.
fn marquee(s: &str, width: usize, offset: usize) -> String {
    let mut ring: Vec<char> = s.chars().collect();
    if ring.len() <= width {
        return s.to_string();
    }
    ring.extend("   ·   ".chars());
    let len = ring.len();
    // Hold at the start for a beat so the beginning is readable before scrolling.
    const PAUSE: usize = 8;
    let start = offset.saturating_sub(PAUSE) % len;
    (0..width).map(|k| ring[(start + k) % len]).collect()
}

/// A stable color for a file extension: a small curated map for common
/// languages, else a deterministic hash into the same small palette, so any
/// extension still gets a consistent color across a session (and restarts,
/// since the hash is pure) even without a curated entry.
fn chip_color(ext: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Blue,
        Color::Yellow,
        Color::Green,
        Color::Red,
        Color::Cyan,
        Color::Magenta,
    ];
    const CURATED: &[(&str, Color)] = &[
        ("ts", Color::Blue),
        ("tsx", Color::Blue),
        ("js", Color::Yellow),
        ("jsx", Color::Yellow),
        ("py", Color::Green),
        ("rs", Color::Red),
        ("md", Color::Gray),
        ("json", Color::Cyan),
        ("yaml", Color::Cyan),
        ("yml", Color::Cyan),
        ("toml", Color::Cyan),
        ("css", Color::Magenta),
        ("scss", Color::Magenta),
        ("html", Color::Magenta),
    ];
    if let Some(&(_, c)) = CURATED.iter().find(|&&(e, _)| e == ext) {
        return c;
    }
    let hash = ext
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[hash as usize % PALETTE.len()]
}

/// Adds `Modifier::UNDERLINED` on top of `style` when `hovered` — the
/// uniform "this is clickable and the mouse is over it" treatment shared by
/// chips, the sort indicator, and file-list rows. Never changes colors or
/// removes any modifier already present.
fn maybe_underline(style: Style, hovered: bool) -> Style {
    if hovered {
        style.add_modifier(Modifier::UNDERLINED)
    } else {
        style
    }
}

fn marker_style(kind: ChangeKind) -> (char, Color) {
    match kind {
        ChangeKind::Modified => ('M', Color::Yellow),
        ChangeKind::Added => ('A', Color::Green),
        ChangeKind::Deleted => ('D', Color::Red),
        ChangeKind::Renamed => ('R', Color::Cyan),
        ChangeKind::Untracked => ('U', Color::Blue),
    }
}

/// Proportional position of combined-diff `row` on a scrollbar rail of
/// `rail_height` cells over `total` rows: first row → first cell, last row →
/// last cell, monotonic in between.
fn rail_y(row: usize, total: usize, rail_height: u16) -> u16 {
    if rail_height == 0 {
        return 0;
    }
    let denom = total.saturating_sub(1).max(1);
    (row.min(total.saturating_sub(1)) * (rail_height as usize - 1) / denom) as u16
}

/// Split the diff header's available columns (`total`, after the marker
/// prefix and the never-truncated behind/position suffix are already
/// accounted for) between the path and the base label. The path keeps at
/// least `PATH_MIN` columns; only once that floor is hit does the base
/// label itself start shrinking (via `truncate_middle`). Returns
/// `(path_avail, base_avail)`.
fn split_diff_header_width(total: usize, base_label_len: usize) -> (usize, usize) {
    const GAP: usize = 3;
    const PATH_MIN: usize = 12;
    let base_full_w = GAP + base_label_len;
    if total >= PATH_MIN + base_full_w {
        (total - base_full_w, base_label_len)
    } else {
        let base_avail = total.saturating_sub(PATH_MIN + GAP);
        (PATH_MIN.min(total), base_avail)
    }
}

/// Split the history header's width between the path, the short hash, and
/// the subject. Fixed parts (the 3-col marker, the pos/scroll suffix, the
/// pill when shown) always render; then the path claims what it needs, the
/// subject keeps at least some room, and the hash is the first thing dropped
/// under pressure.
fn history_header_widths(
    total: usize,
    path_len: usize,
    hash_len: usize,
    suffix_len: usize,
    pill: bool,
) -> (usize, bool, usize) {
    // " pre-branch " is 12 cols; the marker " ⟲ " is 3.
    let fixed = 3 + suffix_len + if pill { 12 } else { 0 };
    let avail = total.saturating_sub(fixed);
    let path_avail = path_len.min(avail);
    let rest = avail - path_avail;
    let hash_cost = 3 + hash_len; // " · " + hash
    // Keep the hash only while the subject still gets ≥10 readable chars.
    let hash_shown = rest >= hash_cost + 11;
    let subject_avail = if hash_shown {
        rest - hash_cost - 1
    } else {
        rest.saturating_sub(1)
    };
    (path_avail, hash_shown, subject_avail)
}

/// Truncate to `avail` chars, ending with `…` when something was cut.
fn truncate_end(s: &str, avail: usize) -> String {
    if s.chars().count() <= avail {
        return s.to_string();
    }
    if avail == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(avail - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Color, Modifier, Style, chip_color, history_header_widths, maybe_underline, rail_y,
        split_diff_header_width, truncate_end, window_position,
    };

    #[test]
    fn history_header_widths_gives_path_priority_then_hash_then_subject() {
        // Wide: everything fits. total=80, path=10, hash=7, suffix=14.
        let (path, hash, subject) = history_header_widths(80, 10, 7, 14, false);
        assert_eq!(path, 10);
        assert!(hash);
        // 80 - 3 (marker) - 14 (suffix) - 10 (path) - 10 (" · " + hash) - 1 = 42
        assert_eq!(subject, 42);
    }

    #[test]
    fn history_header_widths_drops_the_hash_before_the_subject_when_tight() {
        // Just enough for the path and a sliver of subject — hash must go.
        let (path, hash, subject) = history_header_widths(40, 20, 7, 14, false);
        assert_eq!(path, 20);
        assert!(!hash, "hash is dropped first under width pressure");
        assert_eq!(subject, 2); // 40 - 3 - 14 - 20 - 1
    }

    #[test]
    fn history_header_widths_truncates_the_path_last_and_never_underflows() {
        let (path, hash, subject) = history_header_widths(20, 60, 7, 14, true);
        // fixed = 3 + 14 + 12 (pill) = 29 > 20 → nothing left; must not panic.
        assert_eq!((path, hash, subject), (0, false, 0));
    }

    #[test]
    fn truncate_end_appends_an_ellipsis_only_when_cutting() {
        assert_eq!(truncate_end("short", 10), "short");
        assert_eq!(truncate_end("exactly-10", 10), "exactly-10");
        assert_eq!(truncate_end("much too long", 8), "much to…");
        assert_eq!(truncate_end("anything", 0), "");
    }

    #[test]
    fn rail_y_maps_ends_and_stays_monotonic() {
        assert_eq!(rail_y(0, 100, 10), 0);
        assert_eq!(rail_y(99, 100, 10), 9);
        let mut last = 0;
        for row in 0..100 {
            let y = rail_y(row, 100, 10);
            assert!(y >= last && y <= 9);
            last = y;
        }
    }

    #[test]
    fn rail_y_degenerate_inputs() {
        assert_eq!(rail_y(0, 1, 10), 0);
        assert_eq!(rail_y(5, 1, 10), 0); // row clamped to total
        assert_eq!(rail_y(50, 100, 1), 0); // one-cell rail
        assert_eq!(rail_y(7, 0, 10), 0); // empty content
    }

    #[test]
    fn split_diff_header_width_gives_base_label_full_room_when_wide() {
        // Plenty of space: base label shown in full, path gets the rest.
        let (path_avail, base_avail) = split_diff_header_width(100, 4); // "main"
        assert_eq!(base_avail, 4); // untruncated
        assert_eq!(path_avail, 100 - (3 + 4)); // total - (gap + base label)
    }

    #[test]
    fn split_diff_header_width_truncates_base_label_before_shrinking_path_below_floor() {
        // Long base label, moderate width: path holds its floor (12), base
        // label truncates to fill the rest.
        let (path_avail, base_avail) = split_diff_header_width(30, 20);
        assert_eq!(path_avail, 12);
        assert_eq!(base_avail, 15); // 30 - (12 + 3)
    }

    #[test]
    fn split_diff_header_width_degenerates_gracefully_when_extremely_narrow() {
        // Not even room for the gap + a shrunk base label: base label is
        // fully hidden (avail 0), path takes whatever's left up to its floor.
        let (path_avail, base_avail) = split_diff_header_width(10, 20);
        assert_eq!(path_avail, 10); // floor (12) clamped to total (10)
        assert_eq!(base_avail, 0); // 10 - 15 saturates to 0
    }

    #[test]
    fn window_position_counts_within_the_window_and_matches_the_old_format_for_full_range() {
        // Follow off: the window is the whole diff, so the output is
        // byte-identical to the old " [scroll/total]" format.
        assert_eq!(window_position(12, (0, 96)), " [12/96]");
        // Focus: line 17 of a file spanning 10..25 renders as 7 of 15.
        assert_eq!(window_position(17, (10, 25)), " [7/15]");
        // A stale scroll above the window floors at 0 rather than wrapping.
        assert_eq!(window_position(3, (10, 25)), " [0/15]");
    }

    #[test]
    fn maybe_underline_adds_the_modifier_only_when_hovered() {
        let base = Style::default().fg(Color::Green);
        assert_eq!(maybe_underline(base, false), base);
        assert_eq!(
            maybe_underline(base, true),
            base.add_modifier(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn chip_color_curated_extensions_are_fixed() {
        assert_eq!(chip_color("ts"), Color::Blue);
        assert_eq!(chip_color("tsx"), Color::Blue);
        assert_eq!(chip_color("rs"), Color::Red);
        assert_eq!(chip_color("md"), Color::Gray);
    }

    #[test]
    fn chip_color_uncovered_extension_is_deterministic_and_in_palette() {
        let palette = [
            Color::Blue,
            Color::Yellow,
            Color::Green,
            Color::Red,
            Color::Cyan,
            Color::Magenta,
        ];
        let c1 = chip_color("zig");
        let c2 = chip_color("zig");
        assert_eq!(c1, c2); // same input -> same output
        assert!(palette.contains(&c1));
    }
}
