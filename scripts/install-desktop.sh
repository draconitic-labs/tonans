#!/usr/bin/env bash
# Install the Tonans desktop app for this user, or remove it again.
#
#   scripts/install-desktop.sh              Build a release and install it
#   scripts/install-desktop.sh --uninstall  Remove what an earlier install put there
#
# Everything goes under ~/.local and nothing needs root. It installs one binary, one menu
# entry, and an icon set. Your projects are not part of this: they live in the workspace
# folder, and neither installing nor uninstalling touches them.
#
# It replaces or removes a file only if it can recognise it as its own. Each install
# records the SHA-256 of every file it writes, and a file that no longer matches that
# record, or was never in it, is left alone and named. Nothing here deletes a folder.
set -euo pipefail

# The application id. The menu entry's file name and icon name must match it, because
# under Wayland that is how the desktop connects a window to its launcher.
APP_ID="com.flintglade.tonans"

repo_root() { cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P; }

# Paths are worked out here and nowhere else, so that uninstalling acts on this list and
# never on paths read back from a file.
set_paths() {
  : "${HOME:?HOME is not set}"
  BIN_DIR="$HOME/.local/bin"
  case "${XDG_DATA_HOME:-}" in
    /*) DATA_DIR="$XDG_DATA_HOME" ;;
    *) DATA_DIR="$HOME/.local/share" ;;
  esac
  BIN="$BIN_DIR/tonans-desktop"
  ENTRY="$DATA_DIR/applications/$APP_ID.desktop"
  ICON_DIR="$DATA_DIR/icons/hicolor"
  MANIFEST="$DATA_DIR/tonans-desktop/installed-files.sha256"
  ICONS=(
    "$ICON_DIR/32x32/apps/$APP_ID.png"
    "$ICON_DIR/128x128/apps/$APP_ID.png"
    "$ICON_DIR/256x256/apps/$APP_ID.png"
    "$ICON_DIR/512x512/apps/$APP_ID.png"
    "$ICON_DIR/scalable/apps/$APP_ID.svg"
  )
  OURS=("$BIN" "$ENTRY" "${ICONS[@]}")
}

sha_of() { sha256sum -- "$1" | cut -d' ' -f1; }

# The hash an earlier install recorded for a path, or nothing.
recorded_hash() {
  [ -f "$MANIFEST" ] || return 0
  awk -v want="$1" 'substr($0, 67) == want { print $1; exit }' "$MANIFEST"
}

# Whether a file on disk is exactly what an earlier install wrote.
is_ours() {
  local recorded
  recorded="$(recorded_hash "$1")"
  [ -n "$recorded" ] && [ -f "$1" ] && [ ! -L "$1" ] && [ "$(sha_of "$1")" = "$recorded" ]
}

# Refuse before writing anything, so a refusal never leaves half an install.
check_targets() {
  local refused=0 target
  for target in "${OURS[@]}"; do
    if [ -e "$target" ] || [ -L "$target" ]; then
      if ! is_ours "$target"; then
        echo "install-desktop: not replacing $target: this script did not write it, or it has changed since." >&2
        refused=1
      fi
    fi
  done
  if [ "$refused" -ne 0 ]; then
    echo "install-desktop: move those files away if they are not yours to keep, then run this again." >&2
    return 1
  fi
}

# Write one file by copying then renaming, so that a running binary is never overwritten
# in place and a failure never leaves a partial file under the real name.
put_file() {
  local source="$1" target="$2" mode="$3" staged
  mkdir -p -- "$(dirname "$target")"
  staged="$target.installing.$$"
  install -m "$mode" -- "$source" "$staged"
  mv -f -- "$staged" "$target"
  # The record moves with each file, so a failure partway leaves it true.
  {
    if [ -f "$MANIFEST" ]; then awk -v want="$target" 'substr($0, 67) != want' "$MANIFEST"; fi
    printf '%s  %s\n' "$(sha_of "$target")" "$target"
  } > "$MANIFEST.new"
  mv -f -- "$MANIFEST.new" "$MANIFEST"
}

refresh_desktop_caches() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$DATA_DIR/applications" || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$ICON_DIR" || true
  fi
}

# Install from a built binary and a repository checkout.
install_from() {
  local built="$1" root="$2" entry_text escaped
  case "$BIN" in
    *[\"\`\$\\%]*)
      echo "install-desktop: the install path holds a character a menu entry cannot quote: $BIN" >&2
      return 1
      ;;
  esac
  check_targets

  mkdir -p -- "$(dirname "$MANIFEST")"
  put_file "$built" "$BIN" 755

  local icons="$root/crates/tonans-desktop/icons" scratch
  scratch="$MANIFEST.entry"
  escaped="$(printf '%s' "$BIN" | sed -e 's/[&|]/\\&/g')"
  entry_text="$(sed -e "s|@EXEC@|$escaped|" "$root/packaging/linux/$APP_ID.desktop")"
  printf '%s\n' "$entry_text" > "$scratch"
  put_file "$scratch" "$ENTRY" 644
  rm -- "$scratch"

  put_file "$icons/32x32.png" "${ICONS[0]}" 644
  put_file "$icons/128x128.png" "${ICONS[1]}" 644
  put_file "$icons/128x128@2x.png" "${ICONS[2]}" 644
  put_file "$icons/icon.png" "${ICONS[3]}" 644
  put_file "$root/crates/tonans-app/ui/favicon.svg" "${ICONS[4]}" 644

  refresh_desktop_caches
}

uninstall() {
  local target kept=0
  for target in "${OURS[@]}"; do
    if [ ! -e "$target" ] && [ ! -L "$target" ]; then
      continue
    fi
    if is_ours "$target"; then
      rm -- "$target"
      echo "removed $target"
    else
      echo "left $target in place: this script did not write it, or it has changed since."
      kept=1
    fi
  done
  if [ "$kept" -eq 0 ] && [ -f "$MANIFEST" ]; then
    rm -- "$MANIFEST"
    rmdir -- "$(dirname "$MANIFEST")" 2>/dev/null || true
  fi
  refresh_desktop_caches
  echo
  echo "Your projects were not touched. They are in the workspace folder, which by default"
  echo "is $DATA_DIR/Flintglade/Tonans. Remove it yourself if you want them gone."
}

check_build_needs() {
  command -v cargo >/dev/null 2>&1 || {
    echo "install-desktop: cargo was not found. Install Rust first: the pinned version is in rust-toolchain.toml." >&2
    return 1
  }
  if command -v pkg-config >/dev/null 2>&1 && ! pkg-config --exists webkit2gtk-4.1 gtk+-3.0; then
    echo "install-desktop: the WebKitGTK and GTK development packages are missing." >&2
    echo "  Fedora:        sudo dnf install webkit2gtk4.1-devel gtk3-devel libsoup3-devel" >&2
    echo "  Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev" >&2
    return 1
  fi
}

main() {
  set_paths
  case "${1:-}" in
    "")
      local root target_dir
      root="$(repo_root)"
      check_build_needs
      check_targets
      (cd "$root" && cargo build --release --locked -p tonans-desktop)
      target_dir="$(cd "$root" && cargo metadata --format-version 1 --no-deps --locked \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
      install_from "$target_dir/release/tonans-desktop" "$root"
      echo "Installed Tonans. It is in your applications menu, or run: $BIN"
      case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) echo "Note: $BIN_DIR is not on your PATH, so the menu entry is the easy way in." ;;
      esac
      ;;
    --uninstall)
      uninstall
      ;;
    -h|--help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'
      ;;
    *)
      echo "install-desktop: unknown argument '$1'. Try --help." >&2
      return 2
      ;;
  esac
}

# Runs only when executed, so the functions above can be tested in a scratch folder.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
