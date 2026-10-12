//! Compiles the echo and health services with protox, so no system `protoc` is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/echo.proto");
    println!("cargo:rerun-if-changed=proto/health.proto");
    let fds = protox::compile(["proto/echo.proto", "proto/health.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
