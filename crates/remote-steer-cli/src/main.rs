use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(unix)]
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use remote_steer_core::{
    profile_by_id, ConditionAxis, ConditionKind, EffectId, FfbCommand, FfbCommandKind, FfbEffect,
    FfbEffectKind, FfbEnvelope, FfbReplay, FfbReplyKind, PeriodicWaveform, PhysicalWheelBackend,
    VirtualWheelBackend, WheelProfileId, WheelStateSnapshot,
};
use remote_steer_transport::{
    profile_hash, Channel, Handshake, InputStaleDrop, TransportMessage, UdpPeer,
};
use serde::{Deserialize, Serialize};
use tokio::net::lookup_host;
use tokio::time;
use tracing::{debug, info};

const DEFAULT_PORT: u16 = 43150;
const DEFAULT_LISTEN: &str = "0.0.0.0:43150";
const DEFAULT_BIND: &str = "0.0.0.0:0";
const CONFIG_ENV: &str = "REMOTE_STEER_CONFIG";
const SERVER_ENV: &str = "REMOTE_STEER_SERVER";
const TOKEN_ENV: &str = "REMOTE_STEER_TOKEN";
const INPUT_RESYNC_INTERVAL: Duration = Duration::from_millis(250);
const MAX_FFB_COMMANDS_PER_TICK: usize = 16;

#[derive(Debug, Parser)]
#[command(name = "remote-steer")]
#[command(version)]
#[command(about = "Remote steering wheel bridge")]
#[command(arg_required_else_help = true)]
#[command(after_help = "\
Quick start:
  First run on the Windows wheel machine:
    remote-steer server --token <shared-token>

  First run on the Linux game machine:
    remote-steer client <server-ip> --token <shared-token>

  After a successful first connection:
    remote-steer server start
    remote-steer client start
    remote-steer test

Notes:
  The default port is 43150.
  The same token must be used on both machines.
  Server, token, and config path can also be set with REMOTE_STEER_SERVER,
  REMOTE_STEER_TOKEN, and REMOTE_STEER_CONFIG.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the machine that has the real wheel attached.
    #[command(visible_alias = "serve")]
    Server(ServerCommand),
    /// Run the machine where the game will see a virtual T150.
    #[command(visible_alias = "connect")]
    Client(ClientCommand),
    /// Play the 12 Thrustmaster Test Forces presets.
    Test {
        /// Server address for remote testing. If omitted, uses the saved server first.
        #[arg(env = SERVER_ENV, value_name = "SERVER")]
        server: Option<String>,
        /// Play one effect and exit.
        #[arg(long, value_enum)]
        effect: Option<TestForce>,
        /// Local UDP bind address for remote testing.
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Shared token for remote testing. Can also be set with REMOTE_STEER_TOKEN.
        #[arg(long, env = TOKEN_ENV, value_name = "TOKEN")]
        token: Option<String>,
    },
    /// Inspect the current wheel/backend.
    Probe {
        #[arg(value_enum)]
        target: ProbeTarget,
    },
    /// Dump Windows DirectInput details for the T150.
    DumpDirectInput,
    #[command(hide = true)]
    Physical {
        #[arg(long)]
        listen: SocketAddr,
        #[arg(long, env = TOKEN_ENV)]
        token: Option<String>,
        #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
        profile: ProfileArg,
    },
    #[command(hide = true)]
    Virtual {
        #[arg(long)]
        connect: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: SocketAddr,
        #[arg(long, env = TOKEN_ENV)]
        token: Option<String>,
        #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
        profile: ProfileArg,
    },
    #[command(hide = true)]
    TestFfb {
        #[arg(value_enum)]
        target: ProbeTarget,
        #[arg(long, value_enum)]
        effect: Option<TestForce>,
        #[arg(long)]
        connect: Option<SocketAddr>,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: SocketAddr,
        #[arg(long, env = TOKEN_ENV)]
        token: Option<String>,
        #[arg(long)]
        device: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
struct ServerCommand {
    #[command(subcommand)]
    action: Option<ServerAction>,
    /// Address to listen on.
    #[arg(long, default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
    #[arg(long, env = TOKEN_ENV, value_name = "TOKEN")]
    token: Option<String>,
    /// Wheel profile to expose.
    #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
    profile: ProfileArg,
}

#[derive(Debug, Subcommand)]
enum ServerAction {
    /// Start the server in the background.
    Start(ServerStartArgs),
    /// Stop the background server.
    Stop,
    /// Show background server status.
    Status,
}

#[derive(Debug, Args)]
struct ServerStartArgs {
    /// Address to listen on.
    #[arg(long, default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
    #[arg(long, env = TOKEN_ENV, value_name = "TOKEN")]
    token: Option<String>,
    /// Wheel profile to expose.
    #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
    profile: ProfileArg,
}

#[derive(Debug, Args)]
struct ClientCommand {
    #[command(subcommand)]
    action: Option<ClientAction>,
    /// Server address. A missing port uses 43150. Can also be set with REMOTE_STEER_SERVER.
    #[arg(env = SERVER_ENV, value_name = "SERVER")]
    server: Option<String>,
    /// Local UDP bind address.
    #[arg(long, default_value = DEFAULT_BIND)]
    bind: SocketAddr,
    /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
    #[arg(long, env = TOKEN_ENV, value_name = "TOKEN")]
    token: Option<String>,
    /// Wheel profile to create.
    #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
    profile: ProfileArg,
}

#[derive(Debug, Subcommand)]
enum ClientAction {
    /// Start the client in the background.
    Start(ClientStartArgs),
    /// Stop the background client.
    Stop,
    /// Show background client status.
    Status,
}

#[derive(Debug, Args)]
struct ClientStartArgs {
    /// Server address. A missing port uses 43150. Can also be set with REMOTE_STEER_SERVER.
    #[arg(env = SERVER_ENV, value_name = "SERVER")]
    server: Option<String>,
    /// Local UDP bind address.
    #[arg(long, default_value = DEFAULT_BIND)]
    bind: SocketAddr,
    /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
    #[arg(long, env = TOKEN_ENV, value_name = "TOKEN")]
    token: Option<String>,
    /// Wheel profile to create.
    #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
    profile: ProfileArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProfileArg {
    T150,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProbeTarget {
    Physical,
    Virtual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TestForce {
    Engine,
    BlownTire,
    Boing,
    Explosion,
    OpenSea,
    TurboBoost,
    Gong,
    BumpyRoad,
    CarCrash,
    Punch,
    ForceField,
    Whiplash,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SavedConfig {
    server: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedValue {
    value: String,
    remember: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RememberDefaults {
    server: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum ProcessRole {
    Server,
    Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessRecord {
    pid: u32,
    log_path: PathBuf,
}

impl From<ProfileArg> for WheelProfileId {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::T150 => WheelProfileId::T150,
        }
    }
}

impl ProfileArg {
    fn as_cli_value(self) -> &'static str {
        match self {
            ProfileArg::T150 => "t150",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remote_steer=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Server(command) => run_server_command(command).await,
        Command::Client(command) => run_client_command(command).await,
        Command::Test {
            server,
            effect,
            bind,
            token,
        } => {
            let saved_config = load_saved_config()?;
            run_easy_test(effect, server, bind, token, saved_config).await
        }
        Command::Probe { target } => run_probe(target),
        Command::DumpDirectInput => run_dump_direct_input(),
        Command::Physical {
            listen,
            token,
            profile,
        } => {
            let saved_config = load_saved_config()?;
            let token = resolve_token(token, &saved_config, "physical")?;
            run_physical(
                listen,
                token.value,
                profile.into(),
                RememberDefaults::default(),
            )
            .await
        }
        Command::Virtual {
            connect,
            bind,
            token,
            profile,
        } => {
            let saved_config = load_saved_config()?;
            let token = resolve_token(token, &saved_config, "virtual")?;
            run_virtual(
                bind,
                connect,
                token.value,
                profile.into(),
                RememberDefaults::default(),
            )
            .await
        }
        Command::TestFfb {
            target,
            effect,
            connect,
            bind,
            token,
            device,
        } => run_test_ffb(target, effect, connect, bind, token, device).await,
    }
}

async fn run_server_command(command: ServerCommand) -> Result<()> {
    match command.action {
        Some(ServerAction::Start(args)) => start_server(args).await,
        Some(ServerAction::Stop) => stop_managed(ProcessRole::Server),
        Some(ServerAction::Status) => status_managed(ProcessRole::Server),
        None => {
            let saved_config = load_saved_config()?;
            let token = resolve_token(command.token, &saved_config, "server")?;
            run_physical(
                command.listen,
                token.value,
                command.profile.into(),
                RememberDefaults {
                    token: token.remember,
                    ..RememberDefaults::default()
                },
            )
            .await
        }
    }
}

async fn run_client_command(command: ClientCommand) -> Result<()> {
    match command.action {
        Some(ClientAction::Start(args)) => start_client(args).await,
        Some(ClientAction::Stop) => stop_managed(ProcessRole::Client),
        Some(ClientAction::Status) => status_managed(ProcessRole::Client),
        None => {
            let saved_config = load_saved_config()?;
            let server = resolve_server_name(command.server, &saved_config)?;
            let token = resolve_token(command.token, &saved_config, "client")?;
            let connect = resolve_server(&server.value).await?;
            run_virtual(
                command.bind,
                connect,
                token.value,
                command.profile.into(),
                RememberDefaults {
                    server: server.remember,
                    token: token.remember,
                },
            )
            .await
        }
    }
}

async fn start_server(args: ServerStartArgs) -> Result<()> {
    let saved_config = load_saved_config()?;
    let token = resolve_token(args.token, &saved_config, "server start")?;
    let remember = RememberDefaults {
        token: token.remember,
        ..RememberDefaults::default()
    };
    if let Some(path) = remember_defaults(remember)? {
        println!("saved defaults: {}", path.display());
    }

    start_managed(
        ProcessRole::Server,
        vec![
            "physical".to_string(),
            "--listen".to_string(),
            args.listen.to_string(),
            "--profile".to_string(),
            args.profile.as_cli_value().to_string(),
        ],
    )
}

async fn start_client(args: ClientStartArgs) -> Result<()> {
    let saved_config = load_saved_config()?;
    let server = resolve_server_name(args.server, &saved_config)?;
    let token = resolve_token(args.token, &saved_config, "client start")?;
    let connect = resolve_server(&server.value).await?;
    let remember = RememberDefaults {
        server: server.remember,
        token: token.remember,
    };
    if let Some(path) = remember_defaults(remember)? {
        println!("saved defaults: {}", path.display());
    }

    start_managed(
        ProcessRole::Client,
        vec![
            "virtual".to_string(),
            "--connect".to_string(),
            connect.to_string(),
            "--bind".to_string(),
            args.bind.to_string(),
            "--profile".to_string(),
            args.profile.as_cli_value().to_string(),
        ],
    )
}

fn start_managed(role: ProcessRole, args: Vec<String>) -> Result<()> {
    match read_process_record(role)? {
        Some(record) if process_is_running(record.pid) => {
            bail!(
                "{} is already running with pid {}; log: {}",
                role.label(),
                record.pid,
                record.log_path.display()
            )
        }
        Some(_) => {
            let _ = fs::remove_file(process_record_path(role)?);
        }
        None => {}
    }

    let log_path = process_log_path(role)?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(&log_path)
        .with_context(|| format!("failed to create log {}", log_path.display()))?;

    let pid = spawn_managed_child(&args, &log_path)
        .with_context(|| format!("failed to start {}", role.label()))?;
    thread::sleep(Duration::from_millis(500));
    if !process_is_running(pid) {
        bail!(
            "{} exited immediately; log: {}",
            role.label(),
            log_path.display()
        );
    }
    let record = ProcessRecord { pid, log_path };
    write_process_record(role, &record)?;
    println!("started {}: pid {}", role.label(), record.pid);
    println!("log: {}", record.log_path.display());
    println!("stop: remote-steer {} stop", role.command_name());
    Ok(())
}

#[cfg(unix)]
fn spawn_managed_child(args: &[String], log_path: &Path) -> Result<u32> {
    let stdout = File::create(log_path)
        .with_context(|| format!("failed to create log {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to clone log {}", log_path.display()))?;
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let mut command = ProcessCommand::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_detached_child(&mut command);
    let child = command.spawn()?;
    Ok(child.id())
}

#[cfg(windows)]
fn spawn_managed_child(args: &[String], log_path: &Path) -> Result<u32> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let mut command_line = format!("cmd.exe /C \"\"{}\"", exe.display());
    for arg in args {
        command_line.push(' ');
        command_line.push_str(&quote_windows_cmd_arg(arg));
    }
    command_line.push_str(&format!(" > \"{}\" 2>&1\"", log_path.display()));

    let output = ProcessCommand::new("wmic")
        .args(["process", "call", "create", &command_line])
        .output()
        .context("failed to run wmic")?;
    if !output.status.success() {
        bail!(
            "wmic failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    parse_wmic_process_id(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| anyhow::anyhow!("wmic did not return a process id"))
}

#[cfg(not(any(unix, windows)))]
fn spawn_managed_child(_args: &[String], _log_path: &Path) -> Result<u32> {
    bail!("background start is not implemented on this OS")
}

#[cfg(windows)]
fn quote_windows_cmd_arg(arg: &str) -> String {
    if arg.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'-' | b'/' | b'\\')
    }) {
        return arg.to_string();
    }
    format!("\"{}\"", arg.replace('"', "\\\""))
}

#[cfg(windows)]
fn parse_wmic_process_id(output: &str) -> Option<u32> {
    let marker = "ProcessId =";
    let start = output.find(marker)? + marker.len();
    let rest = &output[start..];
    let value = rest
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    value.parse().ok()
}

fn stop_managed(role: ProcessRole) -> Result<()> {
    let Some(record) = read_process_record(role)? else {
        println!("{} stopped", role.label());
        return Ok(());
    };

    if !process_is_running(record.pid) {
        let _ = fs::remove_file(process_record_path(role)?);
        println!("{} stale pid removed: {}", role.label(), record.pid);
        return Ok(());
    }

    terminate_process(record.pid)?;
    for _ in 0..20 {
        if !process_is_running(record.pid) {
            let _ = fs::remove_file(process_record_path(role)?);
            println!("stopped {}: pid {}", role.label(), record.pid);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    force_terminate_process(record.pid)?;
    let _ = fs::remove_file(process_record_path(role)?);
    println!("stopped {}: pid {}", role.label(), record.pid);
    Ok(())
}

fn status_managed(role: ProcessRole) -> Result<()> {
    let Some(record) = read_process_record(role)? else {
        println!("{} stopped", role.label());
        return Ok(());
    };

    if process_is_running(record.pid) {
        println!("{} running: pid {}", role.label(), record.pid);
    } else {
        println!("{} stale: pid {}", role.label(), record.pid);
    }
    println!("log: {}", record.log_path.display());
    Ok(())
}

impl ProcessRole {
    fn command_name(self) -> &'static str {
        match self {
            ProcessRole::Server => "server",
            ProcessRole::Client => "client",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ProcessRole::Server => "server",
            ProcessRole::Client => "client",
        }
    }
}

fn resolve_server_name(server: Option<String>, config: &SavedConfig) -> Result<ResolvedValue> {
    if let Some(server) = server {
        let server = clean_config_value("server address", server)?;
        return Ok(ResolvedValue {
            value: server.clone(),
            remember: Some(server),
        });
    }
    if let Some(server) = config.server.as_ref() {
        let server = clean_config_value("saved server address", server.clone())?;
        return Ok(ResolvedValue {
            value: server,
            remember: None,
        });
    }
    bail!(
        "missing server address; first run `remote-steer client <server-ip> --token <shared-token>`, or set {SERVER_ENV}"
    )
}

fn resolve_optional_server_name(
    server: Option<String>,
    config: &SavedConfig,
) -> Result<Option<ResolvedValue>> {
    if server.is_some() {
        return resolve_server_name(server, config).map(Some);
    }
    match config.server.as_ref() {
        Some(server) => Ok(Some(ResolvedValue {
            value: clean_config_value("saved server address", server.clone())?,
            remember: None,
        })),
        None => Ok(None),
    }
}

fn resolve_token(
    token: Option<String>,
    config: &SavedConfig,
    command_name: &str,
) -> Result<ResolvedValue> {
    if let Some(token) = token {
        let token = clean_config_value("token", token)?;
        return Ok(ResolvedValue {
            value: token.clone(),
            remember: Some(token),
        });
    }
    if let Some(token) = config.token.as_ref() {
        let token = clean_config_value("saved token", token.clone())?;
        return Ok(ResolvedValue {
            value: token,
            remember: None,
        });
    }
    bail!(
        "missing token; first run `remote-steer {command_name} --token <shared-token>`, or set {TOKEN_ENV}"
    )
}

fn clean_config_value(label: &str, value: String) -> Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("{label} is empty");
    }
    Ok(value)
}

fn load_saved_config() -> Result<SavedConfig> {
    let path = config_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(SavedConfig::default()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read remote-steer config {}", path.display()))
        }
    };
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse remote-steer config {}", path.display()))
}

fn remember_defaults(update: RememberDefaults) -> Result<Option<PathBuf>> {
    if update.server.is_none() && update.token.is_none() {
        return Ok(None);
    }

    let mut config = load_saved_config()?;
    let mut changed = false;
    if let Some(server) = update.server {
        if config.server.as_deref() != Some(server.as_str()) {
            config.server = Some(server);
            changed = true;
        }
    }
    if let Some(token) = update.token {
        if config.token.as_deref() != Some(token.as_str()) {
            config.token = Some(token);
            changed = true;
        }
    }

    if changed {
        save_saved_config(&config).map(Some)
    } else {
        Ok(None)
    }
}

fn save_saved_config(config: &SavedConfig) -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create remote-steer config directory {}",
                parent.display()
            )
        })?;
    }
    let contents = format!("{}\n", serde_json::to_string_pretty(config)?);
    write_private_file(&path, contents.as_bytes())
        .with_context(|| format!("failed to write remote-steer config {}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)
}

fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(CONFIG_ENV) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            bail!("{CONFIG_ENV} is empty");
        }
        return Ok(path);
    }

    if cfg!(windows) {
        if let Some(appdata) = env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata)
                .join("remote-steer")
                .join("config.json"));
        }
    } else if let Some(xdg_config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home)
            .join("remote-steer")
            .join("config.json"));
    }

    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("could not find HOME or USERPROFILE for config path"))?;
    Ok(home
        .join(".config")
        .join("remote-steer")
        .join("config.json"))
}

fn state_dir() -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            return Ok(parent.to_path_buf());
        }
    }
    env::current_dir().context("failed to resolve current directory")
}

fn process_record_path(role: ProcessRole) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("remote-steer-{}.pid.json", role.command_name())))
}

fn process_log_path(role: ProcessRole) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("remote-steer-{}.log", role.command_name())))
}

fn read_process_record(role: ProcessRole) -> Result<Option<ProcessRecord>> {
    let path = process_record_path(role)?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read process state {}", path.display()))
        }
    };
    serde_json::from_str(&contents)
        .map(Some)
        .with_context(|| format!("failed to parse process state {}", path.display()))
}

fn write_process_record(role: ProcessRole, record: &ProcessRecord) -> Result<()> {
    let path = process_record_path(role)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create remote-steer state directory {}",
                parent.display()
            )
        })?;
    }
    let contents = format!("{}\n", serde_json::to_string_pretty(record)?);
    write_private_file(&path, contents.as_bytes())
        .with_context(|| format!("failed to write process state {}", path.display()))
}

#[cfg(unix)]
fn configure_detached_child(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    let output = ProcessCommand::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.trim_start().starts_with('"'))
        .any(|line| {
            line.split(',')
                .nth(1)
                .map(|field| field.trim_matches('"') == pid.to_string())
                .unwrap_or(false)
        })
}

#[cfg(not(any(unix, windows)))]
fn process_is_running(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == -1 {
        return Err(io::Error::last_os_error()).context("failed to terminate process");
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    force_terminate_process(pid)
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32) -> Result<()> {
    bail!("process stop is not implemented on this OS")
}

#[cfg(unix)]
fn force_terminate_process(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if rc == -1 {
        return Err(io::Error::last_os_error()).context("failed to force terminate process");
    }
    Ok(())
}

#[cfg(windows)]
fn force_terminate_process(pid: u32) -> Result<()> {
    let status = ProcessCommand::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .context("failed to run taskkill")?;
    if !status.success() {
        bail!("taskkill failed with {status}");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn force_terminate_process(_pid: u32) -> Result<()> {
    bail!("process stop is not implemented on this OS")
}

async fn run_easy_test(
    effect: Option<TestForce>,
    server: Option<String>,
    bind: SocketAddr,
    token: Option<String>,
    saved_config: SavedConfig,
) -> Result<()> {
    if let Some(server) = resolve_optional_server_name(server, &saved_config)? {
        let connect = resolve_server(&server.value).await?;
        let token = resolve_token(token, &saved_config, "test")?;
        println!("remote-steer test");
        println!("server: {connect}");
        run_remote_test_ffb(
            bind,
            connect,
            token.value,
            effect,
            RememberDefaults {
                server: server.remember,
                token: token.remember,
            },
        )
        .await
    } else {
        println!("remote-steer local physical test");
        println!("tip: pass a server address to test a remote wheel, for example `remote-steer test <server-ip> --token <shared-token>`\n");
        run_test_ffb(ProbeTarget::Physical, effect, None, bind, None, None).await
    }
}

async fn resolve_server(server: &str) -> Result<SocketAddr> {
    let endpoint = normalize_server_endpoint(server)?;
    let mut addrs = lookup_host(&endpoint)
        .await
        .with_context(|| format!("could not resolve server address `{server}`"))?;
    addrs.next().ok_or_else(|| {
        anyhow::anyhow!("server address `{server}` did not resolve to a socket address")
    })
}

fn normalize_server_endpoint(server: &str) -> Result<String> {
    let server = server.trim();
    if server.is_empty() {
        bail!("server address is empty");
    }
    if server.parse::<SocketAddr>().is_ok() {
        return Ok(server.to_string());
    }
    if server.starts_with('[') && server.ends_with(']') {
        return Ok(format!("{server}:{DEFAULT_PORT}"));
    }
    if server.matches(':').count() > 1 {
        bail!("IPv6 server addresses must use brackets, for example `[::1]:{DEFAULT_PORT}`");
    }
    if let Some((_, port)) = server.rsplit_once(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(server.to_string());
        }
    }
    Ok(format!("{server}:{DEFAULT_PORT}"))
}

async fn run_physical(
    listen: SocketAddr,
    token: String,
    profile: WheelProfileId,
    remember: RememberDefaults,
) -> Result<()> {
    let selected = profile_by_id(profile);
    println!("remote-steer server");
    println!("listening: {listen}");
    println!("profile: {}", selected.id.as_str());
    println!("first client run: remote-steer client <this-machine-ip> --token <shared-token>");
    println!("after that: remote-steer client");
    println!("stop: Ctrl+C\n");
    info!(%listen, profile = selected.id.as_str(), "starting server");
    let mut backend =
        open_physical_backend().context("failed to open the physical wheel backend")?;
    let mut peer = UdpPeer::bind(listen, token, 1)
        .await
        .context("failed to bind the server UDP socket")?;
    if let Some(path) = remember_defaults(remember)? {
        println!("saved defaults: {}", path.display());
    }
    let mut buf = vec![0_u8; 64 * 1024];
    let mut input_tick = time::interval(Duration::from_millis(4));
    let mut session_ready = false;
    let mut connected_addr = None;
    let mut input_send_state = InputSendState::new(INPUT_RESYNC_INTERVAL);

    loop {
        tokio::select! {
            packet = peer.recv(&mut buf) => {
                let (addr, channel, message) = packet?;
                debug!(%addr, ?channel, ?message, "received packet");
                match message {
                    TransportMessage::Hello(_) => {
                        let handshake = handshake("physical", profile);
                        peer.send_to_remote(Channel::Session, &TransportMessage::HelloAck(handshake)).await?;
                        if connected_addr != Some(addr) {
                            println!("connected: {addr}");
                            connected_addr = Some(addr);
                        }
                        session_ready = true;
                    }
                    TransportMessage::FfbCommand(command) => {
                        debug!(
                            command_id = command.command_id,
                            kind = ?command.kind,
                            "applying ffb command"
                        );
                        let reply = backend.apply_ffb(command)?;
                        debug!(
                            command_id = reply.command_id,
                            kind = ?reply.kind,
                            "sending ffb reply"
                        );
                        peer.send_to_remote(Channel::FfbControl, &TransportMessage::FfbReply(reply)).await?;
                    }
                    _ => {}
                }
            }
            _ = input_tick.tick() => {
                if session_ready {
                    if let Some(snapshot) = backend.poll_input()? {
                        if input_send_state.should_send(&snapshot, Instant::now()) {
                            peer.send_to_remote(Channel::Input, &TransportMessage::Input(snapshot)).await?;
                        }
                    }
                }
            }
        }
    }
}

async fn run_virtual(
    bind: SocketAddr,
    connect: SocketAddr,
    token: String,
    profile: WheelProfileId,
    remember: RememberDefaults,
) -> Result<()> {
    let selected = profile_by_id(profile);
    println!("remote-steer client");
    println!("server: {connect}");
    println!("profile: {}", selected.id.as_str());
    println!("creating virtual wheel...\n");
    info!(%bind, %connect, profile = selected.id.as_str(), "starting client");
    let mut backend = open_virtual_backend().context(
        "failed to create the virtual wheel; check /dev/uinput permissions and that the uinput module is loaded",
    )?;
    let mut peer = UdpPeer::connect(bind, connect, token, 1)
        .await
        .context("failed to create the client UDP socket")?;
    let hello = TransportMessage::Hello(handshake("virtual", profile));
    let mut hello_buf = vec![0_u8; 64 * 1024];
    connect_until_hello_ack(&mut peer, &mut hello_buf, &hello, connect)
        .await
        .with_context(|| {
            format!(
                "could not connect to {connect}; start the Windows side with `remote-steer.exe server`, then check address/port, UDP firewall, and token"
            )
        })?;
    if let Some(path) = remember_defaults(remember)? {
        println!("saved defaults: {}", path.display());
    }
    println!("connected to server");
    println!("start the game and select the virtual Thrustmaster T150.");
    println!("stop: Ctrl+C\n");

    let mut buf = vec![0_u8; 64 * 1024];
    let mut ffb_tick = time::interval(Duration::from_millis(1));
    let mut hello_tick = time::interval(Duration::from_secs(1));
    let mut input_drop = InputStaleDrop::default();
    loop {
        tokio::select! {
            packet = peer.recv(&mut buf) => {
                let (addr, channel, message) = packet?;
                debug!(%addr, ?channel, ?message, "received packet");
                match message {
                    TransportMessage::Input(snapshot) => {
                        if input_drop.accept(&snapshot) {
                            backend.inject_input(snapshot)?;
                        }
                    }
                    TransportMessage::HelloAck(_) => {}
                    TransportMessage::FfbReply(reply) => {
                        debug!(
                            command_id = reply.command_id,
                            kind = ?reply.kind,
                            "received ffb reply"
                        );
                        backend.complete_ffb(reply)?
                    }
                    _ => {}
                }
            }
            _ = ffb_tick.tick() => {
                for _ in 0..MAX_FFB_COMMANDS_PER_TICK {
                    let Some(command) = backend.poll_ffb()? else {
                        break;
                    };
                    debug!(
                        command_id = command.command_id,
                        kind = ?command.kind,
                        "sending ffb command"
                    );
                    peer.send_to_remote(Channel::FfbControl, &TransportMessage::FfbCommand(command)).await?;
                }
            }
            _ = hello_tick.tick() => {
                peer.send_to_remote(Channel::Session, &hello).await?;
            }
        }
    }
}

#[derive(Debug)]
struct InputSendState {
    last_sent: Option<WheelStateSnapshot>,
    last_sent_at: Option<Instant>,
    resync_interval: Duration,
}

impl InputSendState {
    fn new(resync_interval: Duration) -> Self {
        Self {
            last_sent: None,
            last_sent_at: None,
            resync_interval,
        }
    }

    fn should_send(&mut self, snapshot: &WheelStateSnapshot, now: Instant) -> bool {
        let changed = self
            .last_sent
            .as_ref()
            .map(|last| last.axes != snapshot.axes || last.buttons != snapshot.buttons)
            .unwrap_or(true);
        let resync_due = self
            .last_sent_at
            .map(|last| now.duration_since(last) >= self.resync_interval)
            .unwrap_or(true);

        if changed || resync_due {
            self.last_sent = Some(snapshot.clone());
            self.last_sent_at = Some(now);
            return true;
        }

        false
    }
}

fn run_probe(target: ProbeTarget) -> Result<()> {
    match target {
        ProbeTarget::Physical => probe_physical(),
        ProbeTarget::Virtual => {
            let profile = profile_by_id(WheelProfileId::T150);
            println!("{}", serde_json::to_string_pretty(&profile)?);
            Ok(())
        }
    }
}

#[cfg(windows)]
fn open_physical_backend() -> Result<Box<dyn PhysicalWheelBackend>> {
    Ok(Box::new(
        remote_steer_backend_windows::WindowsPhysicalBackend::open_t150()?,
    ))
}

#[cfg(all(target_os = "linux", not(windows)))]
fn open_physical_backend() -> Result<Box<dyn PhysicalWheelBackend>> {
    Ok(Box::new(
        remote_steer_backend_linux::LinuxPhysicalBackend::open_default()?,
    ))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open_physical_backend() -> Result<Box<dyn PhysicalWheelBackend>> {
    bail!("server backend is not implemented on this OS")
}

#[cfg(target_os = "linux")]
fn open_virtual_backend() -> Result<Box<dyn VirtualWheelBackend>> {
    let backend = remote_steer_backend_linux::LinuxVirtualBackend::create_t150()?;
    let evdev_path = backend.evdev_path()?;
    println!("virtual wheel: {}", evdev_path.display());
    info!(path = %evdev_path.display(), "created virtual T150");
    Ok(Box::new(backend))
}

#[cfg(not(target_os = "linux"))]
fn open_virtual_backend() -> Result<Box<dyn VirtualWheelBackend>> {
    bail!("client virtual-wheel backend is only implemented on Linux")
}

#[cfg(target_os = "linux")]
fn probe_physical() -> Result<()> {
    let probe = remote_steer_backend_linux::probe_t150_event()?;
    println!("{}", serde_json::to_string_pretty(&format_probe(probe))?);
    Ok(())
}

#[cfg(windows)]
fn probe_physical() -> Result<()> {
    let backend = remote_steer_backend_windows::WindowsPhysicalBackend::open_t150()?;
    println!("{:#?}", backend.capabilities());
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn probe_physical() -> Result<()> {
    bail!("physical probe is not implemented on this OS")
}

async fn run_test_ffb(
    target: ProbeTarget,
    effect: Option<TestForce>,
    connect: Option<SocketAddr>,
    bind: SocketAddr,
    token: Option<String>,
    device: Option<PathBuf>,
) -> Result<()> {
    match target {
        ProbeTarget::Physical => {
            let mut backend = open_physical_backend()?;
            match effect {
                Some(force) => {
                    println!("playing: {}", force.display_name());
                    run_physical_test_force(backend.as_mut(), force)?;
                    println!("done");
                    Ok(())
                }
                None => run_interactive_test_forces(|force| {
                    run_physical_test_force(backend.as_mut(), force)
                }),
            }
        }
        ProbeTarget::Virtual => {
            if let Some(connect) = connect {
                let token = token.ok_or_else(|| {
                    anyhow::anyhow!(
                        "remote test requires a token; use `--token <shared-token>` or set REMOTE_STEER_TOKEN on both machines"
                    )
                })?;
                run_remote_test_ffb(bind, connect, token, effect, RememberDefaults::default()).await
            } else {
                run_virtual_test_ffb(effect, device)
            }
        }
    }
}

async fn run_remote_test_ffb(
    bind: SocketAddr,
    connect: SocketAddr,
    token: String,
    effect: Option<TestForce>,
    remember: RememberDefaults,
) -> Result<()> {
    let mut peer = UdpPeer::connect(bind, connect, token, 1)
        .await
        .context("failed to create the test UDP socket")?;
    let mut buf = vec![0_u8; 64 * 1024];
    let hello = TransportMessage::Hello(handshake("test-ffb", WheelProfileId::T150));
    connect_until_hello_ack(&mut peer, &mut buf, &hello, connect)
        .await
        .with_context(|| {
            format!(
                "could not connect to {connect}; start the Windows side with `remote-steer.exe server`, then check address/port, UDP firewall, and token"
            )
        })?;
    if let Some(path) = remember_defaults(remember)? {
        println!("saved defaults: {}", path.display());
    }
    println!("connected");

    match effect {
        Some(force) => {
            println!("playing: {}", force.display_name());
            run_remote_test_force(&mut peer, &mut buf, force).await?;
            println!("done");
            Ok(())
        }
        None => run_interactive_remote_test_forces(&mut peer, &mut buf).await,
    }
}

#[cfg(target_os = "linux")]
fn run_virtual_test_ffb(effect: Option<TestForce>, device: Option<PathBuf>) -> Result<()> {
    let event_path = match device {
        Some(path) => path,
        None => remote_steer_backend_linux::probe_t150_event()?
            .map(|probe| probe.path)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no T150 event device found; run `remote-steer client <server-ip> --token <shared-token>` first or pass --device /dev/input/eventXX"
                )
            })?,
    };

    match effect {
        Some(force) => {
            println!("playing: {}", force.display_name());
            run_virtual_test_force(&event_path, force)?;
            println!("done");
            Ok(())
        }
        None => run_interactive_test_forces(|force| run_virtual_test_force(&event_path, force)),
    }
}

#[cfg(not(target_os = "linux"))]
fn run_virtual_test_ffb(_effect: Option<TestForce>, _device: Option<PathBuf>) -> Result<()> {
    bail!("virtual FFB test command is only implemented on Linux")
}

impl TestForce {
    fn all() -> &'static [TestForce] {
        &[
            TestForce::Engine,
            TestForce::BlownTire,
            TestForce::Boing,
            TestForce::Explosion,
            TestForce::OpenSea,
            TestForce::TurboBoost,
            TestForce::Gong,
            TestForce::BumpyRoad,
            TestForce::CarCrash,
            TestForce::Punch,
            TestForce::ForceField,
            TestForce::Whiplash,
        ]
    }

    fn display_name(self) -> &'static str {
        match self {
            TestForce::Engine => "Engine",
            TestForce::BlownTire => "Blown Tire",
            TestForce::Boing => "Boing",
            TestForce::Explosion => "Explosion",
            TestForce::OpenSea => "Open Sea",
            TestForce::TurboBoost => "Turbo Boost",
            TestForce::Gong => "Gong",
            TestForce::BumpyRoad => "Bumpy Road",
            TestForce::CarCrash => "Car Crash",
            TestForce::Punch => "Punch",
            TestForce::ForceField => "Force Field",
            TestForce::Whiplash => "Whiplash",
        }
    }
}

#[derive(Debug, Clone)]
struct TestStep {
    effect: FfbEffect,
    duration_ms: u64,
}

fn run_interactive_test_forces(mut play: impl FnMut(TestForce) -> Result<()>) -> Result<()> {
    print_test_force_menu();

    loop {
        print!("effect> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(());
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "q" | "quit" | "exit") {
            return Ok(());
        }
        let selected = match input.parse::<i32>() {
            Ok(value) => value,
            Err(_) => {
                println!("invalid selection: enter 0-11, q, or -1");
                continue;
            }
        };
        if selected == -1 {
            return Ok(());
        }
        let Some(force) = TestForce::all().get(selected as usize).copied() else {
            println!("invalid selection: enter 0-11, q, or -1");
            continue;
        };

        println!("playing: {}", force.display_name());
        if let Err(err) = play(force) {
            println!("failed: {err}");
        } else {
            println!("done\n");
        }
    }
}

async fn run_interactive_remote_test_forces(peer: &mut UdpPeer, buf: &mut [u8]) -> Result<()> {
    print_test_force_menu();

    loop {
        print!("effect> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(());
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "q" | "quit" | "exit") {
            return Ok(());
        }
        let selected = match input.parse::<i32>() {
            Ok(value) => value,
            Err(_) => {
                println!("invalid selection: enter 0-11, q, or -1");
                continue;
            }
        };
        if selected == -1 {
            return Ok(());
        }
        let Some(force) = TestForce::all().get(selected as usize).copied() else {
            println!("invalid selection: enter 0-11, q, or -1");
            continue;
        };

        println!("playing: {}", force.display_name());
        if let Err(err) = run_remote_test_force(peer, buf, force).await {
            println!("failed: {err}");
        } else {
            println!("done\n");
        }
    }
}

fn print_test_force_menu() {
    println!("Force feedback test");
    println!("Hold the wheel firmly before selecting an effect.");
    println!("Type a number, q, or -1 to exit.\n");
    for (index, force) in TestForce::all().iter().enumerate() {
        println!("{index:>2}: {}", force.display_name());
    }
    println!();
}

fn run_physical_test_force(backend: &mut dyn PhysicalWheelBackend, force: TestForce) -> Result<()> {
    let mut command_id = 1;
    for step in test_force_steps(force, EffectId(1)) {
        let effect_id = step.effect.id;
        apply_test_command(
            backend,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Upload {
                    effect: step.effect,
                },
            },
        )?;
        command_id += 1;
        apply_test_command(
            backend,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Play {
                    effect_id,
                    repetitions: 1,
                },
            },
        )?;
        std::thread::sleep(Duration::from_millis(step.duration_ms));
        command_id += 1;
        apply_test_command(
            backend,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Stop { effect_id },
            },
        )?;
        command_id += 1;
        apply_test_command(
            backend,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Erase { effect_id },
            },
        )?;
        command_id += 1;
    }
    Ok(())
}

async fn run_remote_test_force(peer: &mut UdpPeer, buf: &mut [u8], force: TestForce) -> Result<()> {
    let mut command_id = 1;
    for step in test_force_steps(force, EffectId(1)) {
        let effect_id = step.effect.id;
        send_ffb_command(
            peer,
            buf,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Upload {
                    effect: step.effect,
                },
            },
        )
        .await?;
        command_id += 1;
        send_ffb_command(
            peer,
            buf,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Play {
                    effect_id,
                    repetitions: 1,
                },
            },
        )
        .await?;
        time::sleep(Duration::from_millis(step.duration_ms)).await;
        command_id += 1;
        send_ffb_command(
            peer,
            buf,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Stop { effect_id },
            },
        )
        .await?;
        command_id += 1;
        send_ffb_command(
            peer,
            buf,
            FfbCommand {
                command_id,
                kind: FfbCommandKind::Erase { effect_id },
            },
        )
        .await?;
        command_id += 1;
    }
    Ok(())
}

async fn send_ffb_command(peer: &mut UdpPeer, buf: &mut [u8], command: FfbCommand) -> Result<()> {
    let command_id = command.command_id;
    peer.send_to_remote(Channel::FfbControl, &TransportMessage::FfbCommand(command))
        .await?;
    wait_for_ffb_reply(peer, buf, command_id).await
}

async fn connect_until_hello_ack(
    peer: &mut UdpPeer,
    buf: &mut [u8],
    hello: &TransportMessage,
    connect: SocketAddr,
) -> Result<()> {
    let mut announced_wait = false;
    loop {
        peer.send_to_remote(Channel::Session, hello).await?;
        if wait_for_hello_ack_once(peer, buf).await? {
            return Ok(());
        }
        if !announced_wait {
            println!("waiting for server at {connect}; press Ctrl+C to stop");
            announced_wait = true;
        }
        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_hello_ack_once(peer: &mut UdpPeer, buf: &mut [u8]) -> Result<bool> {
    let deadline = Duration::from_secs(3);
    match time::timeout(deadline, async {
        loop {
            let (_, _, message) = peer.recv(buf).await?;
            if matches!(message, TransportMessage::HelloAck(_)) {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    {
        Ok(result) => {
            result?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

async fn wait_for_ffb_reply(peer: &mut UdpPeer, buf: &mut [u8], command_id: u64) -> Result<()> {
    let deadline = Duration::from_secs(3);
    time::timeout(deadline, async {
        loop {
            let (_, _, message) = peer.recv(buf).await?;
            if let TransportMessage::FfbReply(reply) = message {
                if reply.command_id != command_id {
                    continue;
                }
                return match reply.kind {
                    FfbReplyKind::Ack => Ok(()),
                    FfbReplyKind::Rejected { reason } => {
                        Err(anyhow::anyhow!("physical FFB command rejected: {reason}"))
                    }
                };
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for FFB reply {command_id}; check that the server window is still running and the token matches"
        )
    })?
}

#[cfg(target_os = "linux")]
fn run_virtual_test_force(event_path: &PathBuf, force: TestForce) -> Result<()> {
    for step in test_force_steps(force, EffectId(-1)) {
        remote_steer_backend_linux::play_ffb_test_effect(
            event_path,
            step.effect,
            Duration::from_millis(step.duration_ms),
        )?;
    }
    Ok(())
}

fn test_force_steps(force: TestForce, id: EffectId) -> Vec<TestStep> {
    match force {
        TestForce::Engine => vec![periodic_step(
            id,
            PeriodicWaveform::Sine,
            0x4000,
            35,
            9_000,
            1_600,
        )],
        TestForce::BlownTire => vec![
            constant_step(id, 0x4000, 26_000, 160),
            constant_step(id, 0xc000, 24_000, 160),
            periodic_step(id, PeriodicWaveform::SawDown, 0x4000, 70, 24_000, 380),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 55, 18_000, 500),
        ],
        TestForce::Boing => vec![
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 220, 26_000, 450),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 270, 18_000, 400),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 340, 10_000, 350),
        ],
        TestForce::Explosion => vec![
            constant_step(id, 0x4000, 31_000, 170),
            constant_step(id, 0xc000, 31_000, 170),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 45, 28_000, 650),
        ],
        TestForce::OpenSea => vec![periodic_step(
            id,
            PeriodicWaveform::Sine,
            0x4000,
            650,
            12_000,
            2_200,
        )],
        TestForce::TurboBoost => vec![
            periodic_step(id, PeriodicWaveform::SawUp, 0x4000, 90, 10_000, 350),
            periodic_step(id, PeriodicWaveform::SawUp, 0x4000, 60, 18_000, 450),
            periodic_step(id, PeriodicWaveform::SawUp, 0x4000, 35, 26_000, 550),
        ],
        TestForce::Gong => vec![periodic_step(
            id,
            PeriodicWaveform::Sine,
            0x4000,
            380,
            26_000,
            1_400,
        )],
        TestForce::BumpyRoad => vec![
            periodic_step(id, PeriodicWaveform::SawUp, 0x4000, 90, 18_000, 240),
            periodic_step(id, PeriodicWaveform::SawDown, 0x4000, 95, 18_000, 240),
            periodic_step(id, PeriodicWaveform::SawUp, 0x4000, 80, 22_000, 260),
            periodic_step(id, PeriodicWaveform::SawDown, 0x4000, 85, 22_000, 260),
        ],
        TestForce::CarCrash => vec![
            constant_step(id, 0x4000, 32_000, 240),
            constant_step(id, 0xc000, 32_000, 240),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 30, 28_000, 600),
        ],
        TestForce::Punch => vec![constant_step(id, 0xc000, 30_000, 240)],
        TestForce::ForceField => vec![
            condition_step(id, ConditionKind::Spring, 2_000),
            condition_step(id, ConditionKind::Damper, 900),
        ],
        TestForce::Whiplash => vec![
            constant_step(id, 0x4000, 28_000, 180),
            constant_step(id, 0xc000, 30_000, 300),
            periodic_step(id, PeriodicWaveform::Sine, 0x4000, 70, 16_000, 400),
        ],
    }
}

fn test_condition_axis() -> ConditionAxis {
    ConditionAxis {
        right_saturation: 0x7fff,
        left_saturation: 0x7fff,
        right_coefficient: 0x3000,
        left_coefficient: 0x3000,
        deadband: 0,
        center: 0,
    }
}

fn constant_step(id: EffectId, direction: u16, level: i16, duration_ms: u64) -> TestStep {
    step(
        id,
        direction,
        duration_ms,
        FfbEffectKind::Constant {
            level,
            envelope: FfbEnvelope::default(),
        },
    )
}

fn periodic_step(
    id: EffectId,
    waveform: PeriodicWaveform,
    direction: u16,
    period_ms: u16,
    magnitude: i16,
    duration_ms: u64,
) -> TestStep {
    step(
        id,
        direction,
        duration_ms,
        FfbEffectKind::Periodic {
            waveform,
            period_ms,
            magnitude,
            offset: 0,
            phase: 0,
            envelope: FfbEnvelope::default(),
        },
    )
}

fn condition_step(id: EffectId, condition: ConditionKind, duration_ms: u64) -> TestStep {
    step(
        id,
        0,
        duration_ms,
        FfbEffectKind::Condition {
            condition,
            axes: [test_condition_axis(), test_condition_axis()],
        },
    )
}

fn step(id: EffectId, direction: u16, duration_ms: u64, kind: FfbEffectKind) -> TestStep {
    TestStep {
        effect: FfbEffect {
            id,
            direction,
            trigger_button: 0,
            trigger_interval_ms: 0,
            replay: FfbReplay {
                length_ms: duration_ms.min(u16::MAX as u64) as u16,
                delay_ms: 0,
            },
            kind,
        },
        duration_ms,
    }
}

#[cfg(windows)]
fn run_dump_direct_input() -> Result<()> {
    print!("{}", remote_steer_backend_windows::dump_t150_directinput()?);
    Ok(())
}

#[cfg(not(windows))]
fn run_dump_direct_input() -> Result<()> {
    bail!("DirectInput dump is only available on Windows")
}

fn apply_test_command(backend: &mut dyn PhysicalWheelBackend, command: FfbCommand) -> Result<()> {
    let reply = backend.apply_ffb(command)?;
    match reply.kind {
        FfbReplyKind::Ack => Ok(()),
        FfbReplyKind::Rejected { reason } => bail!("physical FFB test rejected: {reason}"),
    }
}

fn handshake(peer_name: &str, profile: WheelProfileId) -> Handshake {
    let selected = profile_by_id(profile);
    Handshake {
        peer_name: peer_name.to_string(),
        profile,
        profile_hash: profile_hash(profile),
        max_effects: selected.ffb.max_effects,
    }
}

#[cfg(target_os = "linux")]
fn format_probe(probe: Option<remote_steer_backend_linux::LinuxEventProbe>) -> serde_json::Value {
    match probe {
        Some(probe) => serde_json::json!({
            "found": true,
            "path": probe.path,
            "name": probe.name,
            "bustype": format!("{:04x}", probe.bustype),
            "vendor": format!("{:04x}", probe.vendor),
            "product": format!("{:04x}", probe.product),
            "version": format!("{:04x}", probe.version),
            "capabilities": {
                "ev": probe.ev_bits,
                "key": probe.key_bits,
                "abs": probe.abs_bits,
                "ff": probe.ff_bits,
            }
        }),
        None => serde_json::json!({
            "found": false,
            "expected": {
                "vendor": "044f",
                "product": "b677"
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_steer_core::{AxisKind, AxisValue, ButtonValue};

    #[test]
    fn saved_server_is_used_when_client_argument_is_missing() {
        let config = SavedConfig {
            server: Some("wheel-host".to_string()),
            token: None,
        };

        let resolved = resolve_server_name(None, &config).unwrap();

        assert_eq!(resolved.value, "wheel-host");
        assert!(resolved.remember.is_none());
    }

    #[test]
    fn explicit_server_overrides_saved_server_and_is_remembered() {
        let config = SavedConfig {
            server: Some("old-host".to_string()),
            token: None,
        };

        let resolved = resolve_server_name(Some("new-host".to_string()), &config).unwrap();

        assert_eq!(resolved.value, "new-host");
        assert_eq!(resolved.remember.as_deref(), Some("new-host"));
    }

    #[test]
    fn saved_token_is_used_when_token_argument_is_missing() {
        let config = SavedConfig {
            server: None,
            token: Some("secret".to_string()),
        };

        let resolved = resolve_token(None, &config, "client").unwrap();

        assert_eq!(resolved.value, "secret");
        assert!(resolved.remember.is_none());
    }

    #[test]
    fn input_send_state_sends_first_change_and_resync_only() {
        let start = Instant::now();
        let mut state = InputSendState::new(Duration::from_millis(250));
        let snapshot = WheelStateSnapshot {
            seq: 1,
            timestamp_micros: 0,
            axes: vec![AxisValue {
                axis: AxisKind::Wheel,
                value: 10,
            }],
            buttons: vec![ButtonValue {
                linux_code: 0x120,
                pressed: false,
            }],
        };

        assert!(state.should_send(&snapshot, start));
        assert!(!state.should_send(&snapshot, start + Duration::from_millis(100)));
        assert!(state.should_send(&snapshot, start + Duration::from_millis(250)));
    }

    #[test]
    fn input_send_state_sends_changed_values_before_resync() {
        let start = Instant::now();
        let mut state = InputSendState::new(Duration::from_millis(250));
        let mut snapshot = WheelStateSnapshot {
            seq: 1,
            timestamp_micros: 0,
            axes: vec![AxisValue {
                axis: AxisKind::Wheel,
                value: 10,
            }],
            buttons: vec![ButtonValue {
                linux_code: 0x120,
                pressed: false,
            }],
        };

        assert!(state.should_send(&snapshot, start));
        snapshot.seq = 2;
        snapshot.timestamp_micros = 10;
        assert!(!state.should_send(&snapshot, start + Duration::from_millis(10)));
        snapshot.axes[0].value = 11;
        assert!(state.should_send(&snapshot, start + Duration::from_millis(20)));
    }
}
