# HY2-MultiPort / port-forward

`port-forwardd` 是一个仅面向 Linux 的 root 常驻服务，用原生
`nftables` 管理端口转发。它将配置文件完整校验、目标 DNS 解析、nft 批处理
预检和原子提交放在一次重载中；`port-forward` 是通过 Unix socket 请求状态或
重载的本地 CLI，而不是另一套防火墙实现。

本项目不迁移或管理既有 iptables chain、Shell 转发脚本或其他 nft table。
当检测到外部 nftables base-chain 使用相同的 `prerouting`、`forward` 或
`postrouting` hook 时，daemon 会拒绝应用，而不会猜测规则优先级。

## 架构

```text
/etc/port-forward/config.toml ──> port-forwardd (root)
                                      │  DNS / 本地接口 / nft -c
port-forward ── Unix socket ──────────┤
                                      └── nft 原子提交（仅自有 table）

/var/lib/port-forward/state.json <── DNS 缓存与运行状态
journald / syslog <────────────────── daemon 事件
```

规则放在独立的 `table ip port_forward_v4` 和
`table ip6 port_forward_v6` 中。一次成功重载会替换这两个自有 table；失败的
配置、DNS、网络预检或 `nft -c` 不会替换之前已提交的规则。daemon 退出或崩溃
时不会主动删除已提交规则。

## 配置模型

唯一人工维护的配置是 TOML，当前必须设置 `schema_version = 1`。每个
`[[profiles]]` 只属于 `ipv4` 或 `ipv6`，并含一个必须确实属于本机接口的
`listen_address`。同一监听地址、协议和端口不能在 profile 间重叠。

监听端口二选一：

- `ports = [2053, 3053]`：显式端口列表。
- `range_start`、`range_end`、`suffix`：右开区间 `[start, end)` 中十进制结尾为
  `suffix` 的端口；`range_end = 65536` 表示可涵盖端口 65535。

`protocols` 是不重复且非空的 `tcp` / `udp` 列表。省略 `source_cidrs` 会允许
所有同族来源（IPv4 为 `0.0.0.0/0`、IPv6 为 `::/0`），即公开暴露该 profile。

目标类型如下：

- `redirect`：转发到本机相同地址族的端口；适用于 IPv4 和 IPv6。
- `loopback-dnat`：仅 IPv4，目标为 `127.0.0.1`；必须显式写
  `allow_route_localnet = true`。daemon 只会在能唯一确定入口接口时调整该接口
  的 `route_localnet`，并保存原值以便以后恢复。
- `remote`：目标为同族 IP 字面量或 FQDN。IPv4 必须写
  `source_mode = "masquerade"` 或 `"preserve"`；IPv6 禁止 `source_mode`，不会
  生成 NAT66。IPv6 的回程路由由部署者负责。

FQDN 仅使用系统 resolver。成功时选择同族活动地址、按 TTL 的一半（限制在
60 秒至 15 分钟并加入少量抖动）刷新；刷新失败保持当前地址并在状态中标为
`degraded`。回环、未指定、组播、广播及 IPv6 link-local 的远程目标会被拒绝。

可直接从 [examples/config.toml](examples/config.toml) 开始。里面的地址是文档
保留地址，必须替换为真实地址，不能直接用于生产。

## 路径与权限

| 路径 | 所有者与模式 | 用途 |
| --- | --- | --- |
| `/etc/port-forward/config.toml` | `root:root`, `0600` | 唯一配置来源 |
| `/run/port-forwardd.sock` | `root:root`, daemon 强制 `0660` | 本地控制 socket；daemon 仍拒绝非 root peer |
| `/var/lib/port-forward/state.json` | `root:root`, `0600` | DNS 缓存和最近运行状态 |

daemon 必须以有效 UID 0 运行，因为它检查本机接口、调整 IPv4
`route_localnet`（如有需要）并调用 nft。CLI 的 `validate` 不需要 root；连接
控制 socket 的 `apply`、在线 `status` 与 `logs` 需要 root。daemon 不提供 HTTP
或 TCP 控制面。

## 安装与运行

需要 Linux、`nft`、`ip`（iproute2）和可用的系统 DNS resolver。开发构建：

```bash
cargo build --release --bins
sudo install -Dm0755 target/release/port-forwardd /usr/local/sbin/port-forwardd
sudo install -Dm0755 target/release/port-forward /usr/local/bin/port-forward
sudo install -d -m0700 /etc/port-forward /var/lib/port-forward
sudo install -m0600 examples/config.toml /etc/port-forward/config.toml
sudoedit /etc/port-forward/config.toml
sudo /usr/local/bin/port-forward validate --config /etc/port-forward/config.toml
```

确认地址、路由和来源网段后，可以临时启动 daemon：

```bash
sudo /usr/local/sbin/port-forwardd
sudo /usr/local/bin/port-forward status
sudo /usr/local/bin/port-forward status --json
sudo /usr/local/bin/port-forward logs
sudo /usr/local/bin/port-forward apply
```

`apply` 只请求 daemon 对当前 TOML 执行一次完整、全有或全无的重载；它不写配置。
daemon 不可用时，`status` 会尝试读取 state 文件；其中已逾期的 DNS 刷新会显示为
降级。

### systemd

安装 [systemd/port-forwardd.service](systemd/port-forwardd.service) 后：

```bash
sudo install -Dm0644 systemd/port-forwardd.service /etc/systemd/system/port-forwardd.service
sudo systemctl daemon-reload
sudo systemctl enable --now port-forwardd
sudo systemctl status port-forwardd
```

unit 直接启动 daemon；daemon 自身在每次启动时会进行完整 reload，没有额外的
`nft` 预加载步骤。`ExecReload` 通过 CLI 请求同一重载路径。

### OpenRC

```bash
sudo install -Dm0755 openrc/port-forwardd /etc/init.d/port-forwardd
sudo rc-update add port-forwardd default
sudo rc-service port-forwardd start
```

OpenRC 脚本使用 `supervise-daemon` respawn daemon；仍由 daemon 自身在启动时
执行完整 reload。

### 静态发布包

在已安装两个 Rust musl target 和对应 C linker 的构建机上运行：

```bash
scripts/build-release.sh
```

该脚本生成 `dist/port-forward-<version>-x86_64-unknown-linux-musl.tar.gz` 与
`dist/port-forward-<version>-aarch64-unknown-linux-musl.tar.gz`。它在缺少 target、
linker 或 Cargo 时会停止并给出安装提示，不会产生伪成功包。

## 验证范围与限制

以下 Rust 质量检查已在**无 root 环境**实际完成：

```text
cargo fmt --check
cargo check --all-targets
cargo test --all-targets       # 19 tests passed
cargo clippy --all-targets -- -D warnings
```

这些检查覆盖配置语义、端口投影、规则文本生成、DNS 状态和 Unix socket 等单元级
行为；它们不证明内核 nftables 行为、CAP_NET_ADMIN、`route_localnet`、IPv6 路由或
真实数据面已经验证。

必须在具备 root、`nft`、`ip` 及网络命名空间权限的 Linux CI / 测试机执行：

```bash
sudo ./tests/integration_nft.sh
```

该脚本会在独立 network namespace 中进行 nft 预检，启动 daemon，并验证 IPv6
表没有 NAT66 / postrouting，以及被拒绝的重载保留旧 nft 规则。若运行器缺少所需
权限或工具，脚本会输出 `SKIP: ...` 并退出 0；这代表**没有运行集成测试**，而不是
集成测试通过。具备前提条件后，任何断言失败都会以非零退出。

尚需在特权 Linux CI 验证的完整矩阵包括真实 IPv4/IPv6 数据包转发、IPv4
`masquerade` 与 `preserve` 回程、`redirect`、`loopback-dnat` 的
`route_localnet` 调整、DNS 轮换/缓存降级、外部 hook 冲突、已建立连接在重载后
的存续，以及 systemd/OpenRC 的实际重启行为。项目不声称这些集成场景已经通过。
