use super::*;
#[test]
fn packet_ids_follow_the_dataset_but_parser_shape_changes_fail_closed() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../chunk-protocol/data/26.2/protocol.json")).unwrap();
    let original = generate(&data).unwrap().to_string();
    let mappings = data["play"]["toClient"]["types"]["packet"][1][0]["type"][1]["mappings"].as_object_mut().unwrap();
    let key = mappings.iter().find(|(_, name)| **name == "declare_commands").unwrap().0.clone();
    let value = mappings.remove(&key).unwrap();
    mappings.insert("0x100".into(), value);
    assert_ne!(generate(&data).unwrap().to_string(), original);
    data["types"]["command_node"][1][0]["type"][1][0]["size"] = 3.into();
    assert!(generate(&data).is_err());
}
