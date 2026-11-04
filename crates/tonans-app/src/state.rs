//! Server state: the workspace, the launch token, and the one
//! staged upload slot.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tonans_core::error::{Error, Result};
use tonans_core::limits;
use tonans_core::store::Workspace;

use crate::clock;

/// The name of the bundled demonstration project.
///
/// Reserved: a user project may not take it. The demo exists so that a first launch
/// has something to open.
pub const DEMO_PROJECT_NAME: &str = "Demo - Riverside Works";

/// Requests allowed in one window. Generous for a human driving a browser, and low
/// enough that a runaway script in a page cannot spin the engine.
const RATE_LIMIT_REQUESTS: u32 = 600;
/// The window [`RATE_LIMIT_REQUESTS`] applies over.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);

/// A CSV waiting to be mapped and imported.
///
/// Exactly one at a time, held in memory and never written to disk. A second upload
/// evicts the first. This is what lets the mapping screen show the file's real
/// headers before anything is interpreted — and it means an abandoned import leaves
/// nothing behind.
#[derive(Debug)]
pub struct StagedUpload {
    /// Opaque handle the import step refers back to.
    pub id: String,
    /// The file's name, already sanitized.
    pub filename: String,
    /// The exact bytes uploaded.
    pub bytes: Vec<u8>,
    /// Whether these are the bundled synthetic sample, which an import must say.
    pub synthetic: bool,
}

/// Everything a request handler can reach.
#[derive(Debug)]
pub struct AppState {
    /// Where projects and exports live.
    pub workspace: Workspace,
    /// The bearer token minted at launch. Required by every `/api` route.
    pub token: String,
    /// The address actually bound. A request whose `Host` does not match is refused,
    /// which is what stops DNS rebinding.
    pub bind: SocketAddr,
    /// The single staged upload. Shared, so that reading it holds no lock during an
    /// import, which can take seconds.
    pub upload: Mutex<Option<Arc<StagedUpload>>>,
    /// The rate limiter.
    pub rate: Mutex<RateWindow>,
    /// What the one heavy job is doing, while one is running.
    pub heavy_job: Mutex<Option<String>>,
    /// Set when the UI asks the server to exit.
    pub stop: Arc<AtomicBool>,
}

impl AppState {
    /// Build the state for a launch.
    pub fn new(workspace: Workspace, token: String, bind: SocketAddr) -> Self {
        Self {
            workspace,
            token,
            bind,
            upload: Mutex::new(None),
            rate: Mutex::new(RateWindow::new()),
            heavy_job: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stage an upload, evicting any previous one.
    pub fn stage_upload(&self, filename: String, bytes: Vec<u8>) -> Result<String> {
        self.stage(filename, bytes, false)
    }

    /// Stage the bundled sample, marked so that importing it says it is synthetic.
    pub fn stage_sample(&self, filename: String, bytes: Vec<u8>) -> Result<String> {
        self.stage(filename, bytes, true)
    }

    fn stage(&self, filename: String, bytes: Vec<u8>, synthetic: bool) -> Result<String> {
        if bytes.len() > limits::MAX_CSV_BYTES {
            return Err(Error::limit(
                "limit.csv_bytes",
                limits::MAX_CSV_BYTES as u64,
                bytes.len() as u64,
                "the uploaded file is larger than Tonans will read",
            ));
        }
        let id = crate::token::opaque_id();
        *recover(&self.upload) = Some(Arc::new(StagedUpload {
            id: id.clone(),
            filename,
            bytes,
            synthetic,
        }));
        Ok(id)
    }

    /// The staged upload, if its id matches. It stays staged.
    pub fn staged_upload(&self, id: &str) -> Result<Arc<StagedUpload>> {
        match recover(&self.upload).as_ref() {
            Some(upload) if upload.id == id => Ok(Arc::clone(upload)),
            Some(_) => Err(Error::validation(
                "validation.unknown_upload",
                "import.upload_id",
                "that upload is no longer staged. Choose the file again.",
            )),
            None => Err(Error::validation(
                "validation.unknown_upload",
                "import.upload_id",
                "no file is staged for import. Choose a file first.",
            )),
        }
    }

    /// Drop the staged upload, if its id matches.
    ///
    /// A stale handle cannot be replayed once this has run, and the bytes leave memory
    /// when the last reader is done with them. A handle that no longer matches is not
    /// an error: the upload it named is already gone.
    pub fn discard_upload(&self, id: &str) {
        let mut slot = recover(&self.upload);
        if slot.as_ref().is_some_and(|upload| upload.id == id) {
            *slot = None;
        }
    }

    /// Claim the process's one heavy job, or say what is already running.
    ///
    /// One run of the largest project allowed peaks near 2.6 GB, and running out of memory
    /// kills the process, because an allocation failure cannot be caught. So two at once
    /// would be a way to lose a window's unsaved work, and they are refused rather than
    /// stacked. The claim ends when the returned value drops, including in a panic.
    pub fn begin_heavy_job(&self, what: &str, project: &str) -> Result<HeavyJob<'_>> {
        let mut slot = recover(&self.heavy_job);
        if let Some(running) = slot.as_ref() {
            return Err(Error::Conflict {
                code: "server.busy",
                message: format!(
                    "Tonans is already running {running}. It does one of these at a time, so try \
                     again when that finishes."
                ),
            });
        }
        *slot = Some(format!("{what} of '{project}'"));
        Ok(HeavyJob { state: self })
    }

    /// Whether this request is inside the rate limit.
    pub fn allow_request(&self) -> bool {
        recover(&self.rate).allow()
    }
}

/// The claim on the one heavy job. See [`AppState::begin_heavy_job`].
#[must_use]
pub struct HeavyJob<'a> {
    state: &'a AppState,
}

impl Drop for HeavyJob<'_> {
    fn drop(&mut self) {
        *recover(&self.state.heavy_job) = None;
    }
}

/// Lock a mutex, carrying on if a panicking request poisoned it.
///
/// A request that panicked has already been answered with a 500. Everything these
/// mutexes guard stays valid after a panic, because each is replaced or incremented in
/// one step. Refusing every later request would turn one bad request into a dead server.
fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fixed-window rate limiter.
///
/// Crude on purpose. The threat is not a distributed attacker — nothing off this
/// machine can reach the socket — it is a runaway loop in a page, or a local process
/// hammering the engine. A fixed window stops both and has no moving parts.
#[derive(Debug)]
pub struct RateWindow {
    started: std::time::Instant,
    count: u32,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started: clock::monotonic(),
            count: 0,
        }
    }

    fn allow(&mut self) -> bool {
        let now = clock::monotonic();
        if now.duration_since(self.started) >= RATE_LIMIT_WINDOW {
            self.started = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= RATE_LIMIT_REQUESTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_heavy_job_at_a_time_and_a_panic_gives_up_the_claim() {
        let root = std::env::temp_dir().join(format!("tonans-heavy-{}", std::process::id()));
        let workspace = Workspace::open(&root).unwrap();
        let state = AppState::new(workspace, "t".into(), "127.0.0.1:1".parse().unwrap());

        let first = state.begin_heavy_job("a run", "Big").unwrap();
        let refused = state.begin_heavy_job("a study", "Demo").err().unwrap();
        assert_eq!(refused.code(), "server.busy");
        assert_eq!(refused.http_status(), 409);
        assert!(refused.to_string().contains("a run of 'Big'"), "{refused}");
        drop(first);
        let second = state.begin_heavy_job("a study", "Demo").unwrap();
        drop(second);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _job = state.begin_heavy_job("a run", "Big").unwrap();
            panic!("boom");
        }));
        assert!(result.is_err());
        assert!(state.begin_heavy_job("a run", "Big").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_state() -> (AppState, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "tonans-state-test-{}-{}",
            std::process::id(),
            crate::token::opaque_id()
        ));
        let workspace = Workspace::open(&root).unwrap();
        let bind = "127.0.0.1:1".parse().unwrap();
        (AppState::new(workspace, "t".to_string(), bind), root)
    }

    #[test]
    fn a_panic_holding_a_lock_does_not_take_later_requests_with_it() {
        let (state, root) = test_state();
        let id = state.stage_upload("a.csv".into(), b"x".to_vec()).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _upload = state.upload.lock().unwrap();
            let _rate = state.rate.lock().unwrap();
            panic!("a deliberate panic while holding the locks");
        }));
        assert!(state.upload.is_poisoned() && state.rate.is_poisoned());

        assert!(state.allow_request(), "the rate limit stopped answering");
        assert_eq!(state.staged_upload(&id).unwrap().bytes, b"x");
        let replacement = state.stage_upload("b.csv".into(), b"y".to_vec()).unwrap();
        assert_eq!(state.staged_upload(&replacement).unwrap().filename, "b.csv");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reading_a_staged_upload_holds_no_lock_and_discarding_needs_its_id() {
        let (state, root) = test_state();
        let id = state.stage_upload("a.csv".into(), b"x".to_vec()).unwrap();
        let held = state.staged_upload(&id).unwrap();
        // With a reader still running, a new upload can be staged rather than wait for it.
        let second = state.stage_upload("b.csv".into(), b"y".to_vec()).unwrap();
        assert_eq!(held.filename, "a.csv");
        assert!(
            state.staged_upload(&id).is_err(),
            "the old handle was replayed"
        );

        state.discard_upload(&id);
        assert!(
            state.staged_upload(&second).is_ok(),
            "a stale id discarded the new file"
        );
        state.discard_upload(&second);
        assert!(state.staged_upload(&second).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_rate_window_lets_a_burst_through_and_then_stops() {
        let mut window = RateWindow::new();
        for i in 0..RATE_LIMIT_REQUESTS {
            assert!(window.allow(), "request {i} was refused inside the limit");
        }
        assert!(!window.allow(), "the request past the limit was allowed");
        assert!(!window.allow());
    }
}
