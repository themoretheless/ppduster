#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
app_dir="$project_root/target/macos/ppduster.app"

cargo build --manifest-path "$project_root/Cargo.toml" --release --bin ppduster-ui
mkdir -p "$app_dir/Contents/MacOS"
cp "$project_root/target/release/ppduster-ui" "$app_dir/Contents/MacOS/ppduster-ui"
cp "$project_root/packaging/macos/Info.plist" "$app_dir/Contents/Info.plist"
chmod 755 "$app_dir/Contents/MacOS/ppduster-ui"

echo "$app_dir"
