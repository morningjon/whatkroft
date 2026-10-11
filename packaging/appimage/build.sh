#!/usr/bin/env bash
# Build a WhatKroft AppImage containing the window, the daemon, GTK4 and
# libadwaita.
#
#   packaging/appimage/build.sh
#
# Needs: cargo, GTK4 and libadwaita development files, patchelf, librsvg and
# curl. Build on the oldest distribution you want to support: the result runs
# on systems with the same or a newer glibc than the build machine.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
work=${WORK_DIR:-$root/target/appimage}
tools=$work/tools
appdir=$work/AppDir
app_id=io.github.morningjon.WhatKroft
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)
arch=$(uname -m)

mkdir -p "$tools"
fetch() {
    [ -s "$tools/$1" ] || curl -fsSL -o "$tools/$1" "$2"
    chmod +x "$tools/$1"
}
fetch linuxdeploy.AppImage \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$arch.AppImage"
fetch linuxdeploy-plugin-gtk.sh \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"

cargo build --release --manifest-path "$root/Cargo.toml"

rm -rf "$appdir"
install -Dm755 "$root/target/release/whatkroft" "$appdir/usr/bin/whatkroft"
install -Dm755 "$root/target/release/whatkroftd" "$appdir/usr/bin/whatkroftd"
install -Dm644 "$root/data/$app_id.desktop" "$appdir/usr/share/applications/$app_id.desktop"
install -Dm644 "$root/data/icons/$app_id.svg" \
    "$appdir/usr/share/icons/hicolor/scalable/apps/$app_id.svg"
install -Dm644 "$root/LICENSE" "$appdir/usr/share/licenses/whatkroft/LICENSE"

cd "$work"
# The tools are AppImages themselves; extract-and-run avoids needing FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1
export DEPLOY_GTK_VERSION=4
export PATH="$tools:$PATH"
export LINUXDEPLOY_OUTPUT_VERSION=$version
"$tools/linuxdeploy.AppImage" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/whatkroftd" \
    --desktop-file "$appdir/usr/share/applications/$app_id.desktop" \
    --icon-file "$root/data/icons/$app_id.svg" \
    --plugin gtk

# Adjust the launch hook the GTK plugin wrote, then pack.
hook=$appdir/apprun-hooks/linuxdeploy-plugin-gtk.sh
# libadwaita follows the desktop's light/dark preference by itself; forcing a
# GTK theme would freeze it at whatever was set at launch.
sed -i '/^export GTK_THEME=/d' "$hook"
# The plugin pins X11 (through XWayland on Wayland sessions) because bundled
# GTK on native Wayland is not reliable everywhere. Keep that default, but
# let it be overridden: WHATKROFT_GDK_BACKEND=wayland ./WhatKroft-*.AppImage
sed -i 's|^export GDK_BACKEND=x11.*|export GDK_BACKEND="${WHATKROFT_GDK_BACKEND:-x11}"|' "$hook"
grep -q 'WHATKROFT_GDK_BACKEND' "$hook" || { echo "GTK launch hook changed upstream; update build.sh" >&2; exit 1; }

"$tools/linuxdeploy.AppImage" --appdir "$appdir" --output appimage

out=$(ls -t "$work"/WhatKroft-*.AppImage | head -n1)
echo "Built $out"
