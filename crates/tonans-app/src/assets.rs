//! The embedded user interface.
//!
//! Every byte the browser loads is compiled into the binary. There is no asset
//! directory to ship, nothing to fetch at runtime, and no way for a stale file on
//! disk to change what the application does. It is also what makes the
//! `Content-Security-Policy` in `server.rs` truthful: `default-src 'none'` is only
//! honest if the page genuinely needs nothing from anywhere.

use crate::http::Response;

const INDEX: &str = include_str!("../ui/index.html");
const SCRIPT: &str = include_str!("../ui/app.js");
const STYLESHEET: &str = include_str!("../ui/styles.css");
const FAVICON: &str = include_str!("../ui/favicon.svg");

/// `GET /`
pub fn index() -> Response {
    Response::new(200, "text/html; charset=utf-8", INDEX)
}

/// `GET /app.js`
pub fn script() -> Response {
    Response::new(200, "text/javascript; charset=utf-8", SCRIPT)
}

/// `GET /styles.css`
pub fn stylesheet() -> Response {
    Response::new(200, "text/css; charset=utf-8", STYLESHEET)
}

/// `GET /favicon.svg`
pub fn favicon() -> Response {
    Response::new(200, "image/svg+xml", FAVICON)
}

/// `GET /palette.css`
///
/// Generated from the single validated table in `tonans_core::svg::palette`, rather
/// than copied into `styles.css`. The live UI and the exported report therefore
/// cannot drift apart: change a hue in one place and both move, and the
/// colour-vision test in `tonans-core` guards both.
pub fn palette() -> Response {
    Response::new(
        200,
        "text/css; charset=utf-8",
        tonans_core::svg::palette_css(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drop comment-only lines.
    ///
    /// The source scanners below look for forbidden *code*. Without this they also
    /// find the comments that explain why it is forbidden, which is a special kind of
    /// stupid: the file would fail its own test for documenting itself.
    fn code_only(source: &str) -> String {
        source
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !(trimmed.starts_with("//")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                    || trimmed.starts_with("<!--"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every hex colour literal in a stylesheet, ignoring `#id` selectors.
    ///
    /// A run of exactly 3, 4, 6, or 8 hex digits after a `#` is a colour. `#project-list`
    /// is not, and neither is `#csv-file` — even though `c` is a hex digit, the run
    /// stops at `s`.
    fn hex_colours(css: &str) -> Vec<String> {
        let bytes: Vec<char> = css.chars().collect();
        let mut found = Vec::new();
        for (index, c) in bytes.iter().enumerate() {
            if *c != '#' {
                continue;
            }
            let run: String = bytes[index + 1..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            let next = bytes.get(index + 1 + run.len());
            let terminated =
                next.is_none_or(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_');
            if matches!(run.len(), 3 | 4 | 6 | 8) && terminated {
                found.push(format!("#{run}"));
            }
        }
        found
    }

    #[test]
    fn the_ui_fetches_nothing_from_the_network() {
        // The CSP would block it anyway, but a reference to a CDN in the source is a
        // sign someone tried, and it must not survive review.
        for (name, source) in [
            ("index.html", INDEX),
            ("app.js", SCRIPT),
            ("styles.css", STYLESHEET),
        ] {
            for forbidden in [
                "http://",
                "https://",
                "//cdn",
                "googleapis",
                "unpkg",
                "jsdelivr",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{name} references '{forbidden}'. The UI must be entirely self-contained."
                );
            }
        }
    }

    #[test]
    fn untrusted_text_can_never_reach_the_dom_as_markup() {
        // A scenario name, a warning message, and a source filename all come from data
        // Tonans does not control. Every one of them reaches the page through
        // `textContent`. An `innerHTML` here would be an injection waiting to happen.
        let code = code_only(SCRIPT);
        for forbidden in [
            "eval(",
            "new Function(",
            "innerHTML",
            "outerHTML",
            "document.write",
            "insertAdjacentHTML",
        ] {
            assert!(
                !code.contains(forbidden),
                "app.js uses '{forbidden}'. Build DOM nodes and set textContent instead."
            );
        }
    }

    #[test]
    fn import_mapping_requires_explicit_meaning_and_unit_choices() {
        assert!(SCRIPT.contains("['__unmapped', 'Choose meaning…']"));
        assert!(SCRIPT.contains("['__timestamp', 'Timestamp']"));
        assert!(SCRIPT.contains("text: 'Choose unit…'"));
        assert!(SCRIPT.contains("if (role === '__unmapped') unresolved.push(header)"));
        assert!(SCRIPT.contains("if (!unit) missingUnits.push(header)"));
        assert!(!SCRIPT.contains("guessRole("));
        assert!(!SCRIPT.contains("guessUnit("));
    }

    #[test]
    fn the_page_carries_no_inline_script_or_style() {
        // The CSP is `script-src 'self'; style-src 'self'` with no `unsafe-inline`, so
        // an inline block would simply not run — a confusing bug to chase later.
        assert!(
            !INDEX.contains("<script>"),
            "index.html has an inline script block"
        );
        assert!(
            !INDEX.contains("<style>"),
            "index.html has an inline style block"
        );
        assert!(INDEX.contains(r#"<script src="/app.js""#));
        assert!(INDEX.contains(r#"href="/styles.css""#));
        assert!(INDEX.contains(r#"href="/palette.css""#));
    }

    #[test]
    fn the_disclaimer_is_in_the_markup_not_only_in_the_script() {
        // If the script fails to load, the page must still say what this is.
        assert!(INDEX.contains("Planning estimate"));
        assert!(INDEX.contains("not an AC power-flow study"));
        assert!(
            INDEX.contains("never controls equipment")
                || INDEX.contains("It never controls equipment")
        );
    }

    #[test]
    fn the_diagram_is_called_an_energy_flow_and_never_a_power_flow() {
        // The product boundary in PRODUCT-SPEC.md. Tonans does not compute AC power
        // flow, and no screen may imply that it does. The phrase may appear only in a
        // negation, so what is checked is the *label* of the diagram itself.
        assert!(
            SCRIPT.contains("'Energy flow'"),
            "the diagram must be labelled 'Energy flow'"
        );
        assert!(
            !SCRIPT.contains("'Power flow'") && !SCRIPT.contains("\"Power flow\""),
            "something in the UI is labelled 'Power flow'"
        );
        // And every mention of power flow anywhere in the UI must be a denial.
        for source in [INDEX, SCRIPT] {
            for (index, _) in source.match_indices("power-flow") {
                let context = &source[index.saturating_sub(40)..index];
                assert!(
                    context.contains("not") || context.contains("Not"),
                    "a mention of power-flow is not a denial: ...{context}"
                );
            }
        }
    }

    #[test]
    fn the_palette_reaches_the_ui_from_the_one_validated_table() {
        let css = tonans_core::svg::palette_css();
        assert!(css.contains("--c-load"));
        assert!(css.contains("prefers-color-scheme:dark"));

        // styles.css must not hard-code a hue. The palette is generated from one
        // validated table, and a literal hex here would drift away from the
        // colour-vision test that guards it — silently, and only for some readers.
        //
        // `#fff` is the exception: it is the ink *on top of* a filled mark, not a
        // series colour, and it is not part of the categorical palette.
        for colour in hex_colours(&code_only(STYLESHEET)) {
            assert!(
                colour == "#fff" || colour == "#ffffff",
                "styles.css hard-codes the colour {colour}. Take it from palette.css, so that the \
                 colour-vision test in tonans-core still guards it."
            );
        }
    }

    #[test]
    fn the_hex_scanner_can_tell_a_colour_from_an_id_selector() {
        // Guarding the guard: if this scanner had a false negative, the test above
        // would pass while the stylesheet quietly hard-coded a hue.
        assert_eq!(hex_colours("a { color: #fff; }"), vec!["#fff"]);
        assert_eq!(hex_colours("a { color: #2a78d6; }"), vec!["#2a78d6"]);
        assert!(hex_colours("#project-list { margin: 0; }").is_empty());
        assert!(hex_colours("#csv-file { display: none; }").is_empty()); // 'c' is hex, 's' is not
        assert!(hex_colours("#empty-state {}").is_empty());
        assert!(hex_colours("#view-data {}").is_empty());
        assert_eq!(
            hex_colours("#abc {}"),
            vec!["#abc"],
            "a 3-hex id is indistinguishable and must be flagged"
        );
    }
}
