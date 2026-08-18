# HY2-MultiPort / port-forward

```bash
curl -fsSL https://raw.githubusercontent.com/VoidNov/HY2-MultiPort/main/install.sh | sudo bash
```

当前默认安装版本是 **0.0.7**。这条命令只下载 GitHub Release 中与本机架构匹配的
版本化资产，并在安装前验证 `SHA256SUMS`；它不会下载或执行 main 分支的二进制。

## 中文 5 分钟路径（0.0.7）

1. 在 Linux root 主机执行上面的安装命令。安装器会保留已有
   `/etc/port-forward/config.toml`；首次安装只保存文档示例，不会复制它、不启动服务，
   并显示醒目的 5 分钟向导。交互式终端可选择立即进入向导；`curl | sudo bash` 等
   非交互管道绝不等待输入。
2. 首次使用运行 `sudo port-forward configure`。向导使用目标主机实际探测的地址，
   用业务语言询问协议、端口、目标服务和来源范围；本机 `127.0.0.1:443` 是正式支持的
   本机服务场景。高级用户也可使用 `sudo port-forward add` 增量添加规则。
3. 查看或删除规则：`sudo port-forward list`、`sudo port-forward remove NAME`。
4. 复查并启动：`sudo port-forward validate && sudo port-forward doctor && sudo port-forward start`。
   需要开机启动时，再执行 `sudo systemctl enable port-forwardd`
   （或 OpenRC 的 `sudo rc-update add port-forwardd default`）。
4. `start` / `restart` 只有在 10 秒内同时确认 daemon 为 active 且
   `/run/port-forwardd.sock` 可用时才会报告成功。失败会直接显示 systemd 状态与最近
   100 行 journal（OpenRC 则使用 service 状态和 journald/syslog）。
   `sudo port-forward logs` 在 socket 不存在时也会自动显示这些启动日志。

升级时可固定版本并保留配置：

```bash
curl -fsSL https://raw.githubusercontent.com/VoidNov/HY2-MultiPort/main/install.sh | sudo bash -s -- upgrade --version 0.0.7
```

回滚也是以目标 Release 版本运行同一条命令；安装器保存此前安装文件的备份，且不改动
`config.toml`。卸载默认同样保留配置和安装备份：

```bash
sudo bash install.sh uninstall --yes
```

只有明确传入 `--purge-config` 才会删除 `/etc/port-forward/config.toml`：

```bash
sudo bash install.sh uninstall --yes --purge-config
```

`port-forwardd` 是一个仅面向 Linux 的 root 常驻服务，用原生
`nftables` 管理端口转发。它将配置文件完整校验、目标 DNS 解析、nft 批处理
预检和原子提交放在一次重载中；`port-forward` 是通过 Unix socket 请求状态或
重载的本地 CLI，而不是另一套防火墙实现。

本项目不迁移或管理既有 iptables chain、Shell 转发脚本或其他 nft table。
默认情况下，当检测到外部 nftables base-chain 使用相同的 `prerouting`、`forward` 或
`postrouting` hook 时，daemon 会拒绝应用，而不会猜测规则优先级。

`doctor` 会只读列出本机地址、文档保留地址错误、依赖、控制 socket 和外部 hook。
如果目标主机已有其他软件或人工配置创建的 nftables base chain，doctor 会列出冲突的
family/table/hook；这不是安装器可以安全猜测或自动修复的情况。请先确认 hook priority、
包流和安全边界，再手动编辑配置开启下列开关；程序不会自动改为 `true`，也永不修改外部
nft table/chain。

如果运维人员已经确认规则优先级和数据流安全，可以在配置顶层显式开启：

```toml
allow_external_chains = true
```

该开关只允许本项目自有 table 与外部 chain 共存，不会修改、清理、复用或调整
外部 table/chain；省略该字段等同于 `false`。

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

## nftables 兼容性、升级与回滚

v0.0.7 保留不使用 `destroy table` 的兼容策略。每次重载会先通过 `nft -j list tables` 查询
`port_forward_v4` 与 `port_forward_v6` 是否存在：首次启动只创建 table；已有自有
table 的重载才在同一个 batch 内先执行相应的 `delete table`、再重建。随后仍严格
按 `nft -c -f -` 预检成功、再单次 `nft -f -` 原子提交的顺序执行。因此预检失败
不会删除旧 table。

本版本的 CI 兼容性矩阵使用真实 namespace 集成入口和真实 `nft -c`，目标为
nftables 1.0.6、1.0.9、1.1.3（分别对应 Debian 12、Ubuntu 24.04、Debian 13 的
常见基线）。1.0.6 是当前的**验证边界**，不是对更低版本或任意发行版补丁版本的
最低可用承诺。规则使用稳定的 `table ip`/`table ip6`、NAT/filter base chain 和数值
priority：prerouting `-100`、forward `0`、postrouting `100`，不依赖命名 priority。

v0.0.7 保留了 v0.0.2 消除旧版 `destroy table` 所要求的 nftables 1.0.7 与 Linux kernel 6.3
组合；这不等于项目已验证某个更低的内核下限。内核的 nf_tables/NAT 功能、发行版
backport 和本机防火墙策略仍会影响可用性，目前没有经过验证的精确最低内核版本。
部署前请在目标内核上运行下文的 namespace 集成测试或等价的 `nft -c` 验证。

从 v0.0.1 升级时，先保留现有配置和状态文件、安装新二进制并重启 daemon；新版本
会识别并原子替换同名自有 table，不接触外部规则。回滚前应保存配置和
`nft list table ip port_forward_v4` / `nft list table ip6 port_forward_v6` 输出。若回滚
到 v0.0.1，目标环境仍必须满足其 `destroy table` 前提；在不支持该命令的旧环境中应
保留 v0.0.7，或手工验证并恢复所需的 nft 规则，而不要把旧二进制当作兼容回滚路径。

## 配置模型

唯一人工维护的配置是 TOML，当前必须设置 `schema_version = 1`。可选的
`allow_external_chains` 默认是 `false`。每个
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

可参考 [examples/config.toml](examples/config.toml)，但首次部署应优先使用
`port-forward init`。示例中的地址是文档保留地址，`validate`/`start`/daemon 都会拒绝，
因此不能直接用于生产。

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
sudo install -m0600 examples/config.toml /etc/port-forward/config.toml.example
sudo /usr/local/bin/port-forward init
sudoedit /etc/port-forward/config.toml
sudo /usr/local/bin/port-forward validate --config /etc/port-forward/config.toml
sudo /usr/local/bin/port-forward doctor --config /etc/port-forward/config.toml
```

确认地址、路由、来源网段及外部 hook 顺序后，可以启动 daemon：

```bash
sudo /usr/local/sbin/port-forwardd
sudo /usr/local/bin/port-forward status
sudo /usr/local/bin/port-forward status --json
sudo /usr/local/bin/port-forward logs
sudo /usr/local/bin/port-forward apply
```

`apply` 只请求 daemon 对当前 TOML 执行一次完整、全有或全无的重载；它不写配置。
daemon 不可用时，`status` 会尝试读取 state 文件；其中已逾期的 DNS 刷新会显示为
降级。`logs` 在 daemon 已运行时读取内存事件；socket 缺失时会明确提示 daemon 未运行，
并回退显示 systemd journal 或 OpenRC/syslog 启动日志。

### systemd

安装 [systemd/port-forwardd.service](systemd/port-forwardd.service) 后：

```bash
sudo install -Dm0644 systemd/port-forwardd.service /etc/systemd/system/port-forwardd.service
sudo systemctl daemon-reload
sudo systemctl enable port-forwardd
sudo /usr/local/bin/port-forward start
sudo systemctl status port-forwardd --no-pager
```

unit 直接启动 daemon；daemon 自身在每次启动时会进行完整 reload，没有额外的
`nft` 预加载步骤。`ExecReload` 通过 CLI 请求同一重载路径。

### OpenRC

```bash
sudo install -Dm0755 openrc/port-forwardd /etc/init.d/port-forwardd
sudo rc-update add port-forwardd default
sudo /usr/local/bin/port-forward start
```

OpenRC 脚本使用 `supervise-daemon` respawn daemon；仍由 daemon 自身在启动时
执行完整 reload。

### 发布包

在已安装 Rust target 和对应 C linker 的构建机上运行：

```bash
scripts/build-release.sh
```

默认生成以下经过内容校验的包，以及 `dist/SHA256SUMS`：

- `port-forward-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `port-forward-<version>-x86_64-unknown-linux-musl.tar.gz`
- `port-forward-<version>-aarch64-unknown-linux-musl.tar.gz`

每个包均包含 `port-forward`、`port-forwardd`、README、示例配置、systemd unit 和
OpenRC service。脚本可用 `--target <triple>` 只构建一个目标；默认 `cargo` 构建器在
缺少 target 或 linker 时会明确失败。发布工作流以同一脚本的 `--builder cross` 模式
交叉构建，任何目标未产出可执行二进制或包内容不完整都会使发布失败，而不会上传
伪造包。推送匹配 `v<version>` 的 tag（或手动输入相同 tag）后才会创建 GitHub Release；
已存在的 Release 会原样保留，避免重复发布。

## 验证范围与限制

发布前应执行以下 Rust 质量检查：

```text
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

这些检查覆盖配置语义、端口投影、规则文本生成、DNS 状态和 Unix socket 等单元级
行为；它们不证明内核 nftables 行为、CAP_NET_ADMIN、`route_localnet`、IPv6 路由或
真实数据面已经验证。

必须在具备 root、`nft`、`ip` 及网络命名空间权限的 Linux CI / 测试机执行：

```bash
sudo ./tests/integration_nft.sh
```

该脚本会在独立 network namespace 中进行 nft 预检，覆盖首次 IPv4/IPv6 table
创建、已有自有 table 的成功原子重载、被拒绝的 `nft -c` 重载保留旧规则、默认拒绝
外部 hook 与 `allow_external_chains = true` 的显式共存，并验证 IPv6 表没有 NAT66 /
postrouting。若运行器缺少所需权限或工具，脚本会输出 `SKIP: ...` 并退出 0；这代表
**没有运行集成测试**，而不是集成测试通过。具备前提条件后，任何断言失败都会以非零退出。

尚需在特权 Linux CI 验证的完整矩阵包括真实 IPv4/IPv6 数据包转发、IPv4
`masquerade` 与 `preserve` 回程、`redirect`、`loopback-dnat` 的
`route_localnet` 调整、DNS 轮换/缓存降级、外部 hook 冲突、已建立连接在重载后
的存续，以及 systemd/OpenRC 的实际重启行为。项目不声称这些集成场景已经通过。
