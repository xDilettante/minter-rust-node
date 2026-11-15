#!/usr/bin/env bash
set -euo pipefail
source_dir=$(realpath "$1")
mkdir -p "$2"
output=$(realpath "$2")
desktop_dir=$(realpath "$(dirname "${BASH_SOURCE[0]}")/..")
version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$desktop_dir/src-tauri/tauri.conf.json")
root=$(mktemp -d)
chmod 0755 "$root"
install -d "$root/usr/lib/minter-node" "$root/usr/bin" "$root/usr/share/applications" "$root/usr/share/icons/hicolor/128x128/apps" "$root/DEBIAN"
for binary in minter-desktop minterd minterctl; do
    install -m 0755 "$source_dir/$binary" "$root/usr/lib/minter-node/$binary"
done
ln -s ../lib/minter-node/minter-desktop "$root/usr/bin/minter-node"
install -m 0644 "$desktop_dir/src-tauri/icons/icon.png" "$root/usr/share/icons/hicolor/128x128/apps/network.minter.node.png"
cat > "$root/DEBIAN/control" <<EOF
Package: minter-node
Version: $version
Section: net
Priority: optional
Architecture: amd64
Maintainer: Local Minter Rust build
Depends: libc6 (>= 2.39), libstdc++6, libwebkit2gtk-4.1-0, libayatana-appindicator3-1, libgtk-3-0t64
Description: Minter node desktop application
 Rust Minter full node and verified light client with Tauri desktop management.
EOF
cat > "$root/usr/share/applications/network.minter.node.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Minter Node
Comment=Minter full node and verified light client
Exec=/usr/bin/minter-node
Icon=network.minter.node
Terminal=false
Categories=Network;
StartupWMClass=network.minter.node
EOF
dpkg-deb --build --root-owner-group "$root" "$output/minter-node_${version}_amd64.deb"
