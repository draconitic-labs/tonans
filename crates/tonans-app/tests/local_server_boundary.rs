//! The local server's boundary, tested over a real socket with forged requests.
//!
//! A web page in the user's normal browser can reach this socket, can rebind its own
//! DNS name to 127.0.0.1, and can send a `text/plain` POST with no preflight. These tests
//! send what such a page could send, and a good deal it could not, and check that the
//! answer is a refusal and that nothing on disk moved.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tonans_app::demo;
use tonans_app::server::Server;
use tonans_app::state::{AppState, DEMO_PROJECT_NAME};
use tonans_core::store::Workspace;

const DEMO: &str = "Demo%20-%20Riverside%20Works";

/// Every route that changes anything.
const STATE_CHANGING: &[(&str, &str)] = &[
    ("POST", "/api/projects"),
    ("PUT", "/api/projects/Demo%20-%20Riverside%20Works"),
    ("POST", "/api/upload"),
    ("POST", "/api/upload/sample"),
    ("POST", "/api/upload/cancel"),
    ("POST", "/api/upload/suggest"),
    ("POST", "/api/projects/Demo%20-%20Riverside%20Works/import"),
    ("POST", "/api/projects/Demo%20-%20Riverside%20Works/run"),
    (
        "POST",
        "/api/projects/Demo%20-%20Riverside%20Works/sensitivity",
    ),
    ("POST", "/api/projects/Demo%20-%20Riverside%20Works/export"),
    ("POST", "/api/shutdown"),
];

/// Every route that reads something, including the assets.
const READING: &[&str] = &[
    "/",
    "/app.js",
    "/styles.css",
    "/palette.css",
    "/favicon.svg",
    "/fonts/barlow-400.woff2",
    "/api/session",
    "/api/projects",
    "/api/projects/Demo%20-%20Riverside%20Works",
    "/api/projects/Demo%20-%20Riverside%20Works/profile",
    "/api/templates/interval-csv",
];

struct Running {
    state: Arc<AppState>,
    host: String,
    origin: String,
    token: String,
    port: u16,
    root: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start() -> Self {
        let root = std::env::temp_dir().join(format!(
            "tonans-boundary-{}-{}",
            std::process::id(),
            tonans_app::token::opaque_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // The workspace sits one level down, so a traversal has somewhere to land.
        let workspace = Workspace::open(root.join("workspace")).unwrap();
        demo::ensure_installed(&workspace).unwrap();
        let server = Server::bind(workspace, 0).unwrap();
        let address = server.address;
        let state = Arc::clone(&server.state);
        let token = state.token.clone();
        let thread = std::thread::spawn(move || server.serve().unwrap());
        Self {
            state,
            host: address.to_string(),
            origin: format!("http://{address}"),
            token,
            port: address.port(),
            root,
            thread: Some(thread),
        }
    }

    fn workspace(&self) -> &Path {
        self.state.workspace.root()
    }

    /// Send one raw request, with exactly the headers given.
    fn raw(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = format!("{method} {target} HTTP/1.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        if !body.is_empty() {
            // A refusal is answered from the head alone, and the server then closes with
            // the body unread. Sending the body anyway would race that close, and on
            // Windows the reset can discard the answer. So the body goes only if no answer
            // has come after a moment, which is also what a slow client would see.
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut first = [0u8; 4096];
            match stream.read(&mut first) {
                Ok(count) => response.extend_from_slice(&first[..count]),
                Err(_) => stream.write_all(body.as_bytes()).unwrap(),
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
        }
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        (status, text)
    }

    fn get(&self, target: &str, token: bool) -> (u16, String) {
        let bearer = format!("Bearer {}", self.token);
        let mut headers = vec![("Host", self.host.as_str())];
        if token {
            headers.push(("Authorization", bearer.as_str()));
        }
        self.raw("GET", target, &headers, "")
    }

    fn files(&self) -> Vec<PathBuf> {
        fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.root, &mut found);
        found.sort();
        found
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let bearer = format!("Bearer {}", self.token);
        let host = self.host.clone();
        let _ = self.raw(
            "POST",
            "/api/shutdown",
            &[("Host", host.as_str()), ("Authorization", bearer.as_str())],
            "",
        );
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_rebound_host_is_refused_on_every_route_even_with_the_token() {
    let server = Running::start();
    let bearer = format!("Bearer {}", server.token);
    let wrong_port = "127.0.0.1:1".to_string();
    let hosts = [
        "rebind.attacker.example",
        &format!("rebind.attacker.example:{}", server.port),
        &format!("127.0.0.1.nip.io:{}", server.port),
        &format!("[::1]:{}", server.port),
        &format!("0.0.0.0:{}", server.port),
        "127.0.0.1",
        wrong_port.as_str(),
        "",
    ];
    for host in hosts {
        for target in READING {
            let (status, _) = server.raw("GET", target, &[("Host", host)], "");
            assert_eq!(status, 403, "GET {target} with Host {host:?}");
            let (status, _) = server.raw(
                "GET",
                target,
                &[("Host", host), ("Authorization", bearer.as_str())],
                "",
            );
            assert_eq!(status, 403, "GET {target} with Host {host:?} and the token");
        }
        for (method, target) in STATE_CHANGING {
            let (status, _) = server.raw(
                method,
                target,
                &[("Host", host), ("Authorization", bearer.as_str())],
                "{}",
            );
            assert_eq!(
                status, 403,
                "{method} {target} with Host {host:?} and the token"
            );
        }
    }
    // A request with no Host header at all is not ours either.
    let (status, _) = server.raw("GET", "/", &[], "");
    assert_eq!(status, 403);
    // And the address it really is on still works, under either of its names.
    assert_eq!(server.get("/", false).0, 200);
    let localhost = format!("localhost:{}", server.port);
    assert_eq!(server.raw("GET", "/", &[("Host", &localhost)], "").0, 200);
}

#[test]
fn a_foreign_origin_is_refused_on_every_route_even_with_the_token() {
    let server = Running::start();
    let bearer = format!("Bearer {}", server.token);
    let before = server.files();
    let origins = [
        "https://rebind.attacker.example",
        &format!("http://rebind.attacker.example:{}", server.port),
        "null",
        "http://127.0.0.1:1",
        &format!("{}.evil.example", server.origin),
        &server.origin.to_uppercase(),
        "file://",
    ];
    let host = server.host.as_str();
    for origin in origins {
        for target in READING {
            let (status, _) = server.raw("GET", target, &[("Host", host), ("Origin", origin)], "");
            assert_eq!(status, 403, "GET {target} from {origin}");
        }
        for (method, target) in STATE_CHANGING {
            let (status, _) = server.raw(
                method,
                target,
                &[
                    ("Host", host),
                    ("Origin", origin),
                    ("Authorization", &bearer),
                ],
                r#"{"name":"pwned","timezone":"UTC"}"#,
            );
            assert_eq!(status, 403, "{method} {target} from {origin}");
        }
    }
    assert_eq!(server.files(), before, "a refused request changed the disk");
    assert!(!server.state.stop.load(Ordering::SeqCst));
    // The page's own origin is fine.
    let (status, _) = server.raw(
        "GET",
        "/api/projects",
        &[
            ("Host", host),
            ("Origin", &server.origin),
            ("Authorization", &bearer),
        ],
        "",
    );
    assert_eq!(status, 200);
}

#[test]
fn a_cross_site_post_without_the_token_changes_nothing() {
    let server = Running::start();
    let before = server.files();
    let host = server.host.as_str();
    let wrong = format!("Bearer {}", "0".repeat(64));
    let query_only = |target: &str| format!("{target}?token={}", server.token);
    for (method, target) in STATE_CHANGING {
        for content_type in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/json",
        ] {
            let body = r#"{"name":"pwned","timezone":"UTC"}"#;
            let plain = [("Host", host), ("Content-Type", content_type)];
            assert_eq!(
                server.raw(method, target, &plain, body).0,
                401,
                "{method} {target}"
            );
            let wrong_token = [
                ("Host", host),
                ("Content-Type", content_type),
                ("Authorization", wrong.as_str()),
            ];
            assert_eq!(
                server.raw(method, target, &wrong_token, body).0,
                401,
                "{method} {target} with a wrong token"
            );
            // The token only ever counts in the Authorization header. A URL cannot carry it.
            assert_eq!(
                server.raw(method, &query_only(target), &plain, body).0,
                401,
                "{method} {target} with the token in the query"
            );
        }
    }
    assert_eq!(server.files(), before, "a refused request changed the disk");
    assert!(
        !server.state.stop.load(Ordering::SeqCst),
        "refused shutdown stopped the server"
    );
    assert!(
        server.state.upload.lock().unwrap().is_none(),
        "a refused upload was staged"
    );
}

#[test]
fn every_spelling_of_a_control_route_needs_the_token() {
    let server = Running::start();
    let absolute = format!("http://{}/api/projects", server.host);
    for target in [
        "/api/projects",
        "//api/projects",
        "///api//session",
        "/%61pi/projects",
        "/api%2Fprojects",
        "/./api/projects",
        "/x/../api/projects",
        "/api/../api/projects",
        "/API/projects",
        "/api;x/projects",
        absolute.as_str(),
    ] {
        let (status, body) = server.get(target, false);
        assert!(
            (400..500).contains(&status),
            "{target} answered {status} without the token"
        );
        assert!(
            !body.contains("scenario_count"),
            "{target} gave out project data"
        );
        assert!(
            !body.contains(server.workspace().to_str().unwrap()),
            "{target}"
        );
    }
}

#[test]
fn no_path_reaches_outside_the_workspace_even_with_the_token() {
    let server = Running::start();
    // Files a traversal would hit: beside the workspace, and in its root.
    std::fs::write(server.root.join("canary.txt"), "CANARY-OUTSIDE").unwrap();
    std::fs::write(server.workspace().join("canary.txt"), "CANARY-ROOT").unwrap();
    let bundle = server
        .state
        .workspace
        .export_dir(DEMO_PROJECT_NAME, "k1")
        .unwrap();
    std::fs::write(bundle.join("ok.txt"), "ARTIFACT-OK").unwrap();

    let real = format!("/api/projects/{DEMO}/exports/k1/ok.txt");
    assert_eq!(server.get(&real, true).0, 200);

    let attempts = [
        "/api/projects/..%2F..%2Fcanary.txt".to_string(),
        "/api/projects/..%2Fcanary.txt".to_string(),
        "/api/projects/../canary.txt".to_string(),
        "/api/projects/%2e%2e%2fcanary.txt".to_string(),
        "/api/projects/%252e%252e%252fcanary.txt".to_string(),
        "/api/projects/..%5c..%5ccanary.txt".to_string(),
        "/api/projects//etc/passwd".to_string(),
        "/api/projects/%2Fetc%2Fpasswd".to_string(),
        "/api/projects/%2F%2Fetc%2Fpasswd".to_string(),
        "/api/projects/C:%5CWindows%5Cwin.ini".to_string(),
        "/api/projects/%c0%ae%c0%ae%2fcanary.txt".to_string(),
        "/api/projects/foo%00.tonans.json".to_string(),
        "/api/projects/CON".to_string(),
        format!("/api/projects/{DEMO}/exports/k1/..%2f..%2f..%2f..%2fcanary.txt"),
        format!("/api/projects/{DEMO}/exports/k1/../../../../canary.txt"),
        format!("/api/projects/{DEMO}/exports/k1/%2e%2e"),
        format!("/api/projects/{DEMO}/exports/k1//etc/passwd"),
        format!("/api/projects/{DEMO}/exports/k1/%2Fetc%2Fpasswd"),
        format!("/api/projects/{DEMO}/exports/..%2F../canary.txt"),
        format!("/api/projects/{DEMO}/exports/%2Fetc/passwd"),
        format!("/api/projects/{DEMO}/exports/k1/%2e%2e%5c%2e%2e%5ccanary.txt"),
        "/api/projects/..%2F../exports/k1/ok.txt".to_string(),
        "/api/projects/%2Fetc/exports/x/passwd".to_string(),
        "/fonts/..%2f..%2f..%2fcanary.txt".to_string(),
        "/fonts/%2Fetc%2Fpasswd".to_string(),
        "/fonts/OFL.txt".to_string(),
        "/../../etc/passwd".to_string(),
        "/%2e%2e/%2e%2e/etc/passwd".to_string(),
        "//etc/passwd".to_string(),
        "/app.js/../../etc/passwd".to_string(),
    ];
    for target in &attempts {
        let (status, body) = server.get(target, true);
        assert!((400..500).contains(&status), "{target} answered {status}");
        assert!(!body.contains("CANARY"), "{target} read a canary");
        assert!(!body.contains("root:"), "{target} read the password file");
    }

    // A name written by a state-changing route cannot land anywhere else either.
    let bearer = format!("Bearer {}", server.token);
    let headers = [
        ("Host", server.host.as_str()),
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/json"),
    ];
    let before = server.files();
    let absolute = server.root.join("escaped");
    let absolute = absolute.to_str().unwrap().replace('\\', "\\\\");
    for name in [
        absolute.as_str(),
        "../../escaped",
        "..\\\\..\\\\escaped",
        "a/b",
        "..",
        ".",
        "CON",
    ] {
        let body = format!(r#"{{"name":"{name}","timezone":"UTC"}}"#);
        let (status, _) = server.raw("POST", "/api/projects", &headers, &body);
        assert!(
            (400..500).contains(&status),
            "project name {name} answered {status}"
        );
    }
    assert_eq!(
        server.files(),
        before,
        "a refused project name changed the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_in_the_workspace_is_not_followed() {
    use std::os::unix::fs::symlink;

    let server = Running::start();
    std::fs::write(server.root.join("canary.txt"), "CANARY-OUTSIDE").unwrap();
    let outside = server.root.join("canary.txt");
    let bundle = server
        .state
        .workspace
        .export_dir(DEMO_PROJECT_NAME, "k1")
        .unwrap();
    symlink(&outside, bundle.join("link.txt")).unwrap();
    symlink(&server.root, bundle.parent().unwrap().join("k2")).unwrap();
    symlink(
        &outside,
        server
            .state
            .workspace
            .projects_dir()
            .join("evil.tonans.json"),
    )
    .unwrap();

    for target in [
        format!("/api/projects/{DEMO}/exports/k1/link.txt"),
        format!("/api/projects/{DEMO}/exports/k2/canary.txt"),
        "/api/projects/evil".to_string(),
    ] {
        let (status, body) = server.get(&target, true);
        assert!((400..500).contains(&status), "{target} answered {status}");
        assert!(!body.contains("CANARY"), "{target} followed a link");
    }
}

#[test]
fn a_missing_bundle_is_a_refusal_not_a_server_error() {
    let server = Running::start();
    for target in [
        format!("/api/projects/{DEMO}/exports/nothing/here.txt"),
        "/api/projects/nobody/exports/x/y.txt".to_string(),
    ] {
        let (status, _) = server.get(&target, true);
        assert_eq!(status, 422, "{target}");
    }
}

#[test]
fn refused_requests_cannot_spend_the_real_uis_request_budget() {
    let server = Running::start();
    // A page that found the port can send no-cors GETs, which carry no Origin. It cannot
    // read the answers, but it must not be able to lock the real UI out with them either.
    for _ in 0..700 {
        assert_eq!(server.get("/api/projects", false).0, 401);
    }
    assert_eq!(server.get("/api/projects", true).0, 200);
}

#[test]
fn no_response_invites_a_cross_origin_reader() {
    let server = Running::start();
    let bearer = format!("Bearer {}", server.token);
    let responses = [
        server.raw(
            "OPTIONS",
            "/api/projects",
            &[
                ("Host", &server.host),
                ("Origin", "https://evil.example"),
                ("Access-Control-Request-Method", "POST"),
            ],
            "",
        ),
        server.raw("OPTIONS", "/api/projects", &[("Host", &server.host)], ""),
        server.raw(
            "GET",
            "/api/projects",
            &[("Host", &server.host), ("Authorization", &bearer)],
            "",
        ),
        server.get("/", false),
    ];
    for (_, text) in responses {
        assert!(!text.to_lowercase().contains("access-control-"), "{text}");
    }
    // The public assets are public, so they must hold no secret.
    for target in READING.iter().filter(|target| !target.starts_with("/api")) {
        let (_, text) = server.get(target, false);
        assert!(!text.contains(&server.token), "{target} holds the token");
    }
}
