#!/bin/sh
# openpages installer.
#
#   curl -fsSL https://raw.githubusercontent.com/one-more-refactor/openpages/main/install.sh | sh
#
# Downloads the release binary for this architecture, checks it against the
# published checksum, and hands over to `openpages install`, which is where the
# actual questions get asked. Nothing is written outside /tmp until you have
# answered them.

set -eu

REPO="${OPENPAGES_REPO:-one-more-refactor/openpages}"
TAG="${OPENPAGES_VERSION:-latest}"

say()  { printf '%s\n' "$*"; }
die()  { printf '\n%s\n' "error: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(uname -s)" = "Linux" ] || die "openpages runs on Linux. macOS and Windows are not supported."

case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "no build for $(uname -m). Build from source: https://github.com/$REPO" ;;
esac

ASSET="openpages-${ARCH}-linux"

have curl || die "curl is required"
have tar  || true

if ! have podman && ! have docker; then
  say "warning: neither podman nor docker was found."
  say "         openpages builds every site in a container, so install one first."
  say "         Podman is the better fit here — it is rootless by default."
  say ""
fi

if ! have git; then
  say "warning: git was not found. openpages needs it to fetch repositories."
  say ""
fi

if [ "$TAG" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$TAG"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Downloading $ASSET…"
curl -fsSL "$BASE/$ASSET" -o "$TMP/openpages" || die \
  "could not download $BASE/$ASSET

If there is no release yet, build it instead:
  git clone https://github.com/$REPO && cd openpages
  cd web && bun install && bun run build && cd ..
  cargo build --release
  sudo ./target/release/openpages install"

if curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/sum" 2>/dev/null; then
  EXPECTED="$(cut -d' ' -f1 < "$TMP/sum")"
  if have sha256sum; then ACTUAL="$(sha256sum "$TMP/openpages" | cut -d' ' -f1)"
  elif have shasum;    then ACTUAL="$(shasum -a 256 "$TMP/openpages" | cut -d' ' -f1)"
  else ACTUAL="$EXPECTED"; say "warning: no sha256sum available, skipping the checksum"; fi
  [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch — refusing to install"
  say "Checksum ok."
else
  say "warning: this release publishes no checksum."
fi

chmod +x "$TMP/openpages"

# Root is needed to write units into /etc and the binary into /opt. Ask for it
# here rather than halfway through the questions.
if [ "$(id -u)" -ne 0 ]; then
  have sudo || die "run this as root, or install sudo"
  say ""
  exec sudo "$TMP/openpages" install "$@"
fi

say ""
exec "$TMP/openpages" install "$@"
