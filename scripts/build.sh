#!/usr/bin/env bash
# Build the portable Sharescreen desktop app for the current OS.
#
# Linux/macOS: compiles the app, stages it in dist/sharescreen-<os>, and
# writes a .tar.gz unless --no-archive is passed. FFmpeg is expected from the
# system (pkg-config); see README.md for the required libraries.
#
# Windows (Git Bash/MSYS/Cygwin): delegates to scripts/build.ps1 so the FFmpeg
# DLLs are bundled the same way as a native PowerShell build.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build.sh [options]

  -p, --profile <debug|release>  Cargo profile (default: release)
  -o, --out-dir <dir>            Output directory (default: dist)
      --no-archive               Do not create dist/sharescreen-<os>.tar.gz
  -h, --help                     Show this help
EOF
}

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

profile="release"
out_dir="dist"
archive=1

while (($#)); do
    case "$1" in
        -p|--profile)
            profile="${2:-}"
            shift 2
            ;;
        -o|--out-dir)
            out_dir="${2:-}"
            shift 2
            ;;
        --no-archive)
            archive=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$profile" in
    debug|release) ;;
    *)
        echo "profile must be 'debug' or 'release', got: $profile" >&2
        exit 2
        ;;
esac

out_root="$repo/$out_dir"
if [[ "$out_dir" = /* ]]; then
    out_root="$out_dir"
fi

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        args=(-NoProfile -ExecutionPolicy Bypass -File "$repo/scripts/build.ps1" \
            -Profile "$profile" -OutDir "$out_root")
        ((archive)) || args+=(-NoArchive)
        exec powershell "${args[@]}"
        ;;
    Darwin)
        platform="macos"
        ;;
    Linux)
        platform="linux"
        ;;
    *)
        platform="$(uname -s | tr '[:upper:]' '[:lower:]')"
        ;;
esac

stage_name="sharescreen-$platform"
if [[ "$profile" != "release" ]]; then
    stage_name="$stage_name-$profile"
fi
stage="$out_root/$stage_name"
target_dir="$repo/target/$profile"
binary="$target_dir/sharescreen"

cd "$repo"

cargo_args=(build -p sharescreen-ui)
if [[ "$profile" == "release" ]]; then
    cargo_args+=(--release)
fi
cargo "${cargo_args[@]}"

if [[ ! -x "$binary" ]]; then
    echo "expected executable not found: $binary" >&2
    exit 1
fi

rm -rf -- "$stage"
mkdir -p -- "$stage"
cp "$binary" "$stage/"

cat > "$stage/README.txt" <<'EOF'
Sharescreen portable build

Run ./sharescreen from this folder. This build links against the system
FFmpeg, Wayland/X11, Vulkan, xkbcommon, and fontconfig libraries. See the
project README for the packages needed on your distribution.
EOF

if ((archive)); then
    tarball="$out_root/$stage_name.tar.gz"
    tar -C "$out_root" -czf "$tarball" "$stage_name"
    echo "Archive: $tarball"
fi

echo "Portable build: $stage"
