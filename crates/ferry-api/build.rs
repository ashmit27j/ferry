//! Compiles the protobuf definitions with protox, so no system `protoc` is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = ["proto/ferry/v1/snapshot.proto"];
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    let fds = protox::compile(protos, ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
