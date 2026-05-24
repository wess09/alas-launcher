fn main() {
    tauri_build::build();

    // Ensure icons directory is watched for changes
    println!("cargo:rerun-if-changed=icons/");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by Cargo");
    let target = std::path::Path::new(&out_dir).join("bootstrap_uv.bin");
    if let Ok(source) = std::env::var("ALAS_BOOTSTRAP_UV") {
        std::fs::copy(&source, &target).expect("copy ALAS_BOOTSTRAP_UV");
        println!("cargo:rerun-if-env-changed=ALAS_BOOTSTRAP_UV");
        println!("cargo:rerun-if-changed={source}");
    } else {
        std::fs::write(&target, []).expect("write empty bootstrap uv placeholder");
        println!("cargo:warning=ALAS_BOOTSTRAP_UV is not set; launcher will use PATH uv for local builds");
    }
}
