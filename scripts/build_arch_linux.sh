#!/bin/bash
set -euo pipefail

if [ -z "${1:-}" ]; then
    echo "Missing version argument"
    echo "Usage: $0 <version>"
    exit 1
fi

version=${1#v}
export PANDORA_RELEASE_VERSION="$version"

# Build dependencies
sudo pacman -S --needed \
    openssl \
    dbus \
    libx11 \
    libxcb \
    libxkbcommon-x11 \
    pkgconf \
    libseccomp \
    fontconfig 

# Build
cargo build --release --frozen

strip target/release/pandora_launcher

mkdir -p dist

mv target/release/pandora_launcher \
    "dist/PandoraLauncher-Linux-x86_64"

# Install cargo-packager if necessary
if ! command -v cargo-packager >/dev/null 2>&1; then
    cargo install cargo-packager
fi

# Package
env -u CARGO_PACKAGER_SIGN_PRIVATE_KEY \
    cargo packager --config "{
        \"name\": \"pandora-launcher\",
        \"outDir\": \"./dist\",
        \"formats\": [\"appimage\"],
        \"productName\": \"Pandora Launcher\",
        \"version\": \"$version\",
        \"identifier\": \"com.moulberry.pandoralauncher\",
        \"resources\": [],
        \"authors\": [\"Moulberry\"],
        \"binaries\": [{
            \"path\": \"PandoraLauncher-Linux-x86_64\",
            \"main\": true
        }],
        \"icons\": [
            \"package/windows_icons/icon_16x16.png\",
            \"package/windows_icons/icon_32x32.png\",
            \"package/windows_icons/icon_48x48.png\",
            \"package/windows_icons/icon_256x256.png\"
        ]
    }"

mv -f \
    dist/PandoraLauncher-Linux-x86_64 \
    dist/PandoraLauncher-Linux-x86_64-Portable

mv -f \
    "dist/PandoraLauncher-Linux-x86_64_${version}_x86_64.AppImage" \
    dist/PandoraLauncher-Linux-x86_64.AppImage

if [[ -n "${CARGO_PACKAGER_SIGN_PRIVATE_KEY:-}" ]]; then
    cargo packager signer sign \
        dist/PandoraLauncher-Linux-x86_64-Portable

    cargo packager signer sign \
        dist/PandoraLauncher-Linux-x86_64.AppImage

    cat > dist/update_manifest_linux.json <<EOF
{
    "version": "$version",
    "downloads": {
        "x86_64": {
            "executable": {
                "download": "https://github.com/Moulberry/PandoraLauncher/releases/download/v$version/PandoraLauncher-Linux-x86_64-Portable",
                "size": $(wc -c < dist/PandoraLauncher-Linux-x86_64-Portable),
                "sha1": "$(sha1sum dist/PandoraLauncher-Linux-x86_64-Portable | cut -d ' ' -f 1)",
                "sig": "$(cat dist/PandoraLauncher-Linux-x86_64-Portable.sig)"
            },
            "appimage": {
                "download": "https://github.com/Moulberry/PandoraLauncher/releases/download/v$version/PandoraLauncher-Linux-x86_64.AppImage",
                "size": $(wc -c < dist/PandoraLauncher-Linux-x86_64.AppImage),
                "sha1": "$(sha1sum dist/PandoraLauncher-Linux-x86_64.AppImage | cut -d ' ' -f 1)",
                "sig": "$(cat dist/PandoraLauncher-Linux-x86_64.AppImage.sig)"
            }
        }
    }
}
EOF

    rm -f dist/*.sig
fi

echo
echo "Build complete:"
ls -lh dist/
