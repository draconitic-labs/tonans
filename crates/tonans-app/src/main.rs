//! The `tonans` command: Tonans without a window.
//!
//! The product is the native window in `crates/tonans-desktop`. This command is what
//! remains useful without one:
//!
//! * `tonans run --project <name> --out <dir>` runs a project headlessly and writes
//!   the full export bundle. This is what a user would put in a script.
//! * `tonans` starts the same loopback server the window uses. It never opens a browser,
//!   and it shows its one-launch link only to a terminal. The packaged-binary smoke test
//!   drives the HTTP boundary through it, asking for the link with `--print-link`.
//!
//! Nothing here talks to the network, phones home, or writes outside the workspace.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use tonans_app::{clock, default_workspace, demo, server};
use tonans_core::error::{Error, Result};
use tonans_core::export;
use tonans_core::run;
use tonans_core::store::Workspace;
use tonans_core::version::{
    APP_VERSION, MODEL_VERSION, PRODUCT_NAME, SCHEMA_VERSION, SUPPORT_EMAIL,
};

const HELP: &str = "\
Tonans — microgrid scenario workbench, without a window

  tonans [OPTIONS]                       Start the loopback server
  tonans run --project <NAME> [OPTIONS]  Run a project and write an export bundle
  tonans --version
  tonans --help

Options:
  --workspace <DIR>   Where projects and exports live.
                      Default: a folder for this user, \"Draconitic Labs/Tonans\"
                      under %LOCALAPPDATA% on Windows and under $XDG_DATA_HOME
                      or ~/.local/share elsewhere.
  --port <PORT>       Loopback port. Default: 0, meaning the operating system
                      chooses one, which keeps it from being a fixed target.
  --print-link        Print the link with its one-launch token even when output is
                      being captured. Without this it goes only to a terminal. Whatever
                      captures it can drive the application until the server stops.
  --no-browser        Accepted for older scripts. This command never opens a browser.
  --project <NAME>    (run) The project to run.
  --out <DIR>         (run) Where to write the bundle. Default: the workspace's
                      exports directory.

For the application itself, run tonans-desktop, which opens a native window.

Tonans binds only to 127.0.0.1, serves only assets compiled into this binary, and
sends nothing anywhere. Results are planning estimates for human review.

Draconitic Labs — support@draconitic.com
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
        Command::Serve => serve(workspace, args.port, args.print_link),
        Command::Run => headless(workspace, args.project, args.out),
    }
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// Whether to print the link that carries the launch token.
///
/// The token grants control of the application, so it should not land in a log, a
/// service journal, or a CI transcript just because this command's output went there.
/// A terminal is a person reading it; anywhere else has to ask.
const fn show_link(stdout_is_terminal: bool, asked: bool) -> bool {
    stdout_is_terminal || asked
}

fn serve(workspace: Workspace, port: u16, print_link: bool) -> Result<()> {
    // The demonstration project is installed on first launch by importing the embedded
    // sample CSV through the real importer. It is not a fixture smuggled in through a
    // side door: if the importer would reject it, the demo does not exist.
    demo::ensure_installed(&workspace)?;

    let server = server::Server::bind(workspace, port)?;

    println!("{PRODUCT_NAME} {APP_VERSION} (model {MODEL_VERSION})");
    println!("  workspace: {}", server.state.workspace.root().display());
    println!();
    if show_link(std::io::stdout().is_terminal(), print_link) {
        println!("  This link carries the one-launch token; without it the API refuses.");
        println!("  {}", server.url);
    } else {
        println!("  Listening on http://{}/", server.address);
        println!("  The link with its one-launch token goes only to a terminal, and this");
        println!("  output is being captured. Run this in a terminal to see it, or use the");
        println!("  desktop app, tonans-desktop.");
    }
    println!();
    println!("  Bound to loopback only. Nothing is sent anywhere. Press Ctrl+C to stop.");

    server.serve()
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
    print_link: bool,
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
            print_link: false,
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
                // Older scripts pass this. The command never opens a browser now, so
                // there is nothing left for it to suppress.
                "--no-browser" => {}
                "--print-link" => parsed.print_link = true,
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

    /// Where the workspace lives: the explicit flag, or the per-user default.
    fn workspace(&self) -> Result<PathBuf> {
        match &self.workspace {
            Some(explicit) => Ok(explicit.clone()),
            None => default_workspace(),
        }
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
        assert!(HELP.contains("support@draconitic.com"));
    }

    #[test]
    fn the_link_goes_only_to_a_terminal_unless_someone_asks() {
        assert!(show_link(true, false));
        assert!(
            !show_link(false, false),
            "captured output must not get the token"
        );
        assert!(show_link(false, true));
        assert!(show_link(true, true));
        assert!(!parse(&[]).unwrap().print_link, "off by default");
        assert!(parse(&["--print-link"]).unwrap().print_link);
    }

    #[test]
    fn this_command_never_starts_another_program() {
        // The point of the native window: nothing in this binary may launch a browser.
        // It spawns no process of any kind.
        let source = include_str!("main.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        assert!(!code.contains("process::Command"));
    }
}
