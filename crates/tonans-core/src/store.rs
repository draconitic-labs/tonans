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

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canonical::hash_bytes;
use crate::error::{Error, Result};
use crate::limits;
use crate::paths::{confine, project_file_name, safe_name};
use crate::schema::{migrate, Project};

/// The suffix every project file carries. Chosen by Tonans, never by the user.
const PROJECT_SUFFIX: &str = ".tonans.json";

/// What a save names as its revision once it has been told the project's file has gone.
/// Not a digest, so it can never match a real file.
pub const REVISION_FILE_GONE: &str = "gone";

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
        self.load_project_at_revision(name)
            .map(|(project, _)| project)
    }

    /// Read a project, with the revision of the exact bytes that were read.
    ///
    /// A revision is the digest of the project file, so it needs no field in the schema,
    /// changes with any write by any process, and cannot collide the way a timestamp can.
    /// Two windows on one workspace are two processes, and this is how they notice each
    /// other.
    pub fn load_project_at_revision(&self, name: &str) -> Result<(Project, String)> {
        let bytes = self.read_project_bytes(name)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| Error::Schema {
            code: "schema.unreadable",
            message: format!("the project file for '{name}' is not UTF-8: {e}"),
        })?;
        let project = migrate(text)?;
        if project.name != name {
            return Err(Error::Schema {
                code: "schema.project_name_mismatch",
                message: format!(
                    "the project file named '{name}' identifies itself as '{}'.",
                    project.name
                ),
            });
        }
        Ok((project, hash_bytes(&bytes).bare_hex().to_string()))
    }

    /// The revision of a project on disk, without parsing it. Cheap enough for a window to
    /// ask whenever it comes back into focus.
    pub fn project_revision(&self, name: &str) -> Result<String> {
        let bytes = self.read_project_bytes(name)?;
        Ok(hash_bytes(&bytes).bare_hex().to_string())
    }

    fn read_project_bytes(&self, name: &str) -> Result<Vec<u8>> {
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
                format!("the project file for '{name}' is over the size limit"),
            ));
        }
        fs::read(&path).map_err(|e| Error::io(format!("reading the project '{name}'"), e))
    }

    /// Hold the workspace's write lock until the returned guard drops.
    ///
    /// Reading a revision and then writing is one operation only if nothing else can
    /// write in between, and "anything else" includes another window, which is another
    /// process. An advisory lock on a file in the workspace root covers both.
    pub fn lock_for_write(&self) -> Result<WriteLock> {
        let path = self.root.join(".tonans-write-lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| metadata_is_reparse(&m)) {
            return Err(Error::UnsafePath {
                code: "path.reparse_lock",
                message: "the workspace's lock file is a link".to_string(),
            });
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::io("opening the workspace's lock file", e))?;
        file.lock()
            .map_err(|e| Error::io("locking the workspace for writing", e))?;
        Ok(WriteLock { _file: file })
    }

    /// Write a project only if the file on disk is still at `expected_revision`, and
    /// return the new revision.
    ///
    /// A stale window gets [`Error::Conflict`] and writes nothing, so it cannot put back
    /// what it last read over what another window has saved since.
    ///
    /// If the file has gone, a save is told so, and goes through once it names
    /// [`REVISION_FILE_GONE`], so a project on screen always has somewhere to go.
    pub fn save_project_at_revision(
        &self,
        project: &Project,
        expected_revision: &str,
    ) -> Result<String> {
        let _lock = self.lock_for_write()?;
        // Only "not found" is a file that has gone. A folder that cannot be read says nothing
        // about the file inside it.
        let exists = match fs::metadata(self.project_path(&project.name)?) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(Error::io(
                    format!(
                        "the projects folder cannot be read, so '{}' was not saved",
                        project.name
                    ),
                    e,
                ))
            }
        };
        let changed = || Error::Conflict {
            code: "conflict.project_changed",
            message: format!(
                "'{}' was changed somewhere else after this window read it, probably in \
                 another Tonans window. Nothing was saved.",
                project.name
            ),
        };
        match (exists, expected_revision == REVISION_FILE_GONE) {
            (false, false) => {
                return Err(Error::Conflict {
                    code: "conflict.project_missing",
                    message: format!(
                        "the file for '{}' has gone from the workspace, moved or deleted outside \
                         Tonans. Nothing was saved. Saving again writes what is on screen back to \
                         the workspace.",
                        project.name
                    ),
                })
            }
            (false, true) => {}
            (true, true) => return Err(changed()),
            // By digest alone: a file that no longer parses is still a file that changed.
            (true, false) => {
                if self.project_revision(&project.name)? != expected_revision {
                    return Err(changed());
                }
            }
        }
        self.write_project(project)
    }

    /// Write a project, atomically.
    ///
    /// The project's own `name` decides the file it lands in, so a rename is a
    /// copy-then-delete by the user, not a silent overwrite by us.
    pub fn save_project(&self, project: &Project) -> Result<PathBuf> {
        self.write_project(project)?;
        self.project_path(&project.name)
    }

    /// Validate and write a project, and return the revision written.
    pub fn write_project(&self, project: &Project) -> Result<String> {
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
                "this project is over the size limit",
            ));
        }

        // An atomic write replaces the file, so it would go straight past a read-only mark.
        if fs::metadata(&path).is_ok_and(|metadata| metadata.permissions().readonly()) {
            return Err(Error::UnsafePath {
                code: "path.read_only",
                message: format!(
                    "the file for '{}' is read-only, so nothing was saved. Make it writable, then \
                     save again.",
                    project.name
                ),
            });
        }

        write_atomically(&path, json.as_bytes())?;
        Ok(hash_bytes(json.as_bytes()).bare_hex().to_string())
    }

    /// The directory one export bundle is written to, created if needed.
    ///
    /// Bundles are keyed by everything that determines their bytes, including the time of
    /// the run they hold, so each export through the application is a new bundle in a new
    /// directory. Existing files in a directory are preserved, never overwritten.
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
                "the {label} directory is not a direct physical directory inside the workspace"
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
            message: format!("the {label} is not a regular file inside the workspace"),
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

/// The workspace's write lock. Released when this drops.
#[derive(Debug)]
pub struct WriteLock {
    _file: File,
}

/// Write `bytes` over a file the user chose, in place in every way they would notice.
///
/// A dialog answered "replace", which is permission to change the contents and nothing
/// else. So an existing file keeps its permissions, a link stays a link and its target
/// gets the bytes, and a read-only file is refused. The write itself is still atomic.
pub fn write_user_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|e| Error::io(format!("following the link at {}", path.display()), e))?,
        _ => path.to_path_buf(),
    };
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => {
            if metadata.permissions().readonly() {
                return Err(Error::UnsafePath {
                    code: "path.read_only",
                    message: format!("{} is read-only, so it was not changed", target.display()),
                });
            }
            Some(metadata.permissions())
        }
        Err(_) => None,
    };
    write_with_permissions(&target, bytes, permissions)
}

/// Write `bytes` to `path` such that a reader sees all of it or none of it.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    write_with_permissions(path, bytes, None)
}

fn write_with_permissions(
    path: &Path,
    bytes: &[u8],
    permissions: Option<Permissions>,
) -> Result<()> {
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
        // Before the rename, so the file is never visible at the wrong permissions.
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)
                .map_err(|e| Error::io(format!("setting up {}", temporary.display()), e))?;
        }
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

    fn revision_project(name: &str) -> Project {
        let now = chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        Project::new("p", name, crate::schema::Site::default(), now).unwrap()
    }

    fn revision_workspace(label: &str) -> (Workspace, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "tonans-store-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        (Workspace::open(&root).unwrap(), root)
    }

    #[test]
    fn a_stale_revision_is_refused_and_writes_nothing() {
        let (workspace, root) = revision_workspace("stale");
        let mut project = revision_project("Site");
        let first = workspace.write_project(&project).unwrap();
        assert_eq!(workspace.load_project_at_revision("Site").unwrap().1, first);

        project.notes = "first window".to_string();
        let second = workspace
            .save_project_at_revision(&project, &first)
            .unwrap();
        assert_ne!(first, second);

        project.notes = "second window, still holding the first revision".to_string();
        let refused = workspace
            .save_project_at_revision(&project, &first)
            .unwrap_err();
        assert_eq!(refused.code(), "conflict.project_changed");
        assert_eq!(refused.http_status(), 409);
        let (on_disk, current) = workspace.load_project_at_revision("Site").unwrap();
        assert_eq!(on_disk.notes, "first window");
        assert_eq!(current, second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_revision_is_the_digest_of_the_file_so_any_writer_changes_it() {
        let (workspace, root) = revision_workspace("digest");
        let project = revision_project("Site");
        let revision = workspace.write_project(&project).unwrap();
        // Any other writer: a second process, a text editor, a restored backup.
        let path = workspace.project_path("Site").unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push('\n');
        fs::write(&path, text).unwrap();
        assert_ne!(
            workspace.load_project_at_revision("Site").unwrap().1,
            revision
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_write_lock_makes_a_second_taker_wait() {
        let (workspace, root) = revision_workspace("lock");
        let first = workspace.lock_for_write().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let other = workspace.clone();
        let waiter = std::thread::spawn(move || {
            let _second = other.lock_for_write().unwrap();
            sender.send(()).unwrap();
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "the lock was taken twice"
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the lock was never released");
        waiter.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_project_whose_file_has_gone_can_be_saved_once_it_says_it_knows() {
        let (workspace, root) = revision_workspace("gone");
        let project = revision_project("Gone");
        let revision = workspace.write_project(&project).unwrap();
        let path = workspace.project_path("Gone").unwrap();
        let moved = root.join("moved-away.json");
        fs::rename(&path, &moved).unwrap();

        let refused = workspace
            .save_project_at_revision(&project, &revision)
            .unwrap_err();
        assert_eq!(refused.code(), "conflict.project_missing");
        assert_eq!(refused.http_status(), 409);
        assert!(!path.exists(), "told, and not written");

        let written = workspace
            .save_project_at_revision(&project, REVISION_FILE_GONE)
            .unwrap();
        assert!(path.exists());
        assert_eq!(workspace.project_revision("Gone").unwrap(), written);

        // Once it is back, the same promise is no longer true.
        let refused = workspace
            .save_project_at_revision(&project, REVISION_FILE_GONE)
            .unwrap_err();
        assert_eq!(refused.code(), "conflict.project_changed");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_projects_folder_is_reported_as_that_and_not_as_a_file_gone() {
        use std::os::unix::fs::PermissionsExt;
        let (workspace, root) = revision_workspace("unreadable");
        let project = revision_project("Shut");
        let revision = workspace.write_project(&project).unwrap();
        let path = workspace.project_path("Shut").unwrap();
        let folder = path.parent().unwrap();
        let open = fs::metadata(folder).unwrap().permissions();
        fs::set_permissions(folder, fs::Permissions::from_mode(0o000)).unwrap();

        let refused = workspace.save_project_at_revision(&project, &revision);
        let again = workspace.save_project_at_revision(&project, REVISION_FILE_GONE);
        fs::set_permissions(folder, open).unwrap();

        for result in [refused, again] {
            let error = result.unwrap_err();
            assert_eq!(error.code(), "io.failed", "{error}");
            let message = error.to_string();
            assert!(message.contains("cannot be read"), "{message}");
            assert!(!message.contains("moved or deleted"), "{message}");
        }
        assert_eq!(workspace.project_revision("Shut").unwrap(), revision);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_save_over_a_file_that_no_longer_parses_is_a_conflict_not_a_schema_error() {
        let (workspace, root) = revision_workspace("garbled");
        let project = revision_project("Garbled");
        let revision = workspace.write_project(&project).unwrap();
        let path = workspace.project_path("Garbled").unwrap();
        fs::write(&path, "not json").unwrap();
        let refused = workspace
            .save_project_at_revision(&project, &revision)
            .unwrap_err();
        assert_eq!(refused.code(), "conflict.project_changed");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_save_over_a_read_only_project_file_is_refused_clearly() {
        let (workspace, root) = revision_workspace("readonly");
        let project = revision_project("Locked");
        let revision = workspace.write_project(&project).unwrap();
        let path = workspace.project_path("Locked").unwrap();
        let before = fs::read(&path).unwrap();
        let writable = fs::metadata(&path).unwrap().permissions();
        let mut read_only = writable.clone();
        read_only.set_readonly(true);
        fs::set_permissions(&path, read_only).unwrap();

        let refused = workspace
            .save_project_at_revision(&project, &revision)
            .unwrap_err();
        assert_eq!(refused.code(), "path.read_only");
        assert!(refused.to_string().contains("read-only"), "{refused}");
        assert_eq!(fs::read(&path).unwrap(), before);

        // The test's own folder has to be removable on Windows.
        fs::set_permissions(&path, writable).unwrap();
        workspace
            .save_project_at_revision(&project, &revision)
            .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_chosen_file_keeps_its_permissions_and_a_link_stays_a_link() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (_workspace, root) = revision_workspace("chosen");

        let private = root.join("private.csv");
        fs::write(&private, "old").unwrap();
        fs::set_permissions(&private, Permissions::from_mode(0o600)).unwrap();
        write_user_file(&private, b"new").unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let target = root.join("target.csv");
        fs::write(&target, "old").unwrap();
        let link = root.join("link.csv");
        symlink(&target, &link).unwrap();
        write_user_file(&link, b"through the link").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"through the link");

        let locked = root.join("locked.csv");
        fs::write(&locked, "old").unwrap();
        fs::set_permissions(&locked, Permissions::from_mode(0o444)).unwrap();
        let refused = write_user_file(&locked, b"new").unwrap_err();
        assert_eq!(refused.code(), "path.read_only");
        assert_eq!(fs::read(&locked).unwrap(), b"old");

        let fresh = root.join("fresh.csv");
        write_user_file(&fresh, b"made").unwrap();
        assert_eq!(fs::read(&fresh).unwrap(), b"made");

        // Nothing but the files named above: no temporary left behind.
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains("tonans-tmp"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
        fs::remove_dir_all(root).unwrap();
    }
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
