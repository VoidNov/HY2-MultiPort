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

if [[ ! -x $daemon || ! -x $cli ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
        fail 'port-forwardd/port-forward are not built and cargo is unavailable'
    fi
    (
        cd "$repo_root"
        cargo build --bins
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

ip link set lo up || fail 'cannot bring loopback up in network namespace'
ip address add 127.0.0.2/8 dev lo || fail 'cannot add IPv4 listener address'
ip address add 127.0.0.3/8 dev lo || fail 'cannot add IPv4 remote listener address'
ip -6 address add 2001:db8:100::1/64 dev lo || fail 'cannot add IPv6 listener address'
ip -6 route add 2001:db8:200::/64 dev lo || fail 'cannot add IPv6 route for daemon preflight'

# First assert a standalone nft batch can be kernel-preflighted without change.
if ! nft -c -f - <<'NFT_BATCH'
table ip port_forward_integration_preflight {
    chain prerouting {
        type nat hook prerouting priority dstnat; policy accept;
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

"$daemon" --config "$config" --socket "$socket" --state "$state" >"$daemon_log" 2>&1 &
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

# The IPv6 table must only contain DNAT/FORWARD rules: NAT66 is forbidden.
ipv6_rules=$(nft list table ip6 port_forward_v6) || fail 'IPv6 daemon table was not installed'
if grep -Eqi 'masquerade|snat|postrouting' <<<"$ipv6_rules"; then
    printf '%s\n' "$ipv6_rules" >&2
    fail 'IPv6 table contains NAT66/postrouting'
fi
grep -Fq 'dnat to [2001:db8:200::53]:443' <<<"$ipv6_rules" || fail 'IPv6 remote DNAT rule is absent'

# Preserve the installed owned table, request an invalid reload, and prove the
# daemon retained the old table rather than committing a partial replacement.
nft list table ip port_forward_v4 >"$work_dir/old-v4.nft" || fail 'IPv4 daemon table was not installed'
printf 'schema_version = 999\n' >"$config"
if "$cli" apply --socket "$socket"; then
    fail 'invalid reload unexpectedly succeeded'
fi
nft list table ip port_forward_v4 >"$work_dir/new-v4.nft" || fail 'IPv4 table disappeared after rejected reload'
cmp -- "$work_dir/old-v4.nft" "$work_dir/new-v4.nft" || fail 'rejected reload changed old IPv4 rules'

printf 'PASS: nft namespace integration checks completed\n'
