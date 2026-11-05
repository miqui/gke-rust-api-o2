//! Compiles the gRPC contract (proto/message/v1/message_service.proto, unchanged from the Java
//! service) with a vendored protoc, and writes the descriptor set that server reflection serves.

use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Hermetic build: no system protoc needed (also what the Docker build stage relies on).
    // SAFETY: build scripts are single threaded at this point.
    unsafe { env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    let include = protoc_bin_vendored::include_path()?;
    let include = include.to_str().ok_or("non-utf8 include path")?;
    let out = PathBuf::from(env::var("OUT_DIR")?);

    tonic_prost_build::configure()
        .file_descriptor_set_path(out.join("message_service_descriptor.bin"))
        .compile_protos(
            &["proto/message/v1/message_service.proto"],
            &["proto", "third_party/proto", include],
        )?;

    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=third_party/proto");
    Ok(())
}
