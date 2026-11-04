//! Stamps the Windows version resource into `tonans.exe`.
//!
//! Windows shows this block in a file's Properties tab, and a signing or
//! allow-listing tool reads it. A binary that ships without one looks like
//! something a user should not run — which, for an unsigned executable asking to
//! open a browser, is a bad first impression to make.
//!
//! It is generated here rather than taken from a crate, because a build-dependency
//! is a dependency: it runs on the machine that produces the release, and the whole
//! pitch of this binary is that its tree is small enough to audit. The resource is
//! forty lines of text and `windres` is already in the toolchain that builds us.
//!
//! The identity in it is the *published* identity — Flintglade and its support
//! address — and nothing else. `packaging/verify-package.ps1` reads the block back
//! out of the built executable and fails the release if it says anything different.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The resource is a Windows PE concept. On any other target there is nothing to
    // stamp, and the crate still has to build (a developer runs the test suite on
    // whatever they have).
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");

    // VERSIONINFO wants four comma-separated fields. Cargo gives us three.
    let mut parts: Vec<&str> = version.split('.').collect();
    parts.resize(4, "0");
    let quad = parts.join(",");

    let script = out_dir.join("tonans.rc");
    let object = out_dir.join("tonans-version.o");
    std::fs::write(&script, resource_script(&quad, &version))
        .expect("writing the generated resource script");

    // `llvm-windres` ships with the llvm-mingw toolchain; `windres` is the binutils
    // spelling. Either compiles this.
    let compiled = ["llvm-windres", "windres"].into_iter().find_map(|tool| {
        Command::new(tool)
            .arg("--input")
            .arg(&script)
            .arg("--output")
            .arg(&object)
            .arg("--output-format=coff")
            .status()
            .ok()
            .filter(std::process::ExitStatus::success)
    });

    assert!(
        compiled.is_some(),
        "could not run a resource compiler (llvm-windres or windres) to stamp the Windows \
         version block. Both ship with the llvm-mingw toolchain this crate is built with; \
         put its `bin` directory on PATH. A release must not go out without the version block."
    );

    // Binaries only: a resource object in a test harness or a build script is noise.
    println!("cargo:rustc-link-arg-bins={}", object.display());
}

/// The VERSIONINFO block, as `windres` reads it.
///
/// No `#include <winver.h>`: the constants are written out so this does not depend
/// on a header search path that varies between the two resource compilers.
/// `0x40004` is `VOS_NT_WINDOWS32`, `0x1` is `VFT_APP`, and `0x409, 1200` is
/// US English in UTF-16.
fn resource_script(quad: &str, version: &str) -> String {
    format!(
        r#"1 VERSIONINFO
FILEVERSION {quad}
PRODUCTVERSION {quad}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Flintglade"
            VALUE "FileDescription", "Tonans - Microgrid Scenario Workbench"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "tonans"
            VALUE "LegalCopyright", "Copyright (C) 2026 Flintglade. Licensed under Apache-2.0."
            VALUE "OriginalFilename", "Tonans.exe"
            VALUE "ProductName", "Tonans"
            VALUE "ProductVersion", "{version}"
            VALUE "Comments", "Planning estimates for human review. Not a power-flow, protection, or interconnection study. support@flintglade.com"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}
