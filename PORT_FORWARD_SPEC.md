# Port Forward 重构规格

## 状态

已确认的实现规格。本文是后续 Rust 实现、测试与部署的唯一设计基线。

## 目标

交付一个原生 nftables 的 Linux 端口转发管理器，支持：

- 本机服务的 `REDIRECT`；
- 显式启用的 IPv4 回环 `DNAT`；
- 远程 IPv4 / IPv6 目标的 `DNAT`；
- 多个独立 profile、TCP、UDP 或二者；
- IPv4、IPv6、域名目标、DNS 自动刷新；
- systemd 与 OpenRC 持久化，以及 `x86_64-unknown-linux-gnu`、musl `x86_64` /
  `aarch64` 发布包。

不兼容、也不迁移现有 Shell 脚本、iptables chain、配置文件或服务。

## 组件与权限边界

| 组件 | 职责 |
| --- | --- |
| `port-forwardd` | root 常驻 daemon：读取配置、解析 DNS、生成 nft 批处理、维护 DNS 状态。 |
| `port-forward` | CLI：`validate`、`apply`、`status`、`status --json`、`logs`；不修改配置。 |
| `nft` | 已安装的原生 nftables CLI；Rust 生成规则文件，以其预检及应用。 |

daemon 以 root 运行，只监听 root 可访问的 Unix socket `/run/port-forwardd.sock`。不提供 HTTP、TCP 或其他网络控制面。

配置、状态和日志路径：

```text
/etc/port-forward/config.toml       # root:root，0600，唯一配置来源
/run/port-forwardd.sock             # root:root，0660 或更严格
/var/lib/port-forward/state.json    # root:root，0600，DNS 缓存和运行状态
```

日志写入 journald 或 syslog；不维护私有日志文件。

## 配置模型

TOML 是唯一人工维护的事实来源。一个 profile 只处理一种地址族，且必须指定一个本机监听 IP；同一 DNS 名称的 IPv4 与 IPv6 转发使用两个 profile。

示意配置：

```toml
schema_version = 1

[[profiles]]
name = "public-https-v4"
family = "ipv4"
listen_address = "203.0.113.10"
protocols = ["tcp", "udp"]

[profiles.listen_ports]
range_start = 20000
range_end = 65536       # 右开区间，实际最大监听端口为 65535
suffix = 443

[profiles.target]
kind = "remote"
host = "origin.example.net"  # IPv4/IPv6 字面量或 FQDN
port = 443
source_mode = "masquerade"  # 远程 IPv4 必填：masquerade 或 preserve

[[profiles]]
name = "dns-local-v6"
family = "ipv6"
listen_address = "2001:db8::10"
protocols = ["udp"]
source_cidrs = ["2001:db8:feed::/48"]

[profiles.listen_ports]
ports = [2053, 3053]

[profiles.target]
kind = "redirect"
port = 53
```

### 规则

- `name` 是稳定唯一标识；重复 profile 名称拒绝整个重载。
- 监听端口支持二选一：`range_start` + `range_end` + `suffix`，或 `ports` 列表。两种格式不可混用。
- 端口范围使用 `[range_start, range_end)`；允许 `range_end = 65536`。
- 一个 profile 选出的所有监听端口映射至同一个 `target.port`；首版不支持保留端口或逐端口目标映射。
- `protocols` 为 `tcp`、`udp` 的非空去重集合。
- `source_cidrs` 可省略，省略时允许该地址族任意来源：IPv4 为 `0.0.0.0/0`，IPv6 为 `::/0`。状态输出必须标明 profile 公开暴露。
- profile 间任一 `(family, protocol, listen_address, listen_port)` 重叠，均拒绝整次重载；不按来源 CIDR 设置优先级。
- `remote`：目标与监听地址族必须一致。IPv4 必须显式设置 `source_mode`；IPv6 禁止 `source_mode` 与 NAT66。
- `redirect`：只接受本机 `target.port`，同时适用于 IPv4、IPv6。
- `loopback-dnat`：仅 IPv4；必须显式启用 `allow_route_localnet = true`，并且 daemon 必须能唯一确定入口接口后才会调整对应接口的 `route_localnet`。IPv6 回环目标一律拒绝，改用 `redirect`。

## DNS 与目标选择

- 仅使用系统本地 resolver；不支持 profile 级 DNS、DoH 或 DoT。
- 目标既可为同族 IP 字面量，也可为 FQDN。FQDN 首次成功解析后选择一个活动地址。
- 当前活动地址仍在 DNS 结果中时继续使用；仅当其消失时，在同族合法地址中按稳定排序选择替代地址。
- 刷新间隔为 DNS TTL 的一半，并钳制到 60 秒至 15 分钟，附加少量随机抖动。
- 初次解析失败、配置语义错误或 nft 预检失败，均拒绝整次重载并保留旧版规则与状态。
- 已运行 profile 刷新失败时保留当前活动地址、状态标为 `degraded` 并写入日志。
- DNS 缓存写入状态文件；daemon 重启时优先重新解析。解析失败时，仅当缓存年龄不超过 1 小时才恢复该地址。
- 允许公网、RFC1918 私网和 IPv6 ULA 目标；拒绝回环、未指定、广播、组播和 IPv6 link-local 地址。

## nftables 规则与应用

工具独占自己的 nftables table；不得解析、清理或复用用户 table。推荐使用独立的 IPv4 和 IPv6 table，以避免不同内核版本对 `inet` NAT 的差异：

```text
table ip  port_forward_v4
table ip6 port_forward_v6
```

每次重载：

1. 读取并验证完整 TOML；解析所有域名并复用符合年龄限制的缓存。
2. 预检 profile 冲突、监听 IP 本机归属、目标地址限制、IPv4 源地址模式和 IPv6 路由可达性（`ip route get`）。IPv6 回程路由由部署者负责。
3. 通过 `nft -j list tables` 仅查询本工具的 IPv4/IPv6 table 是否已存在。
4. 生成包含本工具全部 table 的单个 nft 批处理：首次安装只创建 table；已有自有
   table 的同一次重载才先 `delete table` 后重建。不得使用 `destroy table`，也不得
   无条件使用 `delete table`。
5. 使用 `nft -c -f -` 预检；成功后使用一次 `nft -f -` 原子提交。
6. 仅在提交成功后替换内存状态与持久 DNS 缓存。

base chain 使用数值 priority，分别为 prerouting `-100`、forward `0`、postrouting
`100`，不依赖 `dstnat`、`filter`、`srcnat` 等命名 priority。

兼容性验证范围为 nftables 1.0.6、1.0.9、1.1.3；其中 1.0.6 是当前验证边界，而不
是尚未实测版本的精确最低支持声明。v0.0.2 不依赖 `destroy table`，因此不再把其
nftables 1.0.7 / Linux kernel 6.3 前提传递给部署者。项目仍未验证 nf_tables/NAT 的
精确最低内核版本；发行版 backport、内核配置和防火墙策略必须在目标环境实际预检。

远程 IPv4 profile 生成受精确监听条件约束的 PREROUTING DNAT、FORWARD 放行以及按 `source_mode` 选择的 SNAT/MASQUERADE。远程 IPv6 只生成 DNAT 和转发放行，绝不生成 NAT66。规则仅放行由 profile 建立的新流量及其 `ESTABLISHED,RELATED` 回包。

重载、目标切换和删除只停止新连接；既有 conntrack 连接自然结束，不主动清空 conntrack。

daemon 必须在应用前检测外部 nftables base chain/hook 冲突。无法确认与 firewalld、UFW、Docker、Kubernetes 或人工规则共存安全时，拒绝应用并报告冲突，而不是猜测优先级。

### 升级与回滚

从 v0.0.1 升级到 v0.0.2 时，新 daemon 查询同名自有 table 并以预检后的单一事务
替换；它不修改外部 table。升级前应备份配置、状态及两个自有 table 的 `nft list`
输出。回滚到 v0.0.1 仅适用于满足旧版 `destroy table` 前提的环境；不支持该语法的
环境不能把旧版二进制视为兼容的回滚工具，应保留 v0.0.2 或经人工验证恢复规则。

## 服务行为

- daemon 启动后加载规则并持续执行 DNS 刷新。
- `port-forward apply` 通过 Unix socket 请求一次完整重载；CLI 不写 TOML。
- daemon 崩溃时不卸载已提交 nft 规则；systemd/OpenRC 自动重启 daemon。状态应显示 DNS 刷新过期。
- systemd 与 OpenRC 服务都在启动时执行完整重载；配置失效时不破坏此前已生效的规则。
- 删除服务或显式清理时，只删除本工具的 table、socket、状态和服务文件；不修改其他防火墙规则。

## 状态与日志

`status` 提供可读摘要；`status --json` 至少包括：

- 配置版本与当前 nft 规则版本；
- 每个 profile 的地址族、协议、监听 IP/端口、目标、`source_mode` 与来源限制；
- 域名活动地址、上次和下次刷新时间、缓存年龄；
- `healthy`、`degraded`、`failed` 等状态及失败原因；
- 是否向任意来源公开。

必须记录配置重载、DNS 地址切换/失败、缓存回退、nft 预检/提交失败、daemon 重启和 profile 降级。日志可被 journald/syslog 结构化采集。

## 测试与验收

除 Rust 单元测试（TOML 解析、端口投影、冲突检测、DNS 状态机和规则生成）外，必须在 Linux 特权 CI 中使用 network namespace 和 nftables 集成测试。

集成测试至少覆盖：

- IPv4/IPv6 的 `redirect` 与远程 DNAT；
- IPv4 `masquerade` 与 `preserve` 模式；
- IPv6 不产生 NAT66；
- TCP、UDP、端口尾号范围和端口列表；
- 域名活动地址稳定、刷新切换、失败降级和一小时缓存恢复；
- 全配置预检失败时旧规则不变；
- profile 重叠、非法目标地址与外部 hook 冲突被拒绝；
- 已建立连接在重载/删除后不被主动中断；
- systemd 与 OpenRC 服务生成和重启行为。

实际 nft 集成入口还必须验证：两个自有 table 均不存在时首次启动成功、两个 table
已存在时有效重载成功、真实 `nft -c` 拒绝时旧 table 字节级不变，以及默认外部 hook
拒绝和 `allow_external_chains = true` 的显式共存。

发布物必须包含 `x86_64-unknown-linux-gnu`、musl 静态 `x86_64` 与 `aarch64` 的
`port-forward`、`port-forwardd`，以及 README、示例 TOML、systemd unit、OpenRC
service 和 SHA256SUMS。GitHub Release 仅由匹配 `Cargo.toml` 版本的 `v*` tag 创建；
已存在的 Release 不覆盖或重复发布。
