use std::{env, fs, path::PathBuf};

use base64::{prelude::BASE64_STANDARD, Engine};

const MTLS_IDENTITY_ENV: &str = "ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64";
const REQUIRE_MTLS_ENV: &str = "REQUIRE_LAUNCHER_MTLS_IDENTITY";
const LAUNCHER_UPDATE_URL_ENV: &str = "LAUNCHER_UPDATE_URL";
const DEFAULT_LAUNCHER_UPDATE_URL: &str =
    "https://ap.launcher-update.nanoda.work/updata/stable.json";

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
    println!("cargo:rerun-if-env-changed={MTLS_IDENTITY_ENV}");
    println!("cargo:rerun-if-env-changed={REQUIRE_MTLS_ENV}");
    println!("cargo:rerun-if-env-changed={LAUNCHER_UPDATE_URL_ENV}");

    let launcher_update_url = env::var(LAUNCHER_UPDATE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LAUNCHER_UPDATE_URL.to_string());
    println!("cargo:rustc-env={LAUNCHER_UPDATE_URL_ENV}={launcher_update_url}");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by Cargo");
    let out_dir = PathBuf::from(out_dir);

    let bootstrap_uv_target = out_dir.join("bootstrap_uv.bin");
    if let Ok(source) = env::var("ALAS_BOOTSTRAP_UV") {
        fs::copy(&source, &bootstrap_uv_target).expect("copy ALAS_BOOTSTRAP_UV");
        println!("cargo:rerun-if-env-changed=ALAS_BOOTSTRAP_UV");
        println!("cargo:rerun-if-changed={source}");
    } else {
        fs::write(&bootstrap_uv_target, []).expect("write empty bootstrap uv placeholder");
        println!("cargo:warning=ALAS_BOOTSTRAP_UV is not set; launcher will use PATH uv for local builds");
    }

    let mtls_identity_target = out_dir.join("launcher_mtls_identity.pem");
    let mtls_identity = match env::var(MTLS_IDENTITY_ENV) {
        Ok(encoded) if !encoded.trim().is_empty() => Some(
            BASE64_STANDARD
                .decode(encoded.trim().as_bytes())
                .expect("decode ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64"),
        ),
        _ => {
            let manifest_dir =
                PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
            let local_cert_path = manifest_dir.join("证书.txt");
            if local_cert_path.exists() {
                println!(
                    "cargo:warning=ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is not set; using local {}",
                    local_cert_path.display()
                );
                println!("cargo:rerun-if-changed={}", local_cert_path.display());
                Some(fs::read(&local_cert_path).expect("read local certificate file"))
            } else {
                if env::var_os(REQUIRE_MTLS_ENV).is_some() {
                    panic!("ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is required but not set");
                }
                println!(
                    "cargo:warning=ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is not set; launcher update client will build without mTLS identity"
                );
                None
            }
        }
    };
    match mtls_identity {
        Some(bytes) => {
            fs::write(&mtls_identity_target, bytes).expect("write launcher mTLS identity")
        }
        None => {
            fs::write(&mtls_identity_target, []).expect("write empty launcher mTLS placeholder")
        }
    }
}
