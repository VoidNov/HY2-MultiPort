use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hy2_multiport::{
    config::{AddressFamily, Config, ListenPorts, Profile, Protocol, SourceMode, Target},
    control::{self, Request},
    dns::{Health, unix_now},
    nft::NftCommand,
    state::{self, RuntimeState},
};
use serde::Serialize;

const TODO_CONFIG: &str = r#"# HY2-MultiPort first-use template. It is deliberately NOT startable.
# Replace every TODO value by running `sudo port-forward configure` in a terminal,
# or edit this file with real values and then validate it.
schema_version = 1
allow_external_chains = false

[[profiles]]
name = "TODO-profile-name"
family = "ipv4"
listen_address = "TODO-listen-address"
protocols = ["tcp"]
source_cidrs = ["TODO-source-cidr"]

[profiles.listen_ports]
ports = [443]

[profiles.target]
kind = "redirect"
port = 8443
"#;

#[derive(Debug, Parser)]
#[command(name = "port-forward", about = "HY2-MultiPort control CLI", version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Interactively create the first rule, or add one when a configuration already exists.
    Configure {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
        /// Never open /dev/tty; write an explicit TODO template and exit.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Interactively add one forwarding rule without overwriting existing rules.
    Add {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List configured forwarding rules in business terms.
    List {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Remove exactly one forwarding rule by name.
    Remove {
        name: String,
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Migrate unambiguous legacy loopback remote targets to local-service targets.
    Migrate {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Safely create a starter config without ever overwriting an existing one.
    Init {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        /// Write the intentionally incomplete TODO template instead of prompting.
        #[arg(long)]
        template: bool,
        /// Never read from the terminal; write the explicit TODO template.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Start the installed system service after validating the configuration.
    Start,
    /// Stop the installed system service.
    Stop,
    /// Restart the installed system service after validating the configuration.
    Restart,
    /// Print the installed CLI version.
    Version,
    /// Diagnose configuration, dependencies, init system, control socket, and owned nft tables.
    Doctor {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Parse and semantically validate TOML without changing the system.
    Validate {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Check the configuration, then ask the root daemon to run one all-or-nothing full reload.
    Apply {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Display daemon status; JSON has the stable `RuntimeState` schema.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_STATE_PATH)]
        state: PathBuf,
    },
    /// Retrieve the daemon's in-memory event stream; system logs remain in journald/syslog.
    Logs {
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Configure {
            config,
            socket,
            non_interactive,
        } => {
            if non_interactive {
                init_config(&config, true).await?;
            } else {
                configure(&config, &socket).await?;
            }
        }
        Command::Add { config, socket } => add_profile(&config, &socket).await?,
        Command::List { config } => list_profiles(&config)?,
        Command::Remove {
            name,
            config,
            socket,
        } => remove_profile(&config, &socket, &name).await?,
        Command::Migrate { config } => migrate_config_file(&config)?,
        Command::Init {
            config,
            template,
            non_interactive,
        } => init_config(&config, template || non_interactive).await?,
        Command::Start => service_with_valid_config(ServiceAction::Start)?,
        Command::Stop => run_service_action(ServiceAction::Stop)?,
        Command::Restart => service_with_valid_config(ServiceAction::Restart)?,
        Command::Version => println!("port-forward {}", env!("CARGO_PKG_VERSION")),
        Command::Doctor {
            json,
            config,
            socket,
        } => print_doctor(&config, &socket, json)?,
        Command::Validate { config } => {
            let profiles = Config::from_path(&config)
                .and_then(|parsed| parsed.validate_deployable())
                .with_context(|| {
                    format!(
                        "配置验证失败：{}。请修复后重试：sudo port-forward validate --config {}",
                        config.display(),
                        config.display()
                    )
                })?;
            println!("valid: schema_version=1, profiles={}", profiles.len());
        }
        Command::Apply { config, socket } => {
            Config::from_path(&config)
                .and_then(|parsed| parsed.validate_deployable())
                .with_context(|| {
                    format!(
                        "应用前配置检查失败：{}。请根据上面的 profile/字段错误修复后运行：sudo port-forward validate --config {}",
                        config.display(),
                        config.display()
                    )
                })?;
            let response = control::call(&socket, &Request::Apply)
                .await
                .with_context(|| {
                    format!(
                        "无法请求 daemon 应用配置（socket：{}）。下一步：确认服务已启动，再运行 sudo port-forward doctor；日志：{}",
                        socket.display(),
                        service_log_hint()
                    )
                })?;
            println!("{}", response.message);
        }
        Command::Status {
            json,
            socket,
            state,
        } => {
            let runtime = match control::call(&socket, &Request::Status).await {
                Ok(response) => response.state.with_context(|| {
                    format!(
                        "daemon 未返回运行状态。下一步：运行 sudo port-forward doctor；服务日志：{}",
                        service_log_hint()
                    )
                })?,
                Err(error) => {
                    let cached = state::load(&state).with_context(|| {
                        format!(
                            "无法连接 daemon（{error:#}），且无法读取缓存 {}。下一步：运行 sudo port-forward doctor；日志：{}",
                            state.display(),
                            service_log_hint()
                        )
                    })?;
                    mark_refresh_expired(cached, unix_now())
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&runtime)?);
            } else {
                print!("{}", state::render_human(&runtime, unix_now()));
            }
        }
        Command::Logs { socket } => print_logs(&socket).await?,
    }
    Ok(())
}

/// A private terminal is deliberately used for the business wizard. This
/// keeps `curl | sudo bash` usable: stdin may be a pipe while `/dev/tty` is
/// still the operator's terminal. Without a terminal this fails immediately.
struct TtyPrompt {
    reader: BufReader<fs::File>,
    writer: fs::File,
}

fn open_tty_prompt() -> Result<TtyPrompt> {
    let reader = OpenOptions::new()
        .read(true)
        .open("/dev/tty")
        .context("未检测到可交互的终端（/dev/tty）；向导不会读取管道输入或等待。请在 SSH/本地终端运行：sudo port-forward configure")?;
    let writer = OpenOptions::new().write(true).open("/dev/tty").context(
        "无法写入交互终端 /dev/tty；请在 SSH/本地终端重新运行：sudo port-forward configure",
    )?;
    Ok(TtyPrompt {
        reader: BufReader::new(reader),
        writer,
    })
}

fn tty_line(tty: &mut TtyPrompt, label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(value) => write!(tty.writer, "{label} [{value}]：")?,
        None => write!(tty.writer, "{label}：")?,
    }
    tty.writer.flush()?;
    let mut line = String::new();
    let read = tty.reader.read_line(&mut line)?;
    if read == 0 {
        bail!("交互终端已关闭；未写入任何配置")
    }
    let value = line.trim();
    if value.is_empty() {
        return default
            .map(str::to_owned)
            .context("该字段不能为空；未写入任何配置");
    }
    Ok(value.to_owned())
}

fn tty_note(tty: &mut TtyPrompt, message: impl AsRef<str>) -> Result<()> {
    writeln!(tty.writer, "{}", message.as_ref())?;
    tty.writer.flush()?;
    Ok(())
}

fn affirmative(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是"
    )
}

async fn init_config(config: &Path, force_template: bool) -> Result<()> {
    if config.exists() {
        bail!(
            "拒绝覆盖已有配置 {}。请运行 sudo port-forward add 添加规则，或 sudo port-forward list 查看已有规则。",
            config.display()
        );
    }
    if force_template {
        write_new_config(config, TODO_CONFIG)?;
        println!("已创建首次使用 TODO 配置：{}", config.display());
        println!(
            "[ERROR] 该配置刻意不能启动。请在交互式终端运行：sudo port-forward configure --config {}",
            config.display()
        );
        return Ok(());
    }
    let mut tty = open_tty_prompt()?;
    tty_note(&mut tty, "\n`init` 兼容入口：将使用新的业务配置向导。")?;
    let (profile, allow_external_chains) = profile_wizard(&mut tty, &[], false)?;
    let candidate = Config {
        schema_version: 1,
        allow_external_chains,
        profiles: vec![profile],
    };
    commit_config_update(
        config,
        &candidate,
        Path::new(hy2_multiport::DEFAULT_SOCKET_PATH),
    )
    .await?;
    print_setup_next_steps(config);
    Ok(())
}

fn migrate_config_file(config: &Path) -> Result<()> {
    let source = fs::read_to_string(config)
        .with_context(|| format!("无法读取配置 {}；未执行迁移", config.display()))?;
    let mut parsed: Config = toml::from_str(&source)
        .with_context(|| format!("配置 {} 不是可迁移的 TOML；未执行迁移", config.display()))?;
    if !parsed.migrate_legacy_local_targets() {
        println!("配置无需迁移：未发现明确的旧式本机回环 remote 目标。");
        return Ok(());
    }
    let rendered = toml::to_string_pretty(&parsed).context("无法序列化迁移后的配置")?;
    let previous = fs::read(config)
        .with_context(|| format!("无法备份配置 {}；未执行迁移", config.display()))?;
    let backup = backup_configuration(config, &previous)?;
    atomic_write_config(config, rendered.as_bytes())?;
    println!(
        "已迁移旧式本机回环目标为本机服务语义：{}（备份：{}）",
        config.display(),
        backup.display()
    );
    Ok(())
}

async fn configure(config: &Path, socket: &Path) -> Result<()> {
    if config.exists() {
        println!(
            "已发现配置 {}；configure 不会覆盖它，将以“添加一条规则”方式继续。",
            config.display()
        );
        return add_profile(config, socket).await;
    }
    let mut tty = open_tty_prompt()?;
    tty_note(
        &mut tty,
        "\nHY2-MultiPort 配置向导：先描述业务，再自动生成内部配置。",
    )?;
    let (profile, allow_external_chains) = profile_wizard(&mut tty, &[], false)?;
    let candidate = Config {
        schema_version: 1,
        allow_external_chains,
        profiles: vec![profile],
    };
    commit_config_update(config, &candidate, socket).await?;
    print_setup_next_steps(config);
    Ok(())
}

async fn add_profile(config: &Path, socket: &Path) -> Result<()> {
    let mut candidate = Config::from_path(config)
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "无法读取现有配置 {}；add 只会在可解析的配置上新增规则，不会覆盖文件。请先运行 sudo port-forward validate --config {}",
                config.display(),
                config.display()
            )
        })?;
    let mut tty = open_tty_prompt()?;
    tty_note(
        &mut tty,
        "\n添加规则：现有规则会保留，确认后才写入并尝试应用。",
    )?;
    let (profile, allow_external_chains) = profile_wizard(
        &mut tty,
        &candidate.profiles,
        candidate.allow_external_chains,
    )?;
    candidate.profiles.push(profile);
    candidate.allow_external_chains = allow_external_chains;
    commit_config_update(config, &candidate, socket).await
}

async fn remove_profile(config: &Path, socket: &Path, name: &str) -> Result<()> {
    let mut candidate = Config::from_path(config)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("无法读取配置 {}；未删除任何规则", config.display()))?;
    let before = candidate.profiles.len();
    candidate.profiles.retain(|profile| profile.name != name);
    if candidate.profiles.len() == before {
        bail!("未找到名为 {name:?} 的规则；运行 sudo port-forward list 查看名称。")
    }
    if candidate.profiles.is_empty() {
        bail!(
            "拒绝删除最后一条规则；daemon 不能部署空配置。若要停用服务，请运行 sudo port-forward stop。"
        )
    }
    candidate
        .validate_deployable()
        .map_err(anyhow::Error::from)
        .context("删除后的配置无效；未删除任何规则")?;
    commit_config_update(config, &candidate, socket).await?;
    println!("已删除规则 {name:?}；其余规则保持不变。");
    Ok(())
}

fn list_profiles(config: &Path) -> Result<()> {
    let parsed = Config::from_path(config)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("无法读取配置 {}", config.display()))?;
    if parsed.profiles.is_empty() {
        println!("当前没有规则；运行 sudo port-forward configure 创建第一条规则。");
        return Ok(());
    }
    println!("已配置的端口转发规则（{} 条）：", parsed.profiles.len());
    for profile in &parsed.profiles {
        let ports = profile
            .listen_ports
            .project()
            .map_err(anyhow::Error::from)?;
        let ports = human_ports(&ports);
        let protocols = profile
            .protocols
            .iter()
            .map(|protocol| protocol.nft_name())
            .collect::<Vec<_>>()
            .join("+");
        let target = match &profile.target {
            Target::Redirect { port } => format!("本机服务（redirect）:{port}"),
            Target::LoopbackDnat { port } => format!("本机服务 127.0.0.1:{port}"),
            Target::Remote { host, port, .. } => format!("远程服务器 {host}:{port}"),
        };
        println!(
            "- {}：{} {} → {}；来源 {}",
            profile.name,
            profile.listen_address,
            protocols,
            target,
            source_description(&profile.source_cidrs)
        );
        println!("  监听端口：{ports}");
    }
    Ok(())
}

fn profile_wizard(
    tty: &mut TtyPrompt,
    existing: &[Profile],
    existing_allow_external_chains: bool,
) -> Result<(Profile, bool)> {
    let addresses = local_interface_addresses()?;
    if addresses.is_empty() {
        bail!(
            "未从目标主机的 `ip -brief address` 发现可选地址；请确认 iproute2 可用后重新运行 configure"
        )
    }
    tty_note(tty, "\n当前目标主机实际探测到的接口地址：")?;
    for (index, (interface, address)) in addresses.iter().enumerate() {
        tty_note(tty, format!("  {}) {}  ({interface})", index + 1, address))?;
    }
    let chosen = tty_line(tty, "选择对外监听地址序号，或直接输入上面的地址", None)?;
    let listen_ip = chosen
        .parse::<usize>()
        .ok()
        .and_then(|index| addresses.get(index.saturating_sub(1)))
        .map(|(_, address)| *address)
        .or_else(|| chosen.parse().ok())
        .context("监听地址不是有效 IP；未写入配置")?;
    if !addresses.iter().any(|(_, address)| *address == listen_ip) {
        bail!(
            "监听地址 {listen_ip} 不属于刚才从目标主机探测到的接口；请重新运行 configure 并选择列表中的地址"
        )
    }
    if hy2_multiport::config::is_documentation_address(listen_ip) {
        bail!("监听地址 {listen_ip} 是文档保留地址，不能部署；请选择目标主机真实地址")
    }
    let family = if listen_ip.is_ipv4() {
        AddressFamily::Ipv4
    } else {
        AddressFamily::Ipv6
    };
    let protocols = match tty_line(tty, "协议（tcp / udp / both）", Some("tcp"))?
        .to_ascii_lowercase()
        .as_str()
    {
        "tcp" => vec![Protocol::Tcp],
        "udp" => vec![Protocol::Udp],
        "both" => vec![Protocol::Tcp, Protocol::Udp],
        value => bail!("协议 {value:?} 无效；请输入 tcp、udp 或 both"),
    };
    let listen_ports = wizard_listen_ports(tty)?;
    let target = wizard_target(tty, family, &addresses)?;
    let source_input = tty_line(
        tty,
        "允许哪些来源（输入 all 表示全部，或输入一个同地址族 CIDR）",
        Some("all"),
    )?;
    let source_cidrs = if source_input.eq_ignore_ascii_case("all") {
        vec![family.any_source().to_owned()]
    } else {
        vec![source_input]
    };
    let name = tty_line(tty, "给这条规则起名称", Some("web-forward"))?;
    if name.to_ascii_uppercase().contains("TODO") {
        bail!("规则名称不能包含 TODO")
    }
    let profile = Profile {
        name,
        family,
        listen_address: listen_ip.to_string(),
        protocols,
        source_cidrs,
        allow_route_localnet: matches!(target, Target::LoopbackDnat { .. }),
        listen_ports,
        target,
    };
    let mut preview_profiles = existing.to_vec();
    preview_profiles.push(profile.clone());
    Config {
        schema_version: 1,
        allow_external_chains: existing_allow_external_chains,
        profiles: preview_profiles,
    }
    .validate()
    .map_err(anyhow::Error::from)
    .context("这条规则与已有规则冲突或字段无效；未写入配置")?;

    let allow_external_chains = external_hook_choice(tty, existing_allow_external_chains)?;
    print_profile_summary(tty, &profile, allow_external_chains)?;
    let confirmation = tty_line(tty, "确认新增此规则并写入配置？（y/N）", Some("N"))?;
    if !affirmative(&confirmation) {
        bail!("已取消；未修改配置或 nftables 规则")
    }
    Ok((profile, allow_external_chains))
}

fn wizard_listen_ports(tty: &mut TtyPrompt) -> Result<ListenPorts> {
    let mode = tty_line(
        tty,
        "监听端口方式（single 单端口 / range 显式范围 / suffix 范围内十进制后缀）",
        Some("single"),
    )?
    .to_ascii_lowercase();
    let ports = match mode.as_str() {
        "single" => ListenPorts {
            range_start: None,
            range_end: None,
            suffix: None,
            ports: Some(vec![tty_port(tty, "监听端口", Some("443"))?]),
        },
        "range" => {
            let range = tty_line(tty, "显式范围（包含两端，例如 20000..20010）", None)?;
            let (start, end) = parse_port_range(&range, true)?;
            let ports = (start..=end).collect();
            ListenPorts {
                range_start: None,
                range_end: None,
                suffix: None,
                ports: Some(ports),
            }
        }
        "suffix" => {
            tty_note(
                tty,
                "后缀范围使用右开写法 start..end；例如 20000..65536 + 后缀 443 会生成 20443..65443，共 46 个端口。",
            )?;
            let range = tty_line(tty, "后缀范围（例如 20000..65536）", Some("20000..65536"))?;
            let (start, end) = parse_port_range(&range, false)?;
            let suffix = tty_line(tty, "十进制后缀（例如 443）", Some("443"))?
                .parse::<u32>()
                .context("端口后缀必须是十进制数字")?;
            let ports = ListenPorts {
                range_start: Some(u32::from(start)),
                range_end: Some(u32::from(end)),
                suffix: Some(suffix),
                ports: None,
            };
            let projected = ports.project().map_err(anyhow::Error::from)?;
            tty_note(
                tty,
                format!(
                    "将生成 {} 个端口：{}",
                    projected.len(),
                    human_ports(&projected)
                ),
            )?;
            ports
        }
        _ => bail!("端口方式必须是 single、range 或 suffix"),
    };
    ports
        .project()
        .map_err(anyhow::Error::from)
        .context("监听端口设置无效")?;
    Ok(ports)
}

fn parse_port_range(value: &str, inclusive_end: bool) -> Result<(u16, u16)> {
    let (start, end) = value
        .split_once("..")
        .context("范围格式应为 start..end，例如 20000..65536")?;
    let start = start
        .trim()
        .parse::<u32>()
        .context("范围起点必须是端口数字")?;
    let end = end
        .trim()
        .parse::<u32>()
        .context("范围终点必须是端口数字")?;
    let max_end = if inclusive_end { 65_535 } else { 65_536 };
    if !(1..=65_535).contains(&start) || !(2..=max_end).contains(&end) || start >= end {
        bail!("端口范围无效；起点应小于终点，且范围必须在 1..65535 内（后缀范围终点可为 65536）")
    }
    Ok((
        u16::try_from(start).expect("range start checked"),
        u16::try_from(end).unwrap_or(u16::MAX),
    ))
}

fn tty_port(tty: &mut TtyPrompt, label: &str, default: Option<&str>) -> Result<u16> {
    tty_line(tty, label, default)?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .with_context(|| format!("{label} 必须是 1 到 65535"))
}

fn wizard_target(
    tty: &mut TtyPrompt,
    family: AddressFamily,
    addresses: &[(String, IpAddr)],
) -> Result<Target> {
    let kind = tty_line(
        tty,
        "目标类型（local 本机服务 / remote 远程服务器）",
        Some("local"),
    )?
    .to_ascii_lowercase();
    match kind.as_str() {
        "local" => {
            let target = tty_line(
                tty,
                "本机服务地址（127.0.0.1、localhost 或上面探测到的本机地址）",
                Some("127.0.0.1"),
            )?;
            let port = tty_port(tty, "本机服务端口", Some("443"))?;
            if target.eq_ignore_ascii_case("localhost")
                || target
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
            {
                if family == AddressFamily::Ipv4 {
                    tty_note(
                        tty,
                        "本机回环服务将使用 IPv4 loopback-dnat，并自动启用所需的 route_localnet 标志。",
                    )?;
                    return Ok(Target::LoopbackDnat { port });
                }
                tty_note(
                    tty,
                    "IPv6 本机服务使用 redirect：只改变本机交付端口，不使用回环 DNAT。",
                )?;
                return Ok(Target::Redirect { port });
            }
            let target_ip: IpAddr = target
                .parse()
                .context("本机服务地址必须是 127.0.0.1、localhost 或列表中的本机 IP")?;
            if !family.matches(target_ip)
                || !addresses.iter().any(|(_, address)| *address == target_ip)
            {
                bail!(
                    "本机服务地址 {target_ip} 不在当前目标主机的同地址族接口列表中；远程服务器请改选 remote"
                )
            }
            tty_note(
                tty,
                "该服务绑定在本机实际地址；将使用 redirect 仅改变目标端口。请确认服务能接收该本机地址上的流量。",
            )?;
            Ok(Target::Redirect { port })
        }
        "remote" => {
            let host = tty_line(tty, "远程服务器 IP 或域名", None)?;
            if host.eq_ignore_ascii_case("localhost") {
                bail!("localhost 是本机服务；请选择 local，而不是 remote")
            }
            if let Ok(address) = host.parse::<IpAddr>() {
                if address.is_loopback() {
                    bail!("远程服务器不能使用回环地址 {address}；请选择 local 本机服务")
                }
                if addresses.iter().any(|(_, local)| *local == address) {
                    bail!("远程服务器地址 {address} 属于当前主机；请选择 local 本机服务")
                }
            }
            let port = tty_port(tty, "远程服务端口", Some("443"))?;
            let source_mode = if family == AddressFamily::Ipv4 {
                match tty_line(
                    tty,
                    "远程服务器返回流量方式（masquerade 常用 / preserve 保留客户端地址）",
                    Some("masquerade"),
                )?
                .to_ascii_lowercase()
                .as_str()
                {
                    "masquerade" => Some(SourceMode::Masquerade),
                    "preserve" => Some(SourceMode::Preserve),
                    _ => bail!("返回流量方式必须是 masquerade 或 preserve"),
                }
            } else {
                tty_note(
                    tty,
                    "IPv6 远程转发不使用 NAT66；请确保远程服务器有客户端来源的回程路由。",
                )?;
                None
            };
            Ok(Target::Remote {
                host,
                port,
                source_mode,
            })
        }
        _ => bail!("目标类型必须是 local 或 remote"),
    }
}

fn external_hook_choice(tty: &mut TtyPrompt, current: bool) -> Result<bool> {
    match NftCommand::default().external_hook_conflicts() {
        Ok(conflicts) if conflicts.is_empty() => {
            tty_note(
                tty,
                "外部 nftables base-chain/hook：未检测到冲突（只读检查，未修改任何外部规则）。",
            )?;
            Ok(current)
        }
        Ok(conflicts) => {
            tty_note(
                tty,
                format!(
                    "外部 nftables base-chain/hook：只读检测到 {}。程序不会删除、调整或修改这些外部规则。",
                    conflicts.join(", ")
                ),
            )?;
            let default = if current { "y" } else { "N" };
            let answer = tty_line(
                tty,
                "已确认优先级和数据流安全，设置 allow_external_chains=true 吗？（y/N）",
                Some(default),
            )?;
            if affirmative(&answer) {
                Ok(true)
            } else {
                tty_note(
                    tty,
                    "将保持 allow_external_chains=false；daemon 会拒绝与这些外部 hook 同时应用。",
                )?;
                Ok(false)
            }
        }
        Err(error) => {
            tty_note(
                tty,
                format!(
                    "无法只读检查外部 nftables base-chain/hook：{error:#}；不会自动设置 allow_external_chains=true。"
                ),
            )?;
            Ok(current)
        }
    }
}

fn print_profile_summary(
    tty: &mut TtyPrompt,
    profile: &Profile,
    allow_external_chains: bool,
) -> Result<()> {
    let protocols = profile
        .protocols
        .iter()
        .map(|protocol| protocol.nft_name())
        .collect::<Vec<_>>()
        .join(" + ");
    let target = match &profile.target {
        Target::Redirect { port } => format!("本机服务（redirect）端口 {port}"),
        Target::LoopbackDnat { port } => {
            format!("本机服务 127.0.0.1:{port}（route_localnet 将自动处理）")
        }
        Target::Remote { host, port, .. } => format!("远程服务器 {host}:{port}"),
    };
    tty_note(tty, "\n规则预览：")?;
    tty_note(tty, format!("  名称：{}", profile.name))?;
    tty_note(
        tty,
        format!("  对外监听：{}，协议 {protocols}", profile.listen_address),
    )?;
    tty_note(
        tty,
        format!(
            "  监听端口：{}",
            human_ports(
                &profile
                    .listen_ports
                    .project()
                    .map_err(anyhow::Error::from)?
            )
        ),
    )?;
    tty_note(tty, format!("  转发到：{target}"))?;
    tty_note(
        tty,
        format!("  允许来源：{}", source_description(&profile.source_cidrs)),
    )?;
    tty_note(
        tty,
        format!("  allow_external_chains={allow_external_chains}"),
    )?;
    Ok(())
}

fn human_ports(ports: &[u16]) -> String {
    match ports {
        [] => "无".to_owned(),
        [only] => only.to_string(),
        many if many.len() <= 8 => many
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        many => format!(
            "{}..{}（共 {} 个）",
            many[0],
            many[many.len() - 1],
            many.len()
        ),
    }
}

fn source_description(sources: &[String]) -> String {
    if sources == ["0.0.0.0/0"] || sources == ["::/0"] {
        "全部来源".to_owned()
    } else {
        sources.join(", ")
    }
}

fn write_new_config(config: &Path, content: &str) -> Result<()> {
    if config.exists() {
        bail!("拒绝覆盖已有配置 {}", config.display())
    }
    atomic_write_config(config, content.as_bytes())
}

fn config_backup_path(config: &Path) -> Result<PathBuf> {
    let file_name = config
        .file_name()
        .and_then(|name| name.to_str())
        .context("配置路径必须包含文件名")?;
    let parent = config.parent().context("配置路径必须包含父目录")?;
    Ok(parent.join(format!("{file_name}.backup-{}", unix_now())))
}

fn backup_configuration(config: &Path, contents: &[u8]) -> Result<PathBuf> {
    for attempt in 0..100 {
        let mut backup = config_backup_path(config)?;
        if attempt > 0 {
            backup.set_extension(format!("backup-{}-{attempt}", unix_now()));
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut output) => {
                output.write_all(contents)?;
                output.sync_all()?;
                set_private_file_permissions(&backup)?;
                return Ok(backup);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("无法备份配置到 {}", backup.display()));
            }
        }
    }
    bail!("无法为配置创建唯一备份文件")
}

fn atomic_write_config(config: &Path, contents: &[u8]) -> Result<()> {
    let parent = config.parent().context("配置路径必须包含父目录")?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录 {}", parent.display()))?;
        set_private_directory_permissions(parent)?;
    }
    let file_name = config
        .file_name()
        .and_then(|name| name.to_str())
        .context("配置路径必须包含文件名")?;
    for attempt in 0..100 {
        let temporary = parent.join(format!(".{file_name}.new-{}-{attempt}", std::process::id()));
        let mut output = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法创建临时配置 {}", temporary.display()));
            }
        };
        let result = (|| -> Result<()> {
            output.write_all(contents)?;
            output.sync_all()?;
            set_private_file_permissions(&temporary)?;
            fs::rename(&temporary, config)
                .with_context(|| format!("无法原子替换配置 {}", config.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }
    bail!("无法创建唯一临时配置文件")
}

async fn commit_config_update(config: &Path, candidate: &Config, socket: &Path) -> Result<()> {
    let rendered = toml::to_string_pretty(candidate).context("无法序列化候选配置")?;
    Config::from_toml(&rendered)
        .and_then(|parsed| parsed.validate_deployable().map(|_| ()))
        .map_err(anyhow::Error::from)
        .context("候选配置未通过语义/部署校验；未修改现有配置")?;
    let previous = match fs::read(config) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("无法读取现有配置 {}", config.display()));
        }
    };
    let backup = previous
        .as_deref()
        .map(|contents| backup_configuration(config, contents))
        .transpose()?;
    atomic_write_config(config, rendered.as_bytes())?;
    if !socket_is_usable(socket) {
        let start_result = service_manager()
            .context("配置已写入，但未检测到可用的 systemd/OpenRC；无法自动启动 daemon")
            .and_then(|_| {
                run_service_action(ServiceAction::Start)
                    .context("配置已写入，但 daemon 启动失败；旧 nftables 规则未修改")
            });
        if let Err(error) = start_result {
            let restore = match &previous {
                Some(contents) => atomic_write_config(config, contents),
                None => fs::remove_file(config)
                    .with_context(|| format!("无法移除未能启动的新配置 {}", config.display())),
            };
            restore.context("daemon 启动失败后无法恢复原配置；请使用自动备份手动恢复")?;
            bail!("自动启动 daemon 失败，已恢复原配置：{error:#}");
        }
    }
    if socket_is_usable(socket) {
        if let Err(error) = control::call(socket, &Request::Apply).await {
            let restore = match &previous {
                Some(contents) => atomic_write_config(config, contents),
                None => fs::remove_file(config)
                    .with_context(|| format!("无法移除未能应用的新配置 {}", config.display())),
            };
            restore.context("daemon 拒绝应用后无法恢复原配置；请使用自动备份手动恢复")?;
            bail!(
                "daemon 应用失败，已恢复原配置{}；旧 nftables 规则由 daemon 的原子重载保持不变：{error:#}",
                backup
                    .as_ref()
                    .map(|path| format!("（备份：{}）", path.display()))
                    .unwrap_or_default()
            )
        }
        println!(
            "配置已验证、daemon 已启动并成功应用{}。",
            backup
                .as_ref()
                .map(|path| format!("（备份：{}）", path.display()))
                .unwrap_or_default()
        );
    } else {
        bail!("daemon 启动后控制 socket 仍不可用：{}", socket.display());
    }
    Ok(())
}

fn print_setup_next_steps(config: &Path) {
    println!(
        "验证：sudo port-forward validate --config {}",
        config.display()
    );
    println!(
        "诊断：sudo port-forward doctor --config {}",
        config.display()
    );
    println!(
        "应用：sudo port-forward apply --config {}；启动：sudo port-forward start",
        config.display()
    );
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("无法设置配置目录权限 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("无法设置配置文件权限 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceManager {
    Systemd,
    OpenRc,
}

#[derive(Clone, Copy, Debug)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
}

fn select_service_manager(systemd_runtime: bool, openrc_available: bool) -> Option<ServiceManager> {
    if systemd_runtime && command_exists("systemctl") {
        Some(ServiceManager::Systemd)
    } else if openrc_available && command_exists("rc-service") {
        Some(ServiceManager::OpenRc)
    } else {
        None
    }
}

fn service_manager() -> Result<ServiceManager> {
    select_service_manager(
        Path::new("/run/systemd/system").is_dir(),
        command_exists("rc-service"),
    )
    .context("未检测到可用的 systemd 或 OpenRC；无法管理服务。可直接运行 sudo port-forwardd，或安装支持的 init 系统。")
}

fn service_with_valid_config(action: ServiceAction) -> Result<()> {
    let config = Path::new(hy2_multiport::DEFAULT_CONFIG_PATH);
    if !config.is_file() {
        bail!(
            "未执行服务操作：缺少配置 {}。运行：sudo port-forward init\n日志查看：{}",
            config.display(),
            service_log_hint()
        );
    }
    if let Err(error) =
        Config::from_path(config).and_then(|parsed| parsed.validate_deployable().map(|_| ()))
    {
        bail!(
            "未执行服务操作：配置 {} 无效：{error:#}\n修复后运行：sudo port-forward validate\n日志查看：{}",
            config.display(),
            service_log_hint()
        );
    }
    run_service_action(action)
}

fn service_log_hint() -> &'static str {
    match select_service_manager(
        Path::new("/run/systemd/system").is_dir(),
        command_exists("rc-service"),
    ) {
        Some(ServiceManager::Systemd) => "sudo journalctl -u port-forwardd -n 100 --no-pager",
        Some(ServiceManager::OpenRc) => "sudo rc-service port-forwardd status；并查看系统 syslog",
        None => "未检测到 systemd/OpenRC；查看直接启动命令的终端输出",
    }
}

fn run_service_action(action: ServiceAction) -> Result<()> {
    let manager = service_manager()?;
    let (program, argument) = match (manager, action) {
        (ServiceManager::Systemd, ServiceAction::Start) => ("systemctl", "start"),
        (ServiceManager::Systemd, ServiceAction::Stop) => ("systemctl", "stop"),
        (ServiceManager::Systemd, ServiceAction::Restart) => ("systemctl", "restart"),
        (ServiceManager::OpenRc, ServiceAction::Start) => ("rc-service", "start"),
        (ServiceManager::OpenRc, ServiceAction::Stop) => ("rc-service", "stop"),
        (ServiceManager::OpenRc, ServiceAction::Restart) => ("rc-service", "restart"),
    };
    let output = ProcessCommand::new(program)
        .args(match manager {
            ServiceManager::Systemd => vec![argument, "port-forwardd.service"],
            ServiceManager::OpenRc => vec!["port-forwardd", argument],
        })
        .output()
        .with_context(|| format!("无法执行 {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let detail = if detail.is_empty() {
            "命令未输出详情".to_owned()
        } else {
            detail
        };
        bail!(
            "服务操作失败：{program} {argument} port-forwardd：{detail}\n{}",
            service_failure_diagnostics(manager)
        );
    }
    if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
        wait_for_daemon_ready(manager, Path::new(hy2_multiport::DEFAULT_SOCKET_PATH))?;
        println!("服务已真正就绪：daemon active 且控制 socket 可用。");
    } else {
        println!("服务操作成功：{program} {argument} port-forwardd");
    }
    Ok(())
}

fn wait_for_daemon_ready(manager: ServiceManager, socket: &Path) -> Result<()> {
    wait_for_daemon_ready_with_timeout(manager, socket, Duration::from_secs(10))
}

fn wait_for_daemon_ready_with_timeout(
    manager: ServiceManager,
    socket: &Path,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if service_is_active(manager) && socket_is_usable(socket) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "服务启动未就绪：10 秒内未同时满足 daemon active 与控制 socket 存在（{}）。systemctl/rc-service 返回成功不代表 daemon 已启动。\n{}",
        socket.display(),
        service_failure_diagnostics(manager)
    )
}

fn service_is_active(manager: ServiceManager) -> bool {
    let result = match manager {
        ServiceManager::Systemd => ProcessCommand::new("systemctl")
            .args(["is-active", "--quiet", "port-forwardd.service"])
            .status(),
        ServiceManager::OpenRc => ProcessCommand::new("rc-service")
            .args(["port-forwardd", "status"])
            .status(),
    };
    result.is_ok_and(|status| status.success())
}

fn socket_is_usable(socket: &Path) -> bool {
    fs::metadata(socket).is_ok_and(|metadata| is_socket(&metadata))
}

fn service_failure_diagnostics(manager: ServiceManager) -> String {
    match manager {
        ServiceManager::Systemd => format!(
            "服务状态与最近启动日志（请保留其中的 nft/config 错误）：\n{}\n{}",
            command_output(
                "systemctl",
                &["status", "port-forwardd.service", "--no-pager", "-l"]
            ),
            command_output(
                "journalctl",
                &["-u", "port-forwardd", "-n", "100", "--no-pager"]
            )
        ),
        ServiceManager::OpenRc => format!(
            "服务状态与启动日志：\n{}\n{}",
            command_output("rc-service", &["port-forwardd", "status"]),
            openrc_log_output()
        ),
    }
}

fn command_output(program: &str, arguments: &[&str]) -> String {
    let display = format!("$ {program} {}", arguments.join(" "));
    match ProcessCommand::new(program).args(arguments).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stdout.is_empty() { stderr } else { stdout };
            if detail.is_empty() {
                format!("{display}\n（命令未输出详情）")
            } else {
                format!("{display}\n{detail}")
            }
        }
        Err(error) => format!("{display}\n（无法运行：{error}）"),
    }
}

fn openrc_log_output() -> String {
    if command_exists("journalctl") {
        return command_output(
            "journalctl",
            &["-u", "port-forwardd", "-n", "100", "--no-pager"],
        );
    }
    for path in [
        "/var/log/messages",
        "/var/log/syslog",
        "/var/log/daemon.log",
    ] {
        if Path::new(path).is_file() {
            return command_output("tail", &["-n", "100", path]);
        }
    }
    "OpenRC 未发现 journald 或常见 syslog 文件；请检查系统日志服务。".to_owned()
}

async fn print_logs(socket: &Path) -> Result<()> {
    if !socket_is_usable(socket) {
        let manager = service_manager().ok();
        println!(
            "daemon 未运行，显示启动日志（控制 socket 不存在：{}）。",
            socket.display()
        );
        match manager {
            Some(manager) => print!("{}", service_failure_diagnostics(manager)),
            None => println!("{}", service_log_hint()),
        }
        return Ok(());
    }
    let response = control::call(socket, &Request::Logs)
        .await
        .with_context(|| {
            format!(
                "无法读取 daemon 内存日志（socket：{}）。服务日志：{}",
                socket.display(),
                service_log_hint()
            )
        })?;
    for event in response.logs.unwrap_or_default() {
        println!("{} {} {}", event.timestamp_unix, event.level, event.message);
    }
    Ok(())
}

fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(command).is_file())
    })
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

fn print_doctor(config: &Path, socket: &Path, json: bool) -> Result<()> {
    let report = doctor_report(config, socket);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "HY2-MultiPort 诊断（port-forward {}）",
            env!("CARGO_PKG_VERSION")
        );
        for check in report.checks {
            println!("[{}] {}：{}", check.status, check.name, check.detail);
        }
        println!(
            "提示：配置通过后运行 sudo port-forward start；服务日志：{}",
            service_log_hint()
        );
    }
    Ok(())
}

fn doctor_report(config: &Path, socket: &Path) -> DoctorReport {
    let mut checks = vec![
        command_doctor_check("nft", "nft"),
        command_doctor_check("ip", "ip"),
        local_addresses_doctor_check(),
        config_doctor_check(config),
        socket_doctor_check(socket),
        init_doctor_check(),
    ];
    if command_exists("nft") {
        checks.push(nft_table_doctor_check("ip", "port_forward_v4"));
        checks.push(nft_table_doctor_check("ip6", "port_forward_v6"));
        checks.push(external_hook_doctor_check(config));
    }
    DoctorReport { checks }
}

fn command_doctor_check(name: &str, command: &str) -> DoctorCheck {
    if command_exists(command) {
        DoctorCheck {
            name: name.to_owned(),
            status: "ok".to_owned(),
            detail: format!("找到 {command}"),
        }
    } else {
        DoctorCheck {
            name: name.to_owned(),
            status: "error".to_owned(),
            detail: format!("未找到 {command}；请安装所需系统包"),
        }
    }
}

fn config_doctor_check(config: &Path) -> DoctorCheck {
    match Config::from_path(config).and_then(|parsed| parsed.validate_deployable().map(|_| ())) {
        Ok(()) => DoctorCheck {
            name: "配置".to_owned(),
            status: "ok".to_owned(),
            detail: format!("{} 通过语义验证", config.display()),
        },
        Err(error) => DoctorCheck {
            name: "配置".to_owned(),
            status: "error".to_owned(),
            detail: format!("{}：{error}", config.display()),
        },
    }
}

fn local_addresses_doctor_check() -> DoctorCheck {
    match local_interface_addresses() {
        Ok(addresses) if !addresses.is_empty() => DoctorCheck {
            name: "本机接口地址".to_owned(),
            status: "ok".to_owned(),
            detail: addresses
                .iter()
                .map(|(interface, address)| format!("{interface}:{address}"))
                .collect::<Vec<_>>()
                .join(", "),
        },
        Ok(_) => DoctorCheck {
            name: "本机接口地址".to_owned(),
            status: "warning".to_owned(),
            detail: "未发现可用 IPv4/IPv6 地址".to_owned(),
        },
        Err(error) => DoctorCheck {
            name: "本机接口地址".to_owned(),
            status: "error".to_owned(),
            detail: format!("无法读取 `ip -brief address`：{error:#}"),
        },
    }
}

fn local_interface_addresses() -> Result<Vec<(String, IpAddr)>> {
    let output = ProcessCommand::new("ip")
        .args(["-brief", "address"])
        .output()
        .context("无法执行 ip -brief address")?;
    if !output.status.success() {
        bail!(
            "ip -brief address 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut addresses = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 3 {
            continue;
        }
        for candidate in &fields[2..] {
            let raw = candidate.split('/').next().unwrap_or_default();
            if let Ok(address) = raw.parse::<IpAddr>()
                && !address.is_unspecified()
            {
                addresses.push((fields[0].to_owned(), address));
            }
        }
    }
    Ok(addresses)
}

fn external_hook_doctor_check(config: &Path) -> DoctorCheck {
    let allow_external_chains = match Config::from_path(config) {
        Ok(parsed) => parsed.allow_external_chains,
        Err(_) => false,
    };
    match NftCommand::default().external_hook_conflicts() {
        Ok(conflicts) if conflicts.is_empty() => DoctorCheck {
            name: "外部 nft hook".to_owned(),
            status: "ok".to_owned(),
            detail: format!("未检测到冲突；allow_external_chains={allow_external_chains}"),
        },
        Ok(conflicts) if allow_external_chains => DoctorCheck {
            name: "外部 nft hook".to_owned(),
            status: "warning".to_owned(),
            detail: format!(
                "检测到 {}；allow_external_chains=true。请人工确认外部规则的优先级与数据流；doctor 不会修改外部 table/chain。",
                conflicts.join(", ")
            ),
        },
        Ok(conflicts) => DoctorCheck {
            name: "外部 nft hook".to_owned(),
            status: "error".to_owned(),
            detail: format!(
                "检测到 {}；allow_external_chains=false，daemon 会拒绝启动。确认规则顺序后才手动在配置中设为 true；不会自动更改。",
                conflicts.join(", ")
            ),
        },
        Err(error) => DoctorCheck {
            name: "外部 nft hook".to_owned(),
            status: "warning".to_owned(),
            detail: format!("无法只读检查 nft hook：{error:#}"),
        },
    }
}

fn socket_doctor_check(socket: &Path) -> DoctorCheck {
    match fs::metadata(socket) {
        Ok(metadata) if is_socket(&metadata) => DoctorCheck {
            name: "控制 socket".to_owned(),
            status: "ok".to_owned(),
            detail: format!("{} 存在", socket.display()),
        },
        Ok(_) => DoctorCheck {
            name: "控制 socket".to_owned(),
            status: "warning".to_owned(),
            detail: format!("{} 存在但不是 Unix socket", socket.display()),
        },
        Err(error) => DoctorCheck {
            name: "控制 socket".to_owned(),
            status: "warning".to_owned(),
            detail: format!("{} 不可用：{error}", socket.display()),
        },
    }
}

#[cfg(unix)]
fn is_socket(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;

    metadata.file_type().is_socket()
}

#[cfg(not(unix))]
fn is_socket(_metadata: &fs::Metadata) -> bool {
    false
}

fn init_doctor_check() -> DoctorCheck {
    match service_manager() {
        Ok(ServiceManager::Systemd) => DoctorCheck {
            name: "服务管理".to_owned(),
            status: "ok".to_owned(),
            detail: "检测到 systemd".to_owned(),
        },
        Ok(ServiceManager::OpenRc) => DoctorCheck {
            name: "服务管理".to_owned(),
            status: "ok".to_owned(),
            detail: "检测到 OpenRC".to_owned(),
        },
        Err(error) => DoctorCheck {
            name: "服务管理".to_owned(),
            status: "warning".to_owned(),
            detail: error.to_string(),
        },
    }
}

fn nft_table_doctor_check(family: &str, table: &str) -> DoctorCheck {
    let output = ProcessCommand::new("nft")
        .args(["list", "table", family, table])
        .output();
    match output {
        Ok(result) if result.status.success() => DoctorCheck {
            name: format!("自有 nft table {family} {table}"),
            status: "ok".to_owned(),
            detail: "已安装".to_owned(),
        },
        Ok(result) => DoctorCheck {
            name: format!("自有 nft table {family} {table}"),
            status: "warning".to_owned(),
            detail: format!(
                "未安装或无法查询：{}",
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        },
        Err(error) => DoctorCheck {
            name: format!("自有 nft table {family} {table}"),
            status: "warning".to_owned(),
            detail: format!("无法运行 nft：{error}"),
        },
    }
}

fn mark_refresh_expired(mut state: RuntimeState, now: u64) -> RuntimeState {
    for profile in &mut state.profiles {
        if profile
            .dns
            .as_ref()
            .and_then(|dns| dns.next_refresh_unix)
            .is_some_and(|due| due <= now)
        {
            profile.health = Health::Degraded;
            profile.failure_reason = Some("daemon unavailable; DNS refresh is overdue".to_owned());
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use hy2_multiport::{dns::DnsStatus, state::ProfileStatus};

    use super::*;

    #[test]
    fn cached_status_marks_overdue_dns_when_daemon_is_down() {
        let mut state = RuntimeState::empty(0);
        state.profiles.push(ProfileStatus {
            name: "p".into(),
            family: hy2_multiport::config::AddressFamily::Ipv4,
            protocols: vec![],
            listen_address: "192.0.2.1".into(),
            listen_ports: vec![],
            source_cidrs: vec![],
            publicly_exposed: true,
            target_kind: "remote".into(),
            target_port: 1,
            target_host: Some("x".into()),
            target_address: Some("198.51.100.1".into()),
            source_mode: None,
            dns: Some(DnsStatus {
                host: "x".into(),
                active_address: Some("198.51.100.1".parse().unwrap()),
                last_success_unix: None,
                next_refresh_unix: Some(1),
                cache_written_unix: Some(0),
                status: Health::Healthy,
                failure_reason: None,
            }),
            health: Health::Healthy,
            failure_reason: None,
        });
        assert_eq!(
            mark_refresh_expired(state, 2).profiles[0].health,
            Health::Degraded
        );
    }

    #[test]
    fn service_manager_selection_requires_a_usable_manager() {
        assert_eq!(
            select_service_manager(true, true),
            if command_exists("systemctl") {
                Some(ServiceManager::Systemd)
            } else if command_exists("rc-service") {
                Some(ServiceManager::OpenRc)
            } else {
                None
            }
        );
    }

    #[test]
    fn init_never_overwrites_an_existing_config() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(&config, "operator-owned").unwrap();
        assert!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(init_config(&config, true))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(config).unwrap(), "operator-owned");
    }

    #[test]
    fn doctor_reports_missing_config_as_an_actionable_error() {
        let directory = tempfile::tempdir().unwrap();
        let report = doctor_report(
            &directory.path().join("missing.toml"),
            &directory.path().join("socket"),
        );
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "配置" && check.status == "error")
        );
    }

    #[test]
    fn template_init_writes_explicit_non_startable_todos() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(init_config(&config, true))
            .unwrap();
        let content = std::fs::read_to_string(&config).unwrap();
        assert!(content.contains("TODO-listen-address"));
        assert!(Config::from_path(&config).is_err());
    }

    #[test]
    fn missing_socket_logs_use_service_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("missing.sock");
        assert!(!socket_is_usable(&socket));
    }

    #[test]
    fn startup_never_succeeds_when_socket_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("missing.sock");
        assert!(
            wait_for_daemon_ready_with_timeout(ServiceManager::OpenRc, &socket, Duration::ZERO)
                .is_err()
        );
    }
}
