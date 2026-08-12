use std::{
    fmt::Write as FmtWrite,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hy2_multiport::{
    config::Config,
    control::{self, Request},
    dns::{Health, unix_now},
    nft::NftCommand,
    state::{self, RuntimeState},
};
use serde::Serialize;

const TODO_CONFIG: &str = r#"# HY2-MultiPort first-use template. It is deliberately NOT startable.
# Replace every TODO value by running `sudo port-forward init` in a terminal,
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
    /// Ask the root daemon to run one all-or-nothing full reload.
    Apply {
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
        Command::Init {
            config,
            template,
            non_interactive,
        } => init_config(&config, template || non_interactive)?,
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
        Command::Apply { socket } => {
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

fn init_config(config: &Path, force_template: bool) -> Result<()> {
    if config.exists() {
        bail!(
            "拒绝覆盖已有配置 {}。请先备份并手动编辑，或运行 port-forward validate。",
            config.display()
        );
    }
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let content = if force_template || !interactive {
        if !interactive && !force_template {
            println!("未检测到交互式终端；不会复制文档保留地址示例，改为生成明确的 TODO 配置。");
        }
        TODO_CONFIG.to_owned()
    } else {
        interactive_wizard()?
    };
    write_new_config(config, &content)?;
    println!("已创建首次使用配置：{}", config.display());
    if content == TODO_CONFIG {
        println!(
            "[ERROR] 该配置含 TODO，刻意不能启动。请在交互式终端运行：sudo port-forward init --config {}",
            config.display()
        );
    } else {
        println!("[OK] 已写入向导选择的真实字段；仍请检查规则顺序和暴露范围。");
    }
    println!(
        "验证：sudo port-forward validate --config {}",
        config.display()
    );
    println!(
        "诊断：sudo port-forward doctor --config {}",
        config.display()
    );
    println!("启动：仅在 validate 与 doctor 无 ERROR 后运行 sudo port-forward start");
    print_setup_diagnostics(config);
    Ok(())
}

fn write_new_config(config: &Path, content: &str) -> Result<()> {
    let parent = config.parent().context("配置路径必须包含父目录")?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录 {}", parent.display()))?;
        set_private_directory_permissions(parent)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config)
        .with_context(|| format!("无法安全创建配置 {}", config.display()))?;
    output
        .write_all(content.as_bytes())
        .with_context(|| format!("无法写入配置 {}", config.display()))?;
    output
        .sync_all()
        .with_context(|| format!("无法同步配置 {}", config.display()))?;
    set_private_file_permissions(config)?;
    Ok(())
}

fn interactive_wizard() -> Result<String> {
    println!("\nHY2-MultiPort 首次使用向导（约 5 分钟）");
    println!(
        "不会使用 192.0.2.0/24、198.51.100.0/24、203.0.113.0/24 或 2001:db8::/32 文档地址。\n"
    );
    let addresses = local_interface_addresses()?;
    if addresses.is_empty() {
        bail!(
            "未从 `ip -brief address` 发现可选地址；请确认 iproute2 可用后重试，或使用 --non-interactive 生成 TODO 模板"
        );
    }
    println!("本机接口地址：");
    for (index, (interface, address)) in addresses.iter().enumerate() {
        println!("  {}) {}  ({interface})", index + 1, address);
    }
    let chosen = prompt("选择监听地址序号，或直接输入地址", None)?;
    let listen_address = chosen
        .parse::<usize>()
        .ok()
        .and_then(|index| addresses.get(index.saturating_sub(1)))
        .map(|(_, address)| address.to_string())
        .unwrap_or(chosen);
    let listen_ip: IpAddr = listen_address.parse().with_context(|| {
        format!("监听地址 {listen_address:?} 不是有效 IP 地址；请重新运行 init")
    })?;
    if hy2_multiport::config::is_documentation_address(listen_ip) {
        bail!("监听地址 {listen_ip} 是文档保留地址；请重新运行 init 并选择本机真实地址");
    }
    if !addresses.iter().any(|(_, address)| *address == listen_ip) {
        bail!("监听地址 {listen_ip} 不属于上面列出的本机接口；请重新运行 init 并选择本机地址");
    }
    let family = if listen_ip.is_ipv4() { "ipv4" } else { "ipv6" };
    let protocols = match prompt("协议（tcp / udp / both）", Some("tcp"))?.as_str() {
        "tcp" => "[\"tcp\"]",
        "udp" => "[\"udp\"]",
        "both" => "[\"tcp\", \"udp\"]",
        value => bail!("不支持的协议 {value:?}；请输入 tcp、udp 或 both"),
    };
    let listen_port = prompt_port("监听端口", Some("443"))?;
    let target_kind = prompt(
        "目标类型（redirect / remote / loopback-dnat）",
        Some("redirect"),
    )?;
    if target_kind != "redirect" && target_kind != "remote" && target_kind != "loopback-dnat" {
        bail!("不支持的目标类型 {target_kind:?}");
    }
    if target_kind == "loopback-dnat" && family != "ipv4" {
        bail!("loopback-dnat 仅支持 IPv4；IPv6 请使用 redirect");
    }
    let target_port = prompt_port("目标端口", Some("8443"))?;
    let source_default = if family == "ipv4" {
        "0.0.0.0/0"
    } else {
        "::/0"
    };
    let source_cidr = prompt("允许来源 CIDR（默认代表向全网公开）", Some(source_default))?;
    let name = prompt("配置名称", Some("first-forward"))?;
    if name.to_ascii_uppercase().contains("TODO") {
        bail!("配置名称不能包含 TODO");
    }
    let mut body = format!(
        "# Created by `port-forward init`; review before start.\nschema_version = 1\nallow_external_chains = false\n\n[[profiles]]\nname = {name:?}\nfamily = {family:?}\nlisten_address = {listen_address:?}\nprotocols = {protocols}\nsource_cidrs = [{source_cidr:?}]\n\n[profiles.listen_ports]\nports = [{listen_port}]\n\n[profiles.target]\nkind = {target_kind:?}\nport = {target_port}\n"
    );
    if target_kind == "remote" {
        let host = prompt("远程目标 IP 或 FQDN", None)?;
        if host.to_ascii_uppercase().contains("TODO") {
            bail!("远程目标不能包含 TODO");
        }
        if let Ok(address) = host.parse::<IpAddr>()
            && hy2_multiport::config::is_documentation_address(address)
        {
            bail!("远程目标 {address} 是文档保留地址；请使用真实目标");
        }
        let _ = writeln!(body, "host = {host:?}");
        if family == "ipv4" {
            let source_mode = prompt(
                "IPv4 远程目标来源模式（masquerade / preserve）",
                Some("masquerade"),
            )?;
            if source_mode != "masquerade" && source_mode != "preserve" {
                bail!("来源模式必须是 masquerade 或 preserve");
            }
            let _ = writeln!(body, "source_mode = {source_mode:?}");
        }
    }
    if target_kind == "loopback-dnat" {
        body = body.replacen(
            "protocols = ",
            "allow_route_localnet = true\nprotocols = ",
            1,
        );
    }
    Config::from_toml(&body)
        .map(|_| body)
        .map_err(anyhow::Error::from)
        .context("向导输入未通过语义验证；未创建配置，请重新运行 init")
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(value) => print!("{label} [{value}]："),
        None => print!("{label}："),
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    if value.is_empty() {
        return default
            .map(str::to_owned)
            .context("该字段不能为空；请重新运行 init");
    }
    Ok(value.to_owned())
}

fn prompt_port(label: &str, default: Option<&str>) -> Result<u16> {
    let value = prompt(label, default)?;
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .with_context(|| format!("{label} 必须是 1 到 65535；请重新运行 init"))
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
                "检测到 {}；allow_external_chains=true。请人工确认 NetBird/其他规则的优先级与数据流；doctor 不会修改外部 table/chain。",
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

fn print_setup_diagnostics(config: &Path) {
    println!("\n初始化后的运行时诊断：");
    for check in doctor_report(config, Path::new(hy2_multiport::DEFAULT_SOCKET_PATH)).checks {
        let label = match check.status.as_str() {
            "ok" => "OK",
            "warning" => "WARN",
            _ => "ERROR",
        };
        println!("[{label}] {}：{}", check.name, check.detail);
    }
    println!(
        "下一步：修复所有 ERROR 后运行 sudo port-forward validate，再执行 sudo port-forward doctor 和 sudo port-forward start。"
    );
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
        assert!(init_config(&config, true).is_err());
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
        init_config(&config, true).unwrap();
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
