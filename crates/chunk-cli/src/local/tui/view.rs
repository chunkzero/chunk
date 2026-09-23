use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs},
};

use super::{
    Info,
    model::{Focus, Input, Model, Progress, STARTUP, Tab},
};
use crate::local::report::{self, Source, Step};

mod nodes;
mod players;

pub(super) fn render(frame: &mut Frame, model: &Model, info: &Info) {
    let [header, body, help] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    render_header(frame, model, info, header);
    if model.ready() {
        render_running(frame, model, info, body);
    } else {
        let height = 10.min(body.height.saturating_sub(4));
        let [steps, output] = Layout::vertical([Constraint::Length(height), Constraint::Min(1)]).areas(body);
        render_steps(frame, model, steps);
        render_output(frame, model, output, &format!("{} output", model.tab().name()));
    }
    let keys = match (model.ready(), model.tab(), model.focus) {
        (true, Tab::Players, _) => match model.input {
            Some(Input::Search) => "type to filter · ↑/↓ players · Enter keep · Esc clear",
            Some(Input::Move(_)) => "↑/↓ session type · type a key · Enter move · Esc cancel",
            None => "↑/↓ players · / search · m move · ←/→ tabs · r restart · q quit",
        },
        (true, Tab::Log(Source::Jvm), Focus::Nodes) => {
            "↑/↓ nodes · Enter logs · ←/→ tabs · b details · r restart · q quit"
        }
        (true, Tab::Log(Source::Jvm), Focus::Logs) => "↑/↓ scroll · End follow · Esc/Tab nodes · ←/→ tabs · q quit",
        (true, _, _) => "↑/↓ scroll · End follow · ←/→ tabs · b details · r restart · q quit",
        _ => "↑/↓ PgUp/PgDn scroll · End follow · ←/→ logs · q quit",
    };
    frame.render_widget(Line::from(keys).dark_gray(), help);
}

fn render_header(frame: &mut Frame, model: &Model, info: &Info, area: Rect) {
    let (label, color) = if model.step("Stop").is_some_and(|step| matches!(step.state, Step::Running(_))) {
        (format!("{} STOPPING", model.spinner()), Color::Yellow)
    } else if model.ready() {
        ("READY".into(), Color::Green)
    } else {
        let build = model.step("Build").filter(|step| matches!(step.state, Step::Running(_)));
        let label =
            build.map_or_else(|| "STARTING".into(), |step| format!("BUILDING · {}", report::seconds(step.elapsed())));
        (format!("{} {label}", model.spinner()), Color::Yellow)
    };
    let block = Block::default().borders(Borders::BOTTOM).border_style(Style::new().dark_gray());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [title, status] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(25.min(inner.width))]).areas(inner);
    frame.render_widget(Line::from(info.title.as_str()).bold(), title);
    frame.render_widget(Line::from(label).fg(color).right_aligned(), status);
}

fn render_running(frame: &mut Frame, model: &Model, info: &Info, area: Rect) {
    let activity = model.step("Stop").or_else(|| model.step("Reload"));
    let details = if model.show_details { 10.min(area.height.saturating_sub(12)) } else { 0 };
    let [summary, activity_area, steps, tabs, content] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(if activity.is_some() { 2 } else { 0 }),
        Constraint::Length(details),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(area);
    render_summary(frame, model, info, summary);
    if let Some(activity) = activity {
        render_activity(frame, model, activity, activity_area);
    }
    if model.show_details {
        render_steps(frame, model, steps);
    }
    frame.render_widget(
        Tabs::new(Tab::ALL.map(Tab::name))
            .select(model.selected)
            .highlight_style(Style::new().reversed().bold())
            .block(Block::default().borders(Borders::BOTTOM).border_style(Style::new().dark_gray())),
        tabs,
    );
    match model.tab() {
        Tab::Log(Source::Jvm) => nodes::render(frame, model, content),
        Tab::Log(source) => render_output(frame, model, content, &format!("{} output", source.name())),
        Tab::Players => players::render(frame, model, content),
    }
}

fn render_summary(frame: &mut Frame, model: &Model, info: &Info, area: Rect) {
    let address = if info.address.ip().is_unspecified() {
        format!("localhost:{}", info.address.port())
    } else {
        info.address.to_string()
    };
    let mut connection = vec![Span::raw("Connect  "), Span::raw(address).bold()];
    if let Some(build) = model.step("Build") {
        connection.push(Span::raw(format!("   ✓ Built {}", build.detail())).dark_gray());
    }
    let mut services = Vec::new();
    for name in ["Backend", "Control", "Proxy"] {
        let color = if model.step(name).is_some_and(|step| matches!(step.state, Step::Done(_))) {
            Color::Green
        } else {
            Color::Yellow
        };
        services.push(Span::styled("● ", Style::new().fg(color)));
        services.push(Span::raw(format!("{}   ", name.to_lowercase())));
    }
    services.push(Span::raw(if info.watching { "reloads on save" } else { "watch off" }).dark_gray());
    frame.render_widget(Paragraph::new(vec![Line::from(connection), Line::from(services)]), area);
}

fn render_activity(frame: &mut Frame, model: &Model, step: &Progress, area: Rect) {
    let (mark, color) = mark(model, &step.state);
    let detail = step.detail().lines().next().unwrap_or_default();
    let suffix = match &step.state {
        Step::Running(_) if step.name == "Reload" => {
            let phase = ["Compile", "Release"]
                .into_iter()
                .filter_map(|name| model.step(name))
                .find(|step| matches!(step.state, Step::Running(_)));
            phase.map_or_else(
                || report::seconds(step.elapsed()),
                |phase| format!("{} · {} · current release serving", phase.name, report::seconds(step.elapsed())),
            )
        }
        Step::Running(_) => report::seconds(step.elapsed()),
        Step::Failed(error) if error.contains("the previous release keeps serving") => {
            "previous release serving · details in dev log".into()
        }
        Step::Failed(_) => "details in dev log".into(),
        _ => String::new(),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{mark} {}  ", step.name), Style::new().fg(color)),
                Span::raw(suffix),
            ]),
            Line::from(detail).dark_gray(),
        ]),
        area,
    );
}

fn render_steps(frame: &mut Frame, model: &Model, area: Rect) {
    let rows = STARTUP.into_iter().filter_map(|name| model.step(name)).map(|step| {
        let (mark, color) = mark(model, &step.state);
        let detail = if matches!(step.state, Step::Done(_)) && matches!(step.name, "Compile" | "Release") {
            "Complete"
        } else {
            step.detail().lines().next().unwrap_or_default()
        };
        let elapsed = if matches!(step.state, Step::Done(_)) && matches!(step.name, "Compile" | "Release") {
            step.detail().to_owned()
        } else if step.elapsed().is_zero() {
            String::new()
        } else {
            report::seconds(step.elapsed())
        };
        Row::new(vec![
            Line::from(mark).fg(color),
            Line::from(step.name).bold(),
            Line::from(detail).fg(if matches!(step.state, Step::Pending(_)) { Color::DarkGray } else { Color::Reset }),
            Line::from(elapsed).dark_gray().right_aligned(),
        ])
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(2), Constraint::Length(10), Constraint::Min(1), Constraint::Length(9)])
            .block(
                Block::default()
                    .title("Build & startup")
                    .borders(Borders::BOTTOM)
                    .border_style(Style::new().dark_gray()),
            ),
        area,
    );
}

fn mark<'a>(model: &'a Model, state: &Step) -> (&'a str, Color) {
    match state {
        Step::Pending(_) => ("○", Color::DarkGray),
        Step::Running(_) => (model.spinner(), Color::Yellow),
        Step::Done(_) => ("✓", Color::Green),
        Step::Failed(_) => ("✗", Color::Red),
    }
}

fn render_output(frame: &mut Frame, model: &Model, area: Rect, title: &str) {
    let [header, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    let [name, follow] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(18.min(header.width))]).areas(header);
    frame.render_widget(Line::from(title).bold(), name);
    frame.render_widget(
        Line::from(if model.scroll == 0 { "following" } else { "scroll paused" }).dark_gray().right_aligned(),
        follow,
    );
    render_lines(frame, model, body);
}

fn render_lines(frame: &mut Frame, model: &Model, area: Rect) {
    let lines = model.visible();
    let end = lines.len().saturating_sub(model.scroll);
    let visible: Vec<Line> = lines[end.saturating_sub(usize::from(area.height))..end]
        .iter()
        .map(|line| {
            let line = if model.tab() == Tab::Log(Source::Jvm) && model.node.is_some() {
                line.split_once(' ').map_or(*line, |(_, line)| line)
            } else {
                line
            };
            Line::raw(line)
        })
        .collect();
    frame.render_widget(Paragraph::new(visible), area);
}

#[cfg(test)]
mod tests;
