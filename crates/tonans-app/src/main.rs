//! Tonans — a local-first deterministic microgrid scenario workbench.
//!
//! Two ways in:
//!
//! * `tonans` launches the loopback server and opens the browser. This is the
//!   product.
//! * `tonans run --project <name> --out <dir>` runs a project headlessly and writes
//!   the full export bundle. This is what the end-to-end test drives, and what a
//!   user would put in a script.
//!
//! Nothing here talks to the network, phones home, or writes outside the workspace.

mod api;
mod assets;
mod clock;
mod demo;
mod http;
mod server;
mod state;
mod token;

use std::path::PathBuf;
use std::process::ExitCode;

use tonans_core::error::{Error, Result};
use tonans_core::export;
use tonans_core::run;
use tonans_core::store::Workspace;
use tonans_core::version::{
    APP_VERSION, MODEL_VERSION, PRODUCT_NAME, SCHEMA_VERSION, SUPPORT_EMAIL,
};

const HELP: &str = "\
Tonans — microgrid scenario workbench

  tonans [OPTIONS]                       Launch the application
  tonans run --project <NAME> [OPTIONS]  Run a project and write an export bundle
  tonans --version
  tonans --help

Options:
  --workspace <DIR>   Where projects and exports live.
                      Default: %LOCALAPPDATA%\\Flintglade\\Tonans
  --port <PORT>       Loopback port. Default: 0, meaning the operating system
                      chooses one, which keeps it from being a fixed target.
  --no-browser        Do not open a browser; just print the URL.
  --project <NAME>    (run) The project to run.
  --out <DIR>         (run) Where to write the bundle. Default: the workspace's
                      exports directory.

Tonans binds only to 127.0.0.1, serves only assets compiled into this binary, and
sends nothing anywhere. Results are planning estimates for human review.

Flintglade — support@flintglade.com
";

fn main() -> ExitCode {
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Errors go to stderr with their stable code, so a script can act on them.
            eprintln!("tonans: [{}] {error}", error.code());
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;

    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("{PRODUCT_NAME} {APP_VERSION} (model {MODEL_VERSION}, schema v{SCHEMA_VERSION})");
        return Ok(());
    }

    let workspace = Workspace::open(args.workspace()?)?;

    match args.command {
        Command::Serve => serve(workspace, args.port, args.no_browser),
        Command::Run => headless(workspace, args.project, args.out),
    }
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

fn serve(workspace: Workspace, port: u16, no_browser: bool) -> Result<()> {
    // The demonstration project is installed on first launch by importing the embedded
    // sample CSV through the real importer. It is not a fixture smuggled in through a
    // side door: if the importer would reject it, the demo does not exist.
    demo::ensure_installed(&workspace)?;

    let server = server::Server::bind(workspace, port)?;

    println!("{PRODUCT_NAME} {APP_VERSION} (model {MODEL_VERSION})");
    println!("  workspace: {}", server.state.workspace.root().display());
    println!();
    println!("  Open this link. It carries the one-launch token; without it the API refuses.");
    println!("  {}", server.url);
    println!();
    println!("  Bound to loopback only. Nothing is sent anywhere. Press Ctrl+C to stop.");

    if !no_browser {
        open_browser(&server.url);
    }
    server.serve()
}

/// Ask the desktop to open a URL.
///
/// Windows only for now, and deliberately not clever: `cmd /C start` with an empty
/// window title, and the URL passed as a separate argument rather than interpolated
/// into a command line. The URL contains the launch token, so it must not be logged
/// or handed to a shell that might.
fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        // `start` treats its first quoted argument as a window title, hence the "".
        let launched = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
        if launched.is_err() {
            eprintln!("tonans: could not open a browser. Open the link above yourself.");
        }
    }
    #[cfg(not(windows))]
    {
        // Tonans ships for Windows. On anything else, print and let the user click.
        let _ = url;
        eprintln!("tonans: automatic browser launch is Windows-only. Open the link above.");
    }
}

// ---------------------------------------------------------------------------
// Headless
// ---------------------------------------------------------------------------

fn headless(workspace: Workspace, project: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let Some(name) = project else {
        return Err(Error::validation(
            "validation.missing_project",
            "run.project",
            "`tonans run` needs --project <NAME>. Run `tonans --help`.",
        ));
    };

    demo::ensure_installed(&workspace)?;
    let project = workspace.load_project(&name)?;
    let result = run::run_project(&project, clock::now_utc())?;

    let key = export::bundle_key(&project, &result, &[])?;
    let directory = match out {
        Some(directory) => directory,
        None => workspace.export_dir(&project.name, &key)?,
    };

    let bundle = export::write_bundle(&directory, &project, &result, &[])?;

    println!("{PRODUCT_NAME} {APP_VERSION} (model {MODEL_VERSION})");
    println!("  project:          {}", project.name);
    println!("  project hash:     {}", result.project_hash);
    println!("  model input hash: {}", result.model_input_hash);
    println!("  results hash:     {}", result.results_hash()?);
    println!("  scenarios:        {}", result.scenarios.len());
    println!();
    for scenario in &result.scenarios {
        println!(
            "  {name:<28} grid {cost:>12.2} USD/yr   NPV {npv:>12.2} USD   unmet {unmet:>8.1} kWh   residual {residual:.9} kWh",
            name = scenario.scenario_name,
            cost = scenario.economics.scenario_annual_grid_cost_usd,
            npv = scenario.economics.npv_usd,
            unmet = scenario.energy.unmet_critical_kwh,
            residual = scenario.energy.energy_balance_residual_kwh,
        );
    }
    println!();

    let warnings = result.all_warnings();
    if warnings.is_empty() {
        println!("  no warnings");
    } else {
        println!("  {} warning(s):", warnings.len());
        for warning in &warnings {
            println!(
                "    [{}] {} — {}",
                warning.severity, warning.code, warning.message
            );
        }
    }
    println!();
    println!("  bundle: {}", bundle.directory);
    for artifact in &bundle.artifacts {
        println!(
            "    {:<28} {:>10} bytes  {}",
            artifact.filename, artifact.size_bytes, artifact.sha256
        );
    }
    println!();
    println!("  Results are planning estimates for human review. Questions: {SUPPORT_EMAIL}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Run,
}

#[derive(Debug)]
struct Args {
    command: Command,
    workspace: Option<PathBuf>,
    port: u16,
    no_browser: bool,
    project: Option<String>,
    out: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Args {
    /// Parse the command line.
    ///
    /// Hand-rolled, and that is a deliberate trade. A derive-based argument parser
    /// would add half a dozen crates to a security-sensitive binary's dependency
    /// tree to handle six flags. This is sixty lines and it is auditable.
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut parsed = Args {
            command: Command::Serve,
            workspace: None,
            port: 0,
            no_browser: false,
            project: None,
            out: None,
            help: false,
            version: false,
        };

        let mut args = args.peekable();
        if args.peek().is_some_and(|first| first == "run") {
            parsed.command = Command::Run;
            args.next();
        }

        while let Some(arg) = args.next() {
            let mut value = || -> Result<String> {
                args.next().ok_or_else(|| {
                    Error::validation(
                        "validation.missing_argument",
                        "cli",
                        format!("{arg} needs a value"),
                    )
                })
            };
            match arg.as_str() {
                "--help" | "-h" => parsed.help = true,
                "--version" | "-V" => parsed.version = true,
                "--no-browser" => parsed.no_browser = true,
                "--workspace" => parsed.workspace = Some(PathBuf::from(value()?)),
                "--out" => parsed.out = Some(PathBuf::from(value()?)),
                "--project" => parsed.project = Some(value()?),
                "--port" => {
                    let raw = value()?;
                    parsed.port = raw.parse().map_err(|_| {
                        Error::validation(
                            "validation.bad_argument",
                            "cli.port",
                            format!("'{raw}' is not a port number"),
                        )
                    })?;
                }
                other => {
                    return Err(Error::validation(
                        "validation.unknown_argument",
                        "cli",
                        format!("unknown argument '{other}'. Run `tonans --help`."),
                    ));
                }
            }
        }
        Ok(parsed)
    }

    /// Where the workspace lives.
    ///
    /// Defaults under `%LOCALAPPDATA%`, which is per-user and not roamed. The path is
    /// *derived from the environment at runtime*: no user's home directory is ever
    /// compiled into the binary.
    fn workspace(&self) -> Result<PathBuf> {
        if let Some(explicit) = &self.workspace {
            return Ok(explicit.clone());
        }
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/share"))
            })
            .ok_or(Error::UnsafePath {
                code: "path.no_data_directory",
                message:
                    "Tonans could not work out where to store projects. Pass --workspace <DIR>."
                        .to_string(),
            })?;
        Ok(base.join("Flintglade").join("Tonans"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        Args::parse(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn the_default_command_is_to_serve() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.command, Command::Serve);
        assert_eq!(
            args.port, 0,
            "the port must default to 0 so the kernel picks one"
        );
        assert!(!args.no_browser);
    }

    #[test]
    fn the_run_subcommand_takes_a_project_and_an_output_directory() {
        let args = parse(&["run", "--project", "Site A", "--out", "C:\\tmp\\bundle"]).unwrap();
        assert_eq!(args.command, Command::Run);
        assert_eq!(args.project.as_deref(), Some("Site A"));
        assert_eq!(args.out, Some(PathBuf::from("C:\\tmp\\bundle")));
    }

    #[test]
    fn flags_are_parsed_and_unknown_ones_are_refused() {
        let args = parse(&["--no-browser", "--port", "8080", "--workspace", "W"]).unwrap();
        assert!(args.no_browser);
        assert_eq!(args.port, 8080);
        assert_eq!(args.workspace, Some(PathBuf::from("W")));

        let error = parse(&["--wat"]).unwrap_err();
        assert_eq!(error.code(), "validation.unknown_argument");

        let error = parse(&["--port", "not-a-port"]).unwrap_err();
        assert_eq!(error.code(), "validation.bad_argument");

        let error = parse(&["--project"]).unwrap_err();
        assert_eq!(error.code(), "validation.missing_argument");
    }

    #[test]
    fn help_and_version_are_recognised() {
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["-h"]).unwrap().help);
        assert!(parse(&["--version"]).unwrap().version);
        assert!(parse(&["-V"]).unwrap().version);
    }

    #[test]
    fn the_workspace_default_is_derived_from_the_environment_not_compiled_in() {
        // A hard-coded user directory in a shipped binary would be both a privacy leak
        // and a bug on every machine but one.
        assert!(!HELP.contains("C:\\Users"));
        let args = parse(&["--workspace", "X"]).unwrap();
        assert_eq!(args.workspace().unwrap(), PathBuf::from("X"));
    }

    #[test]
    fn the_help_text_states_the_product_boundary() {
        assert!(HELP.contains("planning estimates"));
        assert!(HELP.contains("127.0.0.1"));
        assert!(HELP.contains("support@flintglade.com"));
    }
}
