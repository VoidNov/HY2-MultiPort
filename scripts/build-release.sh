#!/usr/bin/env bash
# Build static Linux release archives for the two supported musl targets.
set -euo pipefail

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

for required_command in cargo rustup tar install; do
    command -v "$required_command" >/dev/null 2>&1 || die "missing required command: $required_command"
done

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
[[ -n $version ]] || die 'cannot determine package version from Cargo.toml'

target_installed() {
    rustup target list --installed | grep -Fxq "$1"
}

check_linker() {
    local target=$1
    local linker_env=$2
    local default_linker=$3
    local linker=${!linker_env:-$default_linker}

    if [[ -n ${!linker_env:-} ]]; then
        [[ -x $linker || $(command -v "$linker" 2>/dev/null || true) ]] || die "${linker_env} is set to an unavailable linker: $linker"
        return
    fi
    command -v "$default_linker" >/dev/null 2>&1 || die "missing linker for $target: install $default_linker or set $linker_env to a usable cross linker"
}

targets=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
)

for target in "${targets[@]}"; do
    target_installed "$target" || die "Rust target $target is not installed; run: rustup target add $target"
done

check_linker x86_64-unknown-linux-musl CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER musl-gcc
check_linker aarch64-unknown-linux-musl CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER aarch64-linux-musl-gcc

release_dir="$repo_root/dist"
mkdir -p "$release_dir"

for target in "${targets[@]}"; do
    package="port-forward-${version}-${target}"
    archive="$release_dir/${package}.tar.gz"
    [[ ! -e $archive ]] || die "refusing to overwrite existing archive: $archive"

    cargo build --release --target "$target" --bin port-forward --bin port-forwardd

    stage=$(mktemp -d "${TMPDIR:-/tmp}/port-forward-release.XXXXXX")
    trap 'rm -rf -- "$stage"' EXIT
    package_dir="$stage/$package"
    mkdir -p "$package_dir/bin" "$package_dir/examples" "$package_dir/systemd" "$package_dir/openrc"
    install -m0755 "target/$target/release/port-forward" "$package_dir/bin/port-forward"
    install -m0755 "target/$target/release/port-forwardd" "$package_dir/bin/port-forwardd"
    install -m0644 README.md "$package_dir/README.md"
    install -m0644 examples/config.toml "$package_dir/examples/config.toml"
    install -m0644 systemd/port-forwardd.service "$package_dir/systemd/port-forwardd.service"
    install -m0755 openrc/port-forwardd "$package_dir/openrc/port-forwardd"
    tar -C "$stage" -czf "$archive" "$package"
    rm -rf -- "$stage"
    trap - EXIT
    printf 'created %s\n' "$archive"
done
