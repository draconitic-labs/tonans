//! Name and path safety.
//!
//! Tonans never accepts a filesystem path from the browser. The UI uploads CSV
//! *bytes* and addresses projects by *name*; this module is what turns a name the
//! user typed — or a filename a CSV arrived with — into something safe to put on
//! a Windows filesystem and safe to echo into an HTML report.
//!
//! The threats, in the order they are defended against below:
//!
//! * **Traversal.** `../../Windows/System32/drivers/etc/hosts` must never resolve
//!   outside the workspace. We reject separators outright rather than
//!   canonicalising and comparing, because rejecting is checkable and
//!   canonicalisation on Windows has too many equivalent spellings to trust.
//! * **Alternate spellings.** `\\?\`, `\\server\share`, `C:name`, and NTFS
//!   alternate data streams (`file.csv:evil`) all reach places a plain name
//!   cannot. All are rejected.
//! * **Reserved device names.** Opening `CON`, `NUL`, or `LPT1` on Windows opens a
//!   device, not a file, whatever extension is appended.
//! * **Trailing dots and spaces.** Windows silently strips them, so `evil ` and
//!   `evil` are the same file, and a uniqueness check on the raw string lies.
//! * **Bidirectional overrides.** `report\u{202E}gnp.exe` renders as `reportexe.png`.
//!   A name that renders as something other than what it is has no legitimate use.

use crate::error::{Error, Result};

/// Longest accepted file or project name, in characters.
const MAX_FILENAME_CHARS: usize = 96;

/// Windows device names. Reserved with *any* extension, and case-insensitively.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a name that will become a file on disk, or a label in a report.
///
/// Returns the name unchanged on success. Deliberately *not* a sanitizer: it does
/// not strip offending characters and hand back something almost like what the
/// user asked for. A name Tonans cannot store is an error the user gets to see
/// and fix, not a name Tonans quietly invents.
pub fn safe_name(kind: &str, name: &str) -> Result<String> {
    let reject = |code: &'static str, reason: &str| -> Error {
        // The rejected bytes are never echoed back. A name is under an attacker's
        // control, and this message reaches a log, a terminal, and an HTML page.
        Error::UnsafePath {
            code,
            message: format!("that {kind} cannot be used: {reason}"),
        }
    };

    if name.is_empty() {
        return Err(reject("path.empty", "it is empty"));
    }
    let char_count = name.chars().count();
    if char_count > MAX_FILENAME_CHARS {
        return Err(reject(
            "path.too_long",
            &format!("it is {char_count} characters; the limit is {MAX_FILENAME_CHARS}"),
        ));
    }

    if name.contains('/') || name.contains('\\') {
        return Err(reject(
            "path.separator",
            "it contains a path separator. Tonans addresses projects and files by name, never by path.",
        ));
    }
    if name.contains(':') {
        return Err(reject(
            "path.drive_or_stream",
            "it contains a colon, which on Windows names a drive or an alternate data stream",
        ));
    }
    if name == "." || name == ".." {
        return Err(reject("path.traversal", "it is a directory reference"));
    }

    for c in name.chars() {
        if c.is_control() {
            return Err(reject(
                "path.control_character",
                &format!("it contains the control character U+{:04X}", u32::from(c)),
            ));
        }
        if crate::schema::is_bidi_control(c) {
            return Err(reject(
                "path.bidi_override",
                "it contains a bidirectional override, which would make it render as a different name",
            ));
        }
        // The Windows reserved set, plus the shell metacharacters that make a
        // name dangerous to hand to anything else later.
        if matches!(c, '<' | '>' | '"' | '|' | '?' | '*') {
            return Err(reject(
                "path.reserved_character",
                &format!(
                    "it contains the character '{c}', which Windows does not allow in a filename"
                ),
            ));
        }
    }

    if name.ends_with(' ') || name.ends_with('.') {
        return Err(reject(
            "path.trailing_dot_or_space",
            "it ends with a dot or a space, which Windows silently strips",
        ));
    }
    if name.starts_with(' ') {
        return Err(reject("path.leading_space", "it begins with a space"));
    }

    // A device name is reserved whatever follows the first dot: `NUL.csv` is
    // still the null device.
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(reject(
            "path.reserved_device",
            "it is a reserved Windows device name (CON, PRN, AUX, NUL, COM1-9, LPT1-9)",
        ));
    }

    Ok(name.to_string())
}

/// Validate a project name and derive the file name it is stored under.
///
/// The `.tonans.json` suffix is appended by us, never taken from the user, so a
/// project cannot be made to write a `.ps1` or a `.lnk`.
pub fn project_file_name(project_name: &str) -> Result<String> {
    let name = safe_name("project name", project_name)?;
    if name.contains('.') {
        return Err(Error::UnsafePath {
            code: "path.extension_in_name",
            message:
                "that project name cannot be used: it contains a dot. Tonans chooses the file \
                      extension itself."
                    .to_string(),
        });
    }
    Ok(format!("{name}.tonans.json"))
}

/// Validate the name a CSV arrived under.
///
/// The name is never used as a path — it is recorded in the provenance manifest
/// and rendered in the report — but it is validated exactly as strictly as a name
/// we would create ourselves, and it is validated *before* a byte of the file is
/// read.
///
/// It does **not** trim a path down to its leaf. The browser File API gives us
/// `file.name`, which is a bare name and never a path, so anything arriving here
/// with a separator in it came from a client we did not write. Quietly rewriting
/// `../../evil.csv` to `evil.csv` would put a name in the audit trail that was
/// never the name the caller sent — a repair, in the one place where a repair is
/// least defensible. It is refused instead, like every other name Tonans cannot
/// take at face value.
pub fn source_file_name(supplied: &str) -> Result<String> {
    safe_name("file name", supplied)
}

/// Join `name` beneath `root`, refusing anything that could escape it.
///
/// `name` has already been through [`safe_name`], so this is defence in depth
/// rather than the primary control: it re-checks after the join, against the
/// resolved parent, so that a symlink or junction planted in the workspace cannot
/// redirect a write. The parent must exist; the file itself need not.
pub fn confine(root: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    safe_name("name", name)?;
    let candidate = root.join(name);

    let resolved_root = root
        .canonicalize()
        .map_err(|e| Error::io(format!("resolving the workspace at {}", root.display()), e))?;
    let resolved_parent = candidate
        .parent()
        .ok_or(Error::UnsafePath {
            code: "path.no_parent",
            message: "that name cannot be used: it has no parent directory".to_string(),
        })?
        .canonicalize()
        .map_err(|e| Error::io("resolving the workspace directory", e))?;

    if resolved_parent != resolved_root {
        return Err(Error::UnsafePath {
            code: "path.escapes_workspace",
            message: "that name cannot be used: it resolves outside the Tonans workspace"
                .to_string(),
        });
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_accepted() {
        for name in [
            "meter.csv",
            "Warehouse 3 load.csv",
            "site_2024-06.csv",
            "data (final).csv",
            "a",
        ] {
            assert!(safe_name("file name", name).is_ok(), "{name}");
        }
    }

    #[test]
    fn traversal_is_rejected_in_every_spelling() {
        for name in [
            "../secrets.csv",
            "..\\secrets.csv",
            "../../Windows/System32/drivers/etc/hosts",
            "..",
            ".",
            "sub/dir.csv",
            "sub\\dir.csv",
            "/etc/passwd",
            "\\\\server\\share\\x.csv",
            "\\\\?\\C:\\x.csv",
        ] {
            let error = safe_name("file name", name).unwrap_err();
            assert!(
                matches!(
                    error.code(),
                    "path.separator" | "path.traversal" | "path.drive_or_stream"
                ),
                "{name} produced {}",
                error.code()
            );
        }
    }

    #[test]
    fn drive_letters_and_alternate_data_streams_are_rejected() {
        assert_eq!(
            safe_name("f", "C:evil.csv").unwrap_err().code(),
            "path.drive_or_stream"
        );
        assert_eq!(
            safe_name("f", "report.csv:hidden").unwrap_err().code(),
            "path.drive_or_stream"
        );
    }

    #[test]
    fn windows_device_names_are_rejected_with_any_extension() {
        for name in [
            "NUL",
            "nul",
            "CON",
            "con.csv",
            "LPT1.txt",
            "aux.tonans.json",
            "COM9",
        ] {
            assert_eq!(
                safe_name("f", name).unwrap_err().code(),
                "path.reserved_device",
                "{name}"
            );
        }
        // Not reserved: the stem differs.
        assert!(safe_name("f", "console.csv").is_ok());
        assert!(safe_name("f", "nullify.csv").is_ok());
    }

    #[test]
    fn control_characters_and_bidi_overrides_are_rejected() {
        assert_eq!(
            safe_name("f", "load\0.csv").unwrap_err().code(),
            "path.control_character"
        );
        assert_eq!(
            safe_name("f", "load\n.csv").unwrap_err().code(),
            "path.control_character"
        );
        assert_eq!(
            safe_name("f", "load\r\n.csv").unwrap_err().code(),
            "path.control_character"
        );
        // Renders as "report<something>.csv" but is really an executable name.
        assert_eq!(
            safe_name("f", "report\u{202E}gnp.exe").unwrap_err().code(),
            "path.bidi_override"
        );
        assert_eq!(
            safe_name("f", "a\u{200F}b.csv").unwrap_err().code(),
            "path.bidi_override"
        );
    }

    #[test]
    fn trailing_dots_and_spaces_are_rejected() {
        assert_eq!(
            safe_name("f", "report.csv ").unwrap_err().code(),
            "path.trailing_dot_or_space"
        );
        assert_eq!(
            safe_name("f", "report.").unwrap_err().code(),
            "path.trailing_dot_or_space"
        );
        assert_eq!(
            safe_name("f", " report.csv").unwrap_err().code(),
            "path.leading_space"
        );
    }

    #[test]
    fn oversized_and_empty_names_are_rejected() {
        assert_eq!(safe_name("f", "").unwrap_err().code(), "path.empty");
        let long = format!("{}.csv", "a".repeat(MAX_FILENAME_CHARS));
        assert_eq!(safe_name("f", &long).unwrap_err().code(), "path.too_long");
    }

    #[test]
    fn a_source_file_name_is_refused_rather_than_trimmed_to_its_leaf() {
        // The name the browser sends is `file.name`: a bare name, always.
        assert_eq!(source_file_name("meter.csv").unwrap(), "meter.csv");

        // Anything carrying a path came from a client we did not write. Refuse it
        // rather than silently recording `evil.csv` in the provenance manifest as
        // though that were the name we were handed.
        for supplied in [
            "../../evil.csv",
            "..\\..\\evil.csv",
            "/home/user/meter.csv",
            "C:\\fakepath\\meter.csv",
            "C:\\Windows\\x.csv",
            "sub/meter.csv",
            "",
        ] {
            let error = source_file_name(supplied).unwrap_err();
            assert!(
                error.code().starts_with("path."),
                "{supplied} produced {}",
                error.code()
            );
        }
    }

    #[test]
    fn project_names_may_not_choose_their_own_extension() {
        assert_eq!(
            project_file_name("Warehouse 3").unwrap(),
            "Warehouse 3.tonans.json"
        );
        assert_eq!(
            project_file_name("evil.ps1").unwrap_err().code(),
            "path.extension_in_name"
        );
        assert!(project_file_name("../evil").is_err());
    }

    #[test]
    fn confine_rejects_anything_that_leaves_the_root() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confine(root, "Cargo.toml").is_ok());
        assert!(confine(root, "../Cargo.toml").is_err());
        assert!(confine(root, "..\\..\\Cargo.toml").is_err());
        assert!(confine(root, "src/lib.rs").is_err());
    }
}
