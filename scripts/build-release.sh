#!/usr/bin/env bash
# Build verified Linux release archives. The release workflow invokes this
# script too, so target names, package names, contents, and checks are shared.
set -euo pipefail

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'USAGE'
Usage: scripts/build-release.sh [--builder cargo|cross] [--target TARGET]

Builds these Linux targets by default:
  x86_64-unknown-linux-gnu
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl

Use --builder cross in CI after installing cross. The default cargo builder
requires each Rust target and its native/cross linker to be installed.
USAGE
}

builder=cargo
requested_target=
while (($#)); do
    case $1 in
        --builder)
            (($# >= 2)) || die '--builder requires cargo or cross'
            builder=$2
            shift 2
            ;;
        --target)
            (($# >= 2)) || die '--target requires a target triple'
            requested_target=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

case $builder in
    cargo|cross) ;;
    *) die "unsupported builder $builder (expected cargo or cross)" ;;
esac

for required_command in cargo tar install sha256sum; do
    command -v "$required_command" >/dev/null 2>&1 || die "missing required command: $required_command"
done

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
[[ -n $version ]] || die 'cannot determine package version from Cargo.toml'

supported_targets=(
    x86_64-unknown-linux-gnu
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
)

if [[ -n $requested_target ]]; then
    found=false
    for target in "${supported_targets[@]}"; do
        if [[ $target == "$requested_target" ]]; then
            found=true
            break
        fi
    done
    "$found" || die "unsupported target: $requested_target"
    targets=("$requested_target")
else
    targets=("${supported_targets[@]}")
fi

target_installed() {
    rustup target list --installed | grep -Fxq "$1"
}

check_linker() {
    local target=$1
    local linker_env=$2
    local default_linker=$3
    local linker=${!linker_env:-$default_linker}

    if [[ -n ${!linker_env:-} ]]; then
        [[ -x $linker || $(command -v "$linker" 2>/dev/null || true) ]] \
            || die "${linker_env} is set to an unavailable linker: $linker"
        return
    fi
    command -v "$default_linker" >/dev/null 2>&1 \
        || die "missing linker for $target: install $default_linker or set $linker_env to a usable cross linker"
}

if [[ $builder == cargo ]]; then
    command -v rustup >/dev/null 2>&1 || die 'missing required command: rustup (or use --builder cross)'
    for target in "${targets[@]}"; do
        target_installed "$target" \
            || die "Rust target $target is not installed; run: rustup target add $target"
    done
    for target in "${targets[@]}"; do
        case $target in
            x86_64-unknown-linux-gnu)
                check_linker "$target" CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER cc
                ;;
            x86_64-unknown-linux-musl)
                check_linker "$target" CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER musl-gcc
                ;;
            aarch64-unknown-linux-musl)
                check_linker "$target" CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER aarch64-linux-musl-gcc
                ;;
        esac
    done
else
    command -v cross >/dev/null 2>&1 \
        || die 'missing required command: cross; install it or run with --builder cargo'
fi

release_dir="$repo_root/dist"
mkdir -p "$release_dir"
archives=()
stage=
cleanup() {
    if [[ -n ${stage:-} ]]; then
        rm -rf -- "$stage"
    fi
}
trap cleanup EXIT

verify_archive() {
    local archive=$1
    local package=$2
    local contents
    contents=$(tar -tzf "$archive") || die "cannot read archive: $archive"
    for required_path in \
        "$package/bin/port-forward" \
        "$package/bin/port-forwardd" \
        "$package/README.md" \
        "$package/examples/config.toml" \
        "$package/systemd/port-forwardd.service" \
        "$package/openrc/port-forwardd"; do
        grep -Fxq "$required_path" <<<"$contents" \
            || die "archive $archive is missing required path: $required_path"
    done
}

for target in "${targets[@]}"; do
    package="port-forward-${version}-${target}"
    archive="$release_dir/${package}.tar.gz"
    [[ ! -e $archive ]] || die "refusing to overwrite existing archive: $archive"

    if [[ $builder == cargo ]]; then
        cargo build --locked --release --target "$target" --bin port-forward --bin port-forwardd
    else
        cross build --locked --release --target "$target" --bin port-forward --bin port-forwardd
    fi

    for binary in port-forward port-forwardd; do
        [[ -x "target/$target/release/$binary" ]] \
            || die "build did not produce executable target/$target/release/$binary"
    done

    stage=$(mktemp -d "${TMPDIR:-/tmp}/port-forward-release.XXXXXX")
    package_dir="$stage/$package"
    mkdir -p "$package_dir/bin" "$package_dir/examples" "$package_dir/systemd" "$package_dir/openrc"
    install -m0755 "target/$target/release/port-forward" "$package_dir/bin/port-forward"
    install -m0755 "target/$target/release/port-forwardd" "$package_dir/bin/port-forwardd"
    install -m0644 README.md "$package_dir/README.md"
    install -m0644 examples/config.toml "$package_dir/examples/config.toml"
    install -m0644 systemd/port-forwardd.service "$package_dir/systemd/port-forwardd.service"
    install -m0755 openrc/port-forwardd "$package_dir/openrc/port-forwardd"
    tar -C "$stage" -czf "$archive" "$package"
    verify_archive "$archive" "$package"
    archives+=("$archive")
    rm -rf -- "$stage"
    stage=
    printf 'created %s\n' "$archive"
done

(
    cd "$release_dir"
    sha256sum "${archives[@]##*/}" >SHA256SUMS
)
printf 'created %s/SHA256SUMS\n' "$release_dir"
