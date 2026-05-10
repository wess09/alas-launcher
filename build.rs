fn main() {
    tauri_build::build();

    // Ensure icons directory is watched for changes
    println!("cargo:rerun-if-changed=icons/");
}
