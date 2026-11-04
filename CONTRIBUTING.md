# Contributing to Tonans

Bug reports, model questions, documentation corrections and focused pull requests are welcome. Open an issue before starting a large change.

By submitting a contribution, you agree that it is licensed under the [Apache License 2.0](LICENSE), as described in section 5 of that license.

The workspace includes the desktop shell, so on Linux the commands below need the WebKitGTK and GTK development packages (`webkit2gtk4.1-devel gtk3-devel libsoup3-devel` on Fedora, `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev` on Debian and Ubuntu). Test interface changes in the native window, not a browser.

Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Changes to the energy model need a hand-calculated test. Include your Tonans version, operating system and a synthetic example in bug reports; do not attach real site data.

Report security problems privately as described in [docs/SECURITY.md](docs/SECURITY.md).
