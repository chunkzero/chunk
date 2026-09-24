use super::*;
use crate::Contracts;
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn explicit_creation_profile_and_frozen_values_control_reuse_and_host_capacity() {
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "apps":{"arena":{"id":"arena","jar":"arena.jar","sha256":"artifact","java_version":25,
            "sessions":{"default":{"machine_profile":"small","capacity":16}}}},
        "deployment":{"environment":"test","deployment":"release"},"artifact_digest":"artifact",
        "profiles":{"small":{"memory_mib":512,"max_sessions":4},"large":{"memory_mib":1024,"max_sessions":4}},
        "session_types":{"arena/default":{"app":"arena","machine_profile":"small","capacity":16}},
        "max_processes":4,"idle_node_timeout_seconds":0,
        "destinations":{"version":1,"entries":{"apps/arena/destinations/main":{
            "destination":{"key":"public-arena","session_type":"arena/default","machine_profile":"large"},
            "overflow":"replicate","empty_timeout_seconds":60,"creation":{"capacity":80,"configuration":{}}
        }}}
    }))
    .unwrap();
    config.validate().unwrap();
    let demand = chunk_proto::v1::SessionDemand {
        key: "public-arena".into(),
        session_type: "arena/default".into(),
        machine_profile: "large".into(),
    };
    let mut state = State::default();
    let first = select_session(&mut state, &config, &demand, &BTreeSet::new()).unwrap();
    assert_eq!(state.sessions[&first].capacity, 80);
    assert_eq!(state.hosts[&state.sessions[&first].host].profile, "large");
    assert_eq!(select_session(&mut state, &config, &demand, &BTreeSet::new()).unwrap(), first);
    for field in ["capacity", "configuration", "profile"] {
        let mut changed = state.clone();
        match field {
            "capacity" => changed.sessions.get_mut(&first).unwrap().capacity = 81,
            "configuration" => {
                changed.sessions.get_mut(&first).unwrap().configuration = serde_json::json!({"other":true});
            }
            _ => changed.hosts.get_mut(&state.sessions[&first].host).unwrap().profile = "small".into(),
        }
        let next = select_session(&mut changed, &config, &demand, &BTreeSet::new()).unwrap();
        assert_ne!(next, first, "{field}");
        assert_ne!(changed.sessions[&next].host, changed.sessions[&first].host, "{field}");
    }
    let mut wrong_profile = demand.clone();
    wrong_profile.machine_profile = "small".into();
    assert!(select_session(&mut state, &config, &wrong_profile, &BTreeSet::new()).is_err());
    config.contracts.destinations.as_mut().unwrap().entries.values_mut().next().unwrap().creation = None;
    assert!(config.validate().is_err());
}

#[test]
fn placement_groups_only_matching_apps_and_profiles() {
    let mut state = State::default();
    let mut config = Config {
        contracts: Contracts::default(),
        apps: BTreeMap::new(),
        deployment: chunk_proto::v1::DeploymentRef::default(),
        artifact_digest: "release".into(),
        profiles: BTreeMap::from([
            ("small".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 4 }),
            ("large".into(), crate::MachineProfile { memory_mib: 1024, max_sessions: 4 }),
        ]),
        session_types: BTreeMap::new(),
        max_processes: 4,
        idle_node_timeout_seconds: 0,
    };
    for (name, app, profile) in
        [("lobby/default", "lobby", "small"), ("arena/default", "arena", "small"), ("arena/large", "arena", "large")]
    {
        config
            .session_types
            .insert(name.into(), crate::SessionType { app: app.into(), machine_profile: profile.into(), capacity: 16 });
    }
    let mut selected = Vec::new();
    for (key, session_type) in
        [("lobby", "lobby/default"), ("arena1", "arena/default"), ("arena2", "arena/default"), ("large", "arena/large")]
    {
        let session = select_session(
            &mut state,
            &config,
            &chunk_proto::v1::SessionDemand {
                key: key.into(),
                session_type: session_type.into(),
                machine_profile: String::new(),
            },
            &BTreeSet::new(),
        )
        .unwrap();
        selected.push(state.sessions[&session].host.clone());
    }
    assert_ne!(selected[0], selected[1]);
    assert_eq!(selected[1], selected[2]);
    assert_ne!(selected[1], selected[3]);
    assert_eq!(state.hosts.len(), 3);
    assert!(
        select_session(
            &mut state,
            &config,
            &chunk_proto::v1::SessionDemand {
                key: "changed".into(),
                session_type: "arena/large".into(),
                machine_profile: "small".into()
            },
            &BTreeSet::new()
        )
        .is_err()
    );
}
