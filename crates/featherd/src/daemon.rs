//! `featherd` socket, command dispatch, systemd notifications, and shutdown handling.

use std::fs;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::TrySendError;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use tempfile::NamedTempFile;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, oneshot};
use tokio::time::MissedTickBehavior;
use tokio::time::timeout;

use feather_core::config::Config;
use feather_core::error::{FeatherError, Result};
use feather_core::ipc::{Command, ErrorCode, MAX_MESSAGE_BYTES, Request, Response};
use feather_core::types::IPC_PROTOCOL_VERSION;

use crate::drivers::{Hardware, SystemHardware};
use crate::policy::Engine;

struct DaemonState {
    engine: Engine,
    config_path: PathBuf,
    socket_path: PathBuf,
    state_path: PathBuf,
    persisted_profile: Option<String>,
}

const ENGINE_QUEUE_CAPACITY: usize = 64;
const MAX_CONNECTIONS: usize = 32;
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedState {
    schema_version: u32,
    active_profile: String,
}

enum EngineMessage {
    Command {
        command: Command,
        reply: oneshot::Sender<Result<Value>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
struct EngineClient {
    sender: SyncSender<EngineMessage>,
}

impl EngineClient {
    async fn request(&self, command: Command) -> Result<Value> {
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(EngineMessage::Command { command, reply })
            .map_err(engine_send_error)?;
        response
            .await
            .map_err(|_| FeatherError::Daemon("hardware worker stopped before replying".into()))?
    }

    async fn shutdown(&self) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(EngineMessage::Shutdown { reply })
            .map_err(engine_send_error)?;
        response
            .await
            .map_err(|_| FeatherError::Daemon("hardware worker stopped during shutdown".into()))
    }
}

struct EngineWorker {
    client: EngineClient,
    ready: oneshot::Receiver<Result<()>>,
    stopped: oneshot::Receiver<Result<()>>,
    heartbeat: Arc<AtomicU64>,
    thread: JoinHandle<()>,
}

/// Runs the foreground daemon until it receives a termination signal or a fatal driver error.
///
/// The optional socket path overrides the path in the configuration for this process.
///
/// # Errors
///
/// Returns an error when configuration validation, socket setup, hardware
/// initialization, request serving, or shutdown fails.
pub async fn run(config_path: PathBuf, socket_override: Option<PathBuf>) -> Result<()> {
    run_with_hardware(
        config_path,
        socket_override,
        Box::new(SystemHardware::new()),
        std::future::pending(),
    )
    .await
}

async fn run_with_hardware<F>(
    config_path: PathBuf,
    socket_override: Option<PathBuf>,
    hardware: Box<dyn Hardware>,
    external_shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send,
{
    let mut config = Config::from_path(&config_path)?;
    if let Some(socket) = socket_override {
        config.daemon.socket = socket.display().to_string();
    }
    let socket_path = PathBuf::from(&config.daemon.socket);
    let state_path = PathBuf::from(&config.daemon.state_file);
    let persisted_profile = match load_persisted_profile(&state_path) {
        Ok(Some(profile)) if config.profiles.contains_key(&profile) => Some(profile),
        Ok(Some(profile)) => {
            tracing::warn!(
                profile,
                state = %state_path.display(),
                "saved profile is not present in the configuration"
            );
            if let Err(error) = clear_persisted_profile(&state_path) {
                tracing::warn!(
                    %error,
                    state = %state_path.display(),
                    "could not remove the invalid saved profile"
                );
            }
            None
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, state = %state_path.display(), "could not load saved profile");
            None
        }
    };
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path).map_err(|error| {
        FeatherError::Daemon(format!("could not bind {}: {error}", socket_path.display()))
    })?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o660)).map_err(|error| {
        FeatherError::Daemon(format!(
            "could not set permissions on {}: {error}",
            socket_path.display()
        ))
    })?;
    let socket_guard = SocketGuard::new(socket_path.clone());

    let EngineWorker {
        client,
        ready,
        mut stopped,
        heartbeat,
        thread,
    } = spawn_engine(
        config,
        config_path,
        socket_path.clone(),
        state_path,
        persisted_profile,
        hardware,
    )?;
    let ready = ready.await.map_err(|_| {
        FeatherError::Daemon("hardware worker stopped during initialization".into())
    })?;
    if let Err(error) = ready {
        drop(client);
        let _ = join_engine_thread(thread).await;
        return Err(error);
    }

    notify_ready();
    tracing::info!(socket = %socket_path.display(), "daemon ready");

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_heartbeat = None;

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| {
        FeatherError::Daemon(format!("could not install SIGTERM handler: {error}"))
    })?;
    tokio::pin!(external_shutdown);
    let connection_permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    let actor_result = loop {
        tokio::select! {
            _ = interval.tick() => {
                let current = heartbeat.load(Ordering::Relaxed);
                if last_heartbeat != Some(current) {
                    notify_watchdog();
                    last_heartbeat = Some(current);
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _address)) => {
                        match Arc::clone(&connection_permits).try_acquire_owned() {
                            Ok(permit) => {
                                let client = client.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = serve_connection(stream, client).await {
                                        tracing::warn!(%error, "client request failed");
                                    }
                                });
                            }
                            Err(_) => {
                                tracing::warn!(limit = MAX_CONNECTIONS, "client connection limit reached");
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "socket accept failed"),
                }
            }
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "Ctrl+C handler failed");
                }
                break None;
            }
            _ = terminate.recv() => break None,
            _ = &mut external_shutdown => break None,
            result = &mut stopped => {
                let result = result.unwrap_or_else(|_| {
                    Err(FeatherError::Daemon("hardware worker stopped unexpectedly".into()))
                });
                break Some(result);
            }
        }
    };

    notify_stopping();
    let shutdown_result = if actor_result.is_none() {
        client.shutdown().await
    } else {
        Ok(())
    };
    drop(client);
    let join_result = join_engine_thread(thread).await;
    drop(socket_guard);
    if actor_result.is_none() && shutdown_result.is_ok() && join_result.is_ok() {
        tracing::info!("daemon stopped and hardware policies restored");
    }
    if let Some(result) = actor_result {
        result?;
    }
    shutdown_result?;
    join_result?;
    Ok(())
}

fn spawn_engine(
    config: Config,
    config_path: PathBuf,
    socket_path: PathBuf,
    state_path: PathBuf,
    persisted_profile: Option<String>,
    hardware: Box<dyn Hardware>,
) -> Result<EngineWorker> {
    let (sender, receiver) = mpsc::sync_channel(ENGINE_QUEUE_CAPACITY);
    let (ready_tx, ready) = oneshot::channel();
    let (stopped_tx, stopped) = oneshot::channel();
    let heartbeat = Arc::new(AtomicU64::new(0));
    let worker_heartbeat = Arc::clone(&heartbeat);
    let thread = thread::Builder::new()
        .name("featherd-hardware".into())
        .spawn(move || {
            let initial_profile = persisted_profile.clone();
            let engine = match Engine::new(config, hardware, initial_profile) {
                Ok(engine) => engine,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                let mut engine = engine;
                engine.shutdown();
                return;
            }
            let state = DaemonState {
                engine,
                config_path,
                socket_path,
                state_path,
                persisted_profile,
            };
            let result = run_engine(state, receiver, &worker_heartbeat);
            let _ = stopped_tx.send(result);
        })
        .map_err(|error| {
            FeatherError::Daemon(format!("could not start hardware worker: {error}"))
        })?;
    Ok(EngineWorker {
        client: EngineClient { sender },
        ready,
        stopped,
        heartbeat,
        thread,
    })
}

fn run_engine(
    mut state: DaemonState,
    receiver: Receiver<EngineMessage>,
    heartbeat: &AtomicU64,
) -> Result<()> {
    let mut tick_interval = state.engine.tick_interval();
    let mut next_tick = Instant::now();
    loop {
        heartbeat.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        if now >= next_tick {
            if state.engine.tick(now, Local::now()) {
                state.engine.shutdown();
                return Err(FeatherError::Daemon(
                    "hardware failure limit reached".into(),
                ));
            }
            next_tick = now + tick_interval;
        }

        match receiver.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(EngineMessage::Command { command, reply }) => {
                let result = handle_command(command, &mut state);
                tick_interval = state.engine.tick_interval();
                next_tick = next_tick.min(Instant::now() + tick_interval);
                let _ = reply.send(result);
            }
            Ok(EngineMessage::Shutdown { reply }) => {
                state.engine.shutdown();
                let _ = reply.send(());
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.engine.shutdown();
                return Ok(());
            }
        }
    }
}

fn engine_send_error(error: TrySendError<EngineMessage>) -> FeatherError {
    match error {
        TrySendError::Full(_) => FeatherError::Daemon("hardware command queue is full".into()),
        TrySendError::Disconnected(_) => {
            FeatherError::Daemon("hardware worker is not running".into())
        }
    }
}

async fn join_engine_thread(thread: JoinHandle<()>) -> Result<()> {
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|error| FeatherError::Daemon(format!("could not join hardware worker: {error}")))?
        .map_err(|_| FeatherError::Daemon("hardware worker panicked".into()))
}

async fn serve_connection(stream: UnixStream, client: EngineClient) -> Result<()> {
    serve_connection_with_timeout(stream, client, SERVER_IO_TIMEOUT).await
}

async fn serve_connection_with_timeout(
    stream: UnixStream,
    client: EngineClient,
    io_timeout: Duration,
) -> Result<()> {
    timeout(io_timeout, serve_connection_inner(stream, client))
        .await
        .map_err(|_| FeatherError::Daemon("client request timed out".into()))?
}

async fn serve_connection_inner(stream: UnixStream, client: EngineClient) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader).take((MAX_MESSAGE_BYTES + 1) as u64);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response = if line.len() > MAX_MESSAGE_BYTES {
        Response::failure(
            String::new(),
            ErrorCode::InvalidRequest,
            "request is too large",
        )
    } else {
        match serde_json::from_str::<Request>(&line) {
            Ok(request) => dispatch(request, &client).await,
            Err(error) => Response::failure(
                String::new(),
                ErrorCode::InvalidRequest,
                format!("invalid request: {error}"),
            ),
        }
    };
    let mut payload = serde_json::to_vec(&response)?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    writer.shutdown().await?;
    Ok(())
}

async fn dispatch(request: Request, client: &EngineClient) -> Response {
    let request_id = request.request_id;
    if request.protocol_version != IPC_PROTOCOL_VERSION {
        return Response::failure(
            request_id,
            ErrorCode::IncompatibleProtocol,
            format!(
                "client uses protocol {}, daemon uses {IPC_PROTOCOL_VERSION}",
                request.protocol_version
            ),
        );
    }

    let result = client.request(request.command).await;
    match result {
        Ok(data) => Response::success(request_id, data),
        Err(error) => {
            let (code, message) = match error {
                FeatherError::Config(message) => (ErrorCode::InvalidConfig, message),
                FeatherError::Driver(message) => (ErrorCode::HardwareFailure, message),
                FeatherError::Daemon(message) if message.starts_with("unknown") => {
                    (ErrorCode::UnknownResource, message)
                }
                FeatherError::Daemon(message) => (ErrorCode::Internal, message),
                FeatherError::Protocol(message) => (ErrorCode::InvalidRequest, message),
                FeatherError::Io(error) => (ErrorCode::Internal, error.to_string()),
                FeatherError::Json(error) => (ErrorCode::Internal, error.to_string()),
            };
            Response::failure(request_id, code, message)
        }
    }
}

fn handle_command(command: Command, state: &mut DaemonState) -> Result<Value> {
    match command {
        Command::Status => {
            serde_json::to_value(state.engine.status(Instant::now())).map_err(FeatherError::from)
        }
        Command::Devices => {
            serde_json::to_value(state.engine.devices()?).map_err(FeatherError::from)
        }
        Command::ConfigReload => {
            let mut config = Config::from_path(&state.config_path)?;
            config.daemon.socket = state.socket_path.display().to_string();
            config.daemon.state_file = state.state_path.display().to_string();
            state.engine.reload(config)?;
            let profiles = state.engine.profiles();
            if state
                .persisted_profile
                .as_ref()
                .is_some_and(|profile| !profiles.contains(profile))
            {
                if let Err(error) = clear_persisted_profile(&state.state_path) {
                    tracing::warn!(
                        %error,
                        state = %state.state_path.display(),
                        "could not remove a saved profile that no longer exists"
                    );
                }
                state.persisted_profile = None;
            }
            notify_reloaded();
            let status = state.engine.status(Instant::now());
            Ok(json!({
                "generation": status.config_generation,
                "active_profile": status.active_profile,
            }))
        }
        Command::ProfileList => Ok(json!({
            "profiles": state.engine.profiles(),
            "active_profile": state.engine.active_profile(),
            "persisted_profile": state.persisted_profile,
        })),
        Command::ProfileSet { profile, persist } => {
            state.engine.set_profile(&profile)?;
            if persist {
                if let Err(error) = persist_profile(&state.state_path, &profile) {
                    return Err(FeatherError::Daemon(format!(
                        "profile '{profile}' is active for this run, but could not be saved: {error}"
                    )));
                }
                state.persisted_profile = Some(profile.clone());
            }
            Ok(json!({
                "active_profile": profile,
                "persisted": persist,
                "persisted_profile": state.persisted_profile,
            }))
        }
        Command::OverrideFan {
            output,
            percent,
            duration_ms,
            allow_below_minimum,
        } => {
            state.engine.set_fan_override(
                &output,
                percent,
                Duration::from_millis(duration_ms),
                allow_below_minimum,
            )?;
            Ok(json!({ "output": output, "expires_in_ms": duration_ms }))
        }
        Command::OverrideRgb {
            output,
            rgb,
            duration_ms,
        } => {
            state
                .engine
                .set_rgb_override(&output, rgb, Duration::from_millis(duration_ms))?;
            Ok(json!({ "output": output, "expires_in_ms": duration_ms }))
        }
        Command::OverrideDisplay {
            output,
            enabled,
            duration_ms,
        } => {
            state.engine.set_display_override(
                &output,
                enabled,
                Duration::from_millis(duration_ms),
            )?;
            Ok(json!({
                "output": output,
                "enabled": enabled,
                "expires_in_ms": duration_ms,
            }))
        }
        Command::OverrideClear { output } => {
            state.engine.clear_overrides(output.as_deref())?;
            Ok(json!({ "cleared": output.unwrap_or_else(|| "all".into()) }))
        }
        Command::OutputRelease { output } => {
            let released = state.engine.release_outputs(output.as_deref())?;
            Ok(json!({ "released": released }))
        }
        Command::OutputResume { output } => {
            let resumed = state.engine.resume_outputs(output.as_deref())?;
            Ok(json!({ "resumed": resumed }))
        }
        Command::DebugHidDump { device } => {
            let dump = state.engine.debug_hid_dump(&device)?;
            Ok(json!({ "device": device, "dump": dump }))
        }
    }
}

fn load_persisted_profile(path: &Path) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(FeatherError::Daemon(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    if bytes.len() > MAX_STATE_BYTES {
        return Err(FeatherError::Daemon(format!(
            "state file {} exceeds {MAX_STATE_BYTES} bytes",
            path.display()
        )));
    }
    let state: PersistedState = serde_json::from_slice(&bytes).map_err(|error| {
        FeatherError::Daemon(format!("could not parse {}: {error}", path.display()))
    })?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(FeatherError::Daemon(format!(
            "state file {} uses schema {}, expected {STATE_SCHEMA_VERSION}",
            path.display(),
            state.schema_version
        )));
    }
    if state.active_profile.is_empty() {
        return Err(FeatherError::Daemon(format!(
            "state file {} has an empty profile name",
            path.display()
        )));
    }
    Ok(Some(state.active_profile))
}

fn persist_profile(path: &Path, profile: &str) -> Result<()> {
    let parent = state_parent(path);
    fs::create_dir_all(parent).map_err(|error| {
        FeatherError::Daemon(format!("could not create {}: {error}", parent.display()))
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        FeatherError::Daemon(format!(
            "could not create a temporary state file in {}: {error}",
            parent.display()
        ))
    })?;
    serde_json::to_writer(
        &mut temporary,
        &PersistedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_profile: profile.to_owned(),
        },
    )?;
    temporary.write_all(b"\n").map_err(|error| {
        FeatherError::Daemon(format!("could not write {}: {error}", path.display()))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        FeatherError::Daemon(format!("could not sync {}: {error}", path.display()))
    })?;
    temporary.persist(path).map_err(|error| {
        FeatherError::Daemon(format!(
            "could not replace {}: {}",
            path.display(),
            error.error
        ))
    })?;
    sync_directory(parent)
}

fn clear_persisted_profile(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(state_parent(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FeatherError::Daemon(format!(
            "could not remove {}: {error}",
            path.display()
        ))),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            FeatherError::Daemon(format!("could not sync {}: {error}", path.display()))
        })
}

fn state_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn prepare_socket_path(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        FeatherError::Daemon(format!("socket path {} has no parent", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        FeatherError::Daemon(format!("could not create {}: {error}", parent.display()))
    })?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match StdUnixStream::connect(path) {
                Ok(_) => {
                    return Err(FeatherError::Daemon(format!(
                        "socket {} is already in use",
                        path.display()
                    )));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) => {}
                Err(error) => {
                    return Err(FeatherError::Daemon(format!(
                        "could not verify existing socket {}: {error}",
                        path.display()
                    )));
                }
            }
            fs::remove_file(path).map_err(|error| {
                FeatherError::Daemon(format!(
                    "could not remove stale socket {}: {error}",
                    path.display()
                ))
            })
        }
        Ok(_) => Err(FeatherError::Daemon(format!(
            "refusing to replace non-socket path {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FeatherError::Daemon(format!(
            "could not inspect {}: {error}",
            path.display()
        ))),
    }
}

struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, socket = %self.path.display(), "could not remove daemon socket");
        }
    }
}

#[cfg(target_os = "linux")]
fn notify_ready() {
    if let Err(error) = sd_notify::notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Controlling configured thermal outputs"),
    ]) {
        tracing::debug!(%error, "systemd readiness notification failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn notify_ready() {}

#[cfg(target_os = "linux")]
fn notify_watchdog() {
    if sd_notify::watchdog_enabled().is_some()
        && let Err(error) = sd_notify::notify(&[sd_notify::NotifyState::Watchdog])
    {
        tracing::debug!(%error, "systemd watchdog notification failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn notify_watchdog() {}

#[cfg(target_os = "linux")]
fn notify_reloaded() {
    if let Err(error) = sd_notify::notify(&[
        sd_notify::NotifyState::Ready,
        sd_notify::NotifyState::Status("Configuration reloaded"),
    ]) {
        tracing::debug!(%error, "systemd reload notification failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn notify_reloaded() {}

#[cfg(target_os = "linux")]
fn notify_stopping() {
    if let Err(error) = sd_notify::notify(&[sd_notify::NotifyState::Stopping]) {
        tracing::debug!(%error, "systemd stopping notification failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn notify_stopping() {}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use feather_core::config::{OutputConfig, SensorConfig};
    use feather_core::ipc::send_request;
    use feather_core::types::{DaemonStatus, DeviceDescriptor};

    use crate::drivers::FanWrite;

    use super::*;

    #[derive(Default)]
    struct FakeState {
        fan_writes: Vec<u8>,
        shutdowns: usize,
    }

    struct FakeHardware {
        state: Arc<Mutex<FakeState>>,
    }

    impl Hardware for FakeHardware {
        fn preflight(&mut self, _config: &Config) -> Result<Vec<DeviceDescriptor>> {
            Ok(Vec::new())
        }

        fn commit_config(&mut self, _config: &Config) {}

        fn discover(&mut self) -> Result<Vec<DeviceDescriptor>> {
            Ok(Vec::new())
        }

        fn read_sensor(&mut self, _alias: &str, _config: &SensorConfig) -> Result<f64> {
            Ok(50.0)
        }

        fn set_fan(
            &mut self,
            _alias: &str,
            _config: &OutputConfig,
            percent: u8,
        ) -> Result<FanWrite> {
            lock_fake_state(&self.state).fan_writes.push(percent);
            Ok(FanWrite {
                observed: json!({ "percent": percent }),
                warning: None,
            })
        }

        fn set_rgb(&mut self, _alias: &str, _config: &OutputConfig, rgb: [u8; 3]) -> Result<Value> {
            Ok(json!({ "rgb": rgb }))
        }

        fn set_display(
            &mut self,
            _alias: &str,
            _config: &OutputConfig,
            target: crate::drivers::DisplayTarget,
        ) -> Result<Value> {
            Ok(json!({ "target": format!("{target:?}") }))
        }

        fn release(&mut self, _alias: &str, _config: &OutputConfig) -> Result<()> {
            Ok(())
        }

        fn debug_hid_dump(&mut self, _device_alias: &str) -> Result<String> {
            Ok("fake HID data".into())
        }

        fn shutdown(&mut self) {
            lock_fake_state(&self.state).shutdowns += 1;
        }
    }

    fn lock_fake_state(state: &Mutex<FakeState>) -> MutexGuard<'_, FakeState> {
        match state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn integration_config(socket: &Path, state: &Path) -> String {
        format!(
            r#"
schema_version = 1
default_profile = "balanced"

[daemon]
poll_interval = "20ms"
stale_after = "1s"
socket = "{}"
state_file = "{}"
failure_limit = 3

[devices.hub]
driver = "corsair-icue-link"
unique_id = "TEST"
interface = 0

[sensors.cpu]
driver = "linux-hwmon"
device = "test-device"
chip = "test-chip"
label = "test-label"

[curves.main]
points = [{{ temp = 40, percent = 40 }}, {{ temp = 60, percent = 60 }}]

[outputs.case]
kind = "fan"
device = "hub"
channels = [0]
minimum_percent = 30
fail_safe_percent = 100
hysteresis_percent = 1
refresh_interval = "20ms"

[profiles.balanced.outputs.case]
sources = ["cpu"]
curve = "main"

[profiles.quiet.outputs.case]
sources = ["cpu"]
curve = "main"
"#,
            socket.display(),
            state.display()
        )
    }

    async fn wait_for_socket(path: &Path) -> anyhow::Result<()> {
        for _ in 0..100 {
            if path.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::bail!("daemon socket {} was not created", path.display());
    }

    #[test]
    fn removes_a_stale_socket() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stale.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        drop(listener);

        prepare_socket_path(&path)?;

        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn refuses_to_unlink_a_live_socket() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path)?;

        let Err(error) = prepare_socket_path(&path) else {
            anyhow::bail!("live socket was accepted as stale");
        };

        assert!(error.to_string().contains("already in use"));
        assert!(path.exists());
        Ok(())
    }

    #[test]
    fn refuses_to_replace_a_regular_file() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("regular-file");
        fs::write(&path, "keep")?;

        let Err(error) = prepare_socket_path(&path) else {
            anyhow::bail!("regular file was accepted as a socket");
        };

        assert!(error.to_string().contains("refusing to replace"));
        assert_eq!(fs::read_to_string(path)?, "keep");
        Ok(())
    }

    #[test]
    fn persisted_profile_uses_an_atomic_state_file() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state.json");

        persist_profile(&path, "quiet")?;

        assert_eq!(load_persisted_profile(&path)?, Some("quiet".into()));
        clear_persisted_profile(&path)?;
        assert_eq!(load_persisted_profile(&path)?, None);
        Ok(())
    }

    #[tokio::test]
    async fn idle_clients_are_disconnected_after_the_server_timeout() -> anyhow::Result<()> {
        let (server, _peer) = UnixStream::pair()?;
        let (sender, _receiver) = mpsc::sync_channel(1);
        let client = EngineClient { sender };

        let Err(error) =
            serve_connection_with_timeout(server, client, Duration::from_millis(10)).await
        else {
            anyhow::bail!("idle client did not time out");
        };

        assert!(error.to_string().contains("timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn daemon_socket_round_trip_persists_the_selected_profile() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("feather.sock");
        let state_path = directory.path().join("state.json");
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, integration_config(&socket, &state_path))?;
        let hardware_state = Arc::new(Mutex::new(FakeState::default()));
        let hardware = FakeHardware {
            state: Arc::clone(&hardware_state),
        };
        let (stop, stopped) = oneshot::channel();
        let daemon = tokio::spawn(run_with_hardware(
            config_path.clone(),
            None,
            Box::new(hardware),
            async move {
                let _ = stopped.await;
            },
        ));
        let exercised = async {
            wait_for_socket(&socket).await?;
            let data = send_request(&socket, Command::Status).await?;
            let status: DaemonStatus = serde_json::from_value(data)?;
            assert_eq!(status.active_profile, "balanced");

            let profiles = send_request(&socket, Command::ProfileList).await?;
            assert_eq!(profiles["active_profile"], "balanced");
            assert_eq!(profiles["persisted_profile"], Value::Null);

            send_request(
                &socket,
                Command::ProfileSet {
                    profile: "quiet".into(),
                    persist: true,
                },
            )
            .await?;

            let Err(error) = send_request(
                &socket,
                Command::OverrideFan {
                    output: "case".into(),
                    percent: 20,
                    duration_ms: 1_000,
                    allow_below_minimum: false,
                },
            )
            .await
            else {
                anyhow::bail!("unsafe fan override was accepted");
            };
            assert!(error.to_string().contains("--allow-below-minimum"));

            send_request(
                &socket,
                Command::OverrideFan {
                    output: "case".into(),
                    percent: 20,
                    duration_ms: 1_000,
                    allow_below_minimum: true,
                },
            )
            .await?;
            anyhow::Result::<()>::Ok(())
        }
        .await;
        let _ = stop.send(());
        let daemon_result = daemon.await?;
        exercised?;
        daemon_result?;
        assert_eq!(load_persisted_profile(&state_path)?, Some("quiet".into()));
        assert_eq!(lock_fake_state(&hardware_state).shutdowns, 1);
        assert!(!socket.exists());

        let restart_state = Arc::new(Mutex::new(FakeState::default()));
        let restart_hardware = FakeHardware {
            state: Arc::clone(&restart_state),
        };
        let (stop, stopped) = oneshot::channel();
        let daemon = tokio::spawn(run_with_hardware(
            config_path,
            None,
            Box::new(restart_hardware),
            async move {
                let _ = stopped.await;
            },
        ));
        let exercised = async {
            wait_for_socket(&socket).await?;
            let data = send_request(&socket, Command::Status).await?;
            let status: DaemonStatus = serde_json::from_value(data)?;
            assert_eq!(status.active_profile, "quiet");
            anyhow::Result::<()>::Ok(())
        }
        .await;
        let _ = stop.send(());
        let daemon_result = daemon.await?;
        exercised?;
        daemon_result?;
        assert_eq!(lock_fake_state(&restart_state).shutdowns, 1);
        Ok(())
    }
}
