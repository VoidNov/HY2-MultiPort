#!/usr/bin/env bash
# Run nftables integration checks in a disposable network namespace.
# Missing prerequisites are an explicit SKIP; once prerequisites pass, every
# assertion is fatal and leaves a non-zero status.
set -euo pipefail

skip() {
    printf 'SKIP: %s\n' "$*"
    exit 0
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

if [[ ${1:-} != --inside-namespace ]]; then
    [[ $(id -u) -eq 0 ]] || skip 'requires effective root (run with sudo)'

    for required_command in nft ip unshare; do
        command -v "$required_command" >/dev/null 2>&1 || skip "missing required command: $required_command"
    done

    # This exercises the capability required by the test rather than assuming
    # that UID 0 carries CAP_SYS_ADMIN/CAP_NET_ADMIN in the current runner.
    if ! unshare --net --fork -- nft list ruleset >/dev/null 2>&1; then
        skip 'cannot create a network namespace with usable nftables access'
    fi

    script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    exec unshare --net --fork -- "$script_dir/$(basename -- "$0")" --inside-namespace
fi

[[ $(id -u) -eq 0 ]] || fail 'namespace test unexpectedly lost root'
for required_command in nft ip; do
    command -v "$required_command" >/dev/null 2>&1 || fail "command disappeared: $required_command"
done

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
daemon=${PORT_FORWARDD:-"$repo_root/target/debug/port-forwardd"}
cli=${PORT_FORWARD:-"$repo_root/target/debug/port-forward"}

if ! command -v cargo >/dev/null 2>&1; then
    [[ -x $daemon && -x $cli ]] || fail 'cargo is unavailable and binaries are not built'
else
    (
        cd "$repo_root"
        cargo build --locked --bins
    ) || fail 'failed to build port-forward binaries'
fi

[[ -x $daemon ]] || fail "daemon binary is not executable: $daemon"
[[ -x $cli ]] || fail "CLI binary is not executable: $cli"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/port-forward-nft.XXXXXX")
daemon_pid=
cleanup() {
    if [[ -n ${daemon_pid:-} ]] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$work_dir"
}
trap cleanup EXIT

# The daemon uses this wrapper so the test can force only the `nft -c` phase
# to fail after a successful initial apply. Every normal command still invokes
# the real nft binary, including the initial-install and reload preflights.
nft_precheck_failure_marker="$work_dir/fail-nft-precheck"
nft_wrapper="$work_dir/nft-wrapper"
cat >"$nft_wrapper" <<'NFT_WRAPPER'
#!/bin/sh
if [ "$1" = "-c" ] && [ "$2" = "-f" ] && [ "$3" = "-" ] \
    && [ -n "${NFT_PRECHECK_FAILURE_MARKER:-}" ] \
    && [ -e "$NFT_PRECHECK_FAILURE_MARKER" ]; then
    printf '%s\n' 'forced nft preflight failure' >&2
    exit 1
fi
exec nft "$@"
NFT_WRAPPER
chmod 0755 "$nft_wrapper"
export NFT_PRECHECK_FAILURE_MARKER="$nft_precheck_failure_marker"

ip link set lo up || fail 'cannot bring loopback up in network namespace'
ip address add 127.0.0.2/8 dev lo || fail 'cannot add IPv4 listener address'
ip address add 127.0.0.3/8 dev lo || fail 'cannot add IPv4 remote listener address'
ip -6 address add 2001:db8:100::1/64 dev lo || fail 'cannot add IPv6 listener address'
ip -6 route add 2001:db8:200::/64 dev lo || fail 'cannot add IPv6 route for daemon preflight'

# A first installation must not require an owned table to exist. In particular,
# the daemon must not emit `delete table` or the newer `destroy table` here.
if nft list table ip port_forward_v4 >/dev/null 2>&1; then
    fail 'IPv4 owned table unexpectedly existed before first installation'
fi
if nft list table ip6 port_forward_v6 >/dev/null 2>&1; then
    fail 'IPv6 owned table unexpectedly existed before first installation'
fi

# First assert a standalone nft batch can be kernel-preflighted without change.
if ! nft -c -f - <<'NFT_BATCH'
table ip port_forward_integration_preflight {
    chain prerouting {
        type nat hook prerouting priority -100; policy accept;
        ip daddr 127.0.0.2 tcp dport 2053 redirect to :5353
    }
}
NFT_BATCH
then
    fail 'nft -c rejected the integration preflight batch'
fi

config="$work_dir/config.toml"
socket="$work_dir/port-forwardd.sock"
state="$work_dir/state.json"
daemon_log="$work_dir/daemon.log"
default_socket="$work_dir/default.sock"
default_state="$work_dir/default-state.json"
default_log="$work_dir/default-daemon.log"

cat >"$config" <<'CONFIG'
schema_version = 1

[[profiles]]
name = "redirect-v4"
family = "ipv4"
listen_address = "127.0.0.2"
protocols = ["udp"]
[profiles.listen_ports]
ports = [2053]
[profiles.target]
kind = "redirect"
port = 5353

[[profiles]]
name = "remote-v4"
family = "ipv4"
listen_address = "127.0.0.3"
protocols = ["tcp"]
source_cidrs = ["127.0.0.0/8"]
[profiles.listen_ports]
ports = [10443]
[profiles.target]
kind = "remote"
host = "198.51.100.53"
port = 443
source_mode = "preserve"

[[profiles]]
name = "remote-v6"
family = "ipv6"
listen_address = "2001:db8:100::1"
protocols = ["tcp"]
source_cidrs = ["2001:db8:feed::/48"]
[profiles.listen_ports]
ports = [10443]
[profiles.target]
kind = "remote"
host = "2001:db8:200::53"
port = 443
CONFIG

# An external base chain must be rejected by default.
nft -f - <<'NFT_EXTERNAL'
add table ip external_port_forward_test
add chain ip external_port_forward_test external_prerouting { type nat hook prerouting priority -100; policy accept; }
NFT_EXTERNAL

"$daemon" --nft "$nft_wrapper" --config "$config" --socket "$default_socket" --state "$default_state" >"$default_log" 2>&1 &
default_pid=$!
for _attempt in $(seq 1 50); do
    if ! kill -0 "$default_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if kill -0 "$default_pid" 2>/dev/null; then
    kill "$default_pid" 2>/dev/null || true
    wait "$default_pid" 2>/dev/null || true
    fail 'default configuration unexpectedly allowed external hook coexistence'
fi
grep -Fq 'external nftables base-chain/hook conflict' "$default_log" \
    || fail 'default external hook rejection was not reported'

# Explicit opt-in permits coexistence while still using only owned tables.
sed -i '/^schema_version = 1$/a allow_external_chains = true' "$config"

"$daemon" --nft "$nft_wrapper" --config "$config" --socket "$socket" --state "$state" >"$daemon_log" 2>&1 &
daemon_pid=$!

for _attempt in $(seq 1 100); do
    [[ -S $socket ]] && break
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        sed -n '1,160p' "$daemon_log" >&2 || true
        fail 'daemon exited before binding its control socket'
    fi
    sleep 0.1
done
[[ -S $socket ]] || fail 'daemon did not bind its control socket in time'

# This successful first daemon start runs a real `nft -c` and `nft -f` through
# the integration entry point. It proves absent owned IPv4 and IPv6 tables are
# created without the unsupported `destroy table` command.
nft list table ip port_forward_v4 >/dev/null || fail 'IPv4 table was not created on first install'
nft list table ip6 port_forward_v6 >/dev/null || fail 'IPv6 table was not created on first install'

# The IPv6 table must only contain DNAT/FORWARD rules: NAT66 is forbidden.
ipv6_rules=$(nft list table ip6 port_forward_v6) || fail 'IPv6 daemon table was not installed'
if grep -Eqi 'masquerade|snat|postrouting' <<<"$ipv6_rules"; then
    printf '%s\n' "$ipv6_rules" >&2
    fail 'IPv6 table contains NAT66/postrouting'
fi
grep -Fq 'dnat to [2001:db8:200::53]:443' <<<"$ipv6_rules" || fail 'IPv6 remote DNAT rule is absent'

# Reload a changed valid configuration. This can only succeed if the daemon
# detected existing owned tables and placed `delete table` before recreating
# them in the same nft transaction.
sed -i 's/ports = \[10443\]/ports = [11443]/g' "$config"
"$cli" apply --socket "$socket" || fail 'valid reload unexpectedly failed'
reloaded_v4=$(nft list table ip port_forward_v4) || fail 'IPv4 table disappeared after reload'
reloaded_v6=$(nft list table ip6 port_forward_v6) || fail 'IPv6 table disappeared after reload'
grep -Fq 'tcp dport 11443' <<<"$reloaded_v4" || fail 'IPv4 table did not contain reloaded rules'
grep -Fq 'tcp dport 11443' <<<"$reloaded_v6" || fail 'IPv6 table did not contain reloaded rules'
if grep -Fq 'tcp dport 10443' <<<"$reloaded_v4$reloaded_v6"; then
    fail 'reload retained old listening-port rules'
fi

# Preserve the installed owned table when nft preflight fails. The wrapper
# rejects the daemon's actual `nft -c -f -` invocation but delegates all other
# nft commands to the real binary.
nft list table ip port_forward_v4 >"$work_dir/old-v4.nft" || fail 'IPv4 daemon table was not installed'
touch "$nft_precheck_failure_marker"
if "$cli" apply --socket "$socket"; then
    fail 'nft-preflight-failing reload unexpectedly succeeded'
fi
rm -f -- "$nft_precheck_failure_marker"
nft list table ip port_forward_v4 >"$work_dir/new-v4.nft" || fail 'IPv4 table disappeared after rejected reload'
cmp -- "$work_dir/old-v4.nft" "$work_dir/new-v4.nft" || fail 'nft preflight failure changed old IPv4 rules'

printf 'PASS: nft namespace integration checks completed\n'
