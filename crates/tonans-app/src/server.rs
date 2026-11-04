//! Routing and the security envelope.
//!
//! # The threat model, in the order it is defended
//!
//! Tonans listens on a socket. That socket is reachable by every process on the
//! machine and — crucially — by every *web page* the user has open, because a page
//! may issue cross-origin requests to `http://127.0.0.1:<port>` and does not need to
//! read the response to do damage. So:
//!
//! 1. **Bind to loopback, on a port the kernel picks.** Nothing off this machine can
//!    connect, and the port is not a fixed target for a page scanning for local
//!    services.
//! 2. **Require a bearer token** on every `/api` route. 256 bits from the OS CSPRNG,
//!    minted at launch, never written to disk, compared in constant time. A page that
//!    cannot guess it cannot drive the application. See [`crate::token`].
//! 3. **Pin the `Host` header** to the address actually bound. Without this, DNS
//!    rebinding works: an attacker's domain resolves to `127.0.0.1`, the browser
//!    treats the response as same-origin, and the same-origin policy stops helping.
//! 4. **Refuse a cross-origin `Origin`.**
//! 5. **Bound every request** — body size per route, a fixed worker pool, and a
//!    fixed-window rate limit on authenticated control routes. The likeliest attacker
//!    here is a runaway loop.
//! 6. **Send a restrictive CSP**, so the page cannot fetch, frame, or execute
//!    anything it did not ship with.
//!
//! There is no route that commands a device, and there never will be. See
//! `docs/SECURITY.md`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tonans_core::error::{Error, Result};
use tonans_core::limits;
use tonans_core::store::Workspace;

use crate::api;
use crate::assets;
use crate::http::{self, Handler, Request, Response};
use crate::state::AppState;
use crate::token;

/// The largest JSON body any route accepts. A project holding three years of
/// 15-minute data is a few megabytes; this is comfortably above that and far below
/// anything that could exhaust memory.
const MAX_JSON_BYTES: usize = 96 * 1024 * 1024;

/// The `Content-Security-Policy` served with the application page.
///
/// `default-src 'none'` means: fetch nothing, unless a directive below says
/// otherwise. Everything the UI needs is compiled into the binary and served from
/// this origin. There is no CDN, no font service, no analytics — and no way to add
/// one without changing this line, which is the point of writing it this way round.
pub const CSP: &str =
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     font-src 'self'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; \
     base-uri 'none'; object-src 'none'";

/// A bound, ready-to-serve instance.
pub struct Server {
    /// The address it is listening on.
    pub address: SocketAddr,
    /// The URL to open. Carries the launch token.
    pub url: String,
    /// Shared state.
    pub state: Arc<AppState>,
    listener: TcpListener,
}

impl Server {
    /// Bind a loopback socket.
    ///
    /// `port` of zero — the default — lets the kernel choose.
    pub fn bind(workspace: Workspace, port: u16) -> Result<Self> {
        let requested = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = TcpListener::bind(requested)
            .map_err(|e| Error::io(format!("binding {requested}"), e))?;
        let address = listener
            .local_addr()
            .map_err(|e| Error::io("reading the bound address", e))?;

        // Belt and braces. A one-character change to the line above must not be able
        // to publish a user's project files to their local network.
        if !address.ip().is_loopback() {
            return Err(Error::UnsafePath {
                code: "server.not_loopback",
                message: format!("refusing to serve on {address}, which is not a loopback address"),
            });
        }

        let secret = token::mint()?;
        let url = format!("http://{address}/?token={secret}");
        let state = Arc::new(AppState::new(workspace, secret, address));

        Ok(Self {
            address,
            url,
            state,
            listener,
        })
    }

    /// Serve until the UI asks to stop.
    pub fn serve(self) -> Result<()> {
        let state = Arc::clone(&self.state);
        let stop = Arc::clone(&state.stop);
        let address = self.address;

        // The UI's "stop" button sets the flag; this thread then pokes the listener so
        // the blocked `accept` returns and the loop can notice.
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            let _ = TcpStream::connect(address);
        });

        let router = Arc::new(Router {
            state: Arc::clone(&self.state),
        });
        http::serve(&self.listener, router, &self.state.stop).map_err(|e| Error::io("serving", e))
    }
}

/// Dispatches a request, after the security envelope has cleared it.
pub struct Router {
    /// Shared state.
    pub state: Arc<AppState>,
}

impl Handler for Router {
    fn preflight(&self, request: &Request) -> Option<Response> {
        self.guard(request).map(Self::secure)
    }

    fn body_limit(&self, _method: &str, path: &str) -> usize {
        // The upload route carries raw CSV and gets the importer's own limit, so a file
        // that would be refused is refused before it is buffered.
        let segments: Vec<_> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.as_slice() == ["api", "upload"] {
            limits::MAX_CSV_BYTES
        } else if segments.first() == Some(&"api") {
            MAX_JSON_BYTES
        } else {
            0
        }
    }

    fn handle(&self, request: &Request) -> Response {
        Self::secure(self.route(request))
    }
}

impl Router {
    fn secure(response: Response) -> Response {
        response
        .with_header("content-security-policy", CSP)
        .with_header("x-content-type-options", "nosniff")
        .with_header("referrer-policy", "no-referrer")
        // The token rides in the query string of the first navigation. `no-referrer`
        // means it is never sent onward; nothing should cache it either.
        .with_header("cache-control", "no-store, no-cache, must-revalidate")
        .with_header("cross-origin-opener-policy", "same-origin")
        .with_header("cross-origin-resource-policy", "same-origin")
        .with_header("x-frame-options", "DENY")
        .with_header(
            "permissions-policy",
            "geolocation=(), camera=(), microphone=(), usb=(), serial=(), bluetooth=(), payment=()",
        )
    }
    /// Everything that must be true before a handler runs. `None` means "carry on".
    ///
    /// Ordering matters: the cheap structural checks come first, so a hostile request
    /// is refused before it can make us do any work.
    fn guard(&self, request: &Request) -> Option<Response> {
        let state = &self.state;

        // DNS rebinding: a page on `evil.test` whose DNS answer is 127.0.0.1 reaches
        // this socket, and the browser calls it same-origin. Pinning `Host` to the
        // address we bound turns that into a 403 before anything else happens.
        let expected = state.bind.to_string();
        let localhost = format!("localhost:{}", state.bind.port());
        let host = request.header("host").unwrap_or_default();
        if host != expected && host != localhost {
            return Some(problem(
                403,
                "server.bad_host",
                "This request did not arrive at the address Tonans is serving.",
            ));
        }

        // A cross-origin page can send a request without reading the response, which is
        // enough to create or overwrite a project. The token already stops that; this
        // stops it earlier and more legibly.
        if let Some(origin) = request.header("origin") {
            let ours = format!("http://{}", state.bind);
            let ours_localhost = format!("http://localhost:{}", state.bind.port());
            if origin != ours && origin != ours_localhost {
                return Some(problem(
                    403,
                    "server.cross_origin",
                    "Cross-origin requests are refused.",
                ));
            }
        }

        // The token gates every control route, and only the control routes: the static
        // assets hold no secrets, and the page needs them before it has a token to send.
        // Use the router's exact segment interpretation. Raw-prefix checks allow
        // alternate spellings such as //api/projects to reach an ungated route.
        if request.segments().first() == Some(&"api") {
            let presented = request
                .header("authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            if !token::constant_time_eq(presented, &state.token) {
                return Some(problem(
                    401,
                    "server.unauthorized",
                    "This request carried no valid launch token. Tonans mints one per launch and \
                     hands it to the page it serves.",
                ));
            }

            // After the token, deliberately. A web page cannot read these responses, but a
            // no-cors GET carries no Origin and reaches this far, so counting refused
            // requests would let any page spend the budget and lock the real UI out.
            if !state.allow_request() {
                return Some(problem(
                    429,
                    "server.rate_limited",
                    "Too many requests. Tonans is a single-user local application; this many \
                     requests in this short a time means something is looping.",
                ));
            }
        }

        None
    }

    fn route(&self, request: &Request) -> Response {
        let state = &self.state;
        let segments = request.segments();
        let method = request.method.as_str();

        match (method, segments.as_slice()) {
            ("GET", []) => assets::index(),
            ("GET", ["app.js"]) => assets::script(),
            ("GET", ["styles.css"]) => assets::stylesheet(),
            ("GET", ["palette.css"]) => assets::palette(),
            ("GET", ["favicon.svg"]) => assets::favicon(),

            ("GET", ["api", "session"]) => api::session(state),
            ("GET", ["api", "projects"]) => api::list_projects(state),
            ("POST", ["api", "projects"]) => api::create_project(state, request),
            ("GET", ["api", "projects", name]) => api::open_project(state, name),
            ("PUT", ["api", "projects", name]) => api::save_project(state, name, request),
            ("POST", ["api", "upload"]) => api::upload(state, request),
            ("POST", ["api", "projects", name, "import"]) => api::import(state, name, request),
            ("POST", ["api", "projects", name, "run"]) => api::run(state, name),
            ("POST", ["api", "projects", name, "sensitivity"]) => {
                api::sensitivity(state, name, request)
            }
            ("POST", ["api", "projects", name, "export"]) => api::export(state, name, request),
            ("GET", ["api", "projects", name, "exports", key, artifact]) => {
                api::download(state, name, key, artifact)
            }
            ("POST", ["api", "shutdown"]) => api::shutdown(state),

            _ => problem(
                404,
                "server.not_found",
                "No such route. Tonans serves only its own application and API.",
            ),
        }
    }
}

/// A machine-readable error body. The UI maps `code` to a remediation hint.
pub fn problem(status: u16, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "code": code, "message": message });
    Response::json(status, body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn every_routable_api_spelling_requires_a_launch_token() {
        let root = std::env::temp_dir().join(format!(
            "tonans-auth-regression-{}-{}",
            std::process::id(),
            token::mint().unwrap()
        ));
        let server = Server::bind(Workspace::open(&root).unwrap(), 0).unwrap();
        let router = Router {
            state: server.state,
        };
        for path in [
            "/api/projects",
            "//api/projects",
            "///api//session",
            "/api",
            "//api/shutdown",
        ] {
            let mut request = Request {
                method: "GET".into(),
                path: path.into(),
                headers: BTreeMap::from([("host".into(), server.address.to_string())]),
                body: Vec::new(),
            };
            assert_eq!(
                router.preflight(&request).map(|r| r.status),
                Some(401),
                "{path}"
            );
            request.headers.insert(
                "authorization".into(),
                format!("Bearer {}", router.state.token),
            );
            assert!(router.preflight(&request).is_none(), "{path}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refused_request_does_not_spend_the_rate_budget() {
        let root = std::env::temp_dir().join(format!(
            "tonans-rate-regression-{}-{}",
            std::process::id(),
            token::mint().unwrap()
        ));
        let server = Server::bind(Workspace::open(&root).unwrap(), 0).unwrap();
        let router = Router {
            state: server.state,
        };
        let request = |authorized: bool| {
            let mut headers = BTreeMap::from([("host".into(), server.address.to_string())]);
            if authorized {
                headers.insert(
                    "authorization".into(),
                    format!("Bearer {}", router.state.token),
                );
            }
            Request {
                method: "GET".into(),
                path: "/api/projects".into(),
                headers,
                body: Vec::new(),
            }
        };

        // A page can send these with no Origin. They must not lock the real UI out.
        for _ in 0..700 {
            assert_eq!(
                router.preflight(&request(false)).map(|r| r.status),
                Some(401)
            );
        }
        assert!(router.preflight(&request(true)).is_none());

        // The budget still bounds a runaway loop that does hold the token.
        let mut limited = false;
        for _ in 0..700 {
            if router.preflight(&request(true)).map(|r| r.status) == Some(429) {
                limited = true;
                break;
            }
        }
        assert!(limited, "an authorized runaway loop was never rate limited");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_rebound_host_is_forbidden() {
        let root = std::env::temp_dir().join(format!(
            "tonans-host-regression-{}-{}",
            std::process::id(),
            token::mint().unwrap()
        ));
        let server = Server::bind(Workspace::open(&root).unwrap(), 0).unwrap();
        let router = Router {
            state: server.state,
        };
        let request = Request {
            method: "GET".into(),
            path: "/".into(),
            headers: BTreeMap::from([("host".into(), "rebind.attacker.example".to_string())]),
            body: Vec::new(),
        };
        assert_eq!(router.preflight(&request).map(|r| r.status), Some(403));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_csp_forbids_everything_it_does_not_name() {
        assert!(CSP.starts_with("default-src 'none'"));
        // No 'unsafe-inline', no 'unsafe-eval', no remote origin, ever.
        assert!(!CSP.contains("unsafe-inline"));
        assert!(!CSP.contains("unsafe-eval"));
        assert!(!CSP.contains("http://"));
        assert!(!CSP.contains("https://"));
        assert!(!CSP.contains('*'));
        // The page must not be framable, and must not be able to submit a form anywhere.
        assert!(CSP.contains("frame-ancestors 'none'"));
        assert!(CSP.contains("form-action 'none'"));
    }
}
