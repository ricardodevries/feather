//! Isolated NVIDIA temperature and fan control through the driver-provided NVML library.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use nvml_wrapper::{Nvml, enum_wrappers::device::TemperatureSensor, error::NvmlError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use feather_core::{
    config::DeviceConfig,
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_REPLY_GRACE: Duration = Duration::from_secs(1);
const HELPER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SHUTDOWN_REQUEST_TIMEOUT: Duration = DEFAULT_REQUEST_TIMEOUT;
const NVIDIA_HELPER_ARGUMENT: &str = "--nvidia-helper";

/// Parent-side supervisor. Runtime requests use one helper process per GPU so
/// a wedged NVML ioctl for one device cannot block the other drivers or GPUs.
pub(super) struct NvidiaDriver {
    timeout: Duration,
    heartbeat: Option<Arc<AtomicU64>>,
    helpers: BTreeMap<String, HelperClient>,
}

impl NvidiaDriver {
    pub(super) fn new() -> Self {
        Self {
            timeout: DEFAULT_REQUEST_TIMEOUT,
            heartbeat: None,
            helpers: BTreeMap::new(),
        }
    }

    pub(super) fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub(super) fn set_heartbeat(&mut self, heartbeat: Arc<AtomicU64>) {
        self.heartbeat = Some(heartbeat);
    }

    pub(super) fn retain_configured(&mut self, configured: &BTreeMap<String, DeviceConfig>) {
        let retained = configured
            .values()
            .filter_map(|config| match config {
                DeviceConfig::NvidiaNvml { uuid } => Some(uuid.as_str()),
                DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::WireViewProIi { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        let removed = self
            .helpers
            .keys()
            .filter(|uuid| !retained.contains(uuid.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let helpers: Vec<_> = removed
            .into_iter()
            .filter_map(|uuid| self.helpers.remove(&uuid).map(|helper| (uuid, helper)))
            .collect();
        shutdown_helpers(helpers, self.timeout);
    }

    pub(super) fn discover_configured(
        &mut self,
        configured: &BTreeMap<String, DeviceConfig>,
        timeout: Duration,
    ) -> Result<Vec<DeviceDescriptor>> {
        if !configured
            .values()
            .any(|config| matches!(config, DeviceConfig::NvidiaNvml { .. }))
        {
            return Ok(Vec::new());
        }
        let all = self.discover_all_with_timeout(timeout)?;
        let mut found = Vec::new();
        for (alias, config) in configured {
            let DeviceConfig::NvidiaNvml { uuid } = config else {
                continue;
            };
            if let Some(device) = all.iter().find(|device| device.id == *uuid) {
                let mut device = device.clone();
                device.details.insert("alias".into(), alias.clone());
                found.push(device);
            }
        }
        Ok(found)
    }

    pub(super) fn discover_all(&mut self) -> Result<Vec<DeviceDescriptor>> {
        self.discover_all_with_timeout(self.timeout)
    }

    fn discover_all_with_timeout(&mut self, timeout: Duration) -> Result<Vec<DeviceDescriptor>> {
        let mut helper = HelperClient::spawn("discovery", self.heartbeat.clone())?;
        let response = helper.call(HelperRequest::DiscoverAll, timeout);
        helper.shutdown(timeout);
        match response? {
            HelperResult::Devices(devices) => Ok(devices),
            other => Err(unexpected_response("device discovery", &other)),
        }
    }

    pub(super) fn read_temperature(&mut self, alias: &str, config: &DeviceConfig) -> Result<f64> {
        let uuid = uuid(config)?.to_owned();
        let response = self.request_for(
            &uuid,
            HelperRequest::ReadTemperature {
                alias: alias.to_owned(),
                uuid: uuid.clone(),
            },
        )?;
        match response {
            HelperResult::Temperature(value) => Ok(value),
            other => Err(unexpected_response("temperature read", &other)),
        }
    }

    pub(super) fn set_fans(
        &mut self,
        alias: &str,
        config: &DeviceConfig,
        channels: &[u32],
        percent: u8,
    ) -> Result<Value> {
        let uuid = uuid(config)?.to_owned();
        let response = self.request_for(
            &uuid,
            HelperRequest::SetFans {
                alias: alias.to_owned(),
                uuid: uuid.clone(),
                channels: channels.to_vec(),
                percent,
            },
        )?;
        match response {
            HelperResult::Value(value) => Ok(value),
            other => Err(unexpected_response("fan write", &other)),
        }
    }

    pub(super) fn release(&mut self, alias: &str, config: &DeviceConfig) -> Result<()> {
        let uuid = uuid(config)?.to_owned();
        let response = self.request_for(
            &uuid,
            HelperRequest::Release {
                alias: alias.to_owned(),
                uuid: uuid.clone(),
            },
        )?;
        match response {
            HelperResult::Unit => Ok(()),
            other => Err(unexpected_response("fan release", &other)),
        }
    }

    pub(super) fn shutdown(&mut self) {
        shutdown_helpers(std::mem::take(&mut self.helpers), self.timeout);
    }

    fn request_for(&mut self, uuid: &str, request: HelperRequest) -> Result<HelperResult> {
        if !self.helpers.contains_key(uuid) {
            self.helpers.insert(
                uuid.to_owned(),
                HelperClient::spawn(uuid, self.heartbeat.clone())?,
            );
        }
        self.helpers
            .get_mut(uuid)
            .ok_or_else(|| FeatherError::Driver(format!("NVIDIA helper for {uuid} is missing")))?
            .call(request, self.timeout)
    }
}

fn shutdown_helpers(
    helpers: impl IntoIterator<Item = (String, HelperClient)>,
    request_timeout: Duration,
) {
    let request_timeout = request_timeout.min(MAX_SHUTDOWN_REQUEST_TIMEOUT);
    thread::scope(|scope| {
        for (uuid, mut helper) in helpers {
            scope.spawn(move || {
                if let Err(error) = helper.call(HelperRequest::Shutdown, request_timeout) {
                    tracing::error!(%error, gpu_uuid = uuid, "failed to stop NVIDIA helper");
                }
            });
        }
    });
}

impl Drop for NvidiaDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct HelperClient {
    label: String,
    heartbeat: Option<Arc<AtomicU64>>,
    sender: Option<SyncSender<ManagerRequest>>,
    quarantined: Option<String>,
}

impl HelperClient {
    fn spawn(label: &str, heartbeat: Option<Arc<AtomicU64>>) -> Result<Self> {
        let child = Command::new("/proc/self/exe")
            .arg(NVIDIA_HELPER_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                nvidia_error(format!("could not start NVIDIA helper for {label}"), error)
            })?;
        Self::supervise(label, child, heartbeat)
    }

    fn supervise(label: &str, mut child: Child, heartbeat: Option<Arc<AtomicU64>>) -> Result<Self> {
        let stdin = child.stdin.take().ok_or_else(|| {
            FeatherError::Driver(format!("NVIDIA helper for {label} has no standard input"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            FeatherError::Driver(format!("NVIDIA helper for {label} has no standard output"))
        })?;
        let (sender, requests) = mpsc::sync_channel(8);
        let manager_label = label.to_owned();
        thread::Builder::new()
            .name(format!("nvidia-{label}"))
            .spawn(move || run_manager(manager_label, child, stdin, stdout, requests))
            .map_err(|error| {
                nvidia_error(
                    format!("could not supervise NVIDIA helper for {label}"),
                    error,
                )
            })?;
        Ok(Self {
            label: label.to_owned(),
            heartbeat,
            sender: Some(sender),
            quarantined: None,
        })
    }

    fn call(&mut self, request: HelperRequest, timeout: Duration) -> Result<HelperResult> {
        if let Some(reason) = &self.quarantined {
            return Err(quarantined_error(&self.label, reason));
        }
        let shutdown = matches!(request, HelperRequest::Shutdown);
        let (reply, response) = mpsc::sync_channel(1);
        let Some(sender) = &self.sender else {
            return Err(quarantined_error(&self.label, "helper is not running"));
        };
        if sender
            .send(ManagerRequest {
                request,
                timeout,
                reply,
            })
            .is_err()
        {
            return self.quarantine("supervisor stopped before accepting the request".into());
        }
        let wait = timeout.saturating_add(HELPER_REPLY_GRACE);
        let result = match self.receive_with_heartbeat(&response, wait) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return self.quarantine(format!("request exceeded {timeout:?}"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return self.quarantine("supervisor stopped before replying".into());
            }
        };
        if shutdown {
            self.sender = None;
        }
        match result {
            Ok(HelperResponse::Success { result }) => Ok(result),
            Ok(HelperResponse::Error {
                message,
                terminal: true,
            }) => self.quarantine(message),
            Ok(HelperResponse::Error {
                message,
                terminal: false,
            }) => Err(FeatherError::Driver(message)),
            Err(message) => self.quarantine(message),
        }
    }

    fn receive_with_heartbeat(
        &self,
        response: &Receiver<std::result::Result<HelperResponse, String>>,
        wait: Duration,
    ) -> std::result::Result<std::result::Result<HelperResponse, String>, mpsc::RecvTimeoutError>
    {
        let started = Instant::now();
        loop {
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            match response.recv_timeout(remaining.min(HELPER_HEARTBEAT_INTERVAL)) {
                Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() < wait => {
                    if let Some(heartbeat) = &self.heartbeat {
                        heartbeat.fetch_add(1, Ordering::Relaxed);
                    }
                }
                result => return result,
            }
        }
    }

    fn shutdown(&mut self, timeout: Duration) {
        if self.quarantined.is_none() && self.sender.is_some() {
            let _ = self.call(HelperRequest::Shutdown, timeout);
        }
    }

    fn quarantine<T>(&mut self, reason: String) -> Result<T> {
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(ManagerRequest::stop());
        }
        tracing::error!(helper = self.label, %reason, "NVIDIA helper quarantined");
        self.quarantined = Some(reason.clone());
        Err(quarantined_error(&self.label, &reason))
    }
}

struct ManagerRequest {
    request: HelperRequest,
    timeout: Duration,
    reply: SyncSender<std::result::Result<HelperResponse, String>>,
}

impl ManagerRequest {
    fn stop() -> Self {
        let (reply, _response) = mpsc::sync_channel(1);
        Self {
            request: HelperRequest::Stop,
            timeout: Duration::ZERO,
            reply,
        }
    }
}

fn run_manager(
    label: String,
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: impl std::io::Read + Send + 'static,
    requests: Receiver<ManagerRequest>,
) {
    let (lines, responses) = mpsc::sync_channel(1);
    let reader_label = label.clone();
    let reader = thread::Builder::new()
        .name(format!("nvidia-{label}-reader"))
        .spawn(move || read_helper_responses(reader_label, stdout, lines));
    if let Err(error) = reader {
        tracing::error!(helper = label, %error, "could not start NVIDIA response reader");
        let _ = child.kill();
        return;
    }

    while let Ok(message) = requests.recv() {
        if matches!(message.request, HelperRequest::Stop) {
            let _ = child.kill();
            break;
        }
        let shutdown = matches!(message.request, HelperRequest::Shutdown);
        let result = write_helper_request(&mut stdin, &message.request).and_then(|()| {
            match responses.recv_timeout(message.timeout) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                    "NVIDIA helper for {label} timed out after {:?}",
                    message.timeout
                )),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(format!("NVIDIA helper for {label} closed its output"))
                }
            }
        });
        let transport_failed = result.is_err();
        let _ = message.reply.send(result);
        if transport_failed {
            let _ = child.kill();
            break;
        }
        if shutdown {
            break;
        }
    }
    drop(stdin);
    if let Err(error) = child.wait() {
        tracing::debug!(helper = label, %error, "could not reap NVIDIA helper");
    }
}

fn write_helper_request(
    stdin: &mut ChildStdin,
    request: &HelperRequest,
) -> std::result::Result<(), String> {
    serde_json::to_writer(&mut *stdin, request)
        .map_err(|error| format!("could not encode NVIDIA helper request: {error}"))?;
    stdin
        .write_all(b"\n")
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("could not write NVIDIA helper request: {error}"))
}

fn read_helper_responses(
    label: String,
    stdout: impl std::io::Read,
    sender: SyncSender<std::result::Result<HelperResponse, String>>,
) {
    for line in BufReader::new(stdout).lines() {
        let response = match line {
            Ok(line) => serde_json::from_str(&line)
                .map_err(|error| format!("could not decode NVIDIA helper response: {error}")),
            Err(error) => Err(format!("could not read NVIDIA helper response: {error}")),
        };
        if sender.send(response).is_err() {
            return;
        }
    }
    let _ = sender.send(Err(format!("NVIDIA helper for {label} exited")));
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum HelperRequest {
    DiscoverAll,
    ReadTemperature {
        alias: String,
        uuid: String,
    },
    SetFans {
        alias: String,
        uuid: String,
        channels: Vec<u32>,
        percent: u8,
    },
    Release {
        alias: String,
        uuid: String,
    },
    Shutdown,
    Stop,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum HelperResponse {
    Success { result: HelperResult },
    Error { message: String, terminal: bool },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum HelperResult {
    Devices(Vec<DeviceDescriptor>),
    Temperature(f64),
    Value(Value),
    Unit,
}

/// Runs the private newline-delimited JSON helper protocol on stdin/stdout.
pub(crate) fn run_helper() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut driver = LocalNvidiaDriver::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let request = match serde_json::from_str::<HelperRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_helper_response(
                    &mut stdout,
                    &HelperResponse::Error {
                        message: format!("invalid NVIDIA helper request: {error}"),
                        terminal: false,
                    },
                )?;
                continue;
            }
        };
        if matches!(request, HelperRequest::Stop) {
            break;
        }
        let shutdown = matches!(request, HelperRequest::Shutdown);
        let response = match execute_helper_request(&mut driver, request) {
            Ok(result) => HelperResponse::Success { result },
            Err(error) => HelperResponse::Error {
                message: error.message,
                terminal: error.terminal,
            },
        };
        write_helper_response(&mut stdout, &response)?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

fn execute_helper_request(
    driver: &mut LocalNvidiaDriver,
    request: HelperRequest,
) -> LocalResult<HelperResult> {
    match request {
        HelperRequest::DiscoverAll => driver.discover_all().map(HelperResult::Devices),
        HelperRequest::ReadTemperature { alias, uuid } => driver
            .read_temperature(&alias, &uuid)
            .map(HelperResult::Temperature),
        HelperRequest::SetFans {
            alias,
            uuid,
            channels,
            percent,
        } => driver
            .set_fans(&alias, &uuid, &channels, percent)
            .map(HelperResult::Value),
        HelperRequest::Release { alias, uuid } => {
            driver.release(&alias, &uuid).map(|()| HelperResult::Unit)
        }
        HelperRequest::Shutdown => {
            driver.shutdown();
            Ok(HelperResult::Unit)
        }
        HelperRequest::Stop => Ok(HelperResult::Unit),
    }
}

fn write_helper_response(stdout: &mut impl Write, response: &HelperResponse) -> Result<()> {
    serde_json::to_writer(&mut *stdout, response)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

#[derive(Clone)]
struct FanCapabilities {
    count: u32,
    minimum: u32,
    maximum: u32,
}

#[derive(Debug)]
struct LocalError {
    message: String,
    terminal: bool,
}

impl LocalError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            terminal: false,
        }
    }

    fn nvml(context: impl AsRef<str>, error: NvmlError) -> Self {
        Self {
            message: format!("{}: {error}", context.as_ref()),
            terminal: matches!(error, NvmlError::GpuLost | NvmlError::ResetRequired),
        }
    }
}

impl std::fmt::Display for LocalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

type LocalResult<T> = std::result::Result<T, LocalError>;

/// Helper-side NVML owner. It never shares a process with the main daemon's
/// Corsair, hwmon, or WireView control paths.
struct LocalNvidiaDriver {
    nvml: Option<Nvml>,
    controlled: BTreeSet<String>,
    fan_capabilities: BTreeMap<String, FanCapabilities>,
}

impl LocalNvidiaDriver {
    fn new() -> Self {
        Self {
            nvml: None,
            controlled: BTreeSet::new(),
            fan_capabilities: BTreeMap::new(),
        }
    }

    fn discover_all(&mut self) -> LocalResult<Vec<DeviceDescriptor>> {
        let nvml = self.nvml()?;
        let count = nvml
            .device_count()
            .map_err(|error| LocalError::nvml("could not count GPUs", error))?;
        let mut found = Vec::new();
        for index in 0..count {
            let device = nvml
                .device_by_index(index)
                .map_err(|error| LocalError::nvml(format!("could not open GPU {index}"), error))?;
            let uuid = device.uuid().map_err(|error| {
                LocalError::nvml(format!("could not read GPU {index} UUID"), error)
            })?;
            let name = device
                .name()
                .unwrap_or_else(|_| format!("NVIDIA GPU {index}"));
            let fan_count = device.num_fans().unwrap_or(0);
            let mut details = BTreeMap::new();
            details.insert("index".into(), index.to_string());
            details.insert("uuid".into(), uuid.clone());
            details.insert("fan_count".into(), fan_count.to_string());
            if let Ok((minimum, maximum)) = device.min_max_fan_speed() {
                details.insert("fan_range".into(), format!("{minimum}-{maximum}"));
            }
            if let Ok(temperature) = device.temperature(TemperatureSensor::Gpu) {
                details.insert("temperature_c".into(), temperature.to_string());
            }
            for fan in 0..fan_count {
                if let Ok(speed) = device.fan_speed(fan) {
                    details.insert(format!("fan{fan}_percent"), speed.to_string());
                }
                if let Ok(policy) = device.fan_control_policy(fan) {
                    details.insert(format!("fan{fan}_policy"), format!("{policy:?}"));
                }
            }
            found.push(DeviceDescriptor {
                id: uuid,
                driver: "nvidia-nvml".into(),
                name,
                details,
            });
        }
        Ok(found)
    }

    fn read_temperature(&mut self, alias: &str, uuid: &str) -> LocalResult<f64> {
        let device = self
            .nvml()?
            .device_by_uuid(uuid)
            .map_err(|error| LocalError::nvml(format!("device '{alias}' is unavailable"), error))?;
        device
            .temperature(TemperatureSensor::Gpu)
            .map(f64::from)
            .map_err(|error| {
                LocalError::nvml(format!("could not read '{alias}' temperature"), error)
            })
    }

    fn set_fans(
        &mut self,
        alias: &str,
        uuid: &str,
        channels: &[u32],
        percent: u8,
    ) -> LocalResult<Value> {
        self.controlled.insert(uuid.to_owned());
        let capabilities = self.fan_capabilities(alias, uuid)?;
        let targets = selected_channels(channels, capabilities.count, alias)
            .map_err(|error| LocalError::message(error.to_string()))?;
        let target = u32::from(percent).clamp(capabilities.minimum, capabilities.maximum);
        let mut device = self
            .nvml()?
            .device_by_uuid(uuid)
            .map_err(|error| LocalError::nvml(format!("device '{alias}' is unavailable"), error))?;
        for fan in &targets {
            device.set_fan_speed(*fan, target).map_err(|error| {
                LocalError::nvml(format!("could not set '{alias}' fan {fan}"), error)
            })?;
        }
        let speeds = targets
            .iter()
            .map(|fan| (fan.to_string(), target))
            .collect::<BTreeMap<_, _>>();
        Ok(json!({
            "percent": target,
            "fans": speeds,
            "range": [capabilities.minimum, capabilities.maximum]
        }))
    }

    fn release(&mut self, alias: &str, uuid: &str) -> LocalResult<()> {
        let capabilities = self.fan_capabilities(alias, uuid)?;
        let mut device = self
            .nvml()?
            .device_by_uuid(uuid)
            .map_err(|error| LocalError::nvml(format!("device '{alias}' is unavailable"), error))?;
        let mut failures = Vec::new();
        for fan in 0..capabilities.count {
            if let Err(error) = device.set_default_fan_speed(fan) {
                failures.push(LocalError::nvml(format!("fan {fan}"), error));
            }
        }
        if failures.is_empty() {
            self.controlled.remove(uuid);
            Ok(())
        } else {
            let terminal = failures.iter().any(|error| error.terminal);
            let failures = failures
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join(", ");
            Err(LocalError {
                message: format!("could not restore '{alias}' automatic fan policy: {failures}"),
                terminal,
            })
        }
    }

    fn shutdown(&mut self) {
        let uuids = self.controlled.iter().cloned().collect::<Vec<_>>();
        for uuid in uuids {
            if let Err(error) = self.release(&uuid, &uuid) {
                tracing::error!(%error, gpu_uuid = uuid, "failed to restore NVIDIA fan policy");
            }
        }
        self.fan_capabilities.clear();
        self.nvml = None;
    }

    fn fan_capabilities(&mut self, alias: &str, uuid: &str) -> LocalResult<FanCapabilities> {
        if let Some(capabilities) = self.fan_capabilities.get(uuid) {
            return Ok(capabilities.clone());
        }
        let device = self
            .nvml()?
            .device_by_uuid(uuid)
            .map_err(|error| LocalError::nvml(format!("device '{alias}' is unavailable"), error))?;
        let count = device.num_fans().map_err(|error| {
            LocalError::nvml(format!("could not count fans for '{alias}'"), error)
        })?;
        let (minimum, maximum) = device.min_max_fan_speed().unwrap_or((0, 100));
        let capabilities = FanCapabilities {
            count,
            minimum,
            maximum,
        };
        self.fan_capabilities
            .insert(uuid.to_owned(), capabilities.clone());
        Ok(capabilities)
    }

    fn nvml(&mut self) -> LocalResult<&Nvml> {
        if self.nvml.is_none() {
            self.nvml = Some(Nvml::init().map_err(|error| {
                LocalError::nvml("could not load or initialize libnvidia-ml.so.1", error)
            })?);
        }
        self.nvml.as_ref().ok_or_else(|| {
            LocalError::message("NVML initialization completed without a library handle")
        })
    }
}

impl Drop for LocalNvidiaDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn uuid(config: &DeviceConfig) -> Result<&str> {
    match config {
        DeviceConfig::NvidiaNvml { uuid } => Ok(uuid),
        DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::WireViewProIi { .. } => Err(
            FeatherError::Driver("configured device is not an NVIDIA GPU".into()),
        ),
    }
}

fn selected_channels(channels: &[u32], fan_count: u32, alias: &str) -> Result<Vec<u32>> {
    if fan_count == 0 {
        return Err(FeatherError::Driver(format!(
            "device '{alias}' reports no controllable fans"
        )));
    }
    let channels = if channels.is_empty() {
        (0..fan_count).collect::<Vec<_>>()
    } else {
        channels.to_vec()
    };
    if let Some(channel) = channels.iter().find(|channel| **channel >= fan_count) {
        return Err(FeatherError::Driver(format!(
            "fan {channel} does not exist on '{alias}'"
        )));
    }
    Ok(channels)
}

fn unexpected_response(operation: &str, response: &HelperResult) -> FeatherError {
    FeatherError::Driver(format!(
        "NVIDIA helper returned an unexpected response for {operation}: {response:?}"
    ))
}

fn quarantined_error(label: &str, reason: &str) -> FeatherError {
    FeatherError::Driver(format!(
        "NVIDIA helper for {label} is quarantined: {reason}; reset the GPU and restart featherd"
    ))
}

fn nvidia_error(context: impl AsRef<str>, error: impl std::fmt::Display) -> FeatherError {
    FeatherError::Driver(format!("{}: {error}", context.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_terminal_driver_errors_structurally() {
        assert!(LocalError::nvml("read failed", NvmlError::ResetRequired).terminal);
        assert!(LocalError::nvml("read failed", NvmlError::GpuLost).terminal);
        assert!(!LocalError::nvml("alias says reset required", NvmlError::Timeout).terminal);
    }

    #[test]
    fn helper_protocol_round_trips() -> anyhow::Result<()> {
        let request = HelperRequest::SetFans {
            alias: "gpu0".into(),
            uuid: "GPU-test".into(),
            channels: vec![0, 1],
            percent: 42,
        };
        let encoded = serde_json::to_string(&request)?;
        let decoded: HelperRequest = serde_json::from_str(&encoded)?;
        assert!(matches!(
            decoded,
            HelperRequest::SetFans { percent: 42, .. }
        ));
        Ok(())
    }

    #[test]
    fn a_timed_out_helper_is_killed_quarantined_and_isolated() -> anyhow::Result<()> {
        let stalled_child = test_helper("exec sleep 60")?;
        let stalled_pid = stalled_child.id();
        let heartbeat = Arc::new(AtomicU64::new(0));
        let mut stalled =
            HelperClient::supervise("stalled", stalled_child, Some(Arc::clone(&heartbeat)))?;
        let mut responsive = HelperClient::supervise(
            "responsive",
            test_helper(
                "while IFS= read -r line; do printf '%s\\n' '{\"status\":\"success\",\"result\":{\"type\":\"unit\"}}'; done",
            )?,
            None,
        )?;
        let timeout = Duration::from_millis(1_200);

        thread::scope(|scope| -> anyhow::Result<()> {
            let stalled_call = scope.spawn(|| stalled.call(HelperRequest::DiscoverAll, timeout));
            thread::sleep(Duration::from_millis(50));

            let responsive_started = std::time::Instant::now();
            assert!(matches!(
                responsive.call(HelperRequest::DiscoverAll, timeout)?,
                HelperResult::Unit
            ));
            assert!(responsive_started.elapsed() < Duration::from_millis(250));

            let stalled_result = stalled_call
                .join()
                .map_err(|_| anyhow::anyhow!("stalled helper test thread panicked"))?;
            let Err(error) = stalled_result else {
                anyhow::bail!("non-replying helper unexpectedly replied");
            };
            assert!(error.to_string().contains("quarantined"));
            Ok(())
        })?;
        assert!(heartbeat.load(Ordering::Relaxed) > 0);

        let retry_started = std::time::Instant::now();
        let Err(retry_error) = stalled.call(HelperRequest::DiscoverAll, timeout) else {
            anyhow::bail!("quarantined helper accepted another request");
        };
        assert!(retry_error.to_string().contains("quarantined"));
        assert!(retry_started.elapsed() < Duration::from_millis(100));

        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{stalled_pid}")).exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!std::path::Path::new(&format!("/proc/{stalled_pid}")).exists());
        responsive.shutdown(timeout);
        Ok(())
    }

    fn test_helper(script: &str) -> std::io::Result<Child> {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }
}
