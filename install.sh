#!/usr/bin/env bash
# HY2-MultiPort public release installer. It intentionally downloads only
# versioned GitHub Release assets, validates SHA256SUMS, and never writes a
# configuration file supplied by an operator.
set -euo pipefail

readonly PROGRAM_NAME='HY2-MultiPort installer'
readonly DEFAULT_VERSION='0.0.6'
readonly DEFAULT_BASE_URL='https://github.com/VoidNov/HY2-MultiPort/releases/download'

# The environment overrides are primarily useful for package builders and the
# shell fixture. Normal installations use the documented Linux paths.
readonly BIN_DIR="${PORT_FORWARD_BIN_DIR:-/usr/local/bin}"
readonly SBIN_DIR="${PORT_FORWARD_SBIN_DIR:-/usr/local/sbin}"
readonly ETC_DIR="${PORT_FORWARD_ETC_DIR:-/etc/port-forward}"
readonly STATE_DIR="${PORT_FORWARD_STATE_DIR:-/var/lib/port-forward}"
readonly DOC_DIR="${PORT_FORWARD_DOC_DIR:-/usr/share/doc/port-forward}"
readonly SYSTEMD_DIR="${PORT_FORWARD_SYSTEMD_DIR:-/etc/systemd/system}"
readonly OPENRC_DIR="${PORT_FORWARD_OPENRC_DIR:-/etc/init.d}"
readonly CONFIG_PATH="$ETC_DIR/config.toml"
readonly EXAMPLE_PATH="$ETC_DIR/config.toml.example"
readonly MANIFEST_PATH="$STATE_DIR/install-manifest"
readonly BACKUP_ROOT="$STATE_DIR/backups"

action=install
requested_version=$DEFAULT_VERSION
base_url=$DEFAULT_BASE_URL
dry_run=false
assume_yes=false
enable_service=false
start_service=false
purge_config=false
install_option_used=false

die() {
    printf '%s: ERROR: %s\n' "$PROGRAM_NAME" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "$PROGRAM_NAME" "$*"
}

usage() {
    cat <<'USAGE'
用法：install.sh [install|upgrade|status|uninstall|help] [选项]

默认命令为 install；若已安装则等同于安全升级。只会下载 GitHub 正式 Release
中的版本化压缩包和 SHA256SUMS，绝不下载 main 分支二进制。

选项：
  --version VERSION  固定版本，例如 0.0.6 或 v0.0.6（默认 0.0.6）
  --base-url URL     Release 下载根目录（默认 GitHub releases/download）
  --dry-run          下载、校验和解包检查，但不修改文件、服务或 nftables
  --yes              跳过 uninstall 的交互确认
  --enable           安装后启用服务（仅已有且验证通过的 config.toml）
  --start            安装后启动服务（仅已有且验证通过的 config.toml）
  --purge-config     uninstall 时同时删除 /etc/port-forward/config.toml
  -h, --help         显示帮助

示例：
  curl -fsSL https://raw.githubusercontent.com/VoidNov/HY2-MultiPort/main/install.sh | sudo bash
  sudo bash install.sh --version v0.0.6
  sudo bash install.sh status
  sudo bash install.sh uninstall --yes
USAGE
}

normalize_version() {
    local value=${1#v}
    [[ $value =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
    printf '%s\n' "$value"
}

detect_target() {
    local machine libc
    machine=$(uname -m)
    case $machine in
        x86_64|amd64) machine=x86_64 ;;
        aarch64|arm64) machine=aarch64 ;;
        *) die "不支持的 CPU 架构：$machine（发布包仅支持 x86_64 GNU/musl 和 aarch64 musl）" ;;
    esac

    if ldd --version 2>&1 | grep -qi 'musl'; then
        libc=musl
    else
        libc=gnu
    fi

    case "$machine:$libc" in
        x86_64:gnu) printf '%s\n' 'x86_64-unknown-linux-gnu' ;;
        x86_64:musl) printf '%s\n' 'x86_64-unknown-linux-musl' ;;
        aarch64:musl) printf '%s\n' 'aarch64-unknown-linux-musl' ;;
        aarch64:gnu)
            die '检测到 aarch64 GNU/glibc；当前正式 Release 仅提供 aarch64 musl，请使用 musl 系统或等待对应资产。'
            ;;
    esac
}

require_root() {
    if [[ $(id -u) -ne 0 && $dry_run != true ]]; then
        die '需要 root 权限。请使用 sudo bash install.sh，或仅用 --dry-run 做校验。'
    fi
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "缺少必需命令：$1"
}

download() {
    local url=$1 destination=$2
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error --retry 3 --retry-delay 1 "$url" --output "$destination"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --tries=3 --output-document="$destination" "$url"
    else
        die '缺少 curl 或 wget，无法下载 GitHub Release 资产。'
    fi
}

safe_base_url() {
    [[ $base_url != *'raw.githubusercontent.com'* && $base_url != */main && $base_url != */main/* ]] \
        || die '--base-url 不能指向 raw.githubusercontent.com 或 main 分支；安装器只接受 Release 下载目录。'
}

detect_init() {
    if [[ -d /run/systemd/system ]] && command -v systemctl >/dev/null 2>&1; then
        printf '%s\n' systemd
    elif command -v rc-service >/dev/null 2>&1 && command -v rc-update >/dev/null 2>&1; then
        printf '%s\n' openrc
    else
        printf '%s\n' none
    fi
}

run() {
    if [[ $dry_run == true ]]; then
        printf 'DRY-RUN: '
        printf '%q ' "$@"
        printf '\n'
    else
        "$@"
    fi
}

backup_file() {
    local source=$1 backup_dir=$2
    [[ -e $source || -L $source ]] || return 0
    run install -d -m0700 "$backup_dir"
    run cp -a -- "$source" "$backup_dir/"
}

install_atomically() {
    local source=$1 destination=$2 mode=$3 directory temporary
    directory=$(dirname -- "$destination")
    run install -d -m0755 "$directory"
    if [[ $dry_run == true ]]; then
        note "将原子安装 $source 到 $destination（$mode）"
        return
    fi
    temporary=$(mktemp "$directory/.port-forward.new.XXXXXX")
    install -m"$mode" "$source" "$temporary"
    mv -f -- "$temporary" "$destination"
}

validate_archive() {
    local archive=$1 package=$2 listing required_path
    listing=$(tar -tzf "$archive") || die "无法读取发布压缩包：$archive"
    for required_path in \
        "$package/bin/port-forward" \
        "$package/bin/port-forwardd" \
        "$package/README.md" \
        "$package/examples/config.toml" \
        "$package/systemd/port-forwardd.service" \
        "$package/openrc/port-forwardd"; do
        grep -Fxq "$required_path" <<<"$listing" || die "发布压缩包缺少必需文件：$required_path"
    done
    if grep -Eq '(^|/)\.\.?(/|$)' <<<"$listing"; then
        die '发布压缩包包含不安全路径，已拒绝解包。'
    fi
}

verify_checksum() {
    local sums=$1 archive=$2 asset=$3 checksum
    checksum=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1; exit }' "$sums")
    [[ $checksum =~ ^[[:xdigit:]]{64}$ ]] || die "SHA256SUMS 中未找到 $asset 的有效校验和"
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum -c - >/dev/null \
        || die "SHA256 校验失败：$asset（下载内容未被安装）"
}

write_manifest() {
    local version=$1 target=$2 manager=$3 manifest_tmp
    [[ $dry_run == true ]] && return
    install -d -m0700 "$STATE_DIR"
    manifest_tmp=$(mktemp "$STATE_DIR/.install-manifest.XXXXXX")
    {
        printf 'version=%s\n' "$version"
        printf 'target=%s\n' "$target"
        printf 'init=%s\n' "$manager"
        sha256sum "$BIN_DIR/port-forward" "$SBIN_DIR/port-forwardd" "$DOC_DIR/README.md" "$EXAMPLE_PATH" 2>/dev/null || true
        if [[ $manager == systemd ]]; then
            sha256sum "$SYSTEMD_DIR/port-forwardd.service" 2>/dev/null || true
        elif [[ $manager == openrc ]]; then
            sha256sum "$OPENRC_DIR/port-forwardd" 2>/dev/null || true
        fi
    } >"$manifest_tmp"
    chmod 0600 "$manifest_tmp"
    mv -f -- "$manifest_tmp" "$MANIFEST_PATH"
}

config_is_valid() {
    [[ -f $CONFIG_PATH ]] && "$BIN_DIR/port-forward" validate --config "$CONFIG_PATH"
}

service_action() {
    local manager=$1 operation=$2
    case "$manager:$operation" in
        systemd:enable) systemctl enable port-forwardd.service ;;
        systemd:start) systemctl start port-forwardd.service ;;
        systemd:restart) systemctl restart port-forwardd.service ;;
        systemd:stop) systemctl stop port-forwardd.service || true ;;
        systemd:disable) systemctl disable port-forwardd.service || true ;;
        openrc:enable) rc-update add port-forwardd default ;;
        openrc:start) rc-service port-forwardd start ;;
        openrc:restart) rc-service port-forwardd restart ;;
        openrc:stop) rc-service port-forwardd stop || true ;;
        openrc:disable) rc-update del port-forwardd default || true ;;
        none:*) die '未检测到 systemd 或 OpenRC，无法执行服务管理；可手动运行 port-forwardd。' ;;
        *) die "未知服务操作：$operation" ;;
    esac
}

print_first_use_guide() {
    note '============================================================'
    note '首次使用：5 分钟配置向导（服务尚未启动）'
    note "1. 运行：sudo $BIN_DIR/port-forward configure"
    note "2. 查看规则：sudo $BIN_DIR/port-forward list"
    note "3. 验证：sudo $BIN_DIR/port-forward validate --config $CONFIG_PATH"
    note "4. 诊断：sudo $BIN_DIR/port-forward doctor --config $CONFIG_PATH"
    note "5. 仅在没有 ERROR 后启动：sudo $BIN_DIR/port-forward start"
    note '向导只接受真实本机地址；非交互环境只会生成带 TODO、无法启动的模板。'
    note '若发现外部 nftables base-chain/hook，默认会拒绝启动；确认规则顺序后才手动设置 allow_external_chains = true。'
    note '============================================================'
}

offer_first_use_wizard() {
    [[ -t 0 && -t 1 ]] || return 0
    local reply
    read -r -p '现在进入 port-forward 首次配置向导？[y/N] ' reply
    if [[ $reply == y || $reply == Y ]]; then
        "$BIN_DIR/port-forward" configure --config "$CONFIG_PATH"
    else
        note "稍后运行：sudo $BIN_DIR/port-forward configure"
    fi
}

install_or_upgrade() {
    local version target tag asset package archive sums extracted manager backup_dir had_config=false config_valid=false
    require_root
    safe_base_url
    require_command tar
    require_command sha256sum
    manager=$(detect_init)
    if [[ ($enable_service == true || $start_service == true) && $manager == none ]]; then
        die '--enable/--start 需要 systemd 或 OpenRC；当前系统不支持服务管理。二进制尚未写入。'
    fi
    target=$(detect_target)
    version=$(normalize_version "$requested_version") || die "版本格式无效：$requested_version（应为 0.0.6 或 v0.0.6）"
    tag="v$version"
    asset="port-forward-${version}-${target}.tar.gz"
    package="port-forward-${version}-${target}"
    archive="$workdir/$asset"
    sums="$workdir/SHA256SUMS"
    extracted="$workdir/extracted"

    note "准备 $action：版本 $tag，目标 $target"
    download "$base_url/$tag/SHA256SUMS" "$sums" \
        || die "无法下载 Release $tag 的 SHA256SUMS；请检查网络、版本和 --base-url。"
    download "$base_url/$tag/$asset" "$archive" \
        || die "无法下载 Release $tag 的资产 $asset；该版本或架构可能未发布。"
    verify_checksum "$sums" "$archive" "$asset"
    validate_archive "$archive" "$package"
    mkdir -p "$extracted"
    tar -xzf "$archive" -C "$extracted" --no-same-owner --no-same-permissions

    if [[ $dry_run == true ]]; then
        note 'dry-run 校验成功：不会写入二进制、unit、配置或 nftables。'
        return
    fi

    backup_dir="$BACKUP_ROOT/$(date -u +%Y%m%dT%H%M%SZ)-$version"
    if [[ -e $CONFIG_PATH ]]; then
        had_config=true
    fi
    backup_file "$BIN_DIR/port-forward" "$backup_dir"
    backup_file "$SBIN_DIR/port-forwardd" "$backup_dir"
    backup_file "$CONFIG_PATH" "$backup_dir"
    case $manager in
        systemd) backup_file "$SYSTEMD_DIR/port-forwardd.service" "$backup_dir" ;;
        openrc) backup_file "$OPENRC_DIR/port-forwardd" "$backup_dir" ;;
        none) note '未检测到 systemd/OpenRC：只安装二进制和示例，不会声称服务已安装。' ;;
    esac

    install_atomically "$extracted/$package/bin/port-forward" "$BIN_DIR/port-forward" 0755
    install_atomically "$extracted/$package/bin/port-forwardd" "$SBIN_DIR/port-forwardd" 0755
    install_atomically "$extracted/$package/README.md" "$DOC_DIR/README.md" 0644
    run install -d -m0700 "$ETC_DIR" "$STATE_DIR"
    if [[ $had_config == false ]]; then
        install_atomically "$extracted/$package/examples/config.toml" "$EXAMPLE_PATH" 0600
        note "未发现配置：文档示例已保存为 $EXAMPLE_PATH（不会自动启动服务，也不会复制到正式配置）。"
    else
        note "已保护现有配置：$CONFIG_PATH（备份快照：$backup_dir）"
        if ! config_is_valid; then
            if "$BIN_DIR/port-forward" migrate --config "$CONFIG_PATH"; then
                note '已尝试迁移明确的旧式本机回环目标；正在重新验证配置。'
            fi
        fi
        if config_is_valid; then
            config_valid=true
        else
            note '现有配置未通过验证；二进制已安装，但服务不会启用或启动。请修复后运行 port-forward validate。'
        fi
    fi

    case $manager in
        systemd)
            install_atomically "$extracted/$package/systemd/port-forwardd.service" "$SYSTEMD_DIR/port-forwardd.service" 0644
            systemctl daemon-reload
            ;;
        openrc)
            install_atomically "$extracted/$package/openrc/port-forwardd" "$OPENRC_DIR/port-forwardd" 0755
            ;;
    esac
    write_manifest "$version" "$target" "$manager"

    if [[ $enable_service == true || $start_service == true ]]; then
        if [[ $config_valid != true ]]; then
            die '--enable/--start 只允许用于安装前已存在且验证通过的 config.toml；安装完成但未执行服务操作。'
        fi
        [[ $enable_service == true ]] && service_action "$manager" enable
        [[ $start_service == true ]] && service_action "$manager" start
    fi

    note "安装完成：$BIN_DIR/port-forward version"
    if [[ $had_config == false ]]; then
        print_first_use_guide
        offer_first_use_wizard
    fi
    if [[ $had_config == true && $config_valid == true ]]; then
        note "配置已验证。服务默认未启用；需要时运行：sudo port-forward start"
    fi
}

status_command() {
    local manager
    manager=$(detect_init)
    case $manager in
        systemd) systemctl --no-pager status port-forwardd.service || true ;;
        openrc) rc-service port-forwardd status || true ;;
        none)
            die '未检测到 systemd 或 OpenRC，当前系统不支持 installer status。可直接运行：port-forward status 或 port-forward doctor。'
            ;;
    esac
    if [[ -x $BIN_DIR/port-forward ]]; then
        "$BIN_DIR/port-forward" status || true
    else
        note "未找到 $BIN_DIR/port-forward。"
    fi
}

confirm_uninstall() {
    [[ $assume_yes == true ]] && return
    [[ -t 0 ]] || die 'uninstall 需要交互确认；无人值守时请显式传入 --yes。'
    local reply
    read -r -p '删除 HY2-MultiPort 安装文件（保留配置，除非 --purge-config）？[y/N] ' reply
    [[ $reply == y || $reply == Y ]] || die '已取消。'
}

remove_if_managed() {
    local path=$1 expected
    [[ -e $path || -L $path ]] || return
    expected=$(awk -v path="$path" '$2 == path { print $1; exit }' "$MANIFEST_PATH" 2>/dev/null || true)
    if [[ $expected && $(sha256sum "$path" | awk '{print $1}') == "$expected" ]]; then
        run rm -f -- "$path"
    else
        note "保留未由当前安装记录匹配的文件：$path"
    fi
}

remove_owned_nft_tables() {
    command -v nft >/dev/null 2>&1 || {
        note '未找到 nft；跳过本项目 nftables table 清理。'
        return
    }
    if nft list table ip port_forward_v4 >/dev/null 2>&1; then
        run nft delete table ip port_forward_v4 || note '无法删除本项目 IPv4 nft table；请人工检查。'
    fi
    if nft list table ip6 port_forward_v6 >/dev/null 2>&1; then
        run nft delete table ip6 port_forward_v6 || note '无法删除本项目 IPv6 nft table；请人工检查。'
    fi
}

uninstall_command() {
    local manager
    require_root
    confirm_uninstall
    manager=$(detect_init)
    [[ $manager != none ]] \
        || die '未检测到 systemd 或 OpenRC，当前系统不支持 installer uninstall；请先在支持的 init 系统中停用服务，再人工处理安装路径。'
    case $manager in
        systemd|openrc)
            service_action "$manager" stop
            service_action "$manager" disable
            ;;
        none) note '未检测到 systemd/OpenRC：跳过服务停止/禁用。' ;;
    esac
    remove_owned_nft_tables
    remove_if_managed "$BIN_DIR/port-forward"
    remove_if_managed "$SBIN_DIR/port-forwardd"
    remove_if_managed "$DOC_DIR/README.md"
    remove_if_managed "$EXAMPLE_PATH"
    case $manager in
        systemd)
            remove_if_managed "$SYSTEMD_DIR/port-forwardd.service"
            systemctl daemon-reload
            ;;
        openrc) remove_if_managed "$OPENRC_DIR/port-forwardd" ;;
    esac
    # Backups and runtime state may be valuable when an operator reinstalls or
    # investigates a rollback. Never recursively remove a state directory.
    run rm -f -- "$MANIFEST_PATH" /run/port-forwardd.sock
    [[ ! -d $BACKUP_ROOT ]] || note "已保留安装备份：$BACKUP_ROOT"
    [[ ! -f $STATE_DIR/state.json ]] || note "已保留运行状态：$STATE_DIR/state.json"
    if [[ $purge_config == true ]]; then
        run rm -f -- "$CONFIG_PATH"
        note "已按 --purge-config 删除 $CONFIG_PATH。"
    else
        note "已保留用户配置：$CONFIG_PATH"
    fi
    note '卸载完成。'
}

parse_args() {
    if (($#)) && [[ $1 != -* ]]; then
        action=$1
        shift
    fi
    while (($#)); do
        case $1 in
            --version)
                (($# >= 2)) || die '--version 需要一个版本号'
                requested_version=$2
                install_option_used=true
                shift 2
                ;;
            --base-url)
                (($# >= 2)) || die '--base-url 需要一个 URL'
                base_url=${2%/}
                install_option_used=true
                shift 2
                ;;
            --dry-run) dry_run=true; install_option_used=true; shift ;;
            --yes) assume_yes=true; shift ;;
            --enable) enable_service=true; install_option_used=true; shift ;;
            --start) start_service=true; install_option_used=true; shift ;;
            --purge-config) purge_config=true; shift ;;
            -h|--help) action=help; shift ;;
            *) die "未知参数：$1（运行 --help 查看用法）" ;;
        esac
    done
    case $action in
        install|upgrade|status|uninstall|help) ;;
        *) die "未知命令：$action（支持 install、upgrade、status、uninstall、help）" ;;
    esac
    if [[ $action != install && $action != upgrade && $install_option_used == true ]]; then
        die "命令 $action 不支持指定的安装选项"
    fi
    [[ $action == uninstall || $assume_yes == false ]] || die '--yes 仅能与 uninstall 一起使用'
    [[ $action == uninstall || $purge_config == false ]] || die '--purge-config 仅能与 uninstall 一起使用'
}

main() {
    parse_args "$@"
    case $action in
        install|upgrade)
            workdir=$(mktemp -d "${TMPDIR:-/tmp}/port-forward-install.XXXXXX")
            trap 'rm -rf -- "$workdir"' EXIT
            install_or_upgrade
            ;;
        status) status_command ;;
        uninstall) uninstall_command ;;
        help) usage ;;
    esac
}

if [[ -z ${BASH_SOURCE[0]:-} || ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
