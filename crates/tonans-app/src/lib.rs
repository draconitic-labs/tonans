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

use std::ffi::OsString;
use std::path::PathBuf;

use tonans_core::error::{Error, Result};

/// Where the workspace lives when the user does not say.
///
/// Defaults under `%LOCALAPPDATA%` on Windows and the XDG data directory elsewhere,
/// both per-user and not roamed. The path is *derived from the environment at runtime*:
/// no user's home directory is ever compiled into the binary.
pub fn default_workspace() -> Result<PathBuf> {
    workspace_under(
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

fn workspace_under(
    local_app_data: Option<OsString>,
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    // A variable set but empty, or holding a relative path, names nothing. The XDG
    // specification says to ignore a relative value; taking it at its word would put
    // the workspace under whatever directory the process happened to start in.
    let usable =
        |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    let base = usable(local_app_data)
        .or_else(|| usable(xdg_data_home))
        .or_else(|| usable(home).map(|home| home.join(".local/share")))
        .ok_or(Error::UnsafePath {
            code: "path.no_data_directory",
            message: "Could not work out where to store projects. Pass --workspace <DIR>."
                .to_string(),
        })?;
    Ok(base.join("Draconitic Labs").join("Tonans"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(name: &str) -> OsString {
        std::env::temp_dir().join(name).into_os_string()
    }

    #[test]
    fn the_workspace_comes_from_the_first_usable_variable() {
        let expected = |base: &str| {
            PathBuf::from(absolute(base))
                .join("Draconitic Labs")
                .join("Tonans")
        };
        assert_eq!(
            workspace_under(
                Some(absolute("a")),
                Some(absolute("b")),
                Some(absolute("c"))
            )
            .unwrap(),
            expected("a")
        );
        assert_eq!(
            workspace_under(None, Some(absolute("b")), Some(absolute("c"))).unwrap(),
            expected("b")
        );
        assert_eq!(
            workspace_under(None, None, Some(absolute("c"))).unwrap(),
            PathBuf::from(absolute("c"))
                .join(".local/share")
                .join("Draconitic Labs")
                .join("Tonans")
        );
    }

    #[test]
    fn a_variable_set_but_empty_or_relative_names_nothing() {
        for useless in ["", "relative/path", "."] {
            let value = || Some(OsString::from(useless));
            // Falls through to the next variable rather than to the current directory.
            assert_eq!(
                workspace_under(value(), value(), Some(absolute("c"))).unwrap(),
                PathBuf::from(absolute("c"))
                    .join(".local/share")
                    .join("Draconitic Labs")
                    .join("Tonans"),
                "{useless:?}"
            );
            let error = workspace_under(value(), value(), value()).unwrap_err();
            assert_eq!(error.code(), "path.no_data_directory", "{useless:?}");
        }
        assert!(workspace_under(None, None, None).is_err());
    }
}
