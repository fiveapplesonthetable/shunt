use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let root = manifest_dir
        .parent()
        .expect("no crates parent")
        .parent()
        .expect("no workspace root");
    let proto_dir = root.join("proto");
    let tacho_proto = proto_dir.join("tacho.proto");
    let perfetto_proto = proto_dir.join("perfetto/perfetto_trace.proto");

    println!("cargo:rerun-if-changed={}", tacho_proto.display());
    println!("cargo:rerun-if-changed={}", perfetto_proto.display());

    // Generate descriptor set for tacho.proto (FileDescriptorSet)
    let descriptor_path = out_dir.join("tacho_descriptor.bin");
    let protoc = env::var("PROTOC").unwrap_or_else(|_| "/usr/bin/protoc".to_string());
    let status = Command::new(&protoc)
        .arg(format!("-I{}", proto_dir.display()))
        .arg(format!(
            "--descriptor_set_out={}",
            descriptor_path.display()
        ))
        .arg(&tacho_proto)
        .status()
        .expect("failed to execute protoc for descriptor set");
    if !status.success() {
        panic!("protoc failed to generate descriptor set for tacho.proto");
    }

    // Compile protos with prost_build
    let mut config = prost_build::Config::new();
    config.protoc_arg(format!("-I{}", proto_dir.display()));
    config
        .compile_protos(&[&perfetto_proto, &tacho_proto], &[&proto_dir])
        .expect("prost-build failed");
}
