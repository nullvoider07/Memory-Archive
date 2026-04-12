// /Memory-Archive/ma-proto/build.rs

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::Path::new("src/gen");
    std::fs::create_dir_all(out_dir)?;

    tonic_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .out_dir(out_dir)
        .compile_protos(&["proto/control_center.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/control_center.proto");
    Ok(())
}