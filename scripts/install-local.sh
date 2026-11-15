#!/usr/bin/env bash
# Build this checkout and install it for the current user.
set -euo pipefail

if [[ $# -ne 0 ]]; then
    echo "Usage: $0" >&2
    echo "Builds a release binary and installs hx and helix in ~/.local/bin." >&2
    exit 1
fi

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
bin_dir="$HOME/.local/bin"

for name in hx helix; do
    if [[ -d "$bin_dir/$name" ]]; then
        echo "Cannot replace directory: $bin_dir/$name" >&2
        exit 1
    fi
done

cd -- "$repo_dir"
# Keep this checkout available: the installed binary uses its runtime files.
export HELIX_DEFAULT_RUNTIME="$repo_dir/runtime"
unset HELIX_DISABLE_AUTO_GRAMMAR_BUILD
cargo build --locked --release --package helix-term --bin hx \
    --target-dir "$repo_dir/target"

# Stage the binary before replacing hx. In particular, do not copy through an
# existing hx symlink, which may point at a package-managed executable.
mkdir -p -- "$bin_dir"
staged_binary="$(mktemp "$bin_dir/.hx-install.XXXXXX")"
trap 'rm -f -- "$staged_binary"' EXIT
install -m 755 -- "$repo_dir/target/release/hx" "$staged_binary"
"$staged_binary" --version
mv -fT -- "$staged_binary" "$bin_dir/hx"
ln -sfnT -- hx "$bin_dir/helix"

echo "Installed $bin_dir/hx and $bin_dir/helix"
echo "Runtime: $HELIX_DEFAULT_RUNTIME"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Add $bin_dir to PATH to use this build." >&2 ;;
esac
for name in hx helix; do
    if [[ "$(command -v "$name" || true)" != "$bin_dir/$name" ]]; then
        echo "Warning: $name resolves elsewhere; check PATH and shell aliases." >&2
    fi
done
