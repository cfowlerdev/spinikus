//! Generates the Rust code for the protocol. protox compiles the `.proto` files in Rust,
//! so building needs no `protoc` installed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(["spinikus/agent/v1/agent.proto"], ["proto"])?;
    // No generated `AgentServiceClient::connect` constructor: it would clash with the
    // `Connect` RPC's method. Clients build a `Channel` (with mTLS) and call `new`.
    tonic_prost_build::configure().build_transport(false).compile_fds(descriptors)?;
    Ok(())
}
