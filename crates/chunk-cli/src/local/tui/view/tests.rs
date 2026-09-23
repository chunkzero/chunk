use chunk_proto::v1::{NodePhase, NodeStatus};
use ratatui::{Terminal, backend::TestBackend};

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
        nodes: vec![NodeStatus {
            app_id: "lobby".into(),
            host_id: "5a9e4aba-1234".into(),
            phase: NodePhase::Online.into(),
            ..Default::default()
        }],
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
