#!/usr/bin/env bash
# Build SoundCraft from this checkout, sign it with a stable local identity and install it as
# /Applications/SoundCraft.app and ~/.local/bin/soundcraft-cli.
#
# Usage: packaging/macos/install-local.sh [--skip-build]
#
# Signing: the first run creates a self-signed code-signing identity, "SoundCraft Local Signing",
# in its own keychain (~/Library/Keychains/soundcraft-local-signing.keychain-db, password in
# ~/.config/soundcraft-local/keychain-password, mode 600). Every later build signs with the same
# certificate, so the designated requirement (bundle id + certificate leaf hash) never changes and
# macOS keeps the microphone permission across rebuilds. No login keychain, trust setting or
# password prompt is involved. packaging/macos/entitlements.plist lets both binaries load plugins
# signed by other developers. Local use only: the build is not notarized or timestamped
# (package.sh --local), so signing needs no network.
#
# security(1) takes passwords only on its command line, so the keychain password (and the throwaway
# PKCS#12 one) show in `ps` while those commands run; the former is in the password file anyway.
# Any process of this user can read that file and sign as "SoundCraft Local Signing", inheriting
# the app's microphone permission; disable-library-validation already lets any plugin this user
# installs run inside SoundCraft.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/macos"

NAME="SoundCraft Local Signing"
KC="$HOME/Library/Keychains/soundcraft-local-signing.keychain-db"
SECRETS="$HOME/.config/soundcraft-local"
PWFILE="$SECRETS/keychain-password"
APP_DEST=/Applications/SoundCraft.app
CLI_DEST="$HOME/.local/bin/soundcraft-cli"

SKIP=()
case "${1:-}" in
  "") ;;
  --skip-build) SKIP=(--skip-build) ;;
  -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac
case "$(uname -m)" in
  arm64) ARCH=aarch64 ;;
  x86_64) ARCH=x86_64 ;;
  *) echo "unsupported machine: $(uname -m)" >&2; exit 2 ;;
esac

identity_sha() {
  security find-identity -p codesigning "$KC" 2>/dev/null | awk -v n="\"$NAME\"" 'index($0, n) { print $2; exit }'
}

# The unencrypted private key exists only in $tmp while the identity is made; it goes on any exit.
tmp=""
trap 'rm -rf "${tmp:-}"' EXIT

create_identity() {
  echo "==> creating the local signing identity \"$NAME\" (first run only)"
  local pw p12pw
  tmp="$(mktemp -d)"
  [ -s "$PWFILE" ] || (umask 077 && openssl rand -hex 24 >"$PWFILE")
  pw="$(cat "$PWFILE")"
  [ -f "$KC" ] || security create-keychain -p "$pw" "$KC"
  security set-keychain-settings "$KC" # never locks itself
  security unlock-keychain -p "$pw" "$KC"
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$NAME" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
    -addext "extendedKeyUsage=critical,codeSigning" -addext "keyUsage=critical,digitalSignature" \
    -addext "basicConstraints=critical,CA:false" 2>/dev/null
  p12pw="$(openssl rand -hex 12)"
  # OpenSSL 3+ needs -legacy for a PKCS#12 that `security` reads; LibreSSL has no such flag.
  openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" -passout "pass:$p12pw" 2>/dev/null ||
    openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" -passout "pass:$p12pw"
  # The private key is non-extractable and only codesign may use it.
  security import "$tmp/id.p12" -k "$KC" -P "$p12pw" -T /usr/bin/codesign -x
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pw" "$KC" >/dev/null
  rm -rf "$tmp"
  tmp=""
}

# A keychain without its password file can't be unlocked, and a new password would not open it.
if [ -f "$KC" ] && [ ! -s "$PWFILE" ]; then
  echo "error: $KC exists but its password file $PWFILE is missing or empty." >&2
  echo "Delete the keychain to create a new identity: security delete-keychain \"$KC\"" >&2
  exit 1
fi
mkdir -p "$SECRETS"
chmod 700 "$SECRETS"
if [ -f "$PWFILE" ]; then chmod 600 "$PWFILE"; fi

SHA="$(identity_sha)"
if [ -z "$SHA" ]; then
  create_identity
  SHA="$(identity_sha)"
fi
[ -n "$SHA" ] || { echo "error: no \"$NAME\" identity in $KC" >&2; exit 1; }
security unlock-keychain -p "$(cat "$PWFILE")" "$KC"
echo "==> signing identity: $NAME ($SHA)"

MACOS_SIGN_IDENTITY="$SHA" MACOS_KEYCHAIN="$KC" "$HERE/package.sh" --local --arch "$ARCH" ${SKIP[@]+"${SKIP[@]}"}

# Where package.sh --local leaves its signed results (its usage text).
WORK="$CARGO_TARGET_DIR/macos-package"
APP="$WORK/SoundCraft.app"
CLI="$WORK/soundcraft-cli-$VERSION-macos-$ARCH/soundcraft-cli"

echo "==> installing $APP_DEST and $CLI_DEST"
# Swap the bundle in whole. A SoundCraft that is running keeps its old copy until it is reopened.
rm -rf "$APP_DEST.new"
ditto "$APP" "$APP_DEST.new"
rm -rf "$APP_DEST"
mv "$APP_DEST.new" "$APP_DEST"
mkdir -p "$(dirname "$CLI_DEST")"
install -m 755 "$CLI" "$CLI_DEST"

codesign --verify --strict --deep "$APP_DEST"
codesign --verify --strict "$CLI_DEST"
for f in "$APP_DEST" "$CLI_DEST"; do
  # Read whole first: `grep -q` stops reading early, which pipefail would report as a failure.
  ents="$(codesign -d --entitlements - --xml "$f" 2>/dev/null)" || ents=""
  grep -q disable-library-validation <<<"$ents" ||
    { echo "error: $f lacks disable-library-validation" >&2; exit 1; }
done
if [ "$(command -v soundcraft-cli)" != "$CLI_DEST" ]; then
  echo "warning: soundcraft-cli on PATH is $(command -v soundcraft-cli || echo nothing), not $CLI_DEST" >&2
fi
if pgrep -xq SoundCraft; then
  echo "note: SoundCraft is running; quit and reopen it to use the new build"
fi
"$CLI_DEST" --version
echo "==> installed (build ${SOUNDCRAFT_BUILD_SHA:-unknown})"
