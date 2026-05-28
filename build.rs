fn main() {
    let windows = tauri_build::WindowsAttributes::new().app_manifest(
        r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#,
    );
    let attrs = tauri_build::Attributes::new().windows_attributes(windows);
    tauri_build::try_build(attrs).expect("failed to run tauri build script");

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
