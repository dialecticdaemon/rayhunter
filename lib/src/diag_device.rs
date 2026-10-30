use crate::diag::{
    CRC_CCITT, DataType, DiagParsingError, HdlcEncapsulatedMessage, LogConfigRequest,
    LogConfigResponse, Message, MessagesContainer, Request, RequestContainer, ResponsePayload,
    build_log_mask_request,
};
use crate::hdlc::hdlc_encapsulate;
use crate::{Device, log_codes};

use deku::prelude::*;
use futures::TryStream;
use log::{debug, error, info, warn};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::time::Duration;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::sleep;
use tokio_serial::SerialPortBuilderExt;

pub type DiagResult<T> = Result<T, DiagDeviceError>;

#[derive(Error, Debug)]
pub enum DiagDeviceError {
    #[error("Failed to initialize /dev/diag: {0}")]
    InitializationFailed(String),
    #[error("Failed to read diag device: {0}")]
    DeviceReadFailed(std::io::Error),
    #[error("Failed to write diag device: {0}")]
    DeviceWriteFailed(std::io::Error),
    #[error("Nonzero status code {0} for diag request: {1:?}")]
    RequestFailed(u32, Request),
    #[error("Didn't receive response for request: {0:?}")]
    NoResponse(Request),
    #[error("Failed to open QMDL file: {0}")]
    OpenQmdlFileError(std::io::Error),
    #[error("Failed to write to QMDL file: {0}")]
    QmdlFileWriteError(std::io::Error),
    #[error("Failed to open diag device: {0}")]
    OpenDiagDeviceError(std::io::Error),
    #[error("Failed to parse MessagesContainer: {0}")]
    ParseMessagesContainerError(deku::DekuError),
    #[error("Failed to open serial diag port: {0}")]
    OpenSerialError(String),
}

pub const LOG_CODES_FOR_RAW_PACKET_LOGGING: [u32; 17] = [
    // Layer 2:
    log_codes::LOG_GPRS_MAC_SIGNALLING_MESSAGE_C,
    // Layer 3:
    log_codes::LOG_GSM_RR_SIGNALING_MESSAGE_C,
    log_codes::WCDMA_SIGNALLING_MESSAGE,
    log_codes::LOG_LTE_RRC_OTA_MSG_LOG_C,
    log_codes::LOG_NR_RRC_OTA_MSG_LOG_C,
    // NAS:
    log_codes::LOG_UMTS_NAS_OTA_MESSAGE_LOG_PACKET_C,
    log_codes::LOG_LTE_NAS_ESM_OTA_IN_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_ESM_OTA_OUT_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_EMM_OTA_IN_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_EMM_OTA_OUT_MSG_LOG_C,
    // User IP traffic:
    log_codes::LOG_DATA_PROTOCOL_LOGGING_C,
    // LTE physical layer serving cell measurements: RSRP, RSRQ, RSSI
    log_codes::LOG_LTE_ML1_SERVING_CELL_MEAS_AND_EVAL_C,
    log_codes::LOG_LTE_ML1_NEIGHBOR_MEAS,
    // LTE MAC Random Access Channel response: contains Timing Advance
    log_codes::LOG_LTE_MAC_RACH_RESPONSE_C,
    log_codes::LOG_LTE_MAC_DL,
    log_codes::LOG_LTE_MAC_UL,
    0xb114, // maybe timing advance related?
];

/// The RRC and NAS messages the analyzers use
pub const LOG_CODES_SIGNALLING_ONLY: [u32; 10] = [
    log_codes::LOG_GPRS_MAC_SIGNALLING_MESSAGE_C,
    log_codes::LOG_GSM_RR_SIGNALING_MESSAGE_C,
    log_codes::WCDMA_SIGNALLING_MESSAGE,
    log_codes::LOG_LTE_RRC_OTA_MSG_LOG_C,
    log_codes::LOG_NR_RRC_OTA_MSG_LOG_C,
    log_codes::LOG_UMTS_NAS_OTA_MESSAGE_LOG_PACKET_C,
    log_codes::LOG_LTE_NAS_ESM_OTA_IN_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_ESM_OTA_OUT_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_EMM_OTA_IN_MSG_LOG_C,
    log_codes::LOG_LTE_NAS_EMM_OTA_OUT_MSG_LOG_C,
];

const BUFFER_LEN: usize = 1024 * 1024 * 10;
const MEMORY_DEVICE_MODE: u32 = 2;

#[cfg(target_env = "musl")]
const DIAG_IOCTL_REMOTE_DEV: i32 = 32;
#[cfg(all(not(target_env = "musl"), target_arch = "arm"))]
const DIAG_IOCTL_REMOTE_DEV: u32 = 32;
#[cfg(all(not(target_env = "musl"), target_arch = "x86_64"))]
const DIAG_IOCTL_REMOTE_DEV: u64 = 32;
#[cfg(all(not(target_env = "musl"), target_arch = "aarch64"))]
const DIAG_IOCTL_REMOTE_DEV: u64 = 32;

#[cfg(target_env = "musl")]
const DIAG_IOCTL_SWITCH_LOGGING: i32 = 7;
#[cfg(all(not(target_env = "musl"), target_arch = "arm"))]
const DIAG_IOCTL_SWITCH_LOGGING: u32 = 7;
#[cfg(all(not(target_env = "musl"), target_arch = "x86_64"))]
const DIAG_IOCTL_SWITCH_LOGGING: u64 = 7;
#[cfg(all(not(target_env = "musl"), target_arch = "aarch64"))]
const DIAG_IOCTL_SWITCH_LOGGING: u64 = 7;

pub struct DiagDevice {
    transport: Transport,
    read_buf: Vec<u8>,
    log_codes: &'static [u32],
}

enum Transport {
    /// Qualcomm /dev/diag character device (on-device installs)
    DevDiag { file: File, use_mdm: i32 },
    /// HDLC-framed diag over a USB serial port (see [`DiagDevice::new_serial`])
    Serial(SerialDiag),
}

struct SerialDiag {
    /// Either a device path or `usb:VID:PID:INTERFACE`, re-resolved after every reconnect
    spec: String,
    path: String,
    port: Box<dyn DiagPort>,
    pending: Vec<u8>,
    /// HDLC-encapsulated SetMask requests, replayed after the port reappears
    mask_requests: Vec<Vec<u8>>,
    /// When the last frame was handed on, for the heartbeat
    last_kept: std::time::Instant,
}

const HDLC_TERMINATOR: u8 = 0x7e;
const HEARTBEAT_AFTER: Duration = Duration::from_secs(60);
/// Short, because nothing is logged until the masks are set again
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
/// A hung modem can stop draining the port without its tty going away
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for buffered bytes that contain no complete diag frame
const MAX_PENDING: usize = 1024 * 1024;

/// `usb:VID:PID:INTERFACE` is looked up via sysfs: ttyUSB numbers change when the modem re-enumerates.
fn resolve_serial_spec(spec: &str) -> Option<String> {
    let Some(rest) = spec.strip_prefix("usb:") else {
        return Some(spec.to_string());
    };
    let rest = rest.to_lowercase();
    let parts: Vec<&str> = rest.split(':').collect();
    let [vid, pid, iface] = parts[..] else {
        return None;
    };
    for entry in std::fs::read_dir("/sys/class/tty").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let dev = entry.path().join("device");
        let read = |rel: &str| {
            std::fs::read_to_string(dev.join(rel))
                .ok()
                .map(|s| s.trim().to_lowercase())
        };
        if read("../bInterfaceNumber").as_deref() == Some(iface)
            && read("../../idVendor").as_deref() == Some(vid)
            && read("../../idProduct").as_deref() == Some(pid)
        {
            return Some(format!("/dev/{name}"));
        }
    }
    None
}

trait DiagPort: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DiagPort for T {}

/// A modem still logging with an earlier session's masks returns buffered old messages on the
/// first read. Discard input until the port has been quiet for [`STALE_QUIET`].
const STALE_QUIET: Duration = Duration::from_millis(500);
const STALE_MAX: Duration = Duration::from_secs(10);

async fn drain_stale_logs(port: &mut Box<dyn DiagPort>) {
    let mut buf = vec![0; 64 * 1024];
    let mut discarded = 0;
    let deadline = tokio::time::Instant::now() + STALE_MAX;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(STALE_QUIET, port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => discarded += n,
            _ => break,
        }
    }
    if discarded > 0 {
        info!("discarded {discarded} bytes of diag output buffered before startup");
    }
}

/// Writes the frames; false if a write fails or takes longer than `limit`.
async fn write_frames(port: &mut Box<dyn DiagPort>, frames: &[Vec<u8>], limit: Duration) -> bool {
    for frame in frames {
        match tokio::time::timeout(limit, port.write_all(frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("failed to write to serial diag port: {e}");
                return false;
            }
            Err(_) => {
                error!("writing to serial diag port timed out after {limit:?}");
                return false;
            }
        }
    }
    true
}

async fn open_serial(spec: &str) -> DiagResult<(String, Box<dyn DiagPort>)> {
    let path = resolve_serial_spec(spec)
        .ok_or_else(|| DiagDeviceError::OpenSerialError(format!("no tty found for {spec}")))?;
    let port = tokio_serial::new(&path, 115200)
        .open_native_async()
        .map_err(|e| DiagDeviceError::OpenSerialError(format!("{path}: {e}")))?;
    Ok((path, Box::new(port)))
}

impl SerialDiag {
    async fn reconnect(&mut self) {
        // close the old tty first, or the modem re-enumerates under a new ttyUSB number
        self.port = Box::new(tokio::io::duplex(1).0);
        loop {
            sleep(RECONNECT_INTERVAL).await;
            match open_serial(&self.spec).await {
                Ok((path, port)) => {
                    info!("serial diag port reappeared as {path}, replaying log masks");
                    self.path = path;
                    self.port = port;
                    self.pending.clear();
                    if write_frames(&mut self.port, &self.mask_requests, WRITE_TIMEOUT).await {
                        return;
                    }
                    self.port = Box::new(tokio::io::duplex(1).0);
                }
                Err(e) => debug!("serial diag port not available yet: {e}"),
            }
        }
    }

    /// Resends the log masks after [`HEARTBEAT_AFTER`] of silence: the answers keep the web UI from
    /// reporting missing messages, and a modem that lost its masks logs again. False if writing fails.
    async fn heartbeat(&mut self) -> bool {
        let frames = if self.mask_requests.is_empty() {
            let req = Request::LogConfig(LogConfigRequest::RetrieveIdRanges)
                .to_bytes()
                .expect("Failed to serialize Request");
            vec![hdlc_encapsulate(&req, &CRC_CCITT)]
        } else {
            self.mask_requests.clone()
        };
        write_frames(&mut self.port, &frames, WRITE_TIMEOUT).await
    }

    async fn next_container(&mut self, read_buf: &mut [u8]) -> MessagesContainer {
        loop {
            if let Some(last) = self.pending.iter().rposition(|&b| b == HDLC_TERMINATOR) {
                let chunk: Vec<u8> = self.pending.drain(..=last).collect();
                let messages: Vec<HdlcEncapsulatedMessage> = chunk
                    .split_inclusive(|&b| b == HDLC_TERMINATOR)
                    .filter(|frame| frame.len() > 1)
                    .map(|frame| HdlcEncapsulatedMessage {
                        len: frame.len() as u32,
                        data: frame.to_vec(),
                    })
                    .collect();
                if !messages.is_empty() {
                    self.last_kept = std::time::Instant::now();
                    return MessagesContainer {
                        data_type: DataType::UserSpace,
                        num_messages: messages.len() as u32,
                        messages,
                    };
                }
            }
            let quiet_for = self.last_kept.elapsed();
            if quiet_for >= HEARTBEAT_AFTER {
                if !self.heartbeat().await {
                    info!(
                        "serial diag port {} does not take writes, reconnecting",
                        self.path
                    );
                    self.reconnect().await;
                }
                self.last_kept = std::time::Instant::now();
                continue;
            }
            let read =
                match tokio::time::timeout(HEARTBEAT_AFTER - quiet_for, self.port.read(read_buf))
                    .await
                {
                    Ok(read) => read,
                    Err(_) => continue,
                };
            match read {
                Ok(n) if n > 0 => {
                    self.pending.extend_from_slice(&read_buf[..n]);
                    // e.g. the AT or NMEA port, which never sends an HDLC terminator
                    if self.pending.len() > MAX_PENDING && !self.pending.contains(&HDLC_TERMINATOR)
                    {
                        warn!(
                            "no diag frame in {} bytes from {}, is this the diag interface?",
                            self.pending.len(),
                            self.path
                        );
                        self.pending.clear();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // a vanished USB tty shows up as EOF (Ok(0)) or EIO
                result => {
                    info!(
                        "serial diag port {} went away ({result:?}), reconnecting",
                        self.path
                    );
                    self.reconnect().await;
                }
            }
        }
    }
}

impl DiagDevice {
    pub async fn new(configured_device: &Device) -> DiagResult<Self> {
        Self::new_with_retries(Duration::from_secs(30), configured_device).await
    }

    pub async fn new_with_retries(
        max_duration: Duration,
        configured_device: &Device,
    ) -> DiagResult<Self> {
        // For some reason the diag device needs a very long time to become available again with in
        // the same process, on TP-Link M7350 v3. While process restart would reset it faster.

        let start_time = std::time::Instant::now();
        let max_delay = Duration::from_secs(5);

        let mut delay = Duration::from_millis(100);
        let mut num_retries = 0;

        loop {
            match Self::try_new(configured_device).await {
                Ok(device) => {
                    info!("Diag device initialization succeeded after {num_retries} retries");
                    return Ok(device);
                }
                Err(e) => {
                    num_retries += 1;
                    if start_time.elapsed() >= max_duration {
                        error!("Failed to initialize diag device after {max_duration:?}: {e}");
                        return Err(e);
                    }

                    info!(
                        "Diag device initialization failed {num_retries} times, retrying in {delay:?}: {e}"
                    );
                    sleep(delay).await;

                    // Exponential backoff
                    delay = std::cmp::min(delay * 2, max_delay);
                }
            }
        }
    }

    async fn try_new(configured_device: &Device) -> DiagResult<Self> {
        let diag_file = File::options()
            .read(true)
            .write(true)
            .open("/dev/diag")
            .await
            .map_err(DiagDeviceError::OpenDiagDeviceError)?;
        let fd = diag_file.as_raw_fd();

        enable_frame_readwrite(fd, MEMORY_DEVICE_MODE, configured_device)?;
        let use_mdm = determine_use_mdm(fd)?;

        Ok(DiagDevice {
            read_buf: vec![0; BUFFER_LEN],
            log_codes: &LOG_CODES_FOR_RAW_PACKET_LOGGING,
            transport: Transport::DevDiag {
                file: diag_file,
                use_mdm,
            },
        })
    }

    /// Opens diag on a USB serial port, given as a tty path or `usb:VID:PID:INTERFACE`.
    pub async fn new_serial(spec: &str) -> DiagResult<Self> {
        let max_duration = Duration::from_secs(30);
        let start_time = std::time::Instant::now();
        loop {
            match open_serial(spec).await {
                Ok((path, mut port)) => {
                    info!("Opened serial diag port {path}");
                    drain_stale_logs(&mut port).await;
                    return Ok(DiagDevice {
                        read_buf: vec![0; BUFFER_LEN],
                        log_codes: &LOG_CODES_FOR_RAW_PACKET_LOGGING,
                        transport: Transport::Serial(SerialDiag {
                            spec: spec.to_string(),
                            path,
                            port,
                            pending: Vec::new(),
                            mask_requests: Vec::new(),
                            last_kept: std::time::Instant::now(),
                        }),
                    });
                }
                Err(e) if start_time.elapsed() < max_duration => {
                    info!("Serial diag port not ready, retrying: {e}");
                    sleep(Duration::from_secs(1)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Restricts logging to signalling messages (see [`LOG_CODES_SIGNALLING_ONLY`]). Call before
    /// [`DiagDevice::config_logs`].
    pub fn signalling_only(&mut self) {
        self.log_codes = &LOG_CODES_SIGNALLING_ONLY;
    }

    pub fn as_stream(
        &mut self,
    ) -> impl TryStream<Ok = MessagesContainer, Error = DiagDeviceError> + '_ {
        futures::stream::try_unfold(self, |dev| async {
            let container = dev.get_next_messages_container().await?;
            Ok(Some((container, dev)))
        })
    }

    async fn get_next_messages_container(&mut self) -> Result<MessagesContainer, DiagDeviceError> {
        let file = match &mut self.transport {
            Transport::DevDiag { file, .. } => file,
            Transport::Serial(serial) => {
                return Ok(serial.next_container(&mut self.read_buf).await);
            }
        };
        let mut bytes_read = 0;
        // TP-Link M7350 sometimes sends too small messages, we need to be able to deal with short reads.
        while bytes_read <= 8 {
            bytes_read = file
                .read(&mut self.read_buf)
                .await
                .map_err(DiagDeviceError::DeviceReadFailed)?;
        }

        debug!(
            "Parsing messages container size = {:?} [{:?}]",
            bytes_read,
            &self.read_buf[0..bytes_read]
        );

        match MessagesContainer::from_bytes((&self.read_buf[0..bytes_read], 0)) {
            Ok((_, container)) => Ok(container),
            Err(err) => Err(DiagDeviceError::ParseMessagesContainerError(err)),
        }
    }

    async fn write_request(&mut self, req: &Request) -> DiagResult<()> {
        let req_bytes = &req.to_bytes().expect("Failed to serialize Request");
        let (file, use_mdm) = match &mut self.transport {
            Transport::DevDiag { file, use_mdm } => (file, *use_mdm),
            Transport::Serial(serial) => {
                // serial diag takes the HDLC frame as-is, without the /dev/diag RequestContainer
                let frame = hdlc_encapsulate(req_bytes, &CRC_CCITT);
                serial
                    .port
                    .write_all(&frame)
                    .await
                    .map_err(DiagDeviceError::DeviceWriteFailed)?;
                if matches!(req, Request::LogConfig(LogConfigRequest::SetMask { .. })) {
                    serial.mask_requests.push(frame);
                }
                return Ok(());
            }
        };
        let buf = RequestContainer {
            data_type: DataType::UserSpace,
            use_mdm: use_mdm > 0,
            mdm_field: -1,
            hdlc_encapsulated_request: hdlc_encapsulate(req_bytes, &CRC_CCITT),
        }
        .to_bytes()
        .expect("Failed to serialize RequestContainer");
        if let Err(err) = file.write(&buf).await {
            // For reasons I don't entirely understand, calls to write(2) on
            // /dev/diag always return 0 bytes written, though the written
            // requests end up being interpreted. As such, we're not concerned
            // about WriteZero errors
            if err.kind() != ErrorKind::WriteZero {
                return Err(DiagDeviceError::DeviceWriteFailed(err));
            }
        }
        if let Err(err) = file.flush().await
            && err.kind() != ErrorKind::WriteZero
        {
            return Err(DiagDeviceError::DeviceWriteFailed(err));
        }
        Ok(())
    }

    async fn read_response(&mut self) -> DiagResult<Vec<Result<Message, DiagParsingError>>> {
        // On /dev/diag a response arrives in its own read. A serial port keeps streaming log frames
        // around it, so there we skip containers until one carries a response (or we time out).
        let serial = matches!(self.transport, Transport::Serial(_));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let container = if serial {
                match tokio::time::timeout_at(deadline, self.get_next_messages_container()).await {
                    Ok(container) => container?,
                    Err(_) => return Ok(Vec::new()),
                }
            } else {
                self.get_next_messages_container().await?
            };
            if container.data_type != DataType::UserSpace {
                continue;
            }
            let messages = container.messages();
            if serial
                && !messages
                    .iter()
                    .any(|m| matches!(m, Ok(Message::Response { .. })))
            {
                continue;
            }
            return Ok(messages);
        }
    }

    async fn retrieve_id_ranges(&mut self) -> DiagResult<[u32; 16]> {
        let req = Request::LogConfig(LogConfigRequest::RetrieveIdRanges);
        self.write_request(&req).await?;

        for msg in self.read_response().await? {
            match msg {
                Ok(Message::Log { .. }) => info!("skipping log response..."),
                Ok(Message::Response {
                    payload, status, ..
                }) => match payload {
                    ResponsePayload::LogConfig(LogConfigResponse::RetrieveIdRanges {
                        log_mask_sizes,
                    }) => {
                        if status != 0 {
                            return Err(DiagDeviceError::RequestFailed(status, req));
                        }
                        return Ok(log_mask_sizes);
                    }
                    _ => info!("skipping non-LogConfigResponse response..."),
                },
                Err(e) => error!("error parsing message: {e:?}"),
            }
        }

        Err(DiagDeviceError::NoResponse(req))
    }

    async fn set_log_mask(&mut self, log_type: u32, log_mask_bitsize: u32) -> DiagResult<()> {
        let req = build_log_mask_request(log_type, log_mask_bitsize, self.log_codes);
        self.write_request(&req).await?;

        for msg in self.read_response().await? {
            match msg {
                Ok(Message::Log { .. }) => info!("skipping log response..."),
                Ok(Message::Response {
                    payload, status, ..
                }) => {
                    if let ResponsePayload::LogConfig(LogConfigResponse::SetMask { .. }) = payload {
                        if status != 0 {
                            return Err(DiagDeviceError::RequestFailed(status, req));
                        }
                        return Ok(());
                    }
                }
                Err(e) => error!("error parsing message: {e:?}"),
            }
        }

        Err(DiagDeviceError::NoResponse(req))
    }

    pub async fn config_logs(&mut self) -> DiagResult<()> {
        info!("retrieving diag logging capabilities...");
        let log_mask_sizes = self.retrieve_id_ranges().await?;

        for (log_type, &log_mask_bitsize) in log_mask_sizes.iter().enumerate() {
            if log_mask_bitsize > 0 {
                self.set_log_mask(log_type as u32, log_mask_bitsize).await?;
                info!("enabled logging for log type {log_type}");
            }
        }

        Ok(())
    }

    /// Clears the masks set by [`DiagDevice::config_logs`], so a serial modem stops logging after
    /// the daemon exits. /dev/diag is left as it was.
    pub async fn disable_logs(&mut self) -> DiagResult<()> {
        if !matches!(self.transport, Transport::Serial(_)) {
            return Ok(());
        }
        let log_mask_sizes = self.retrieve_id_ranges().await?;
        self.log_codes = &[];
        for (log_type, &log_mask_bitsize) in log_mask_sizes.iter().enumerate() {
            if log_mask_bitsize > 0 {
                self.set_log_mask(log_type as u32, log_mask_bitsize).await?;
            }
        }
        info!("disabled diag logging");
        Ok(())
    }
}

// also found in: https://android.googlesource.com/kernel/msm.git/+/android-7.1.0_r0.3/drivers/char/diag/diagchar.h#399
//
// the code on
// https://github.com/P1sec/QCSuper/blob/master/docs/The%20Diag%20protocol.md#the-diag-protocol-over-devdiag
// is misleading, mode_param is only 8 bits. sending the larger [u32; 3] payload will cause the
// IOCTL to be rejected by TPLINK M7350 HW rev 5
//
// TPLINK M7350 v5 source code can be downloaded at https://www.tp-link.com/de/support/gpl-code/?app=omada
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DiagLoggingModeParam {
    req_mode: u32,
    peripheral_mask: u32,
    mode_param: u8,
}

// Triggers the diag device's debug logging mode
fn enable_frame_readwrite(fd: i32, mode: u32, configured_device: &Device) -> DiagResult<()> {
    unsafe {
        if libc::ioctl(fd, DIAG_IOCTL_SWITCH_LOGGING, mode, 0, 0, 0) < 0 {
            let mut try_params = vec![DiagLoggingModeParam {
                req_mode: mode,
                peripheral_mask: u32::MAX,
                mode_param: 0,
            }];
            if configured_device == &Device::Tplink {
                // tplink M7350 HW revision 3-8 need this mode
                try_params.insert(
                    0,
                    DiagLoggingModeParam {
                        req_mode: mode,
                        peripheral_mask: 0,
                        mode_param: 1,
                    },
                );
            }

            let mut ret = 0;

            for params in &try_params {
                let mut params = *params;
                ret = libc::ioctl(
                    fd,
                    DIAG_IOCTL_SWITCH_LOGGING,
                    &mut params as *mut DiagLoggingModeParam,
                    std::mem::size_of::<DiagLoggingModeParam>(),
                    0,
                    0,
                    0,
                    0,
                );
                if ret == 0 {
                    break;
                }
            }

            if ret < 0 {
                let msg = format!("DIAG_IOCTL_SWITCH_LOGGING ioctl failed with error code {ret}");
                return Err(DiagDeviceError::InitializationFailed(msg));
            }
        }
    }
    Ok(())
}

// Unsure of what MDM actually stands for, but if `use_mdm` is > 0, then
// an additional mask is included in every diag request
fn determine_use_mdm(fd: i32) -> DiagResult<i32> {
    let use_mdm: i32 = 0;
    unsafe {
        if libc::ioctl(fd, DIAG_IOCTL_REMOTE_DEV, &use_mdm as *const i32) < 0 {
            let msg = format!("DIAG_IOCTL_REMOTE_DEV ioctl failed with error code {}", 0);
            return Err(DiagDeviceError::InitializationFailed(msg));
        }
    }
    Ok(use_mdm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_frames_gives_up_on_a_port_that_does_not_drain() {
        // nobody reads the other end, so the small buffer fills up and the write blocks
        let (port, _other_end) = tokio::io::duplex(16);
        let mut port: Box<dyn DiagPort> = Box::new(port);
        let frames = vec![vec![HDLC_TERMINATOR; 64]];
        assert!(!write_frames(&mut port, &frames, Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn write_frames_writes_to_a_draining_port() {
        let (port, _other_end) = tokio::io::duplex(1024);
        let mut port: Box<dyn DiagPort> = Box::new(port);
        let frames = vec![vec![HDLC_TERMINATOR; 64], vec![HDLC_TERMINATOR; 64]];
        assert!(write_frames(&mut port, &frames, Duration::from_millis(100)).await);
    }
}
