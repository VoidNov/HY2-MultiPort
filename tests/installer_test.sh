#!/usr/bin/env bash
# Offline fixture coverage for the public installer. No system path is touched.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
installer="$repo_root/install.sh"
fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/port-forward-installer-test.XXXXXX")
trap 'rm -rf -- "$fixture_root"' EXIT
mkdir -p "$fixture_root/test-bin"
printf '#!/usr/bin/env bash\nexit 0\n' >"$fixture_root/test-bin/systemctl"
chmod 0755 "$fixture_root/test-bin/systemctl"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local needle=$1 file=$2
    grep -F -- "$needle" "$file" >/dev/null || fail "expected $file to contain: $needle"
}

assert_not_exists() {
    [[ ! -e $1 && ! -L $1 ]] || fail "unexpected path: $1"
}

run_installer() {
    local output=$1
    shift
    PORT_FORWARD_BIN_DIR="$fixture_root/bin" \
        PORT_FORWARD_SBIN_DIR="$fixture_root/sbin" \
        PORT_FORWARD_ETC_DIR="$fixture_root/etc/port-forward" \
        PORT_FORWARD_STATE_DIR="$fixture_root/state" \
    PORT_FORWARD_DOC_DIR="$fixture_root/doc" \
        PORT_FORWARD_SYSTEMD_DIR="$fixture_root/systemd" \
        PORT_FORWARD_OPENRC_DIR="$fixture_root/openrc" \
        PATH="$fixture_root/test-bin:$PATH" \
        bash "$installer" "$@" >"$output" 2>&1
}

run_stdin_installer() {
    local output=$1
    shift
    cat "$installer" | \
        PORT_FORWARD_BIN_DIR="$fixture_root/bin" \
        PORT_FORWARD_SBIN_DIR="$fixture_root/sbin" \
        PORT_FORWARD_ETC_DIR="$fixture_root/etc/port-forward" \
        PORT_FORWARD_STATE_DIR="$fixture_root/state" \
        PORT_FORWARD_DOC_DIR="$fixture_root/doc" \
        PORT_FORWARD_SYSTEMD_DIR="$fixture_root/systemd" \
        PORT_FORWARD_OPENRC_DIR="$fixture_root/openrc" \
        PATH="$fixture_root/test-bin:$PATH" \
        bash -s -- "$@" >"$output" 2>&1
}

make_release_fixture() {
    local version=0.0.6 target=x86_64-unknown-linux-gnu
    local package="port-forward-${version}-${target}"
    local stage="$fixture_root/stage/$package"
    local release="$fixture_root/releases/v${version}"

    mkdir -p "$stage/bin" "$stage/examples" "$stage/systemd" "$stage/openrc" "$release"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$stage/bin/port-forward"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$stage/bin/port-forwardd"
    chmod 0755 "$stage/bin/port-forward" "$stage/bin/port-forwardd"
    printf 'fixture README\n' >"$stage/README.md"
    printf 'schema_version = 1\nprofiles = []\n' >"$stage/examples/config.toml"
    printf '[Service]\n' >"$stage/systemd/port-forwardd.service"
    printf '#!/sbin/openrc-run\n' >"$stage/openrc/port-forwardd"
    chmod 0755 "$stage/openrc/port-forwardd"
    tar -C "$fixture_root/stage" -czf "$release/${package}.tar.gz" "$package"
    (
        cd "$release"
        sha256sum "${package}.tar.gz" >SHA256SUMS
    )
}

help_output="$fixture_root/help.out"
run_stdin_installer "$help_output" --help
assert_contains '默认命令为 install' "$help_output"
assert_contains '--version VERSION' "$help_output"

invalid_output="$fixture_root/invalid.out"
if run_installer "$invalid_output" --version 0.0; then
    fail 'invalid version unexpectedly succeeded'
fi
assert_contains '版本格式无效' "$invalid_output"

invalid_option_output="$fixture_root/invalid-option.out"
if run_installer "$invalid_option_output" status --version 0.0.6; then
    fail 'status with install option unexpectedly succeeded'
fi
assert_contains '不支持指定的安装选项' "$invalid_option_output"

make_release_fixture
dry_run_output="$fixture_root/dry-run.out"
run_installer "$dry_run_output" --dry-run --version v0.0.6 --base-url "file://$fixture_root/releases"
assert_contains '版本 v0.0.6' "$dry_run_output"
assert_contains 'dry-run 校验成功' "$dry_run_output"
assert_not_exists "$fixture_root/bin/port-forward"
assert_not_exists "$fixture_root/etc/port-forward/config.toml"

config_dir="$fixture_root/etc/port-forward"
mkdir -p "$config_dir"
printf 'operator-owned configuration\n' >"$config_dir/config.toml"
install_output="$fixture_root/install.out"
run_installer "$install_output" --version 0.0.6 --base-url "file://$fixture_root/releases"
[[ $(<"$config_dir/config.toml") == 'operator-owned configuration' ]] \
    || fail 'installer overwrote existing configuration'
assert_contains '已保护现有配置' "$install_output"

fresh_output="$fixture_root/fresh.out"
rm -f -- "$config_dir/config.toml"
run_stdin_installer "$fresh_output" --version 0.0.6 --base-url "file://$fixture_root/releases"
assert_contains '首次使用：5 分钟配置向导' "$fresh_output"
assert_contains 'sudo ' "$fresh_output"
assert_contains 'port-forward configure' "$fresh_output"
assert_not_exists "$fixture_root/etc/port-forward/config.toml"

printf 'ok: installer fixture tests passed\n'
