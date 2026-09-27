use chunk_proto::sync::v1::{Node, OperatorPlayer, SessionDemand};

use crate::local::Command;

use super::*;

fn log(model: &mut Model, source: Source, line: &str) {
    model.apply(Event::Log { source, line: line.into() });
}

fn nodes(model: &mut Model, hosts: &[&str]) {
    model.apply(Event::Deployments(vec![Deployment {
        id: "release".into(),
        state: "current".into(),
        nodes: hosts.iter().map(|host| ((*host).into(), Node::default())).collect(),
        players: Vec::new(),
        destinations: Vec::new(),
    }]));
}

fn ready(model: &mut Model) {
    model.apply(Event::Step { name: "Ready", state: Step::Done("connect".into()) });
}

#[test]
fn node_selection_previews_logs_and_survives_refreshes_and_tab_changes() {
    let mut model = Model::new();
    nodes(&mut model, &["aaaaaaaa-1111", "bbbbbbbb-2222"]);
    ready(&mut model);
    log(&mut model, Source::Jvm, "aaaaaaaa lobby started");
    log(&mut model, Source::Jvm, "bbbbbbbb arena started");
    log(&mut model, Source::Backend, "backend line");

    assert_eq!(model.focus, Focus::Nodes);
    assert_eq!(model.visible().len(), 2);
    model.move_node(2);
    assert_eq!(model.visible(), ["bbbbbbbb arena started"]);
    model.open_node();
    assert_eq!(model.focus, Focus::Logs);
    model.scroll(1);
    log(&mut model, Source::Jvm, "aaaaaaaa lobby tick");
    assert_eq!(model.scroll, 1);
    nodes(&mut model, &["bbbbbbbb-2222", "aaaaaaaa-1111"]);
    assert_eq!(model.visible(), ["bbbbbbbb arena started"]);
    assert_eq!(model.scroll, 1);

    model.select(-1);
    assert_eq!(model.focus, Focus::Logs);
    model.toggle_focus();
    assert_eq!(model.focus, Focus::Logs);
    assert_eq!(model.visible(), ["backend line"]);
    model.select(1);
    assert_eq!(model.focus, Focus::Nodes);
    assert_eq!(model.visible(), ["bbbbbbbb arena started"]);
    model.open_node();
    model.back();
    assert_eq!(model.focus, Focus::Nodes);
    assert_eq!(model.visible().len(), 1);
    model.move_node(-1);
    assert!(model.node.is_none());
    assert_eq!(model.visible().len(), 3);

    model.move_node(1);
    nodes(&mut model, &["aaaaaaaa-1111"]);
    assert!(model.node.is_none());
    assert_eq!(model.scroll, 0);
}

#[test]
fn failed_reload_preserves_the_dashboard_and_selected_node() {
    let mut model = Model::new();
    nodes(&mut model, &["aaaaaaaa-1111"]);
    ready(&mut model);
    model.move_node(1);
    model.apply(Event::Step { name: "Reload", state: Step::Running("Lobby.kt".into()) });
    model.apply(Event::Step { name: "Compile", state: Step::Running("Gradle".into()) });
    model.apply(Event::Step {
        name: "Reload",
        state: Step::Failed("compile failed\nthe previous release keeps serving".into()),
    });
    assert!(model.ready());
    assert_eq!(model.node.as_deref(), Some("aaaaaaaa-1111"));
    assert!(matches!(model.step("Compile").unwrap().state, Step::Failed(_)));
    assert!(model.step("Compile").unwrap().started.is_none());
    model.apply(Event::Step { name: "Reload", state: Step::Running("Lobby.kt".into()) });
    assert!(matches!(model.step("Compile").unwrap().state, Step::Pending(_)));
    assert!(matches!(model.step("Release").unwrap().state, Step::Pending(_)));
}

#[test]
fn startup_does_not_interrupt_scrolled_build_output_and_elapsed_time_stops() {
    let mut model = Model::new();
    log(&mut model, Source::Build, "build output");
    model.scroll(1);
    model.apply(Event::Step { name: "Compile", state: Step::Running("Gradle".into()) });
    let compile = model.steps.iter_mut().find(|step| step.name == "Compile").unwrap();
    compile.started = Instant::now().checked_sub(Duration::from_secs(2));
    model.apply(Event::Step { name: "Compile", state: Step::Done("2s".into()) });
    let compile = model.step("Compile").unwrap();
    assert!(compile.elapsed() >= Duration::from_secs(2));
    assert!(compile.started.is_none());
    ready(&mut model);
    assert!(model.tab() == Tab::Log(Source::Build));
    assert_eq!(model.scroll, 1);
    assert_eq!(model.focus, Focus::Logs);
}

#[test]
fn failed_steps_keep_their_complete_diagnostic_in_the_dev_log() {
    let mut model = Model::new();
    model.apply(Event::Step {
        name: "Build",
        state: Step::Failed("gradle exited with status 1\ne: Lobby.kt:12:5 Unresolved reference: foo".into()),
    });
    model.select(-1);
    assert_eq!(
        model.visible(),
        ["Build failed", "gradle exited with status 1", "e: Lobby.kt:12:5 Unresolved reference: foo"]
    );
}

#[test]
fn source_index_matches_tab_order() {
    for source in Source::ALL {
        assert!(Tab::ALL[source.index()] == Tab::Log(source));
    }
}

fn player(name: &str, key: &str) -> (String, OperatorPlayer) {
    let demand =
        SessionDemand { session_type: "lobby/default".into(), key: key.into(), machine_profile: "local".into() };
    (format!("{name}-id"), OperatorPlayer { username: name.into(), demand: Some(demand), ..Default::default() })
}

fn destination(name: &str, session_type: &str, key: &str) -> crate::local::report::Destination {
    crate::local::report::Destination {
        name: name.into(),
        demand: SessionDemand { session_type: session_type.into(), key: key.into(), machine_profile: "local".into() },
    }
}

#[test]
fn search_keeps_a_matching_selection_and_the_move_form_targets_it() {
    let mut model = Model::new();
    ready(&mut model);
    model.apply(Event::Deployments(vec![Deployment {
        id: "release".into(),
        state: "current".into(),
        nodes: Vec::new(),
        players: vec![player("jeb_", "arena"), player("Notch", "main"), player("Dinnerbone", "arena")],
        destinations: vec![
            destination("arena/standard", "arena/default", "arena"),
            destination("lobby/main", "lobby/default", "main"),
        ],
    }]));
    model.select(1);
    assert!(model.tab() == Tab::Players);
    assert_eq!(model.selected_player().map(|(_, _, (_, p))| p.username.as_str()), Some("Dinnerbone"));

    model.search();
    for character in "ARENA".chars() {
        model.type_char(character);
    }
    model.move_player(1);
    assert_eq!(model.roster().len(), 2);
    assert_eq!(model.player.as_deref(), Some("jeb_-id"));
    assert!(model.submit().is_none());
    assert_eq!(model.filter, "ARENA");

    model.start_move();
    model.arrow(1);
    model.arrow(1);
    let Some(Command::MovePlayer { deployment, player, demand, .. }) = model.submit() else { panic!("no move") };
    assert_eq!((deployment.as_str(), player.as_str()), ("release", "jeb_-id"));
    assert_eq!((demand.session_type.as_str(), demand.key.as_str()), ("lobby/default", "main"));

    model.search();
    model.cancel();
    assert!(model.filter.is_empty() && model.input.is_none());
}
