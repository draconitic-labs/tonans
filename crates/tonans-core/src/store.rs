//! The local workspace: where projects and exports live on disk.
//!
//! Tonans writes only inside a workspace root the user chose at launch. Nothing
//! in the HTTP API takes a path — projects are addressed by *name*, and the name
//! is validated by [`crate::paths`] before it is ever joined to anything. That is
//! the whole of the path-traversal defence, and it is a defence by construction
//! rather than by sanitisation.
//!
//! # Crash-safe writes
//!
//! A project is written to a temporary file in the same directory, flushed to the
//! platform with `sync_all`, and only then renamed over the target. `rename` is
//! atomic within a volume on both NTFS and every POSIX filesystem, so a reader —
//! or a power cut — sees either the whole old file or the whole new one. A partial
//! project is never observable.
//!
//! The temporary file is removed if any step fails, so a crashed save leaves no
//! debris to confuse the next listing.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::limits;
use crate::paths::{confine, project_file_name, safe_name};
use crate::schema::{migrate, Project};

/// The suffix every project file carries. Chosen by Tonans, never by the user.
const PROJECT_SUFFIX: &str = ".tonans.json";

/// A workspace on disk.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    projects: PathBuf,
    exports: PathBuf,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One project, as seen from a directory listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// The project's name, which is also how it is addressed.
    pub name: String,
    /// Size of the project file.
    pub size_bytes: u64,
    /// When the project was last written, according to the project itself rather
    /// than the filesystem — a copied file keeps its real modification time.
    pub modified_utc: DateTime<Utc>,
    /// Whether the project has data imported yet.
    pub has_dataset: bool,
    /// How many scenarios it holds.
    pub scenario_count: usize,
}

impl Workspace {
    /// Open a workspace, creating its directories if they do not exist.
    ///
    /// The root must be a directory the user named. Tonans never invents one
    /// outside the location the application was launched with.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .map_err(|e| Error::io(format!("creating the workspace at {}", root.display()), e))?;
        let root = root
            .canonicalize()
            .map_err(|e| Error::io(format!("resolving the workspace at {}", root.display()), e))?;
        let projects = create_verified_child_directory(&root, "projects")?;
        let exports = create_verified_child_directory(&root, "exports")?;
        Ok(Self {
            root,
            projects,
            exports,
        })
    }

    /// The workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where projects live.
    #[must_use]
    pub fn projects_dir(&self) -> PathBuf {
        self.projects.clone()
    }

    /// Where exported bundles live.
    #[must_use]
    pub fn exports_dir(&self) -> PathBuf {
        self.exports.clone()
    }

    /// Every project in the workspace, ordered by name.
    ///
    /// A file that is not a readable project is skipped rather than failing the
    /// listing: one corrupt file must not make the others unreachable.
    pub fn list_projects(&self) -> Result<Vec<ProjectEntry>> {
        #[derive(Deserialize)]
        struct Probe {
            name: String,
            modified_utc: DateTime<Utc>,
            scenarios: Vec<serde_json::Value>,
            #[serde(default)]
            dataset: Option<serde_json::Value>,
        }

        let mut entries = Vec::new();
        let projects = verified_child_directory(&self.root, &self.projects, "projects")?;
        let directory = match fs::read_dir(&projects) {
            Ok(directory) => directory,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(e) => return Err(Error::io("listing projects", e)),
        };

        for entry in directory {
            let entry = entry.map_err(|e| Error::io("listing projects", e))?;
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !file_name.ends_with(PROJECT_SUFFIX) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| Error::io("reading project metadata", e))?;
            if metadata_is_reparse(&metadata)
                || !metadata.is_file()
                || metadata.len() > limits::MAX_PROJECT_BYTES as u64
            {
                continue;
            }
            let Ok(path) = verified_existing_file(&projects, &path, "project") else {
                continue;
            };
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(probe) = serde_json::from_str::<Probe>(&text) else {
                continue;
            };
            let Some(expected_name) = file_name.strip_suffix(PROJECT_SUFFIX) else {
                continue;
            };
            if probe.name != expected_name {
                continue;
            }
            entries.push(ProjectEntry {
                name: probe.name,
                size_bytes: metadata.len(),
                modified_utc: probe.modified_utc,
                has_dataset: probe.dataset.is_some(),
                scenario_count: probe.scenarios.len(),
            });
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The path a project with this name is stored at.
    pub fn project_path(&self, name: &str) -> Result<PathBuf> {
        let file_name = project_file_name(name)?;
        let projects = verified_child_directory(&self.root, &self.projects, "projects")?;
        let path = confine(&projects, &file_name)?;
        if path.exists() {
            verified_existing_file(&projects, &path, "project")
        } else {
            Ok(path)
        }
    }

    /// Whether a project with this name exists.
    pub fn project_exists(&self, name: &str) -> Result<bool> {
        let path = self.project_path(name)?;
        Ok(path.is_file())
    }

    /// Read a project.
    pub fn load_project(&self, name: &str) -> Result<Project> {
        let path = self.project_path(name)?;
        let metadata = fs::metadata(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Schema {
                    code: "schema.project_not_found",
                    message: format!("no project named '{name}' is in this workspace"),
                }
            } else {
                Error::io(format!("opening the project '{name}'"), e)
            }
        })?;
        if metadata.len() > limits::MAX_PROJECT_BYTES as u64 {
            return Err(Error::limit(
                "limit.project_bytes",
                limits::MAX_PROJECT_BYTES as u64,
                metadata.len(),
                format!("the project file for '{name}' is larger than Tonans will open"),
            ));
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| Error::io(format!("reading the project '{name}'"), e))?;
        let project = migrate(&text)?;
        if project.name != name {
            return Err(Error::Schema {
                code: "schema.project_name_mismatch",
                message: format!(
                    "the project file named '{name}' identifies itself as '{}'. Tonans will not \
                     load or rewrite a project under a different name.",
                    project.name
                ),
            });
        }
        Ok(project)
    }

    /// Write a project, atomically.
    ///
    /// The project's own `name` decides the file it lands in, so a rename is a
    /// copy-then-delete by the user, not a silent overwrite by us.
    pub fn save_project(&self, project: &Project) -> Result<PathBuf> {
        project.validate()?;
        let path = self.project_path(&project.name)?;

        // Pretty-printed: a project file is something a human may want to read, or
        // diff, or check into version control alongside their own notes.
        let json = serde_json::to_string_pretty(project)
            .map_err(|e| Error::json("serializing the project", e))?;
        if json.len() > limits::MAX_PROJECT_BYTES {
            return Err(Error::limit(
                "limit.project_bytes",
                limits::MAX_PROJECT_BYTES as u64,
                json.len() as u64,
                "this project is larger than Tonans will write",
            ));
        }

        write_atomically(&path, json.as_bytes())?;
        Ok(path)
    }

    /// The directory one export bundle is written to, created if needed.
    ///
    /// Bundles are keyed by all artifact-determining inputs, including the run
    /// metadata and sensitivities. An identical export reuses a verified bundle;
    /// a changed input gets a different key. Existing modified files are preserved.
    pub fn export_dir(&self, project_name: &str, run_key: &str) -> Result<PathBuf> {
        let project = safe_name("project name", project_name)?;
        let key = safe_name("run key", run_key)?;
        let exports = verified_child_directory(&self.root, &self.exports, "exports")?;
        let directory = create_verified_child_directory(&exports, &project)?;
        create_verified_child_directory(&directory, &key)
    }

    /// Read one artifact out of an export bundle.
    ///
    /// Every component is validated, so this cannot be walked out of the workspace
    /// even if a caller passes attacker-controlled strings for all three.
    pub fn read_export(
        &self,
        project_name: &str,
        run_key: &str,
        artifact: &str,
    ) -> Result<Vec<u8>> {
        let project = safe_name("project name", project_name)?;
        let key = safe_name("run key", run_key)?;
        let file = safe_name("artifact name", artifact)?;

        // Asking for a bundle that does not exist is the caller's mistake, not a failure
        // of the machine, at whichever level of the path it turns out to be missing.
        let missing = |error: Error| match error {
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                Error::Schema {
                    code: "schema.artifact_not_found",
                    message: format!("no artifact named '{artifact}' is in that export"),
                }
            }
            other => other,
        };

        let exports =
            verified_child_directory(&self.root, &self.exports, "exports").map_err(missing)?;
        let directory =
            verified_child_directory(&exports, &exports.join(&project), "export project")
                .map_err(missing)?;
        let bundle = verified_child_directory(&directory, &directory.join(&key), "export bundle")
            .map_err(missing)?;
        let path = confine(&bundle, &file)?;
        let path = verified_existing_file(&bundle, &path, "export artifact").map_err(missing)?;

        fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Schema {
                    code: "schema.artifact_not_found",
                    message: format!("no artifact named '{artifact}' is in that export"),
                }
            } else {
                Error::io("reading an export artifact", e)
            }
        })
    }
}

fn create_verified_child_directory(parent: &Path, name: &str) -> Result<PathBuf> {
    safe_name("directory name", name)?;
    let candidate = parent.join(name);
    match fs::create_dir(&candidate) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(Error::io(
                format!("creating directory {}", candidate.display()),
                error,
            ));
        }
    }
    verified_child_directory(parent, &candidate, name)
}

fn verified_child_directory(parent: &Path, candidate: &Path, label: &str) -> Result<PathBuf> {
    let parent = parent
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving directory {}", parent.display()), e))?;
    let metadata = fs::symlink_metadata(candidate)
        .map_err(|e| Error::io(format!("opening {label} directory"), e))?;
    if metadata_is_reparse(&metadata) || !metadata.is_dir() {
        return Err(Error::UnsafePath {
            code: "path.reparse_directory",
            message: format!(
                "the {label} directory is not a direct physical directory inside the Tonans workspace"
            ),
        });
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving {label} directory"), e))?;
    if resolved.parent() != Some(parent.as_path()) {
        return Err(Error::UnsafePath {
            code: "path.escapes_workspace",
            message: format!("the {label} directory resolves outside its expected parent"),
        });
    }
    Ok(resolved)
}

fn verified_existing_file(parent: &Path, candidate: &Path, label: &str) -> Result<PathBuf> {
    let parent = parent
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving directory {}", parent.display()), e))?;
    let metadata =
        fs::symlink_metadata(candidate).map_err(|e| Error::io(format!("opening {label}"), e))?;
    if metadata_is_reparse(&metadata) || !metadata.is_file() {
        return Err(Error::UnsafePath {
            code: "path.reparse_file",
            message: format!("the {label} is not a regular file inside the Tonans workspace"),
        });
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving {label}"), e))?;
    if resolved.parent() != Some(parent.as_path()) {
        return Err(Error::UnsafePath {
            code: "path.escapes_workspace",
            message: format!("the {label} resolves outside its expected directory"),
        });
    }
    Ok(resolved)
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Write `bytes` to `path` such that a reader sees all of it or none of it.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or(Error::UnsafePath {
        code: "path.no_parent",
        message: "that path has no parent directory".to_string(),
    })?;
    fs::create_dir_all(parent).map_err(|e| Error::io("creating the target directory", e))?;
    // The temporary lives beside the target, so the rename stays within one volume
    // and is therefore atomic. A temp file in %TEMP% could be on another volume,
    // where `rename` degrades to a copy and loses the guarantee.
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::UnsafePath {
            code: "path.invalid_file_name",
            message: "the target has no usable file name".to_string(),
        })?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.tonans-tmp-{}-{sequence}",
        std::process::id()
    ));

    let outcome = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| Error::io(format!("creating {}", temporary.display()), e))?;
        file.write_all(bytes)
            .map_err(|e| Error::io(format!("writing {}", temporary.display()), e))?;
        // Without this, `rename` can be ordered before the data reaches the disk,
        // and a power cut leaves a correctly-named file full of zeroes.
        file.sync_all()
            .map_err(|e| Error::io(format!("flushing {}", temporary.display()), e))?;
        drop(file);

        replace_file(&temporary, path)
            .map_err(|e| Error::io(format!("replacing {}", path.display()), e))
    })();

    if outcome.is_err() {
        // Best effort: a failed save must not leave debris behind.
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

/// Rename over `path`, tolerating the moments Windows refuses because another thread or a
/// scanner holds the target open. Every other platform and error fails at once.
fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    let mut attempts = 0_u64;
    loop {
        match fs::rename(temporary, path) {
            Err(error)
                if cfg!(windows)
                    && attempts < 10
                    && error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5 * attempts));
            }
            outcome => return outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Site;
    use chrono::TimeZone;
    use std::sync::{Arc, Barrier};

    /// Windows briefly refuses to open a file while another thread renames over it.
    fn read_while_replaced(path: &Path) -> Vec<u8> {
        for _ in 0..50 {
            match fs::read(path) {
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                outcome => return outcome.unwrap(),
            }
        }
        panic!("{} stayed unreadable", path.display());
    }

    fn temp_root(label: &str) -> PathBuf {
        // Inside the crate's own target directory: the test suite never writes
        // outside the repository.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(label);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn project(name: &str) -> Project {
        Project::new("p1", name, Site::default(), now()).unwrap()
    }

    #[test]
    fn a_project_round_trips_through_the_workspace() {
        let workspace = Workspace::open(temp_root("round-trip")).unwrap();
        let project = project("Warehouse 3");

        assert!(!workspace.project_exists("Warehouse 3").unwrap());
        workspace.save_project(&project).unwrap();
        assert!(workspace.project_exists("Warehouse 3").unwrap());

        let loaded = workspace.load_project("Warehouse 3").unwrap();
        assert_eq!(loaded, project);

        let entries = workspace.list_projects().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Warehouse 3");
        assert_eq!(entries[0].scenario_count, 1);
        assert!(!entries[0].has_dataset);
    }

    #[test]
    fn a_missing_project_is_a_clean_error_not_an_io_panic() {
        let workspace = Workspace::open(temp_root("missing")).unwrap();
        let error = workspace.load_project("Nothing").unwrap_err();
        assert_eq!(error.code(), "schema.project_not_found");
    }

    #[test]
    fn a_project_file_cannot_impersonate_another_name_or_the_reserved_demo() {
        let workspace = Workspace::open(temp_root("name-mismatch")).unwrap();
        let demo = project("Demo - Riverside Works");
        let json = serde_json::to_vec_pretty(&demo).unwrap();
        fs::write(workspace.projects_dir().join("Shadow.tonans.json"), &json).unwrap();

        let error = workspace.load_project("Shadow").unwrap_err();
        assert_eq!(error.code(), "schema.project_name_mismatch");
        assert!(workspace.list_projects().unwrap().is_empty());
        assert_eq!(
            fs::read(workspace.projects_dir().join("Shadow.tonans.json")).unwrap(),
            json
        );
        assert!(!workspace.project_exists("Demo - Riverside Works").unwrap());
    }

    #[test]
    fn hostile_project_names_never_reach_the_filesystem() {
        let workspace = Workspace::open(temp_root("hostile")).unwrap();
        for name in [
            "../../escape",
            "..\\..\\escape",
            "C:\\Windows\\System32\\config",
            "NUL",
            "con",
            "name\0",
            "name\u{202E}gpj.exe",
            "",
        ] {
            assert!(
                workspace.project_path(name).is_err(),
                "{name:?} was not rejected"
            );
            assert!(
                workspace.load_project(name).is_err(),
                "{name:?} was not rejected"
            );
        }
    }

    #[test]
    fn an_export_artifact_cannot_be_walked_out_of_the_workspace() {
        let workspace = Workspace::open(temp_root("export-escape")).unwrap();
        let bundle = workspace.export_dir("Site", "abcdef").unwrap();
        fs::write(bundle.join("report.html"), b"<html></html>").unwrap();

        assert_eq!(
            workspace
                .read_export("Site", "abcdef", "report.html")
                .unwrap(),
            b"<html></html>"
        );
        for artifact in [
            "../../../evil",
            "..\\..\\evil",
            "C:\\Windows\\win.ini",
            "NUL",
        ] {
            assert!(
                workspace.read_export("Site", "abcdef", artifact).is_err(),
                "{artifact:?} was not rejected"
            );
        }
        assert!(workspace
            .read_export("../Site", "abcdef", "report.html")
            .is_err());
        assert!(workspace
            .read_export("Site", "../abcdef", "report.html")
            .is_err());
    }

    #[test]
    fn an_atomic_write_replaces_the_old_file_whole() {
        let root = temp_root("atomic");
        let path = root.join("file.txt");
        write_atomically(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_atomically(&path, b"second, and longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second, and longer");

        // No temporary is left behind.
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tonans-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a temporary file was left behind");
    }

    #[test]
    fn concurrent_atomic_writes_never_publish_mixed_bytes() {
        let root = temp_root("atomic-concurrent");
        let path = Arc::new(root.join("project.json"));
        let first = br#"{"version":1,"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_vec();
        let second =
            br#"{"version":2,"padding":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#.to_vec();
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();

        for index in 0..8 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let expected_first = first.clone();
            let expected_second = second.clone();
            let bytes = if index % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    write_atomically(&path, &bytes).unwrap();
                    let observed = read_while_replaced(&path);
                    assert!(observed == expected_first || observed == expected_second);
                    serde_json::from_slice::<serde_json::Value>(&observed).unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("tonans-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files remain: {leftovers:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_projects_junction_is_rejected_even_when_planted_after_open() {
        let root = temp_root("junction-root");
        let outside = temp_root("junction-outside");
        let workspace = Workspace::open(&root).unwrap();
        fs::remove_dir(workspace.projects_dir()).unwrap();

        let projects_text = workspace.projects_dir().to_string_lossy().into_owned();
        let outside_text = outside
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let projects_cmd = projects_text
            .strip_prefix(r"\\?\")
            .unwrap_or(&projects_text);
        let outside_cmd = outside_text.strip_prefix(r"\\?\").unwrap_or(&outside_text);
        let command = format!("mklink /J {projects_cmd} {outside_cmd}");
        let status = std::process::Command::new("cmd")
            .args(["/d", "/c", &command])
            .status()
            .unwrap();
        assert!(
            status.success(),
            "could not create the junction fixture with: {command}"
        );

        let error = workspace.project_path("Escape").unwrap_err();
        assert_eq!(error.code(), "path.reparse_directory");
        fs::remove_dir(workspace.projects_dir()).unwrap();
    }

    #[test]
    fn an_unreadable_file_does_not_break_the_listing() {
        let workspace = Workspace::open(temp_root("corrupt")).unwrap();
        workspace.save_project(&project("Good")).unwrap();
        fs::write(
            workspace.projects_dir().join("Broken.tonans.json"),
            b"{ not json",
        )
        .unwrap();
        fs::write(workspace.projects_dir().join("notes.txt"), b"ignore me").unwrap();

        let entries = workspace.list_projects().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Good");
    }

    #[test]
    fn projects_are_listed_in_a_stable_order() {
        let workspace = Workspace::open(temp_root("order")).unwrap();
        for name in ["Zulu", "Alpha", "Mike"] {
            workspace.save_project(&project(name)).unwrap();
        }
        let names: Vec<String> = workspace
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "Mike", "Zulu"]);
    }
}
