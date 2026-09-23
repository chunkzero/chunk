use chunk_proto::v1::NodeStatus;

use super::*;

fn log(model: &mut Model, source: Source, line: &str) {
    model.apply(Event::Log { source, line: line.into() });
}

fn nodes(model: &mut Model, hosts: &[&str]) {
    model.apply(Event::Deployments(vec![Deployment {
        id: "release".into(),
        state: "current".into(),
        nodes: hosts.iter().map(|host| NodeStatus { host_id: (*host).into(), ..Default::default() }).collect(),
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
    assert!(model.source() == Source::Build);
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
    for (index, source) in Source::ALL.into_iter().enumerate() {
        assert_eq!(source.index(), index);
    }
}
