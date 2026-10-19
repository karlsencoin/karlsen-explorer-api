//! Generates the karlsend gRPC client from the vendored protos (see proto/NOTICE.md).
//! `protox` compiles the protos in pure Rust, so no `protoc` binary is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["messages.proto"], ["proto"])?;
    tonic_prost_build::configure()
        // The server half is only used by the mock node in tests.
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    Ok(())
}
