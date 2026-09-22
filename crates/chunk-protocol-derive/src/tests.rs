use super::*;

#[test]
fn invalid_declarations_are_rejected() {
    assert!(
        codec(
            &parse_quote!(
                enum Invalid {
                    A,
                }
            ),
            true
        )
        .is_err()
    );
    for input in [
        parse_quote!(
            struct Missing;
        ),
        parse_quote!(
            #[packet(id = 0, id = 1, state = Status, direction = Serverbound)]
            struct Duplicate;
        ),
        parse_quote!(
            #[packet(id = -1, state = Status, direction = Serverbound)]
            struct Negative;
        ),
        parse_quote!(
            #[packet(id = 0, version = 1)]
            struct Unknown;
        ),
    ] {
        assert!(packet_impl(&input).is_err());
    }
}
