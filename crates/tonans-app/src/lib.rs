//! The Tonans application layer: the loopback server, the embedded UI, and the launch
//! token.
//!
//! This is a library so that there can be two front doors onto one implementation:
//! the `tonans` command in this crate, and the native window in `tonans-desktop`. Both
//! run this exact server, so neither can drift from the security envelope in
//! [`server`].
//!
//! Nothing here talks to the network, phones home, or writes outside the workspace.

pub mod api;
pub mod assets;
pub mod clock;
pub mod demo;
pub mod http;
pub mod server;
pub mod state;
pub mod token;

use std::path::PathBuf;

use tonans_core::error::{Error, Result};

/// Where the workspace lives when the user does not say.
///
/// Defaults under `%LOCALAPPDATA%` on Windows and the XDG data directory elsewhere,
/// both per-user and not roamed. The path is *derived from the environment at runtime*:
/// no user's home directory is ever compiled into the binary.
pub fn default_workspace() -> Result<PathBuf> {
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
            message: "Tonans could not work out where to store projects. Pass --workspace <DIR>."
                .to_string(),
        })?;
    Ok(base.join("Flintglade").join("Tonans"))
}
