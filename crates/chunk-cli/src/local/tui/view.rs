use chunk_proto::v1::NodePhase;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Row, Table, TableState, Tabs},
};

use super::model::{Focus, Model};
use crate::local::report::{Source, Step};

pub(super) fn render(frame: &mut Frame, model: &Model, title: &str) {
    let [top, logs, help] =
        Layout::vertical([Constraint::Length(12), Constraint::Min(6), Constraint::Length(1)]).areas(frame.area());
    let [steps, nodes] = Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).areas(top);
    render_steps(frame, model, steps, title);
    render_nodes(frame, model, nodes);
    render_logs(frame, model, logs);
    let keys = match (model.focus, &model.filter) {
        (Focus::Nodes, _) => "↑/↓ select node · Enter open its log · Tab/Esc back to logs · q quit",
        (Focus::Logs, Some(_)) => {
            "Esc all JVM logs · ←/→ logs · ↑/↓ PgUp/PgDn scroll · End follow · Tab nodes · q quit"
        }
        (Focus::Logs, None) => "q quit · r restart · ←/→ logs · ↑/↓ PgUp/PgDn scroll · End follow · Tab nodes",
    };
    frame.render_widget(Line::from(keys).dark_gray(), help);
}

fn render_steps(frame: &mut Frame, model: &Model, area: Rect, title: &str) {
    let lines: Vec<Line> = model
        .steps
        .iter()
        .map(|(name, state)| {
            let (mark, color, detail) = match state {
                Step::Running(detail) => ("…", Color::Yellow, detail.as_str()),
                Step::Done(detail) => ("✔", Color::Green, detail.as_str()),
                Step::Failed(error) => ("✗", Color::Red, error.lines().next().unwrap_or_default()),
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::new().fg(color)),
                Span::styled(format!("{name:<9}"), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(detail.to_owned()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(format!(" {title} "))), area);
}

fn render_nodes(frame: &mut Frame, model: &Model, area: Rect) {
    let mut rows = Vec::new();
    let mut selected = None;
    let mut node = 0;
    for deployment in &model.deployments {
        let release = format!("{} {}", &deployment.id[..deployment.id.len().min(8)], deployment.state);
        if deployment.nodes.is_empty() {
            rows.push(Row::new(vec![release.clone(), "no nodes yet".into()]).dark_gray());
        }
        for status in &deployment.nodes {
            if node == model.node {
                selected = Some(rows.len());
            }
            node += 1;
            let phase = NodePhase::try_from(status.phase).unwrap_or(NodePhase::Unspecified);
            let health = status.health.as_ref();
            rows.push(
                Row::new(vec![
                    release.clone(),
                    status.app_id.clone(),
                    status.host_id[..status.host_id.len().min(8)].to_owned(),
                    phase.as_str_name().trim_start_matches("NODE_PHASE_").to_lowercase(),
                    health.map_or_else(String::new, |health| health.players.to_string()),
                    health.map_or_else(String::new, |health| {
                        format!("{}/{}M", health.heap_used_bytes >> 20, health.heap_max_bytes >> 20)
                    }),
                ])
                .style(Style::new().fg(phase_color(phase))),
            );
        }
    }
    let focused = model.focus == Focus::Nodes;
    let table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(["release", "app", "node", "phase", "players", "heap"]).bold())
    .row_highlight_style(Style::new().reversed())
    .block(Block::bordered().title(format!(" nodes · {} players ", model.players())).border_style(if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new()
    }));
    let mut state = TableState::default().with_selected(selected.filter(|_| focused));
    frame.render_stateful_widget(table, area, &mut state);
}

fn phase_color(phase: NodePhase) -> Color {
    match phase {
        NodePhase::Online => Color::Green,
        NodePhase::Starting | NodePhase::Draining | NodePhase::Stopping => Color::Yellow,
        NodePhase::Unhealthy | NodePhase::Unreachable => Color::Red,
        NodePhase::Stopped | NodePhase::Unspecified => Color::DarkGray,
    }
}

fn render_logs(frame: &mut Frame, model: &Model, area: Rect) {
    let block = Block::bordered();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [tabs, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let titles = Source::ALL.map(|source| match (&model.filter, source) {
        (Some(host), Source::Jvm) => format!("jvm · {host}"),
        _ => source.name().to_owned(),
    });
    frame.render_widget(Tabs::new(titles).select(model.selected).highlight_style(Style::new().reversed()), tabs);
    let lines = model.visible();
    let height = usize::from(body.height);
    let end = lines.len() - model.scroll.min(lines.len());
    let visible: Vec<Line> = lines[end.saturating_sub(height)..end].iter().map(|line| Line::raw(*line)).collect();
    frame.render_widget(Paragraph::new(visible), body);
}
