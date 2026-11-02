//! Drawing: the state of the app, as it is now, onto the screen. The main loop calls
//! this only when something changed. What it costs should depend on the size of the
//! screen, as far as possible, and not on the number of devices or on how much a
//! device said.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{Device, Row, State};
use crate::{App, Job, Mode};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const KEY: Style = Style::new().fg(Color::Cyan);
const BAR: Color = Color::Indexed(236);
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const SPARK_WIDTH: usize = 12;
const NAME_WIDTH: usize = 16;
const KIND_WIDTH: usize = 9;

/// The columns a table can have, in order. One shows when any row has its number.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Percent,
    Celsius,
    Watts,
    Duration,
}

const COLUMNS: [(&str, &str, Format, usize); 7] = [
    ("cpu", "CPU", Format::Percent, 5),
    ("mem", "MEM", Format::Percent, 5),
    ("disk", "DISK", Format::Percent, 5),
    ("gpu", "GPU", Format::Percent, 5),
    ("temp", "TEMP", Format::Celsius, 5),
    ("watts", "POWER", Format::Watts, 7),
    ("uptime", "UP", Format::Duration, 6),
];

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    format!("{:<width$}", truncate(text, width))
}

pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn sparkline(history: &std::collections::VecDeque<f32>, width: usize) -> String {
    let shown: Vec<f32> = history.iter().rev().take(width).rev().copied().collect();
    let mut out = " ".repeat(width - shown.len());
    out.extend(shown.iter().map(|v| SPARK[((v / 100.0).clamp(0.0, 1.0) * 7.0).round() as usize]));
    out
}

fn level_colour(value: f64, warn: f64, bad: f64) -> Style {
    if value >= bad {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if value >= warn {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::Green)
    }
}

fn cell(format: Format, value: f64, width: usize) -> Span<'static> {
    let (text, style) = match format {
        Format::Percent => (format!("{value:.0}%"), level_colour(value, 70.0, 90.0)),
        Format::Celsius => (format!("{value:.0}°"), level_colour(value, 70.0, 85.0)),
        Format::Watts => (format!("{value:.0}W"), Style::new()),
        Format::Duration => (duration(value), DIM),
    };
    Span::styled(format!("{text:>width$} "), style)
}

fn state_mark(state: State) -> Span<'static> {
    match state {
        State::Up => Span::styled("●", Style::new().fg(Color::Green)),
        State::Idle => Span::styled("◐", Style::new().fg(Color::Yellow)),
        State::Down => Span::styled("○", DIM),
        State::Unknown => Span::styled("?", DIM),
    }
}

/// Which columns fit, given the devices on screen and the width.
fn columns_for(app: &App, rows: &[Row], width: usize) -> (Vec<usize>, bool) {
    let has = |metric: &str| {
        rows.iter().any(|row| match row {
            Row::Device(key) => app.model.get(key).is_some_and(|d| d.metrics.contains_key(metric)),
            Row::Header { .. } => false,
        })
    };
    let mut chosen: Vec<usize> = (0..COLUMNS.len()).filter(|&i| has(COLUMNS[i].0)).collect();
    let mut spark = chosen.iter().any(|&i| COLUMNS[i].0 == "cpu");
    let fixed = 4 + NAME_WIDTH + 1 + KIND_WIDTH + 1;
    let used = |chosen: &[usize], spark: bool| {
        fixed + chosen.iter().map(|&i| COLUMNS[i].3 + 1).sum::<usize>() + if spark { SPARK_WIDTH + 1 } else { 0 }
    };
    // Leave room for the note if we can; first the graph goes, then the columns.
    if used(&chosen, spark) + 12 > width {
        spark = false;
    }
    while used(&chosen, spark) + 4 > width && !chosen.is_empty() {
        chosen.pop();
    }
    (chosen, spark)
}

fn header_line(chosen: &[usize], spark: bool) -> Line<'static> {
    let mut text = format!("    {} {} ", pad("NAME", NAME_WIDTH), pad("KIND", KIND_WIDTH));
    for &i in chosen {
        let (metric, title, _, width) = COLUMNS[i];
        if metric == "cpu" && spark {
            text += &" ".repeat(SPARK_WIDTH + 1);
        }
        text += &format!("{title:>width$} ");
    }
    text += "NOTE";
    Line::from(Span::styled(text, DIM))
}

fn device_line(device: &Device, chosen: &[usize], spark: bool, width: usize) -> Line<'static> {
    let down = device.state == State::Down;
    let name_style = if down { DIM } else { BOLD };
    let mut spans = vec![
        Span::styled(if device.tagged { "✓ " } else { "  " }, Style::new().fg(Color::Yellow)),
        state_mark(device.state),
        Span::raw(" "),
        Span::styled(pad(&device.name, NAME_WIDTH), name_style),
        Span::raw(" "),
        Span::styled(pad(&device.kind, KIND_WIDTH), DIM),
        Span::raw(" "),
    ];
    let mut used = 4 + NAME_WIDTH + 1 + KIND_WIDTH + 1;
    for &i in chosen {
        let (metric, _, format, column) = COLUMNS[i];
        if metric == "cpu" && spark {
            spans.push(Span::styled(format!("{} ", sparkline(&device.cpu_history, SPARK_WIDTH)), Style::new().fg(Color::Blue)));
            used += SPARK_WIDTH + 1;
        }
        match device.metrics.get(metric) {
            Some(value) if !down => spans.push(cell(format, *value, column)),
            _ => spans.push(Span::raw(" ".repeat(column + 1))),
        }
        used += column + 1;
    }
    let room = width.saturating_sub(used);
    match device.attention() {
        Some(why) => spans.push(Span::styled(truncate(&format!("! {why}"), room), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))),
        None => spans.push(Span::styled(truncate(&device.note, room), DIM)),
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height < 8 || area.width < 40 {
        frame.render_widget(Paragraph::new("hypertop needs at least 40x8. q quits.").wrap(Wrap { trim: true }), area);
        return;
    }
    let title = Rect::new(area.x, area.y, area.width, 1);
    let footer = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let available = area.height - 2;
    let detail_height = if app.detail_open { (available * 2 / 5).clamp(5, 18) } else { 0 };
    let list = Rect::new(area.x, area.y + 1, area.width, available - detail_height);
    let detail = Rect::new(area.x, list.bottom(), area.width, detail_height);

    draw_title(frame, app, title);
    draw_list(frame, app, list);
    if app.detail_open {
        draw_detail(frame, app, detail);
    }
    draw_footer(frame, app, footer);
    match &app.mode {
        Mode::Menu { .. } => draw_menu(frame, app, area),
        Mode::Input { .. } => draw_input(frame, app, area),
        Mode::Confirm { .. } => draw_confirm(frame, app, area),
        Mode::Jobs { .. } => draw_jobs(frame, app, area),
        Mode::Help => draw_help(frame, area),
        Mode::Normal | Mode::Filter => {}
    }
}

fn draw_title(frame: &mut Frame, app: &App, area: Rect) {
    let (total, up, down, attention) = app.model.counts();
    let bar = Style::new().bg(BAR);
    let mut spans = vec![
        Span::styled(" hypertop ", bar.fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {total} devices  {up} up  {down} off"), bar),
    ];
    if attention > 0 {
        spans.push(Span::styled(format!("  ! {attention} need attention"), bar.fg(Color::Red).add_modifier(Modifier::BOLD)));
    }
    let tagged = app.model.devices.iter().filter(|d| d.tagged).count();
    if tagged > 0 {
        spans.push(Span::styled(format!("  ✓ {tagged} tagged"), bar.fg(Color::Yellow)));
    }
    if !app.filter.is_empty() || matches!(app.mode, Mode::Filter) {
        spans.push(Span::styled(format!("  /{}", app.filter), bar.fg(Color::Cyan)));
    }
    if app.hide_down {
        spans.push(Span::styled("  off hidden", bar.fg(Color::DarkGray)));
    }
    let running = app.jobs.iter().filter(|j| j.outcome.is_none()).count();
    if running > 0 {
        spans.push(Span::styled(format!("  {running} running"), bar.fg(Color::Yellow)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.height < 2 {
        return;
    }
    let rows = &app.rows;
    if rows.is_empty() {
        let text = if app.sources == 0 {
            format!("No sources yet. Put executables that print devices in {}", app.dir.display())
        } else if !app.filter.is_empty() {
            "Nothing matches the filter. Esc clears it.".to_string()
        } else {
            "Asking the sources…".to_string()
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).style(DIM), area);
        return;
    }
    let (chosen, spark) = columns_for(app, rows, area.width as usize);
    frame.render_widget(Paragraph::new(header_line(&chosen, spark)), Rect::new(area.x, area.y, area.width, 1));
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);

    // Keep the selection on screen, with its section header when there's room.
    let selected_row = app
        .selected
        .as_ref()
        .and_then(|s| rows.iter().position(|r| matches!(r, Row::Device(k) if k == s)))
        .unwrap_or(0);
    let height = body.height as usize;
    if selected_row < app.top {
        app.top = if selected_row > 0 && matches!(rows[selected_row - 1], Row::Header { .. }) { selected_row - 1 } else { selected_row };
    } else if selected_row >= app.top + height {
        app.top = selected_row + 1 - height;
    }
    app.top = app.top.min(rows.len().saturating_sub(height));

    let width = area.width as usize;
    let mut lines = Vec::with_capacity(height);
    for row in rows.iter().skip(app.top).take(height) {
        lines.push(match row {
            Row::Header { title, up, total } => {
                let head = format!(" {title} ");
                let tail = format!(" {up}/{total} up ");
                let rule = "─".repeat(width.saturating_sub(head.chars().count() + tail.chars().count() + 1));
                Line::from(vec![
                    Span::styled("─", DIM),
                    Span::styled(head, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(rule, DIM),
                    Span::styled(tail, DIM),
                ])
            }
            Row::Device(key) => match app.model.get(key) {
                Some(device) => device_line(device, &chosen, spark, width),
                None => Line::raw(""),
            },
        });
    }
    frame.render_widget(Paragraph::new(lines), body);
    if selected_row >= app.top && selected_row < app.top + height {
        let row = Rect::new(body.x, body.y + (selected_row - app.top) as u16, body.width, 1);
        frame.buffer_mut().set_style(row, Style::new().bg(Color::Indexed(238)));
    }
    app.list_area = body;
}

fn overview_lines(device: &Device) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(why) = device.attention() {
        lines.push(Line::from(Span::styled(format!("! {why}"), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))));
    }
    let state = match device.state {
        State::Up => "up",
        State::Idle => "idle",
        State::Down => "off or unreachable",
        State::Unknown => "unknown",
    };
    let mut head = vec![
        Span::styled("state ", DIM),
        Span::raw(format!("{state}, for {}  ", duration(device.since.elapsed().as_secs_f64()))),
    ];
    if !device.note.is_empty() {
        head.push(Span::raw(device.note.clone()));
    }
    lines.push(Line::from(head));
    for (key, value) in &device.info {
        lines.push(Line::from(vec![Span::styled(format!("{key} "), DIM), Span::raw(value.clone())]));
    }
    let known = |name: &String| COLUMNS.iter().any(|c| c.0 == name);
    let others: Vec<String> = device.metrics.iter().filter(|(k, _)| !known(k)).map(|(k, v)| format!("{k} {v:.1}")).collect();
    if !others.is_empty() {
        lines.push(Line::from(Span::raw(others.join("   "))));
    }
    if !device.actions.is_empty() {
        let names: Vec<String> = device.actions.iter().map(|a| a.label.clone()).collect();
        lines.push(Line::from(vec![Span::styled("actions ", DIM), Span::raw(names.join(", "))]));
    }
    lines
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(device) = app.selected.as_ref().and_then(|k| app.model.get(k)) else {
        frame.render_widget(Block::new().borders(Borders::TOP).border_style(DIM), area);
        return;
    };
    let mut tabs = vec![Span::styled(format!(" {} ", device.name), BOLD)];
    let mut labels = vec!["overview".to_string()];
    labels.extend(device.peeks.iter().map(|p| p.label.clone()));
    for (i, label) in labels.iter().enumerate() {
        let style = if i == app.tab { Style::new().fg(Color::Black).bg(Color::Cyan) } else { DIM };
        tabs.push(Span::styled(format!(" {label} "), style));
        tabs.push(Span::raw(" "));
    }
    let block = Block::new().borders(Borders::TOP).border_style(DIM).title(Line::from(tabs));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (scroll, height) = (app.detail_scroll as usize, inner.height as usize);
    let lines: Vec<Line> = if app.tab == 0 {
        overview_lines(device).into_iter().skip(scroll).take(height).collect()
    } else {
        let label = device.peeks.get(app.tab - 1).map(|p| p.label.clone()).unwrap_or_default();
        match app.peeks.get(&(device.key.clone(), label)) {
            Some(state) => state.text.lines().skip(scroll).take(height).map(|l| Line::raw(l.to_string())).collect(),
            _ => vec![Line::from(Span::styled("asking…", DIM))],
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if matches!(app.mode, Mode::Filter) {
        spans.push(Span::styled(" type to filter  ", Style::new().fg(Color::Yellow)));
        spans.push(Span::styled("Enter", KEY));
        spans.push(Span::raw(" keep  "));
        spans.push(Span::styled("Esc", KEY));
        spans.push(Span::raw(" clear"));
    } else if let Some((text, _)) = &app.notice {
        spans.push(Span::styled(format!(" {text} "), Style::new().fg(Color::Black).bg(Color::Yellow)));
    } else {
        for (key, what) in [
            ("↑↓", "move"),
            ("space", "tag"),
            ("t", "all"),
            ("Enter", "act"),
            ("!", "run"),
            ("s", "shell"),
            ("←→", "peek"),
            ("/", "filter"),
            ("o", "output"),
            ("?", "more"),
        ] {
            spans.push(Span::styled(format!(" {key}"), KEY));
            spans.push(Span::raw(format!(" {what} ")));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    // The way out is always on screen, whatever else the line says.
    let quit = vec![Span::styled("q", KEY), Span::raw(" or "), Span::styled("Ctrl+C", KEY), Span::raw(" quit ")];
    let width: usize = quit.iter().map(|s| s.content.chars().count()).sum();
    if area.width as usize > width {
        let rect = Rect::new(area.right() - width as u16, area.y, width as u16, 1);
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(Line::from(quit)), rect);
    }
}

fn popup(frame: &mut Frame, area: Rect, width: u16, height: u16, title: &str) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::new().borders(Borders::ALL).title(format!(" {title} "));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn draw_menu(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Menu { items, at, targets } = &app.mode else { return };
    let total = targets.len();
    let who = match targets.as_slice() {
        [key] => app.model.get(key).map_or(key.clone(), |d| d.name.clone()),
        _ => format!("{total} devices"),
    };
    let inner = popup(frame, area, 54, items.len() as u16 + 4, &format!("act on {who}"));
    let mut lines = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut label = item.label.clone();
        if total > 1 && item.count < total {
            label += &format!("  ({} of {total})", item.count);
        }
        if item.confirm {
            label += "  (asks first)";
        }
        if item.tty {
            label += "  (terminal)";
        }
        let style = if i == *at { Style::new().fg(Color::Black).bg(Color::Cyan) } else { Style::new() };
        lines.push(Line::from(vec![Span::styled(format!(" {} ", i + 1), KEY), Span::styled(format!(" {label} "), style)]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" 1-9", KEY),
        Span::raw(" or "),
        Span::styled("Enter", KEY),
        Span::raw(" choose  "),
        Span::styled("Esc", KEY),
        Span::raw(" back"),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Input { plan, text } = &app.mode else { return };
    let inner = popup(frame, area, 70, 6, &format!("{}, on {}", plan.prompt.as_deref().unwrap_or("Input"), plan.who()));
    let lines = vec![
        Line::from(vec![Span::styled("> ", KEY), Span::raw(text.clone()), Span::styled("█", KEY)]),
        Line::raw(""),
        Line::from(vec![Span::styled("Enter", KEY), Span::raw(" run  "), Span::styled("Esc", KEY), Span::raw(" back")]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Confirm { plan, input } = &app.mode else { return };
    let inner = popup(frame, area, 64, (plan.steps.len() as u16).min(10) + 6, "are you sure?");
    let what = match input {
        Some(text) => format!("{}: {text}", plan.label),
        None => plan.label.clone(),
    };
    let mut lines = vec![Line::from(Span::styled(what, BOLD)), Line::raw("on:")];
    for (name, _) in plan.steps.iter().take(8) {
        lines.push(Line::raw(format!("  {name}")));
    }
    if plan.steps.len() > 8 {
        lines.push(Line::raw(format!("  and {} more", plan.steps.len() - 8)));
    }
    lines.push(Line::raw(""));
    let key = if plan.steps.len() > 1 { "Y" } else { "y" };
    lines.push(Line::from(vec![
        Span::styled(key, Style::new().fg(Color::Black).bg(Color::Yellow)),
        Span::raw(" yes   "),
        Span::styled("any other key", KEY),
        Span::raw(" no"),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A job's lines in the output view: its header, then what it printed.
fn job_lines(job: &Job) -> Vec<Line<'static>> {
    let (mark, style) = match &job.outcome {
        None => ("…".to_string(), Style::new().fg(Color::Yellow)),
        Some(Ok(0)) => ("✓".to_string(), Style::new().fg(Color::Green)),
        Some(Ok(code)) => (format!("✗ exit {code}"), Style::new().fg(Color::Red)),
        Some(Err(why)) => (format!("✗ {why}"), Style::new().fg(Color::Red)),
    };
    let took = job.took.map_or(String::new(), |t| format!("  {:.1} s", t.as_secs_f32()));
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} ", job.device), BOLD),
        Span::styled(format!("{} ", job.label), DIM),
        Span::styled(mark, style),
        Span::styled(took, DIM),
    ])];
    lines.extend(job.output.lines().map(|l| Line::raw(format!("  {l}"))));
    lines
}

/// Where a job starts in the output view.
pub fn jobs_line_of(jobs: &[Job], job: usize) -> u16 {
    jobs.iter().take(job).map(|j| job_lines(j).len()).sum::<usize>().min(u16::MAX as usize) as u16
}

fn draw_jobs(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Jobs { scroll } = &app.mode else { return };
    let rect = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    frame.render_widget(Clear, rect);
    let done = app.jobs.iter().filter(|j| j.outcome.is_some()).count();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" output: {done} of {} done ", app.jobs.len()))
        .title_bottom(Line::from(vec![
            Span::styled(" ↑↓", KEY),
            Span::raw(" scroll  "),
            Span::styled("c", KEY),
            Span::raw(" clear  "),
            Span::styled("Esc", KEY),
            Span::raw(" back "),
        ]));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let lines: Vec<Line> = if app.jobs.is_empty() {
        vec![Line::from(Span::styled("Nothing has run yet.", DIM))]
    } else {
        app.jobs.iter().flat_map(job_lines).skip(*scroll as usize).take(inner.height as usize).collect()
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = [
        ("↑ ↓  j k", "move; PgUp PgDn Home End too"),
        ("space", "tag this device and move on"),
        ("t  T", "tag everything shown / clear all tags"),
        ("Enter x", "actions, for the tagged devices or this one"),
        ("!", "run a command on the tagged devices or this one"),
        ("s", "open a shell on this device"),
        ("← →  h l", "peek: overview, then what this device offers"),
        ("J K", "scroll the peek"),
        ("Tab", "show or hide the peek pane"),
        ("/", "filter by name, kind, group or note"),
        ("n", "next device that needs attention"),
        ("b", "group by section, kind, or nothing"),
        ("d", "hide or show what's off"),
        ("o", "output of what has run"),
        ("r", "ask every source again now"),
        ("q Ctrl+C", "quit"),
    ];
    let mut lines: Vec<Line> = text
        .iter()
        .map(|(key, what)| Line::from(vec![Span::styled(format!(" {key:<11}"), KEY), Span::raw(what.to_string())]))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(" A peek runs only while you look at it. Any key closes this.", DIM)));
    let inner = popup(frame, area, 66, lines.len() as u16 + 2, "keys");
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(duration(5.0), "5s");
        assert_eq!(duration(600.0), "10m");
        assert_eq!(duration(7200.0), "2h");
        assert_eq!(duration(300_000.0), "3d");
    }

    #[test]
    fn sparkline_fills_from_the_right_and_stays_in_range() {
        let history = VecDeque::from([0.0, 100.0, 500.0, -5.0]);
        assert_eq!(sparkline(&history, 6), "  ▁██▁");
        assert_eq!(sparkline(&VecDeque::new(), 3), "   ");
    }

    #[test]
    fn long_text_is_cut_to_width() {
        assert_eq!(truncate("a long name", 6), "a lon…");
        assert_eq!(pad("ab", 4), "ab  ");
    }
}
