//! Build script — embeds the Filar icon (.ico) into the Windows executable.
//!
//! On Windows with the GNU toolchain, we compile a `.rc` resource file
//! with `windres` into a COFF object file, then pass it directly to the
//! linker via `cargo:rustc-link-arg`. This is necessary because GNU `ld`
//! does not pull unreferenced objects from static libraries — the resource
//! object has no symbols that the Rust code references, so it would be
//! silently discarded if linked through `libresource.a`.

fn main() {
    // Only on Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    println!("cargo:rerun-if-changed=build.rs");

    // Locate the .ico file relative to the workspace root.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico_path = std::path::Path::new(&manifest_dir)
        .join("..")
        .join("..")
        .join("pics")
        .join("filar.ico");

    if !ico_path.exists() {
        println!("cargo:warning=Icon file not found: {}", ico_path.display());
        return;
    }

    println!("cargo:rerun-if-changed={}", ico_path.display());

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let rc_path = std::path::Path::new(&out_dir).join("filar.rc");
    let obj_path = std::path::Path::new(&out_dir).join("filar_resource.o");

    // Use forward slashes — windres .rc files interpret backslashes as escapes.
    let ico_str = ico_path.to_str().unwrap().replace('\\', "/");

    // Write the .rc resource file.
    let rc_content = format!(
        r#"1 ICON "{ico_str}"
1 VERSIONINFO
FILEVERSION 0,1,0,0
PRODUCTVERSION 0,1,0,0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "000004b0"
    BEGIN
      VALUE "FileDescription", "Filar - Terminal with AI Agent"
      VALUE "ProductName", "Filar"
      VALUE "ProductVersion", "0.1.0"
      VALUE "FileVersion", "0.1.0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0, 0x04b0
  END
END
"#,
        ico_str = ico_str
    );
    std::fs::write(&rc_path, &rc_content).unwrap();

    // Locate windres. On this system it's in the MinGW installation.
    let mingw_bin = r"C:\Users\AdminLocal\mingw\mingw64\bin";
    let windres = if std::path::Path::new(&format!("{mingw_bin}\\windres.exe")).exists() {
        format!("{mingw_bin}\\windres.exe")
    } else {
        "windres".to_string()
    };

    // Also add MinGW to PATH in case windres needs to find its helper tools.
    if std::path::Path::new(mingw_bin).exists() {
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{mingw_bin};{path}"));
    }

    // Compile .rc → .o (COFF object with embedded resource).
    let status = std::process::Command::new(&windres)
        .arg(&rc_path)
        .arg("-O")
        .arg("coff")
        .arg("-o")
        .arg(&obj_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            // Pass the .o file DIRECTLY to the linker (not via a static library).
            // This is the critical step — `cargo:rustc-link-arg` ensures the
            // resource object is included even though no Rust code references it.
            let obj_str = obj_path.to_str().unwrap().replace('\\', "/");
            println!("cargo:rustc-link-arg={obj_str}");
        }
        Ok(s) => {
            println!("cargo:warning=windres failed with status: {s}");
            println!("cargo:warning=The .exe will not have a custom icon.");
        }
        Err(e) => {
            println!("cargo:warning=Failed to run windres: {e}");
            println!("cargo:warning=The .exe will not have a custom icon.");
        }
    }
}
