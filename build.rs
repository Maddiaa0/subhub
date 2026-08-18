use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let protobuf_include = protoc_bin_vendored::include_path()?;
    let proto = PathBuf::from("proto/iron/transform/v1/transform.proto");
    let includes = [PathBuf::from("proto/iron"), protobuf_include];
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc);
    tonic_prost_build::configure()
        .build_client(false)
        .compile_with_config(prost, &[proto], &includes)?;
    println!("cargo:rerun-if-changed=proto/iron/transform/v1/transform.proto");
    Ok(())
}
