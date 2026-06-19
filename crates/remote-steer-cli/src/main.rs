use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use remote_steer_core::{
    profile_by_id, ConditionAxis, ConditionKind, EffectId, FfbCommand, FfbCommandKind, FfbEffect,
    FfbEffectKind, FfbEnvelope, FfbReplay, FfbReplyKind, PeriodicWaveform, PhysicalWheelBackend,
    VirtualWheelBackend, WheelProfileId,
};
use remote_steer_transport::{profile_hash, Channel, Handshake, TransportMessage, UdpPeer};
use tokio::net::lookup_host;
use tokio::time;
use tracing::{debug, info};

const DEFAULT_PORT: u16 = 43150;
const DEFAULT_LISTEN: &str = "0.0.0.0:43150";
const DEFAULT_BIND: &str = "0.0.0.0:0";

#[derive(Debug, Parser)]
#[command(name = "remote-steer")]
#[command(version)]
#[command(about = "Remote steering wheel bridge")]
#[command(arg_required_else_help = true)]
#[command(after_help = "\
Quick start:
  Windows server:
    remote-steer server --token <shared-token>

  Linux client bridge:
    remote-steer client <server-ip> --token <shared-token>

  Direct force-feedback test:
    remote-steer test <server-ip> --token <shared-token>

Notes:
  The default port is 43150.
  The same token must be used on both machines.
  Keep the server command running while the client or test command is connected.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the machine that has the real wheel attached.
    #[command(visible_alias = "serve")]
    Server {
        /// Address to listen on.
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
        /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
        token: String,
        /// Wheel profile to expose.
        #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
        profile: ProfileArg,
    },
    /// Run the machine where the game will see a virtual T150.
    #[command(visible_alias = "connect")]
    Client {
        /// Server address. A missing port uses 43150.
        server: String,
        /// Local UDP bind address.
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Shared token. Can also be set with REMOTE_STEER_TOKEN.
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
        token: String,
        /// Wheel profile to create.
        #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
        profile: ProfileArg,
    },
    /// Play the 12 Thrustmaster Test Forces presets.
    Test {
        /// Server address for remote testing. Omit it to test the local physical wheel.
        server: Option<String>,
        /// Play one effect and exit.
        #[arg(long, value_enum)]
        effect: Option<TestForce>,
        /// Local UDP bind address for remote testing.
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Shared token for remote testing. Can also be set with REMOTE_STEER_TOKEN.
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
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
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
        token: String,
        #[arg(long, value_enum, default_value_t = ProfileArg::T150)]
        profile: ProfileArg,
    },
    #[command(hide = true)]
    Virtual {
        #[arg(long)]
        connect: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: SocketAddr,
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
        token: String,
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
        #[arg(long, env = "REMOTE_STEER_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        device: Option<PathBuf>,
    },
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

impl From<ProfileArg> for WheelProfileId {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::T150 => WheelProfileId::T150,
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
        Command::Server {
            listen,
            token,
            profile,
        } => run_physical(listen, token, profile.into()).await,
        Command::Client {
            server,
            bind,
            token,
            profile,
        } => {
            let connect = resolve_server(&server).await?;
            run_virtual(bind, connect, token, profile.into()).await
        }
        Command::Test {
            server,
            effect,
            bind,
            token,
        } => run_easy_test(effect, server, bind, token).await,
        Command::Probe { target } => run_probe(target),
        Command::DumpDirectInput => run_dump_direct_input(),
        Command::Physical {
            listen,
            token,
            profile,
        } => run_physical(listen, token, profile.into()).await,
        Command::Virtual {
            connect,
            bind,
            token,
            profile,
        } => run_virtual(bind, connect, token, profile.into()).await,
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

async fn run_easy_test(
    effect: Option<TestForce>,
    server: Option<String>,
    bind: SocketAddr,
    token: Option<String>,
) -> Result<()> {
    if let Some(server) = server {
        let connect = resolve_server(&server).await?;
        let token = token.ok_or_else(|| {
            anyhow::anyhow!(
                "remote test requires a token; use `--token <shared-token>` or set REMOTE_STEER_TOKEN on both machines"
            )
        })?;
        println!("remote-steer test");
        println!("server: {connect}");
        run_remote_test_ffb(bind, connect, token, effect).await
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

async fn run_physical(listen: SocketAddr, token: String, profile: WheelProfileId) -> Result<()> {
    let selected = profile_by_id(profile);
    println!("remote-steer server");
    println!("listening: {listen}");
    println!("profile: {}", selected.id.as_str());
    println!("next: remote-steer client <this-machine-ip> --token <shared-token>");
    println!("stop: Ctrl+C\n");
    info!(%listen, profile = selected.id.as_str(), "starting server");
    let mut backend =
        open_physical_backend().context("failed to open the physical wheel backend")?;
    let mut peer = UdpPeer::bind(listen, token, 1)
        .await
        .context("failed to bind the server UDP socket")?;
    let mut buf = vec![0_u8; 64 * 1024];
    let mut input_tick = time::interval(Duration::from_millis(4));
    let mut session_ready = false;

    loop {
        tokio::select! {
            packet = peer.recv(&mut buf) => {
                let (addr, channel, message) = packet?;
                debug!(%addr, ?channel, ?message, "received packet");
                match message {
                    TransportMessage::Hello(_) => {
                        let handshake = handshake("physical", profile);
                        peer.send_to_remote(Channel::Session, &TransportMessage::HelloAck(handshake)).await?;
                        println!("connected: {addr}");
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
                        peer.send_to_remote(Channel::Input, &TransportMessage::Input(snapshot)).await?;
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
    peer.send_to_remote(
        Channel::Session,
        &TransportMessage::Hello(handshake("virtual", profile)),
    )
    .await?;
    let mut hello_buf = vec![0_u8; 64 * 1024];
    wait_for_hello_ack(&mut peer, &mut hello_buf)
        .await
        .with_context(|| {
            format!(
                "could not connect to {connect}; start the Windows side with `remote-steer.exe server --token <shared-token>`, then check address/port, UDP firewall, and token"
            )
        })?;
    println!("connected to server");
    println!("start the game and select the virtual Thrustmaster T150.");
    println!("stop: Ctrl+C\n");

    let mut buf = vec![0_u8; 64 * 1024];
    let mut ffb_tick = time::interval(Duration::from_millis(1));
    loop {
        tokio::select! {
            packet = peer.recv(&mut buf) => {
                let (addr, channel, message) = packet?;
                debug!(%addr, ?channel, ?message, "received packet");
                match message {
                    TransportMessage::Input(snapshot) => backend.inject_input(snapshot)?,
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
                while let Some(command) = backend.poll_ffb()? {
                    debug!(
                        command_id = command.command_id,
                        kind = ?command.kind,
                        "sending ffb command"
                    );
                    peer.send_to_remote(Channel::FfbControl, &TransportMessage::FfbCommand(command)).await?;
                }
            }
        }
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
                run_remote_test_ffb(bind, connect, token, effect).await
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
) -> Result<()> {
    let mut peer = UdpPeer::connect(bind, connect, token, 1)
        .await
        .context("failed to create the test UDP socket")?;
    let mut buf = vec![0_u8; 64 * 1024];
    peer.send_to_remote(
        Channel::Session,
        &TransportMessage::Hello(handshake("test-ffb", WheelProfileId::T150)),
    )
    .await?;
    wait_for_hello_ack(&mut peer, &mut buf)
        .await
        .with_context(|| {
            format!(
                "could not connect to {connect}; start the Windows side with `remote-steer.exe server --token <shared-token>`, then check address/port, UDP firewall, and token"
            )
        })?;
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

async fn wait_for_hello_ack(peer: &mut UdpPeer, buf: &mut [u8]) -> Result<()> {
    let deadline = Duration::from_secs(3);
    time::timeout(deadline, async {
        loop {
            let (_, _, message) = peer.recv(buf).await?;
            if matches!(message, TransportMessage::HelloAck(_)) {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for the server handshake; check that `remote-steer server --token <shared-token>` is running, the address/port is correct, the firewall allows UDP 43150, and the token matches"
        )
    })?
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
