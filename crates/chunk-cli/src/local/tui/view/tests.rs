use chunk_proto::sync::v1::{
    ClaimPhase, JvmHealth, JvmSessionPhase, Node, NodePhase, NodeSession, OperatorPlayer, SessionDemand,
};
use ratatui::{Terminal, backend::TestBackend, style::Color};

use super::*;
use crate::local::report::{Deployment, Event};

fn draw(model: &Model, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let info =
        Info { title: "chunk dev · example".into(), address: "127.0.0.1:25565".parse().unwrap(), watching: true };
    terminal.draw(|frame| render(frame, model, &info)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn startup_shows_phases_and_output_then_nodes_belong_only_to_jvm() {
    let mut model = Model::new();
    model.apply(Event::Step { name: "Build", state: Step::Running("Gradle".into()) });
    model.apply(Event::Step { name: "Compile", state: Step::Running("Gradle chunkArtifacts".into()) });
    model.apply(Event::Log { source: Source::Build, line: "> Task :apps:lobby:compileKotlin".into() });
    let startup = draw(&model, 120, 30);
    assert!(startup.contains("BUILDING"));
    assert!(startup.contains("Waiting for compilation"));
    assert!(startup.contains("> Task :apps:lobby:compileKotlin"));
    assert!(!startup.contains("All nodes"));

    model.apply(Event::Step { name: "Ready", state: Step::Done("connect".into()) });
    model.apply(Event::Deployments(vec![Deployment {
        id: "d87ec655".into(),
        state: "current".into(),
        nodes: vec![(
            "5a9e4aba-1234".into(),
            Node { app: "lobby".into(), phase: NodePhase::Online.into(), ..Default::default() },
        )],
        players: Vec::new(),
        destinations: Vec::new(),
    }]));
    model.apply(Event::Log { source: Source::Jvm, line: "5a9e4aba lobby ready".into() });
    model.move_node(1);
    for (width, height) in [(120, 30), (60, 24), (40, 12)] {
        let running = draw(&model, width, height);
        assert!(running.contains("READY"));
        assert!(running.contains("lobby ready"), "{width}x{height}:\n{running}");
        assert!(!running.contains("Waiting for compilation"));
    }
    model.select(-1);
    let backend = draw(&model, 120, 30);
    assert!(backend.contains("backend output"));
    assert!(!backend.contains("All nodes"));
    assert!(!backend.contains("5a9e4aba"));
}

#[test]
fn stopped_node_header_shows_its_phase_instead_of_its_last_health() {
    let mut model = Model::new();
    model.apply(Event::Step { name: "Ready", state: Step::Done("connect".into()) });
    let health = JvmHealth { players: 3, heap_used_bytes: 512 << 20, heap_max_bytes: 1024 << 20, ..Default::default() };
    let node = |phase: NodePhase| Node {
        app: "lobby".into(),
        phase: phase.into(),
        health: Some(health),
        ..Default::default()
    };
    let deployments = |phase| {
        Event::Deployments(vec![Deployment {
            id: "d87ec655".into(),
            state: "current".into(),
            nodes: vec![("5a9e4aba-1234".into(), node(phase))],
            players: Vec::new(),
            destinations: Vec::new(),
        }])
    };
    model.apply(deployments(NodePhase::Online));
    model.move_node(1);
    assert!(draw(&model, 120, 30).contains("512/1024M heap"));

    model.apply(deployments(NodePhase::Stopped));
    let stopped = draw(&model, 120, 30);
    assert!(stopped.contains("stopped"));
    assert!(stopped.contains("d87ec655 · current"), "{stopped}");
    assert!(!stopped.contains("heap") && !stopped.contains("3 players ·"), "{stopped}");
}

#[test]
fn players_tab_lists_filters_and_shows_the_move_form() {
    let mut model = Model::new();
    model.apply(Event::Step { name: "Ready", state: Step::Done("connect".into()) });
    let player = |name: &str| {
        let player = OperatorPlayer {
            username: name.into(),
            demand: Some(SessionDemand {
                session_type: "lobby/default".into(),
                key: "main".into(),
                ..Default::default()
            }),
            app: "lobby".into(),
            host: "5a9e4aba-1234".into(),
            phase: ClaimPhase::Arrived.into(),
            ..Default::default()
        };
        (format!("{name}-uuid"), player)
    };
    model.apply(Event::Deployments(vec![Deployment {
        id: "d87ec655".into(),
        state: "current".into(),
        nodes: Vec::new(),
        players: vec![player("Notch"), player("jeb_")],
        // More destinations than a short terminal can show, with the player's own last.
        destinations: (0..8)
            .map(|index| (format!("arena/{index}"), "arena/default", format!("arena-{index}")))
            .chain([("lobby/main".into(), "lobby/default", "main".into())])
            .map(|(name, session_type, key)| crate::local::report::Destination {
                name,
                demand: SessionDemand { session_type: session_type.into(), key, machine_profile: "local".into() },
            })
            .collect(),
    }]));
    model.select(1);
    for (width, height) in [(120, 30), (60, 24)] {
        let screen = draw(&model, width, height);
        assert!(screen.contains("Players · 2"), "{width}x{height}:\n{screen}");
        assert!(screen.contains("jeb_  online"), "{width}x{height}:\n{screen}");
    }
    model.search();
    model.type_char('n');
    model.type_char('o');
    let screen = draw(&model, 120, 30);
    assert!(screen.contains("Players · 1/2"), "{screen}");
    assert!(screen.contains("/ no"));
    model.submit();
    model.start_move();
    for (width, height) in [(120, 30), (60, 24)] {
        let screen = draw(&model, width, height);
        assert!(screen.contains("Move Notch to"), "{width}x{height}:\n{screen}");
        assert!(screen.contains("> lobby/main · lobby/default:main"), "{width}x{height}:\n{screen}");
    }
}

#[test]
fn jvm_tab_nests_sessions_shows_counts_and_details_and_colors_interleaved_logs() {
    let mut model = Model::new();
    model.apply(Event::Step { name: "Ready", state: Step::Done("connect".into()) });
    let health = JvmHealth { players: 3, heap_used_bytes: 256 << 20, heap_max_bytes: 512 << 20, ..Default::default() };
    model.apply(Event::Deployments(vec![Deployment {
        id: "d87ec655".into(),
        state: "current".into(),
        nodes: vec![
            (
                "5a9e4aba-1234".into(),
                Node {
                    app: "arena".into(),
                    phase: NodePhase::Online.into(),
                    health: Some(health),
                    sessions: vec![
                        NodeSession {
                            id: "f47ac10b-58cc".into(),
                            session_type: "arena/default".into(),
                            key: "duel-1".into(),
                            players: 2,
                            phase: JvmSessionPhase::Ready.into(),
                        },
                        NodeSession {
                            id: "9c2e1d0a-99aa".into(),
                            session_type: "arena/default".into(),
                            key: "duel-2".into(),
                            players: 1,
                            phase: JvmSessionPhase::Starting.into(),
                        },
                    ],
                    ..Default::default()
                },
            ),
            (
                "b1234567-89ab".into(),
                Node { app: "lobby".into(), phase: NodePhase::Online.into(), ..Default::default() },
            ),
        ],
        players: Vec::new(),
        destinations: Vec::new(),
    }]));
    model.apply(Event::Log { source: Source::Jvm, line: "5a9e4aba JVM booted".into() });
    model.apply(Event::Log { source: Source::Jvm, line: "5a9e4aba [session:f47ac10b-58cc] match started".into() });
    model.apply(Event::Log { source: Source::Jvm, line: "b1234567 lobby listening".into() });

    // All nodes header shows node, session, and player counts, and sidebar nests sessions under their node.
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let info =
        Info { title: "chunk dev · example".into(), address: "127.0.0.1:25565".parse().unwrap(), watching: true };
    terminal.draw(|frame| render(frame, &model, &info)).unwrap();
    let buffer = terminal.backend().buffer();
    let all_nodes = buffer
        .content
        .chunks(120)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_nodes.contains("2 nodes · 2 sessions · 3 players"), "{all_nodes}");
    assert!(all_nodes.contains("duel-1 · ready"), "{all_nodes}");
    assert!(all_nodes.contains("duel-2 · starting"), "{all_nodes}");
    assert!(all_nodes.contains("5a9e4aba/f47ac10b match started"), "{all_nodes}");

    // Verify interleaved node and session prefixes carry distinct non-default colors.
    let tagged_row = all_nodes.lines().position(|row| row.contains("5a9e4aba/f47ac10b match started")).unwrap();
    let line = all_nodes.lines().nth(tagged_row).unwrap();
    let node_byte = line.find("5a9e4aba/f47ac10b").unwrap();
    let node_col = line[..node_byte].chars().count();
    let session_col = node_col + "5a9e4aba/".len();
    let node_fg = buffer.cell((u16::try_from(node_col).unwrap(), u16::try_from(tagged_row).unwrap())).unwrap().fg;
    let session_fg = buffer.cell((u16::try_from(session_col).unwrap(), u16::try_from(tagged_row).unwrap())).unwrap().fg;
    assert_ne!(node_fg, Color::Reset);
    assert_ne!(session_fg, Color::Reset);
    assert_ne!(node_fg, session_fg);

    // Selected node header includes session count alongside players and heap.
    model.move_node(1);
    let node_screen = draw(&model, 120, 30);
    assert!(node_screen.contains("3 players · 2 sessions · 256/512M heap"), "{node_screen}");
    assert!(node_screen.contains("f47ac10b match started"), "{node_screen}");
    assert!(node_screen.contains("JVM booted"), "{node_screen}");

    // Selected session header shows session details and filters log body cleanly.
    model.move_node(1);
    let session_screen = draw(&model, 120, 30);
    assert!(session_screen.contains("arena/default:duel-1 · f47ac10b  ready"), "{session_screen}");
    assert!(session_screen.contains("2 players · arena · 5a9e4aba · d87ec655 · current"), "{session_screen}");
    assert!(session_screen.contains("match started"), "{session_screen}");
    assert!(!session_screen.contains("JVM booted"), "{session_screen}");
}
