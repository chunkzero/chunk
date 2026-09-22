use super::*;

#[test]
fn cache_uses_exact_protocol_versions_without_a_fallback() {
    let cache = Cache::new(Some(256)).unwrap();
    assert!(std::ptr::eq(cache.get(776).unwrap(), cache.get(776).unwrap()));
    assert!(cache.get(774).is_err());
    assert!(Packets::new(774, Some(256)).is_err());
}
