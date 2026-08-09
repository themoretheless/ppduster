#!/bin/bash
set -euo pipefail

# Prevent macOS copy tools from emitting AppleDouble `._*` sidecar files into
# the package payload when the source binary or staging directory has xattrs.
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
PACKAGE_DIR="$(cd "$SCRIPT_DIR/.." && pwd -P)"
MANIFEST="$PACKAGE_DIR/Cargo.toml"
TARGET_DIR="$PACKAGE_DIR/target"
OUTPUT_DIR="$TARGET_DIR/pkg"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: macOS is required to build a .pkg" >&2
  exit 1
fi
if [[ ! -f "$MANIFEST" ]]; then
  echo "error: package manifest not found: $MANIFEST" >&2
  exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required" >&2
  exit 1
fi
for tool in /usr/bin/cpio /usr/bin/gzip /usr/bin/lsbom /usr/bin/mkbom /usr/bin/pkgbuild /usr/sbin/pkgutil; do
  if [[ ! -x "$tool" ]]; then
    echo "error: required packaging tool is unavailable: $tool" >&2
    exit 1
  fi
done

VERSION="$(/usr/bin/awk '
  /^\[package\]$/ { in_package = 1; next }
  in_package && /^\[/ { exit }
  in_package && /^version[[:space:]]*=/ {
    value = $0
    sub(/^[^=]*=[[:space:]]*"/, "", value)
    sub(/"[[:space:]]*$/, "", value)
    print value
    exit
  }
' "$MANIFEST")"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read package version" >&2
  exit 1
fi

ARCH="$(uname -m)"
/bin/mkdir -p "$TARGET_DIR" "$OUTPUT_DIR"
OUTPUT="$OUTPUT_DIR/ppstore-$VERSION-$ARCH.pkg"
if [[ -e "$OUTPUT" ]]; then
  echo "error: output already exists; remove it explicitly before rebuilding: $OUTPUT" >&2
  exit 1
fi

WORK_DIR="$(/usr/bin/mktemp -d "$TARGET_DIR/pkgbuild.XXXXXX")"
case "$WORK_DIR" in
  "$TARGET_DIR"/pkgbuild.*) ;;
  *) echo "error: unsafe working directory: $WORK_DIR" >&2; exit 1 ;;
esac
cleanup() {
  /bin/rm -rf -- "$WORK_DIR"
}
trap cleanup EXIT

CARGO_TARGET_DIR="$TARGET_DIR" cargo build \
  --locked \
  --release \
  --manifest-path "$MANIFEST"

PAYLOAD_ROOT="$WORK_DIR/root"
PAYLOAD_BIN="$PAYLOAD_ROOT/usr/local/bin"
/bin/mkdir -p "$PAYLOAD_BIN"
/bin/cp -X "$TARGET_DIR/release/ppstore" "$PAYLOAD_BIN/ppstore"
/bin/chmod 0755 "$PAYLOAD_BIN/ppstore"
if [[ -x /usr/bin/xattr ]]; then
  /usr/bin/xattr -cr "$PAYLOAD_ROOT"
fi

RAW_PKG="$WORK_DIR/raw.pkg"
EXPANDED_PKG="$WORK_DIR/expanded"
CLEAN_ROOT="$WORK_DIR/clean-root"
CLEAN_PKG="$WORK_DIR/clean.pkg"

/usr/bin/pkgbuild \
  --root "$PAYLOAD_ROOT" \
  --identifier "dev.ppduster.ppstore" \
  --version "$VERSION" \
  --install-location / \
  --ownership recommended \
  "$RAW_PKG"

# Some macOS environments attach an immutable com.apple.provenance xattr to
# every created path. pkgbuild serializes those xattrs as AppleDouble `._*`
# payload entries even with COPYFILE_DISABLE. Repack the generated cpio archive
# data-only, then regenerate its BOM and payload metadata before flattening.
/usr/sbin/pkgutil --expand "$RAW_PKG" "$EXPANDED_PKG"
/bin/mkdir -p "$CLEAN_ROOT"
(
  cd "$CLEAN_ROOT"
  /usr/bin/gzip -dc "$EXPANDED_PKG/Payload" | /usr/bin/cpio -idm
)
/usr/bin/find "$CLEAN_ROOT" -name '._*' -delete
(
  cd "$CLEAN_ROOT"
  /usr/bin/find . -print | /usr/bin/cpio -o -H odc -R root:wheel
) | /usr/bin/gzip -9 >"$EXPANDED_PKG/Payload.clean"
/bin/mv "$EXPANDED_PKG/Payload.clean" "$EXPANDED_PKG/Payload"
/usr/bin/lsbom "$EXPANDED_PKG/Bom" |
  /usr/bin/awk -F '\t' 'BEGIN { OFS = "\t" } $1 !~ /(^|\/)\._/ { $3 = "0/0"; print }' \
    >"$WORK_DIR/Bom.list"
/usr/bin/mkbom -i "$WORK_DIR/Bom.list" "$EXPANDED_PKG/Bom.clean"
/bin/mv "$EXPANDED_PKG/Bom.clean" "$EXPANDED_PKG/Bom"

FILE_COUNT="$(cd "$CLEAN_ROOT" && /usr/bin/find . -print | /usr/bin/wc -l | /usr/bin/tr -d '[:space:]')"
INSTALL_KBYTES="$(
  /usr/bin/find "$CLEAN_ROOT" -type f -exec /usr/bin/stat -f '%z' {} + |
    /usr/bin/awk '{ bytes += $1 } END { print int((bytes + 1023) / 1024) }'
)"
/usr/bin/sed -E \
  "s/<payload numberOfFiles=\"[0-9]+\" installKBytes=\"[0-9]+\"\/>/<payload numberOfFiles=\"$FILE_COUNT\" installKBytes=\"$INSTALL_KBYTES\"\/>/" \
  "$EXPANDED_PKG/PackageInfo" >"$EXPANDED_PKG/PackageInfo.clean"
/bin/mv "$EXPANDED_PKG/PackageInfo.clean" "$EXPANDED_PKG/PackageInfo"

/usr/sbin/pkgutil --flatten "$EXPANDED_PKG" "$CLEAN_PKG"
PAYLOAD_LIST="$(/usr/sbin/pkgutil --payload-files "$CLEAN_PKG")"
if /usr/bin/grep -Eq '(^|/)\._' <<<"$PAYLOAD_LIST"; then
  echo "error: refusing to publish a package containing AppleDouble payload entries" >&2
  exit 1
fi
if ! /usr/bin/grep -qx './usr/local/bin/ppstore' <<<"$PAYLOAD_LIST"; then
  echo "error: package payload does not contain /usr/local/bin/ppstore" >&2
  exit 1
fi
/bin/mv "$CLEAN_PKG" "$OUTPUT"

echo "Created unsigned package: $OUTPUT"
echo "The package was not installed."
