use chunk_proto::v1::NodeStatus;

use super::*;

fn log(model: &mut Model, source: Source, line: &str) {
    model.apply(Event::Log { source, line: line.into() });
}

#[test]
fn opening_a_node_limits_the_jvm_log_until_backing_out() {
    let mut model = Model::new();
    let node = |host: &str| NodeStatus { host_id: host.into(), ..Default::default() };
    model.apply(Event::Deployments(vec![Deployment {
        id: "release".into(),
        state: "current".into(),
        nodes: vec![node("aaaaaaaa-1111"), node("bbbbbbbb-2222")],
    }]));
    log(&mut model, Source::Jvm, "aaaaaaaa lobby started");
    log(&mut model, Source::Jvm, "bbbbbbbb arena started");
    log(&mut model, Source::Proxy, "bbbbbbbb proxy line");

    model.toggle_focus();
    model.move_node(5);
    model.open_node();
    assert_eq!(model.focus, Focus::Logs);
    assert_eq!(model.visible(), ["bbbbbbbb arena started"]);

    log(&mut model, Source::Jvm, "aaaaaaaa lobby tick");
    model.scroll(10);
    assert_eq!(model.scroll, 1);

    assert!(model.back());
    assert_eq!(model.visible().len(), 3);
    assert!(!model.back());
}

#[test]
fn failed_steps_keep_their_complete_diagnostic_in_the_dev_log() {
    let mut model = Model::new();
    model.apply(Event::Step {
        name: "Build",
        state: Step::Failed("gradle exited with status 1\ne: Lobby.kt:12:5 Unresolved reference: foo".into()),
    });
    assert_eq!(
        model.visible(),
        ["Build failed", "gradle exited with status 1", "e: Lobby.kt:12:5 Unresolved reference: foo"]
    );
}
