use chunk_proto::v1::{NodePhase, NodeStatus};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use super::super::model::{Focus, Model};

pub(super) fn render(frame: &mut Frame, model: &Model, area: Rect) {
    let [nodes, logs] = if area.width >= 80 {
        Layout::horizontal([Constraint::Length(30), Constraint::Min(1)]).areas(area)
    } else {
        Layout::vertical([
            Constraint::Length(7.min(area.height / 2).min(area.height.saturating_sub(4))),
            Constraint::Min(1),
        ])
        .areas(area)
    };
    render_list(frame, model, nodes, area.width >= 80);
    let [header, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(logs);
    render_header(frame, model, header);
    super::render_lines(frame, model, body);
}

fn render_list(frame: &mut Frame, model: &Model, area: Rect, sidebar: bool) {
    let focused = model.focus == Focus::Nodes;
    let mut items = vec![ListItem::new("All nodes")];
    let mut selected = 0;
    for deployment in &model.deployments {
        items.push(ListItem::new(format!("{} · {}", short(&deployment.id), deployment.state)).dark_gray());
        if deployment.nodes.is_empty() {
            items.push(ListItem::new("  No nodes yet").dark_gray());
        }
        for node in &deployment.nodes {
            if model.node.as_deref() == Some(node.host_id.as_str()) {
                selected = items.len();
            }
            let phase = phase(node);
            let players = node
                .health
                .as_ref()
                .map_or_else(|| "health unavailable".into(), |health| format!("{} players", health.players));
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled("● ", Style::new().fg(color(phase))),
                    Span::raw(&node.app_id).bold(),
                    Span::raw(format!(" · {}", label(phase))).dark_gray(),
                ]),
                Line::from(format!("  {} · {players}", short(&node.host_id))).dark_gray(),
            ]));
        }
    }
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .highlight_symbol(if focused { "> " } else { "  " })
        .highlight_style(Style::new().reversed())
        .block(
            Block::default()
                .title(format!("Nodes · {}", model.hosts().len()))
                .borders(if sidebar { Borders::RIGHT } else { Borders::BOTTOM })
                .border_style(Style::new().fg(if focused { Color::Cyan } else { Color::DarkGray })),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_header(frame: &mut Frame, model: &Model, area: Rect) {
    let (title, detail) = model.selected_node().map_or_else(
        || (Line::from("All nodes").bold(), format!("{} nodes · {} players", model.hosts().len(), model.players())),
        |(deployment, node)| {
            let phase = phase(node);
            let title = Line::from(vec![
                Span::raw(format!("{} · {}  ", node.app_id, short(&node.host_id))).bold(),
                Span::styled(label(phase), Style::new().fg(color(phase))),
            ]);
            let health = node.health.as_ref().map_or_else(
                || "health unavailable".into(),
                |health| {
                    format!(
                        "{} players · {}/{}M heap",
                        health.players,
                        health.heap_used_bytes >> 20,
                        health.heap_max_bytes >> 20
                    )
                },
            );
            (title, format!("{health} · {} · {}", short(&deployment.id), deployment.state))
        },
    );
    let [heading, metadata] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let [name, follow] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(15.min(heading.width / 3))]).areas(heading);
    frame.render_widget(title, name);
    frame.render_widget(
        Line::from(if model.scroll == 0 { "following" } else { "scroll paused" }).dark_gray().right_aligned(),
        follow,
    );
    frame.render_widget(
        Paragraph::new(detail)
            .dark_gray()
            .block(Block::default().borders(Borders::BOTTOM).border_style(Style::new().dark_gray())),
        metadata,
    );
}

fn phase(node: &NodeStatus) -> NodePhase {
    NodePhase::try_from(node.phase).unwrap_or(NodePhase::Unspecified)
}

fn label(phase: NodePhase) -> &'static str {
    match phase {
        NodePhase::Online => "online",
        NodePhase::Starting => "starting",
        NodePhase::Draining => "draining",
        NodePhase::Stopping => "stopping",
        NodePhase::Unhealthy => "unhealthy",
        NodePhase::Unreachable => "unreachable",
        NodePhase::Stopped => "stopped",
        NodePhase::Unspecified => "unknown",
    }
}

fn color(phase: NodePhase) -> Color {
    match phase {
        NodePhase::Online => Color::Green,
        NodePhase::Starting | NodePhase::Draining | NodePhase::Stopping => Color::Yellow,
        NodePhase::Unhealthy | NodePhase::Unreachable => Color::Red,
        NodePhase::Stopped | NodePhase::Unspecified => Color::DarkGray,
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}
