#!/bin/sh
# Copyright (C) 2026 The TTFM Project Contributors
# See the CONTRIBUTORS file at the top-level directory of this distribution
# for a list of copyright holders.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program. If not, see <https://www.gnu.org/licenses/>.

set -eu

REPO_BASE="${TTFM_DOWNLOAD_BASE:-https://github.com/coponhub/TTFM/releases}"
INSTALL_DIR="${TTFM_INSTALL_DIR:-$HOME/.local/bin}"
BASH_COMP="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/ttfm"
FISH_COMP="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/ttfm.fish"
TTFM_HOME_DIR="${TTFM_HOME:-$HOME/.ttfm}"

die() {
    printf 'ttfm_cli: %s\n' "$1" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    if [ "$os/$arch" = Darwin/x86_64 ] \
        && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
        arch=arm64
    fi
    case "$os/$arch" in
        Linux/x86_64) echo x86_64-unknown-linux-gnu ;;
        Darwin/arm64) echo aarch64-apple-darwin ;;
        *) die "unsupported platform: $os/$arch
build from source: cargo install --git https://github.com/coponhub/TTFM" ;;
    esac
}

release_url() {
    if [ -n "${TTFM_VERSION:-}" ]; then
        echo "$REPO_BASE/download/v$TTFM_VERSION/$1"
    else
        echo "$REPO_BASE/latest/download/$1"
    fi
}

verify_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && sha256sum -c "$2" >/dev/null)
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$1" && shasum -a 256 -c "$2" >/dev/null)
    else
        die "sha256sum or shasum is required"
    fi
}

has_bash_completion() {
    for f in /usr/share/bash-completion/bash_completion \
        /etc/bash_completion \
        /opt/homebrew/etc/profile.d/bash_completion.sh \
        /usr/local/etc/profile.d/bash_completion.sh; do
        [ -f "$f" ] && return 0
    done
    return 1
}

do_install() {
    need curl
    need tar
    need uname
    target=$(detect_target)
    asset="ttfm-$target.tar.gz"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "$(release_url "$asset")" -o "$tmp/$asset" \
        || die "no $asset in the requested release"
    curl -fsSL "$(release_url "$asset.sha256")" -o "$tmp/$asset.sha256" \
        || die "no $asset.sha256 in the requested release"
    verify_sha256 "$tmp" "$asset.sha256" || die "checksum mismatch"
    tar xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$INSTALL_DIR"
    install -m 755 "$tmp/ttfm" "$INSTALL_DIR/ttfm"

    mkdir -p "$(dirname "$BASH_COMP")"
    "$INSTALL_DIR/ttfm" completion bash > "$BASH_COMP"
    if command -v fish >/dev/null 2>&1; then
        mkdir -p "$(dirname "$FISH_COMP")"
        "$INSTALL_DIR/ttfm" completion fish > "$FISH_COMP"
    fi

    printf 'ttfm installed to %s\n' "$INSTALL_DIR/ttfm"
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *) printf 'Add to PATH: export PATH="%s:$PATH"\n' "$INSTALL_DIR" ;;
    esac
    if ! has_bash_completion; then
        printf 'bash: add to ~/.bashrc: eval "$(ttfm completion bash)"\n'
    fi
    printf 'zsh: add to ~/.zshrc: source <(ttfm completion zsh)\n'
}

do_uninstall() {
    rm -f "$INSTALL_DIR/ttfm" "$BASH_COMP" "$FISH_COMP"
    if [ "$1" = purge ]; then
        rm -rf "$TTFM_HOME_DIR"
        printf 'removed %s\n' "$TTFM_HOME_DIR"
    else
        printf 'data kept at %s (use --uninstall --purge to remove)\n' \
            "$TTFM_HOME_DIR"
    fi
}

mode=install
purge=keep
for arg in "$@"; do
    case "$arg" in
        --uninstall) mode=uninstall ;;
        --purge) purge=purge ;;
        *) die "unknown option: $arg" ;;
    esac
done
if [ "$purge" = purge ] && [ "$mode" != uninstall ]; then
    die "--purge requires --uninstall"
fi

case "$mode" in
    install) do_install ;;
    uninstall) do_uninstall "$purge" ;;
esac
