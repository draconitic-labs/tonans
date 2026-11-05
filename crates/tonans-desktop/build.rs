//! Generates the Tauri context and the permission set for the shell's commands.
//!
//! Listing the commands here turns on Tauri's access control for them: a command runs
//! only if a capability grants it, and the one capability, added at launch in
//! `src/main.rs`, grants them to this launch's loopback origin alone.

fn main() {
    let manifest = tauri_build::AppManifest::new().commands(&[
        "quit",
        "set_unsaved",
        "save_artifact",
        "save_bundle",
        "save_csv_template",
        "reveal_bundle",
        "open_link",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("the Tauri build step failed");
}
