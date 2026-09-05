fn main() {
    println!(
        "{}",
        chunk_management::openapi().to_pretty_json().expect("valid OpenAPI")
    );
}
