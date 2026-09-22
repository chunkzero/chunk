use super::*;

#[test]
fn notifications_include_only_changed_ancestry_after_initial_connection() {
    use HookEvent::{DomainEnter, DomainLeave, PlayerConnect};
    assert_eq!(
        transition(None, "games/lobby"),
        vec![
            (PlayerConnect, String::new()),
            (PlayerConnect, "games".into()),
            (PlayerConnect, "games/lobby".into()),
            (DomainEnter, String::new()),
            (DomainEnter, "games".into()),
            (DomainEnter, "games/lobby".into())
        ]
    );
    assert_eq!(
        transition(Some("games/lobby/deep"), "games/match"),
        vec![
            (DomainLeave, "games/lobby/deep".into()),
            (DomainLeave, "games/lobby".into()),
            (DomainEnter, "games/match".into())
        ]
    );
    assert!(transition(Some("games/lobby"), "games/lobby").is_empty());
}
