//! A stale window cannot overwrite a project, and cannot get results for one it isn't showing.
//!
//! Two servers on one workspace are two windows, because each window runs its own server
//! in its own process. These tests send what each window's page would send, over real
//! sockets.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tonans_app::api::REVISION_HEADER;
use tonans_app::server::Server;
use tonans_app::state::AppState;
use tonans_core::store::Workspace;

struct Window {
    state: Arc<AppState>,
    host: String,
    token: String,
    port: u16,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Window {
    fn open(root: &PathBuf) -> Self {
        let server = Server::bind(Workspace::open(root).unwrap(), 0).unwrap();
        let address = server.address;
        let state = Arc::clone(&server.state);
        let token = state.token.clone();
        let thread = std::thread::spawn(move || server.serve().unwrap());
        Self {
            state,
            host: address.to_string(),
            token,
            port: address.port(),
            thread: Some(thread),
        }
    }

    /// Returns the status, the revision header if any, and the body.
    fn send(
        &self,
        method: &str,
        path: &str,
        revision: Option<&str>,
        body: &str,
    ) -> (u16, Option<String>, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n",
            self.host, self.token
        );
        if let Some(revision) = revision {
            request.push_str(&format!("{REVISION_HEADER}: {revision}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response).into_owned();
        let (head, payload) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let revision = head
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{REVISION_HEADER}: ")))
            .map(str::to_string);
        (status, revision, payload.to_string())
    }

    fn create(&self, name: &str) {
        let body = format!(r#"{{"name":"{name}","timezone":"UTC"}}"#);
        let (status, _, text) = self.send("POST", "/api/projects", None, &body);
        assert_eq!(status, 200, "{text}");
    }

    fn import(&self, name: &str) -> String {
        let csv = "t,load\n2024-01-01T00:00:00Z,1\n2024-01-01T01:00:00Z,2\n";
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        write!(
            stream,
            "POST /api/upload HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{csv}",
            self.host,
            self.token,
            csv.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let receipt = response.split_once("\r\n\r\n").unwrap().1;
        let receipt: serde_json::Value = serde_json::from_str(receipt).unwrap();
        let body = serde_json::json!({
            "upload_id": receipt["upload_id"],
            "request": {
                "filename": "x.csv",
                "timezone": "UTC",
                "interval_minutes": 60,
                "gap_policy": { "mode": "reject" },
                "mapping": {
                    "timestamp": "t",
                    "series": { "load": { "header": "load", "unit": "kilowatt" } },
                    "demand_period": null,
                    "ignore_columns": [],
                },
            },
        });
        let (status, revision, text) = self.send(
            "POST",
            &format!("/api/projects/{name}/import"),
            None,
            &body.to_string(),
        );
        assert_eq!(status, 200, "{text}");
        revision.unwrap()
    }

    fn open_project(&self, name: &str) -> (serde_json::Value, String) {
        let (status, revision, text) = self.send("GET", &format!("/api/projects/{name}"), None, "");
        assert_eq!(status, 200, "{text}");
        (serde_json::from_str(&text).unwrap(), revision.unwrap())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.send("POST", "/api/shutdown", None, "");
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn workspace_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tonans-revisions-{label}-{}-{}",
        std::process::id(),
        tonans_app::token::opaque_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn a_stale_window_cannot_delete_data_another_window_imported() {
    let root = workspace_root("stale");
    let a = Window::open(&root);
    let b = Window::open(&root);
    a.create("Site");

    // A opens the project while it has no data. B imports a file.
    let (mut in_a, revision_a) = a.open_project("Site");
    assert!(in_a.get("dataset").is_none());
    let (_, _) = b.open_project("Site");
    b.import("Site");

    // A saves its notes, holding a project with no dataset.
    in_a["notes"] = serde_json::json!("from the stale window");
    let (status, revision, text) = a.send(
        "PUT",
        "/api/projects/Site",
        Some(&revision_a),
        &in_a.to_string(),
    );
    assert_eq!(status, 409, "{text}");
    assert!(revision.is_none());
    assert!(text.contains("conflict.project_changed"), "{text}");
    assert!(text.contains("Nothing was saved"), "{text}");

    let (after, revision_after) = a.open_project("Site");
    assert!(after.get("dataset").is_some(), "the import was lost");
    assert_ne!(after["notes"], "from the stale window");

    // Having reopened, A can save, and keeps the dataset.
    let mut fresh = after;
    fresh["notes"] = serde_json::json!("from the reopened window");
    let (status, new_revision, text) = a.send(
        "PUT",
        "/api/projects/Site",
        Some(&revision_after),
        &fresh.to_string(),
    );
    assert_eq!(status, 200, "{text}");
    assert_ne!(new_revision.unwrap(), revision_after);
    let (saved, _) = b.open_project("Site");
    assert_eq!(saved["notes"], "from the reopened window");
    assert!(saved.get("dataset").is_some());
    drop((a, b));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_save_that_does_not_name_a_revision_is_refused() {
    let root = workspace_root("missing");
    let a = Window::open(&root);
    a.create("Site");
    let (project, revision) = a.open_project("Site");
    let (status, _, text) = a.send("PUT", "/api/projects/Site", None, &project.to_string());
    assert_eq!(status, 428, "{text}");
    assert!(text.contains("conflict.revision_required"), "{text}");
    assert_eq!(a.open_project("Site").1, revision, "the refused save wrote");
    drop(a);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn run_study_and_export_refuse_a_revision_the_window_no_longer_holds() {
    let root = workspace_root("reading");
    let a = Window::open(&root);
    let b = Window::open(&root);
    a.create("Site");
    a.import("Site");
    let (_, held) = a.open_project("Site");

    let study = r#"{"scenario_id":"baseline","axes":[{"param":"capex_usd","values":[1]}]}"#;
    let routes = [("run", ""), ("sensitivity", study), ("export", "")];
    for (route, body) in routes {
        let path = format!("/api/projects/Site/{route}");
        let (status, revision, text) = a.send("POST", &path, Some(&held), body);
        assert_eq!(status, 200, "{route}: {text}");
        assert_eq!(revision.as_deref(), Some(held.as_str()), "{route}");
    }

    // Another window saves. The first still shows what it read.
    let (mut in_b, revision_b) = b.open_project("Site");
    in_b["notes"] = serde_json::json!("changed elsewhere");
    let (status, _, text) = b.send(
        "PUT",
        "/api/projects/Site",
        Some(&revision_b),
        &in_b.to_string(),
    );
    assert_eq!(status, 200, "{text}");

    for (route, body) in routes {
        let path = format!("/api/projects/Site/{route}");
        let (status, _, text) = a.send("POST", &path, Some(&held), body);
        assert_eq!(status, 409, "{route}: {text}");
        assert!(text.contains("conflict.project_changed"), "{text}");
        // Naming no revision is a script asking for whatever is there, and it gets told.
        let (status, revision, text) = a.send("POST", &path, None, body);
        assert_eq!(status, 200, "{route}: {text}");
        assert_ne!(revision.as_deref(), Some(held.as_str()), "{route}");
    }
    drop((a, b));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_cancelled_upload_is_gone() {
    let root = workspace_root("cancel");
    let a = Window::open(&root);
    a.create("Site");
    let mut stream = TcpStream::connect(("127.0.0.1", a.port)).unwrap();
    let csv = "t,load\n2024-01-01T00:00:00Z,1\n";
    write!(
        stream,
        "POST /api/upload HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{csv}",
        a.host,
        a.token,
        csv.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    let id = receipt["upload_id"].as_str().unwrap().to_string();
    assert!(a.state.staged_upload(&id).is_ok());

    let body = serde_json::json!({ "upload_id": id }).to_string();
    let (status, _, text) = a.send("POST", "/api/upload/cancel", None, &body);
    assert_eq!(status, 200, "{text}");
    assert!(
        a.state.staged_upload(&id).is_err(),
        "the bytes are still held"
    );
    drop(a);
    std::fs::remove_dir_all(root).unwrap();
}
