#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: the signed FirstMate bundle can only be built on macOS" >&2
  exit 1
fi

# Use the certificate fingerprint, not its display name: this machine can have
# multiple valid certificates with the same label and codesign rejects an
# ambiguous name. Callers may pin another valid identity explicitly.
identity="${BUZZ_MACOS_SIGNING_IDENTITY:-}"
if [[ -z "$identity" ]]; then
  identity="$({
    security find-identity -v -p codesigning 2>/dev/null || true
  } | awk '
    /"Apple Development:/ { print $2; exit }
    /"Developer ID Application:/ && fallback == "" { fallback = $2 }
    END { if (NR > 0 && fallback != "") print fallback }
  ' | head -n 1)"
fi

if [[ ! "$identity" =~ ^[[:xdigit:]]{40}$ ]]; then
  echo "error: no unambiguous Apple code-signing identity was found" >&2
  echo "Install an Apple Development certificate or set BUZZ_MACOS_SIGNING_IDENTITY." >&2
  exit 1
fi

export APPLE_SIGNING_IDENTITY="$identity"

# Tauri bundles the pre-generated binaries from src-tauri/binaries; building
# only the desktop crate can therefore silently ship an older ACP sidecar.
# Rebuild and stage every external binary before assembling the signed app.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target="$(rustc -vV | sed -n 's|host: ||p')"
target_dir="$(cd "$repo_root" && cargo metadata --format-version 1 --no-deps | node -p "JSON.parse(require('fs').readFileSync(0, 'utf8')).target_directory")"
sidecars=(buzz-acp buzz-agent buzz-dev-mcp git-credential-nostr buzz)
packages=(buzz-acp buzz-agent buzz-dev-mcp git-credential-nostr buzz-cli)
if [[ "$target" != *windows* ]]; then
  sidecars+=(buzz-backend-kubernetes)
  packages+=(buzz-backend-kubernetes)
fi

cargo_args=()
for package in "${packages[@]}"; do
  cargo_args+=(-p "$package")
done
(cd "$repo_root" && cargo build --release "${cargo_args[@]}")
mkdir -p "$repo_root/desktop/src-tauri/binaries"
for bin in "${sidecars[@]}"; do
  install -m 755 "$target_dir/release/$bin" \
    "$repo_root/desktop/src-tauri/binaries/$bin-$target"
done

herdr_source="${BUZZ_FIRSTMATE_HERDR_BINARY:-}"
if [[ ! -f "$herdr_source" || ! -x "$herdr_source" ]]; then
  echo "error: set BUZZ_FIRSTMATE_HERDR_BINARY to a trusted executable Herdr binary" >&2
  exit 1
fi
if [[ "$("$herdr_source" --version 2>/dev/null)" != "herdr 0.8."* ]]; then
  echo "error: Buzz FirstMate currently requires a Herdr 0.8.x binary" >&2
  exit 1
fi

(cd "$repo_root/desktop" && pnpm exec tauri build --bundles app "$@")

# Herdr is not a general Buzz sidecar. Ship the broker-tested version only in
# this FirstMate distribution, beside buzz-desktop where discovery can prove
# and inject its canonical path into every managed FirstMate contract.
bundle_dir="$repo_root/desktop/src-tauri/target/release/bundle/macos"
if [[ ! -d "$bundle_dir" ]]; then
  bundle_dir="$target_dir/release/bundle/macos"
fi
app_bundle="$(find "$bundle_dir" -maxdepth 1 -type d -name '*.app' -print -quit)"
if [[ -z "$app_bundle" ]]; then
  echo "error: Tauri did not produce a macOS app bundle under $bundle_dir" >&2
  exit 1
fi
install -m 755 "$herdr_source" "$app_bundle/Contents/MacOS/herdr"
codesign --force --options runtime --sign "$identity" "$app_bundle/Contents/MacOS/herdr"
codesign --force --deep --options runtime \
  --entitlements "$repo_root/desktop/src-tauri/Entitlements.plist" \
  --sign "$identity" "$app_bundle"
codesign --verify --deep --strict "$app_bundle"
