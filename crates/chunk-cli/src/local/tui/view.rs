use chunk_proto::v1::NodePhase;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Row, Table, Tabs},
};

use super::model::Model;
use crate::local::report::{Source, Step};

pub(super) fn render(frame: &mut Frame, model: &Model, title: &str) {
    let [top, logs, help] =
        Layout::vertical([Constraint::Length(12), Constraint::Min(6), Constraint::Length(1)]).areas(frame.area());
    let [steps, nodes] = Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).areas(top);
    render_steps(frame, model, steps, title);
    render_nodes(frame, model, nodes);
    render_logs(frame, model, logs);
    frame.render_widget(
        Line::from("q quit · r restart · ←/→ logs · ↑/↓ PgUp/PgDn scroll · End follow").dark_gray(),
        help,
    );
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
    let rows = model.deployments.iter().flat_map(|deployment| {
        let release = format!("{} {}", &deployment.id[..deployment.id.len().min(8)], deployment.state);
        let mut rows: Vec<Row> = deployment
            .nodes
            .iter()
            .map(|node| {
                let phase = NodePhase::try_from(node.phase).unwrap_or(NodePhase::Unspecified);
                let health = node.health.as_ref();
                Row::new(vec![
                    release.clone(),
                    node.app_id.clone(),
                    node.host_id[..node.host_id.len().min(8)].to_owned(),
                    phase.as_str_name().trim_start_matches("NODE_PHASE_").to_lowercase(),
                    health.map_or_else(String::new, |health| health.players.to_string()),
                    health.map_or_else(String::new, |health| {
                        format!("{}/{}M", health.heap_used_bytes >> 20, health.heap_max_bytes >> 20)
                    }),
                ])
                .style(Style::new().fg(phase_color(phase)))
            })
            .collect();
        if rows.is_empty() {
            rows.push(Row::new(vec![release, "no nodes yet".into()]).dark_gray());
        }
        rows
    });
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
    .block(Block::bordered().title(format!(" nodes · {} players ", model.players())));
    frame.render_widget(table, area);
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
    frame.render_widget(
        Tabs::new(Source::ALL.map(Source::name)).select(model.selected).highlight_style(Style::new().reversed()),
        tabs,
    );
    let lines = &model.logs[model.selected];
    let height = usize::from(body.height);
    let end = lines.len() - model.scroll.min(lines.len());
    let visible: Vec<Line> =
        lines.range(end.saturating_sub(height)..end).map(|line| Line::raw(line.as_str())).collect();
    frame.render_widget(Paragraph::new(visible), body);
}
