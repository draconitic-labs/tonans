//! Tonans in a native window.
//!
//! The loopback server in `tonans-app` runs on a thread in this process, and a Tauri
//! webview shows the UI it serves. There is one UI and one server, so the window has
//! the same security envelope as the HTTP smoke test exercises: loopback only, a
//! per-launch token, the `Host` and `Origin` checks, and the strict CSP. See
//! `docs/SECURITY.md`.
//!
//! # What this file adds to that envelope
//!
//! * The token reaches the page through an initialization script, not a URL. It is
//!   never printed and never appears in an address, a history entry, or a log.
//! * The webview may only navigate within this launch's loopback origin. Everything
//!   else is refused; a link the user asks to open goes to the operating system.
//! * The page can ask for a native dialog, but it can never name a path or a URL. Paths
//!   come from the dialog the user answers; links come from a fixed list here.
//! * The commands below are granted to this launch's exact origin at launch, and no
//!   plugin command is granted at all.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::CapabilityBuilder;
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Manager, RunEvent, State, Url, WebviewUrl, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tonans_app::server::Server;
use tonans_app::state::AppState;
use tonans_app::{default_workspace, demo};
use tonans_core::error::Error;
use tonans_core::store::{write_atomically, Workspace};
use tonans_core::version::{APP_VERSION, MODEL_VERSION, PRODUCT_NAME, SOURCE_URL, SUPPORT_EMAIL};

const HELP: &str = "\
Tonans — microgrid scenario workbench

  tonans-desktop [--workspace <DIR>]     Open the application window
  tonans-desktop --version
  tonans-desktop --help

The window shows the Tonans interface, served in-process on 127.0.0.1. Nothing is sent
anywhere. Results are planning estimates for human review.

Flintglade — support@flintglade.com
";

/// The one window.
const WINDOW: &str = "main";

/// What the commands need from the running server.
struct Shell {
    /// The server's state: the workspace, and the flag that stops it.
    server: Arc<AppState>,
    /// The thread the server runs on, so exit can wait briefly for it.
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Whether the page holds edits it has not saved. Starts false, so a page that
    /// never loaded can always be closed.
    unsaved: AtomicBool,
    /// Set once the window is going away, so the server stopping is not news.
    exiting: AtomicBool,
}

/// An error, in the shape the UI already shows for API errors.
#[derive(Debug, Serialize)]
struct CommandError {
    code: String,
    message: String,
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        Self {
            code: error.code().to_string(),
            message: error.to_string(),
        }
    }
}

impl CommandError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Close the window, by the same route as its close button.
#[tauri::command]
fn quit(app: AppHandle) -> Result<(), CommandError> {
    let window = app
        .get_webview_window(WINDOW)
        .ok_or_else(|| CommandError::new("desktop.no_window", "the window is already gone"))?;
    window
        .close()
        .map_err(|e| CommandError::new("desktop.close_failed", e.to_string()))
}

/// Record whether the page holds unsaved edits, so closing the window can ask first.
#[tauri::command]
fn set_unsaved(shell: State<'_, Shell>, unsaved: bool) {
    shell.unsaved.store(unsaved, Ordering::SeqCst);
}

/// Save one export artifact wherever the user says.
///
/// Returns the path written, or `None` if the user cancelled. The page names the
/// artifact and the operating-system dialog names the destination: the page never
/// supplies a path. Every name is validated by the workspace, so this cannot read
/// outside it.
#[tauri::command]
async fn save_artifact(
    app: AppHandle,
    shell: State<'_, Shell>,
    project: String,
    key: String,
    artifact: String,
) -> Result<Option<String>, CommandError> {
    let bytes = shell
        .server
        .workspace
        .read_export(&project, &key, &artifact)?;
    let chosen = app
        .dialog()
        .file()
        .set_title(format!("Save {artifact}"))
        .set_file_name(&artifact)
        .blocking_save_file();
    write_chosen(chosen, &bytes)
}

/// Copy a whole export bundle into a new folder inside one the user picks.
///
/// The new folder must not already exist: a bundle is evidence, and copying one over
/// another would leave a mix of two runs under one set of checksums.
#[tauri::command]
async fn save_bundle(
    app: AppHandle,
    shell: State<'_, Shell>,
    project: String,
    key: String,
    artifacts: Vec<String>,
) -> Result<Option<String>, CommandError> {
    // Read everything first, so a bad name fails before anything is written.
    let files = read_bundle(&shell.server.workspace, &project, &key, &artifacts)?;

    let chosen = app
        .dialog()
        .file()
        .set_title("Choose where to save the bundle")
        .blocking_pick_folder();
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let parent = chosen
        .into_path()
        .map_err(|e| CommandError::new("desktop.bad_path", e.to_string()))?;

    let destination = parent.join(bundle_folder_name(&project, &key));
    std::fs::create_dir(&destination).map_err(|e| {
        CommandError::new(
            "desktop.bundle_folder",
            format!(
                "could not create {}: {e}. A bundle is never copied over an existing folder; \
                 choose another location.",
                destination.display()
            ),
        )
    })?;
    for (artifact, bytes) in &files {
        // `read_export` accepted these names, which rules out separators and traversal.
        write_atomically(&destination.join(artifact), bytes)?;
    }
    Ok(Some(destination.display().to_string()))
}

/// Read the named artifacts of a bundle.
///
/// The bundle's own manifest is always read first, so `project` and `key` are validated
/// by the workspace even when `artifacts` is empty. The caller joins them onto a folder
/// the user picked, and `Path::join` with an absolute path discards that folder.
fn read_bundle(
    workspace: &Workspace,
    project: &str,
    key: &str,
    artifacts: &[String],
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    workspace.read_export(project, key, "provenance.json")?;
    artifacts
        .iter()
        .map(|artifact| {
            Ok((
                artifact.clone(),
                workspace.read_export(project, key, artifact)?,
            ))
        })
        .collect()
}

/// Save the blank interval-data template wherever the user says.
#[tauri::command]
async fn save_csv_template(app: AppHandle) -> Result<Option<String>, CommandError> {
    let chosen = app
        .dialog()
        .file()
        .set_title("Save the CSV template")
        .set_file_name(tonans_core::csvimport::TEMPLATE_FILE_NAME)
        .add_filter("CSV", &["csv"])
        .blocking_save_file();
    write_chosen(chosen, tonans_core::csvimport::TEMPLATE_CSV.as_bytes())
}

/// Show an export bundle in the file manager.
#[tauri::command]
fn reveal_bundle(
    shell: State<'_, Shell>,
    project: String,
    key: String,
) -> Result<(), CommandError> {
    let workspace = &shell.server.workspace;
    // Proves the bundle exists, and validates both names, before anything is created.
    workspace.read_export(&project, &key, "provenance.json")?;
    let directory = workspace.export_dir(&project, &key)?;
    // Selecting the file needs the desktop's file-manager service, which not every desktop
    // has. Opening the folder is the next best thing and needs only its default handler.
    if tauri_plugin_opener::reveal_item_in_dir(directory.join("report.html")).is_ok() {
        return Ok(());
    }
    tauri_plugin_opener::open_path(&directory, None::<&str>)
        .map_err(|e| CommandError::new("desktop.reveal_failed", e.to_string()))
}

/// Open one of the application's own links with the operating system's handler.
#[tauri::command]
fn open_link(link: String) -> Result<(), CommandError> {
    let target = link_target(&link).ok_or_else(|| {
        CommandError::new("desktop.unknown_link", format!("no link named '{link}'"))
    })?;
    tauri_plugin_opener::open_url(target, None::<&str>)
        .map_err(|e| CommandError::new("desktop.open_failed", e.to_string()))
}

/// The links the application may open, by name. The page names one; it cannot supply
/// a URL, so it cannot use this to open anything else.
fn link_target(name: &str) -> Option<String> {
    match name {
        "support" => Some(format!("mailto:{SUPPORT_EMAIL}")),
        "source" => Some(SOURCE_URL.to_string()),
        _ => None,
    }
}

fn write_chosen(
    chosen: Option<tauri_plugin_dialog::FilePath>,
    bytes: &[u8],
) -> Result<Option<String>, CommandError> {
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let path = chosen
        .into_path()
        .map_err(|e| CommandError::new("desktop.bad_path", e.to_string()))?;
    write_atomically(&path, bytes)?;
    Ok(Some(path.display().to_string()))
}

/// The folder a saved bundle goes in: readable, and tied to the bundle it holds.
fn bundle_folder_name(project: &str, key: &str) -> String {
    let short: String = key.chars().take(12).collect();
    format!("{project} {short}")
}

// ---------------------------------------------------------------------------
// The webview's boundary
// ---------------------------------------------------------------------------

/// Whether a navigation stays on this launch's server.
fn is_own_origin(url: &Url, origin: &str) -> bool {
    url.origin().ascii_serialization() == origin
}

/// The script that hands the page its token.
///
/// It runs before the page's own script on every load in the main frame, which is what
/// keeps a reload working, and it checks the origin itself so that it cannot give the
/// token to anything else even if a navigation guard were to fail.
fn bootstrap_script(origin: &str, token: &str) -> String {
    let boot = serde_json::json!({ "token": token, "desktop": true });
    let origin = serde_json::Value::String(origin.to_string());
    format!("if (window.location.origin === {origin}) {{ window.__TONANS_BOOT__ = {boot}; }}")
}

/// On the NVIDIA driver, WebKitGTK's DMA-BUF renderer cannot allocate its buffers
/// (`Failed to create GBM buffer`) and the window stays blank under X11; it also crawls
/// under Wayland. Forcing shared-memory buffers fixes both. Every other driver keeps
/// WebKit's defaults, and setting either variable yourself keeps your choice:
/// `WEBKIT_DISABLE_DMABUF_RENDERER=0` is the way to ask for WebKit's own behaviour.
fn webkit_override(
    nvidia_driver: bool,
    disable_dmabuf_set: bool,
    force_shm_set: bool,
) -> Option<(&'static str, &'static str)> {
    (nvidia_driver && !disable_dmabuf_set && !force_shm_set)
        .then_some(("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1"))
}

fn prepare_webview_environment() {
    if let Some((name, value)) = webkit_override(
        std::path::Path::new("/proc/driver/nvidia/version").exists(),
        std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some(),
        std::env::var_os("WEBKIT_DMABUF_RENDERER_FORCE_SHM").is_some(),
    ) {
        // Before any other thread exists, which is what makes this sound.
        std::env::set_var(name, value);
    }
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// Start the server and open the window onto it.
fn open_window(app: &tauri::App, workspace: Option<PathBuf>) -> Result<(), Error> {
    let root = match workspace {
        Some(root) => root,
        None => default_workspace()?,
    };
    let workspace = Workspace::open(root)?;
    // The webview library keeps a small file of its own even in incognito mode. It goes
    // in the workspace, so the one folder documented in docs/PRIVACY.md stays the truth.
    let webview_data = workspace.root().join("webview");
    // The same first-launch demonstration project the command installs, through the
    // same importer.
    demo::ensure_installed(&workspace)?;
    let server = Server::bind(workspace, 0)?;

    let origin = format!("http://{}", server.address);
    let page = Url::parse(&format!("{origin}/")).map_err(|e| Error::UnsafePath {
        code: "server.bad_url",
        message: format!("the server's own address is not a URL: {e}"),
    })?;
    let bootstrap = bootstrap_script(&origin, &server.state.token);
    let state = Arc::clone(&server.state);

    // Granted to this launch's exact origin, port included, and to nothing else.
    let capability = CapabilityBuilder::new("loopback-ui")
        .remote(format!("{origin}/*"))
        .window(WINDOW)
        .permission("allow-quit")
        .permission("allow-set-unsaved")
        .permission("allow-save-artifact")
        .permission("allow-save-bundle")
        .permission("allow-save-csv-template")
        .permission("allow-reveal-bundle")
        .permission("allow-open-link");
    app.add_capability(capability)
        .map_err(|e| Error::UnsafePath {
            code: "desktop.capability",
            message: format!("could not grant the window its commands: {e}"),
        })?;

    let handle = app.handle().clone();
    let thread = std::thread::spawn(move || {
        let outcome = server.serve();
        if let Err(error) = &outcome {
            eprintln!("tonans: [{}] {error}", error.code());
        }
        // The server is the application. If it stopped by itself the window is dead
        // weight, so take the window with it.
        let shell = handle.state::<Shell>();
        if !shell.exiting.load(Ordering::SeqCst) {
            handle.exit(i32::from(outcome.is_err()));
        }
    });

    app.manage(Shell {
        server: state,
        thread: Mutex::new(Some(thread)),
        unsaved: AtomicBool::new(false),
        exiting: AtomicBool::new(false),
    });

    let navigation_origin = origin;
    WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::External(page))
        .title(format!("{PRODUCT_NAME} — Microgrid Scenario Workbench"))
        .inner_size(1360.0, 860.0)
        .min_inner_size(960.0, 600.0)
        // Nothing the webview does is written to disk: no site data, no cache.
        .incognito(true)
        .data_directory(webview_data)
        .initialization_script(bootstrap)
        .on_navigation(move |url| is_own_origin(url, &navigation_origin))
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()
        .map_err(|e| Error::UnsafePath {
            code: "desktop.window",
            message: format!("could not open the window: {e}"),
        })?;
    Ok(())
}

/// Closing the window ends the application; closing it over unsaved edits asks first.
fn on_window_event(window: &Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let Some(shell) = window.try_state::<Shell>() else {
        return;
    };
    if !shell.unsaved.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_close();

    let closing = window.clone();
    window
        .dialog()
        .message("This project has changes you have not saved. They are lost if you quit now.")
        .title("Quit without saving?")
        .kind(MessageDialogKind::Warning)
        .parent(window)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Quit without saving".to_string(),
            "Keep working".to_string(),
        ))
        .show(move |quit_anyway| {
            if quit_anyway {
                if let Some(shell) = closing.try_state::<Shell>() {
                    shell.unsaved.store(false, Ordering::SeqCst);
                }
                let _ = closing.close();
            }
        });
}

/// Stop the server, and give a write in progress a moment to finish.
///
/// Project and export writes are atomic, so a process exiting mid-write leaves the old
/// file intact either way. This is only the courtesy of not abandoning a temporary
/// file. It is bounded, because a long sensitivity study must not hold the app open.
fn stop_server(app: &AppHandle) {
    let Some(shell) = app.try_state::<Shell>() else {
        return;
    };
    shell.exiting.store(true, Ordering::SeqCst);
    shell.server.stop.store(true, Ordering::SeqCst);

    let thread = shell.thread.lock().ok().and_then(|mut slot| slot.take());
    if let Some(thread) = thread {
        for _ in 0..30 {
            if thread.is_finished() {
                let _ = thread.join();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    workspace: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => options.help = true,
                "--version" | "-V" => options.version = true,
                "--workspace" => {
                    let value = args.next().ok_or("--workspace needs a value")?;
                    options.workspace = Some(PathBuf::from(value));
                }
                other => return Err(format!("unknown argument '{other}'. Try --help.")),
            }
        }
        Ok(options)
    }
}

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("tonans-desktop: {message}");
            return ExitCode::FAILURE;
        }
    };
    if options.help {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if options.version {
        println!("{PRODUCT_NAME} {APP_VERSION} (model {MODEL_VERSION})");
        return ExitCode::SUCCESS;
    }

    prepare_webview_environment();

    let context = tauri::generate_context!();
    // Under Wayland, GTK takes the window's app id from the program name, and the desktop
    // uses that id to find the launcher and icon. Tauri's own `enableGTKAppId` would set it
    // too, but it also makes GTK register on the session bus, which stops a second window
    // from opening.
    #[cfg(target_os = "linux")]
    glib::set_prgname(Some(context.config().identifier.as_str()));

    let workspace = options.workspace;
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            quit,
            set_unsaved,
            save_artifact,
            save_bundle,
            save_csv_template,
            reveal_bundle,
            open_link,
        ])
        .on_window_event(on_window_event)
        .setup(move |app| {
            if let Err(error) = open_window(app, workspace.clone()) {
                eprintln!("tonans-desktop: [{}] {error}", error.code());
                let handle = app.handle().clone();
                app.dialog()
                    .message(format!("{error}\n\n[{}]", error.code()))
                    .title("Tonans could not start")
                    .kind(MessageDialogKind::Error)
                    .show(move |_| handle.exit(1));
            }
            Ok(())
        })
        .build(context);

    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("tonans-desktop: {error}");
            return ExitCode::FAILURE;
        }
    };
    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            stop_server(handle);
        }
    });
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_memory_is_forced_on_nvidia_and_never_over_the_users_choice() {
        let force = Some(("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1"));
        assert_eq!(webkit_override(true, false, false), force);
        // Other drivers keep WebKit's defaults.
        assert_eq!(webkit_override(false, false, false), None);
        // Setting either variable, to anything, is the user's choice.
        assert_eq!(webkit_override(true, true, false), None);
        assert_eq!(webkit_override(true, false, true), None);
        assert_eq!(webkit_override(true, true, true), None);
    }

    #[test]
    fn navigation_is_confined_to_this_launchs_origin() {
        let origin = "http://127.0.0.1:43210";
        let allowed = |url: &str| is_own_origin(&Url::parse(url).unwrap(), origin);
        assert!(allowed("http://127.0.0.1:43210/"));
        assert!(allowed("http://127.0.0.1:43210/anything?else#here"));
        assert!(!allowed("http://127.0.0.1:43211/"), "another port");
        assert!(!allowed("https://127.0.0.1:43210/"), "another scheme");
        assert!(!allowed("http://localhost:43210/"), "another host");
        assert!(!allowed("http://127.0.0.1.evil.test:43210/"));
        assert!(!allowed("https://example.com/"));
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("tauri://localhost/"));
    }

    #[test]
    fn the_token_is_handed_over_only_on_our_own_origin() {
        let script = bootstrap_script("http://127.0.0.1:43210", "abc123");
        assert!(script.starts_with(r#"if (window.location.origin === "http://127.0.0.1:43210")"#));
        assert!(script.contains(r#""token":"abc123""#));
    }

    #[test]
    fn the_page_can_only_name_links_it_cannot_supply_them() {
        assert!(link_target("support").unwrap().starts_with("mailto:"));
        assert!(link_target("source")
            .unwrap()
            .starts_with("https://github.com/"));
        assert_eq!(link_target("https://example.com/"), None);
        assert_eq!(link_target("file:///etc/passwd"), None);
        assert_eq!(link_target(""), None);
    }

    #[test]
    fn the_configured_csp_is_the_servers_csp() {
        // Tauri applies this one to anything it serves itself, which is nothing today.
        // Keeping it identical means there is one policy to reason about.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            config["app"]["security"]["csp"].as_str(),
            Some(tonans_app::server::CSP.replace("  ", " ").as_str())
        );
        // No capability file exists, so the only grant is the one made at launch.
        assert_eq!(
            config["app"]["security"]["capabilities"],
            serde_json::json!([])
        );
    }

    #[test]
    fn the_menu_entry_is_named_after_the_app_id() {
        // Under Wayland the desktop connects a window to its launcher and icon through a
        // `.desktop` file named after the app id. If they differ, the window gets a
        // generic icon and the taskbar cannot match it to its launcher.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let identifier = config["identifier"].as_str().unwrap();
        // The include! path itself is half of this check: the file must have this name.
        assert_eq!(identifier, "com.flintglade.tonans");
        let entry = include_str!("../../../packaging/linux/com.flintglade.tonans.desktop");
        assert!(entry.contains(&format!("\nIcon={identifier}\n")), "{entry}");
        assert!(
            entry.contains(&format!("\nStartupWMClass={identifier}\n")),
            "{entry}"
        );
        let installer = include_str!("../../../scripts/install-desktop.sh");
        assert!(installer.contains(&format!("APP_ID=\"{identifier}\"\n")));
    }

    #[cfg(unix)]
    #[test]
    fn scripts_the_readme_runs_directly_are_executable() {
        use std::os::unix::fs::PermissionsExt;
        // A fresh clone gets the mode recorded in git, so in CI this checks the commit.
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/install-desktop.sh"
        );
        let mode = std::fs::metadata(script).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o755,
            0o755,
            "scripts/install-desktop.sh is not executable"
        );
    }

    #[test]
    fn a_bundle_folder_name_cannot_carry_a_path_out_of_the_chosen_folder() {
        let root = std::env::temp_dir().join(format!(
            "tonans-desktop-test-{}-{}",
            std::process::id(),
            tonans_app::token::opaque_id()
        ));
        let workspace = Workspace::open(&root).unwrap();
        let directory = workspace.export_dir("Site", "abc123").unwrap();
        std::fs::write(directory.join("provenance.json"), b"{}").unwrap();

        // Even with no artifacts named, a hostile project or key is refused, before the
        // name reaches `Path::join`.
        for (project, key) in [
            ("/tmp/tonans-escape", "abc123"),
            ("../../tonans-escape", "abc123"),
            ("Site", "/tmp/tonans-escape"),
            ("Site", "../.."),
            ("Site", "missing"),
        ] {
            assert!(
                read_bundle(&workspace, project, key, &[]).is_err(),
                "{project} {key}"
            );
        }
        assert!(read_bundle(&workspace, "Site", "abc123", &["/etc/passwd".to_string()]).is_err());
        assert!(read_bundle(&workspace, "Site", "abc123", &["../x".to_string()]).is_err());

        let files = read_bundle(
            &workspace,
            "Site",
            "abc123",
            &["provenance.json".to_string()],
        )
        .unwrap();
        assert_eq!(files[0].1, b"{}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_saved_bundle_folder_is_named_after_its_project_and_key() {
        assert_eq!(
            bundle_folder_name("Warehouse 3", "0123456789abcdef"),
            "Warehouse 3 0123456789ab"
        );
    }

    #[test]
    fn arguments_are_parsed_and_unknown_ones_refused() {
        let parse = |args: &[&str]| Options::parse(args.iter().map(|s| (*s).to_string()));
        assert_eq!(parse(&[]).unwrap(), Options::default());
        assert_eq!(
            parse(&["--workspace", "W"]).unwrap().workspace,
            Some(PathBuf::from("W"))
        );
        assert!(parse(&["--version"]).unwrap().version);
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["--workspace"]).is_err());
        // The browser-launching flag of the old command never existed here.
        assert!(parse(&["--no-browser"]).is_err());
    }
}
