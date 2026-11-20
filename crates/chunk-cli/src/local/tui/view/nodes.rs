use chunk_proto::sync::v1::{JvmSessionPhase, NodePhase, NodeSession};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use super::super::model::{Focus, JvmSelection, Model};
use crate::local::logs;

const NODE_COLORS: [Color; 6] = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta, Color::Blue, Color::Red];
const SESSION_COLORS: [Color; 6] =
    [Color::LightBlue, Color::LightMagenta, Color::LightCyan, Color::LightGreen, Color::LightYellow, Color::LightRed];

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
    render_lines(frame, model, body);
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
        for (host, node) in &deployment.nodes {
            if matches!(&model.selection, JvmSelection::Node(id) if id == host) {
                selected = items.len();
            }
            let phase = node.phase();
            let short_host = short(host);
            let mut subtitle = vec![Span::raw("  "), Span::styled(short_host, Style::new().fg(node_color(short_host)))];
            if phase != NodePhase::Stopped {
                let players = node
                    .health
                    .as_ref()
                    .map_or_else(|| "health unavailable".into(), |health| count(health.players, "player"));
                subtitle.push(Span::raw(format!(" · {players}")).dark_gray());
            }
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled("● ", Style::new().fg(color(phase))),
                    Span::raw(&node.app).bold(),
                    Span::raw(format!(" · {}", label(phase))).dark_gray(),
                ]),
                Line::from(subtitle),
            ]));
            if phase == NodePhase::Stopped {
                continue;
            }
            for (index, session) in node.sessions.iter().enumerate() {
                if matches!(
                    &model.selection,
                    JvmSelection::Session { host: selected_host, session: selected_session }
                        if selected_host == host && selected_session == &session.id
                ) {
                    selected = items.len();
                }
                let last = index + 1 == node.sessions.len();
                let branch = if last { "  └ " } else { "  ├ " };
                let cont = if last { "      " } else { "  │   " };
                let sphase = session.phase();
                let short_id = short(&session.id);
                items.push(ListItem::new(vec![
                    Line::from(vec![
                        Span::raw(branch).dark_gray(),
                        Span::styled("● ", Style::new().fg(session_phase_color(sphase))),
                        Span::raw(session_label(session)),
                        Span::raw(format!(" · {}", session_phase_label(sphase))).dark_gray(),
                    ]),
                    Line::from(vec![
                        Span::raw(cont).dark_gray(),
                        Span::styled(short_id, Style::new().fg(session_color(short_id))),
                        Span::raw(format!(" · {}", count(session.players, "player"))).dark_gray(),
                    ]),
                ]));
            }
        }
    }
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .highlight_symbol(if focused { "> " } else { "  " })
        // One background for the row; reversing would invert each styled span separately.
        .highlight_style(Style::new().bg(Color::Indexed(237)))
        .block(
            Block::default()
                .title(format!("Nodes · {}", model.hosts().len()))
                .borders(if sidebar { Borders::RIGHT } else { Borders::BOTTOM })
                .border_style(Style::new().fg(if focused { Color::Cyan } else { Color::DarkGray })),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_header(frame: &mut Frame, model: &Model, area: Rect) {
    let (title, detail) = if let Some((deployment, (host, node), session)) = model.selected_session() {
        let sphase = session.phase();
        let name = if session.key.is_empty() {
            session.session_type.clone()
        } else {
            format!("{}:{}", session.session_type, session.key)
        };
        let title = Line::from(vec![
            Span::raw(format!("{name} · {}  ", short(&session.id))).bold(),
            Span::styled(session_phase_label(sphase), Style::new().fg(session_phase_color(sphase))),
        ]);
        let detail = format!(
            "{} · {} · {} · {} · {}",
            count(session.players, "player"),
            node.app,
            short(host),
            short(&deployment.id),
            deployment.state
        );
        (title, detail)
    } else if let Some((deployment, (host, node))) = model.selected_node() {
        let phase = node.phase();
        let title = Line::from(vec![
            Span::raw(format!("{} · {}  ", node.app, short(host))).bold(),
            Span::styled(label(phase), Style::new().fg(color(phase))),
        ]);
        let deployment = format!("{} · {}", short(&deployment.id), deployment.state);
        let sessions = count(node.sessions.len(), "session");
        let detail = match (phase, &node.health) {
            (NodePhase::Stopped, _) => deployment,
            (_, None) => format!("{sessions} · health unavailable · {deployment}"),
            (_, Some(health)) => format!(
                "{} · {sessions} · {}/{}M heap · {deployment}",
                count(health.players, "player"),
                health.heap_used_bytes >> 20,
                health.heap_max_bytes >> 20
            ),
        };
        (title, detail)
    } else {
        (
            Line::from("All nodes").bold(),
            format!(
                "{} · {} · {}",
                count(model.hosts().len(), "node"),
                count(model.sessions(), "session"),
                count(model.players(), "player")
            ),
        )
    };
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

fn render_lines(frame: &mut Frame, model: &Model, area: Rect) {
    let lines = model.visible();
    let end = lines.len().saturating_sub(model.scroll);
    let visible: Vec<Line> = lines[end.saturating_sub(usize::from(area.height))..end]
        .iter()
        .map(|line| format_line(&model.selection, line))
        .collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn format_line<'a>(selection: &JvmSelection, line: &'a str) -> Line<'a> {
    let parsed = logs::parse_jvm_line(line);
    match selection {
        JvmSelection::All => {
            if parsed.host.is_empty() {
                return Line::raw(parsed.body);
            }
            let mut spans = vec![Span::styled(parsed.host, Style::new().fg(node_color(parsed.host)).bold())];
            if let Some(session) = parsed.session {
                let short_id = short(session);
                spans.push(Span::styled("/", Style::new().dark_gray()));
                spans.push(Span::styled(short_id, Style::new().fg(session_color(short_id))));
            }
            if !parsed.body.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::raw(parsed.body));
            }
            Line::from(spans)
        }
        JvmSelection::Node(_) => match parsed.session {
            Some(session) => {
                let short_id = short(session);
                let mut spans = vec![Span::styled(short_id, Style::new().fg(session_color(short_id)))];
                if !parsed.body.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::raw(parsed.body));
                }
                Line::from(spans)
            }
            None => Line::raw(parsed.body),
        },
        JvmSelection::Session { .. } => Line::raw(parsed.body),
    }
}

fn session_label(session: &NodeSession) -> &str {
    if session.key.is_empty() { &session.session_type } else { &session.key }
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

fn session_phase_label(phase: JvmSessionPhase) -> &'static str {
    match phase {
        JvmSessionPhase::Ready => "ready",
        JvmSessionPhase::Starting => "starting",
        JvmSessionPhase::Ending => "ending",
        JvmSessionPhase::Ended => "ended",
        JvmSessionPhase::Failed => "failed",
        JvmSessionPhase::Unspecified => "unknown",
    }
}

fn session_phase_color(phase: JvmSessionPhase) -> Color {
    match phase {
        JvmSessionPhase::Ready => Color::Green,
        JvmSessionPhase::Starting | JvmSessionPhase::Ending => Color::Yellow,
        JvmSessionPhase::Failed => Color::Red,
        JvmSessionPhase::Ended | JvmSessionPhase::Unspecified => Color::DarkGray,
    }
}

fn palette_index(key: &str, len: usize) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    usize::try_from(hash % (len as u64)).unwrap_or(0)
}

fn node_color(host: &str) -> Color {
    NODE_COLORS[palette_index(host, NODE_COLORS.len())]
}

fn session_color(session: &str) -> Color {
    SESSION_COLORS[palette_index(session, SESSION_COLORS.len())]
}

pub(super) fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

pub(super) fn count<N: Copy + std::fmt::Display + PartialEq + From<u8>>(n: N, noun: &str) -> String {
    let suffix = if n == N::from(1) { "" } else { "s" };
    format!("{n} {noun}{suffix}")
}
