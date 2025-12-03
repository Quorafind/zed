fn main() {
    println!("cargo:rerun-if-changed=src/proto/cpp.proto");

    let mut config = prost_build::Config::new();
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");

    config
        .compile_protos(&["src/proto/cpp.proto"], &["src/proto/"])
        .expect("Failed to compile protobuf files");
}
