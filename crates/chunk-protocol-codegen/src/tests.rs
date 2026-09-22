use super::*;

#[test]
fn invocation_requires_a_module_identifier_and_literal_path() {
    assert!(syn::parse_str::<Invocation>("v26_2, \"data/26.2\"").is_ok());
    assert!(syn::parse_str::<Invocation>("26_2, \"data/26.2\"").is_err());
    assert!(syn::parse_str::<Invocation>("v26_2, path").is_err());
    assert!(syn::parse_str::<Invocation>("v26_2, \"data/26.2\", extra").is_err());
}
