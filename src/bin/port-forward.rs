use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hy2_multiport::{
    config::Config,
    control::{self, Request},
    dns::{Health, unix_now},
    state::{self, RuntimeState},
};
use serde::Serialize;

const BUILTIN_EXAMPLE_CONFIG: &str = include_str!("../../examples/config.toml");
const DEFAULT_EXAMPLE_PATH: &str = "/etc/port-forward/config.toml.example";

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
        /// Copy this template instead of the installed or built-in example.
        #[arg(long)]
        template: Option<PathBuf>,
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
        Command::Init { config, template } => init_config(&config, template.as_deref())?,
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
                .and_then(|parsed| parsed.validate())
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
        Command::Logs { socket } => {
            let response = control::call(&socket, &Request::Logs)
                .await
                .with_context(|| {
                    format!(
                        "无法读取 daemon 日志（socket：{}）。下一步：运行 sudo port-forward doctor；服务日志：{}",
                        socket.display(),
                        service_log_hint()
                    )
                })?;
            for event in response.logs.unwrap_or_default() {
                println!("{} {} {}", event.timestamp_unix, event.level, event.message);
            }
        }
    }
    Ok(())
}

fn init_config(config: &Path, template: Option<&Path>) -> Result<()> {
    if config.exists() {
        bail!(
            "拒绝覆盖已有配置 {}。请先备份并手动编辑，或运行 port-forward validate。",
            config.display()
        );
    }
    let content = match template {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("无法读取指定模板 {}", path.display()))?,
        None if Path::new(DEFAULT_EXAMPLE_PATH).is_file() => {
            fs::read_to_string(DEFAULT_EXAMPLE_PATH)
                .with_context(|| format!("无法读取安装器示例 {DEFAULT_EXAMPLE_PATH}"))?
        }
        None => BUILTIN_EXAMPLE_CONFIG.to_owned(),
    };
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
    println!("已创建示例配置（未覆盖任何已有文件）：{}", config.display());
    println!("下一步：sudoedit {}", config.display());
    println!(
        "验证：sudo port-forward validate --config {}",
        config.display()
    );
    println!("启动：sudo port-forward start");
    Ok(())
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
    if let Err(error) = Config::from_path(config).and_then(|parsed| parsed.validate().map(|_| ())) {
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
            "服务操作失败：{program} {argument} port-forwardd：{detail}\n日志查看：{}",
            service_log_hint()
        );
    }
    println!("服务操作成功：{program} {argument} port-forwardd");
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
        config_doctor_check(config),
        socket_doctor_check(socket),
        init_doctor_check(),
    ];
    if command_exists("nft") {
        checks.push(nft_table_doctor_check("ip", "port_forward_v4"));
        checks.push(nft_table_doctor_check("ip6", "port_forward_v6"));
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
    match Config::from_path(config).and_then(|parsed| parsed.validate().map(|_| ())) {
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
        assert!(init_config(&config, None).is_err());
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
}
