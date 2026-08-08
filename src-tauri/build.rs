//! Build script: runs tauri-build (capabilities, ACL, Windows resource with the
//! application manifest) and additionally provides the v6 Common Controls
//! manifest to *test* harnesses.
//!
//! Why: tauri-build links its Windows resource (which embeds the Common-Controls
//! v6 dependency) only into bin targets (`embed-resource` emits
//! `cargo:rustc-link-arg-bins`, which never reaches test harnesses). The
//! `tray-icon`/`muda` and `rfd` (dialog) code linked into the lib/bin test
//! harnesses imports comctl32 v6-only entry points (`SetWindowSubclass`,
//! `DefSubclassProc`, `RemoveWindowSubclass`, `TaskDialogIndirect`); without a
//! manifest requesting Common Controls v6 the loader resolves comctl32 5.x and
//! the test process dies at startup with STATUS_ENTRYPOINT_NOT_FOUND
//! (0xc0000139).
//!
//! Fix: disable tauri-build's own app-manifest resource (the default manifest
//! contains only the Common-Controls dependency) and instead have the MSVC
//! linker embed that same dependency for *every* linkable target (bin, tests,
//! cdylib) via `/MANIFEST:EMBED` + `/MANIFESTINPUT`. The resulting production
//! manifest is identical to tauri's default; test harnesses get the manifest
//! they need at link time (no hand-copied sidecar files).

fn main() {
    // M10: 构建期注入的更新配置变化时强制重编译（option_env! 读取自编译环境）。
    for env_key in [
        "FOLIA_UPDATE_ENDPOINTS",
        "FOLIA_UPDATE_PUBKEY",
        "FOLIA_RELEASES_URL",
    ] {
        println!("cargo:rerun-if-env-changed={env_key}");
    }

    #[cfg(windows)]
    {
        let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if target_env == "msvc" {
            let attributes = tauri_build::Attributes::new()
                .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
            tauri_build::try_build(attributes).unwrap_or_else(|error| {
                // Mirrors `tauri_build::build()`'s failure handling.
                let error = format!("{error:#}");
                eprintln!("{error}");
                if error.starts_with("unknown field") {
                    eprintln!("found an unknown configuration field. This usually means that you are using a CLI version that is newer than `tauri-build` and is incompatible.");
                    eprintln!("Please try updating the Rust crates by running `cargo update` in the Tauri app folder.");
                }
                std::process::exit(1);
            });

            let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
            let manifest_path = out_dir.join("common-controls-v6.manifest");
            std::fs::write(&manifest_path, COMMON_CONTROLS_V6_MANIFEST)
                .expect("write common-controls-v6.manifest");
            println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
                manifest_path.display()
            );
            return;
        }
    }
    tauri_build::build();
}

/// Common-Controls v6 dependency; identical to tauri-build's default
/// `windows-app-manifest.xml`. Only linked into Windows MSVC targets (see the
/// `#[cfg(windows)]` block in `main`), so it is dead on every other target.
#[cfg(windows)]
const COMMON_CONTROLS_V6_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
</assembly>
"#;
