use super::Json;

#[test]
fn equivalent_json_has_the_same_encoding_regardless_of_object_key_order() {
    let first = Json::parse(r#"{"z":0,"a":[{"y":2,"x":1}]}"#).unwrap();
    let second = Json::parse(r#"{"a":[{"x":1,"y":2}],"z":0}"#).unwrap();
    assert_eq!(first.as_str(), second.as_str());
}
