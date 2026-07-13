//! Server state: the workspace, the launch token, the entitlements, and the one
//! staged upload slot.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tonans_core::error::{Error, Result};
use tonans_core::licensing::{BuiltInEvaluation, EntitlementSource, Entitlements};
use tonans_core::limits;
use tonans_core::store::Workspace;

use crate::clock;

/// The name of the bundled demonstration project.
///
/// Reserved: a user project may not take it, and it does not count against the
/// evaluation tier's project limit. The demo exists so that a first launch has
/// something to open.
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
}

/// Everything a request handler can reach.
#[derive(Debug)]
pub struct AppState {
    /// Where projects and exports live.
    pub workspace: Workspace,
    /// The bearer token minted at launch. Required by every `/api` route.
    pub token: String,
    /// What this build permits.
    pub entitlements: Entitlements,
    /// The address actually bound. A request whose `Host` does not match is refused,
    /// which is what stops DNS rebinding.
    pub bind: SocketAddr,
    /// The single staged upload.
    pub upload: Mutex<Option<StagedUpload>>,
    /// The rate limiter.
    pub rate: Mutex<RateWindow>,
    /// Set when the UI asks the server to exit.
    pub stop: Arc<AtomicBool>,
}

impl AppState {
    /// Build the state for a launch.
    pub fn new(workspace: Workspace, token: String, bind: SocketAddr) -> Self {
        Self {
            workspace,
            token,
            // The shipped build is always evaluation. There is no licence file to read
            // and no server to ask; see `tonans_core::licensing`.
            entitlements: BuiltInEvaluation.entitlements(),
            bind,
            upload: Mutex::new(None),
            rate: Mutex::new(RateWindow::new()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stage an upload, evicting any previous one.
    pub fn stage_upload(&self, filename: String, bytes: Vec<u8>) -> Result<String> {
        if bytes.len() > limits::MAX_CSV_BYTES {
            return Err(Error::limit(
                "limit.csv_bytes",
                limits::MAX_CSV_BYTES as u64,
                bytes.len() as u64,
                "the uploaded file is larger than Tonans will read",
            ));
        }
        let id = crate::token::opaque_id();
        let mut slot = self.upload.lock().map_err(poisoned)?;
        *slot = Some(StagedUpload {
            id: id.clone(),
            filename,
            bytes,
        });
        Ok(id)
    }

    /// Take the staged upload, if its id matches.
    ///
    /// Consuming it rather than copying it means a stale handle cannot be replayed,
    /// and the bytes leave memory as soon as the import is done with them.
    pub fn take_upload(&self, id: &str) -> Result<StagedUpload> {
        let mut slot = self.upload.lock().map_err(poisoned)?;
        match slot.take() {
            Some(upload) if upload.id == id => Ok(upload),
            Some(other) => {
                *slot = Some(other); // the caller asked for a different one
                Err(Error::validation(
                    "validation.unknown_upload",
                    "import.upload_id",
                    "that upload is no longer staged. Choose the file again.",
                ))
            }
            None => Err(Error::validation(
                "validation.unknown_upload",
                "import.upload_id",
                "no file is staged for import. Choose a file first.",
            )),
        }
    }

    /// Whether this request is inside the rate limit.
    pub fn allow_request(&self) -> bool {
        self.rate.lock().is_ok_and(|mut window| window.allow())
    }

    /// The user projects that count against the entitlement, i.e. not the demo.
    pub fn user_project_count(&self) -> Result<usize> {
        Ok(self
            .workspace
            .list_projects()?
            .into_iter()
            .filter(|entry| entry.name != DEMO_PROJECT_NAME)
            .count())
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::io(
        "the server's internal lock was poisoned by a panic in another request",
        std::io::Error::other("poisoned lock"),
    )
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
    fn the_rate_window_lets_a_burst_through_and_then_stops() {
        let mut window = RateWindow::new();
        for i in 0..RATE_LIMIT_REQUESTS {
            assert!(window.allow(), "request {i} was refused inside the limit");
        }
        assert!(!window.allow(), "the request past the limit was allowed");
        assert!(!window.allow());
    }
}
