//! Prints the OpenAPI document; `cargo xtask openapi` writes it to `docs/openapi.json`.

fn main() {
    print!("{}", spinikus_api::openapi_json());
}
