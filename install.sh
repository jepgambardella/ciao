#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Install Ciao on macOS or Linux.

Usage:
  install.sh                 Install the latest GitHub release.
  install.sh --local          Install the release binary from this checkout.
  install.sh --version v0.1.12
  install.sh --help

Environment:
  CIAO_REPO           GitHub repository. Default: jepgambardella/ciao
  CIAO_VERSION        Release tag. Default: latest
  CIAO_INSTALL_DIR    Install directory. Default: ~/.local/bin
EOF
}

mode=download
version=${CIAO_VERSION:-latest}
repo=${CIAO_REPO:-jepgambardella/ciao}
install_dir=${CIAO_INSTALL_DIR:-"${HOME:?HOME is not set}/.local/bin"}
add_path=1

while [ "$#" -gt 0 ]; do
    case "$1" in
        --local)
            mode=local
            ;;
        --version)
            if [ "$#" -lt 2 ]; then
                echo "Ciao installer: --version needs a value." >&2
                exit 2
            fi
            version=$2
            shift
            ;;
        --dir)
            if [ "$#" -lt 2 ]; then
                echo "Ciao installer: --dir needs a value." >&2
                exit 2
            fi
            install_dir=$2
            shift
            ;;
        --no-path)
            add_path=0
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Ciao installer: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

case "$repo" in
    */*|*)
        case "$repo" in
            *[!A-Za-z0-9._/-]*|/*|*/|*//*|*/*/*)
                echo "Ciao installer: invalid GitHub repository: $repo" >&2
                exit 2
                ;;
        esac
        ;;
esac

os=$(uname -s)
machine=$(uname -m)
case "$os" in
    Darwin) platform=macos ;;
    Linux) platform=linux ;;
    *)
        echo "Ciao installer: supported systems are macOS and Linux." >&2
        exit 1
        ;;
esac

case "$machine" in
    arm64|aarch64) arch=arm64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *)
        echo "Ciao installer: unsupported CPU architecture: $machine" >&2
        exit 1
        ;;
esac

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/ciao-install.XXXXXX")
trap 'rm -rf -- "$tmp_dir"' EXIT HUP INT TERM

binary="$tmp_dir/ciao"
asset="ciao-$platform-$arch"

if [ "$mode" = local ]; then
    script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    local_binary=${CIAO_BINARY:-"$script_dir/target/release/ciao"}
    if [ ! -f "$local_binary" ]; then
        echo "Ciao installer: release binary not found: $local_binary" >&2
        echo "Build it with: cargo build --release --bin ciao" >&2
        exit 1
    fi
    cp "$local_binary" "$binary"
else
    case "$repo" in
        */*) ;;
        *)
            echo "Ciao installer: invalid GitHub repository: $repo" >&2
            exit 2
            ;;
    esac
    if ! command -v curl >/dev/null 2>&1; then
        echo "Ciao installer: curl is required." >&2
        exit 1
    fi

    release_path=download/$version
    if [ "$version" = latest ]; then
        release_path=latest/download
    fi
    release_url="https://github.com/$repo/releases/$release_path/$asset"
    checksum_url="https://github.com/$repo/releases/$release_path/checksums.txt"
    checksums="$tmp_dir/checksums.txt"

    echo "Downloading Ciao ($platform/$arch)..."
    if ! curl -fsSL --retry 3 --retry-delay 1 "$release_url" -o "$binary"; then
        echo "Ciao installer: release not found: $release_url" >&2
        echo "Use --local for a binary built in this checkout." >&2
        exit 1
    fi
    if ! curl -fsSL --retry 3 --retry-delay 1 "$checksum_url" -o "$checksums"; then
        echo "Ciao installer: release checksums not found." >&2
        exit 1
    fi

    expected=$(awk -v asset="$asset" '{ name=$2; sub(/^\*/, "", name); if (name == asset) { print $1; exit } }' "$checksums")
    if [ -z "$expected" ]; then
        echo "Ciao installer: checksum is missing for $asset." >&2
        exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$binary" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$binary" | awk '{print $1}')
    elif command -v openssl >/dev/null 2>&1; then
        actual=$(openssl dgst -sha256 "$binary" | awk -F'= ' '{print $2}')
    else
        echo "Ciao installer: sha256sum, shasum or openssl is required." >&2
        exit 1
    fi
    if [ "$expected" != "$actual" ]; then
        echo "Ciao installer: checksum verification failed." >&2
        exit 1
    fi
fi

mkdir -p "$install_dir"
if command -v install >/dev/null 2>&1; then
    install -m 0755 "$binary" "$install_dir/ciao"
else
    cp "$binary" "$install_dir/ciao"
    chmod 0755 "$install_dir/ciao"
fi

rc_file=
if [ "$add_path" -eq 1 ] && [ "$install_dir" = "${HOME:?HOME is not set}/.local/bin" ]; then
    case "${SHELL:-}" in
        */zsh) rc_file="${HOME:?HOME is not set}/.zprofile" ;;
        */bash) rc_file="${HOME:?HOME is not set}/.bashrc" ;;
        *) rc_file="${HOME:?HOME is not set}/.profile" ;;
    esac
    path_line='export PATH="$HOME/.local/bin:$PATH"'
    if [ ! -f "$rc_file" ] || ! grep -Fqx "$path_line" "$rc_file"; then
        {
            printf '\n# Ciao\n'
            printf '%s\n' "$path_line"
        } >> "$rc_file"
    fi
fi

echo "Ciao installed at $install_dir/ciao"
if [ -n "$rc_file" ]; then
    echo "Open a new terminal, or run: . $rc_file"
elif [ "$add_path" -eq 1 ]; then
    echo "Add $install_dir to PATH to run ciao from any terminal."
fi
