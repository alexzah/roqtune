//! Google Cast discovery and remote playback manager.
//!
//! This module provides a best-effort Cast v2 controller:
//! - discovers cast targets via mDNS (`mdns-sd`, no system daemon dependency)
//! - connects over TLS to the cast control channel
//! - launches default media receiver
//! - serves local media files via an internal HTTP server
//! - drives remote playback and mirrors progress/events onto the app bus

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::Value;
use tokio::sync::broadcast::{Receiver, Sender};

use crate::protocol::{
    CastConnectionState, CastDeviceInfo, CastMessage, CastPlaybackPathKind, Message,
    PlaybackMessage, TrackStarted,
};

const CAST_DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";
const CAST_NAMESPACE_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const CAST_NAMESPACE_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const CAST_NAMESPACE_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const CAST_NAMESPACE_MEDIA: &str = "urn:x-cast:com.google.cast.media";

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(900);
const IDLE_LOOP_SLEEP: Duration = Duration::from_millis(25);
const CAST_CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const CAST_READ_TIMEOUT: Duration = Duration::from_millis(180);
const CAST_WRITE_TIMEOUT: Duration = Duration::from_millis(1500);
const STREAM_TOKEN_TTL: Duration = Duration::from_secs(60 * 30);

#[derive(Clone)]
struct StreamResource {
    path: PathBuf,
    content_type: String,
    allowed_ip: IpAddr,
    created_at: Instant,
}

#[derive(Clone)]
struct CastStreamServer {
    listen_addr: SocketAddr,
    resources: Arc<Mutex<HashMap<String, StreamResource>>>,
}

impl CastStreamServer {
    fn new() -> Result<Self, String> {
        let listener = TcpListener::bind(("0.0.0.0", 0))
            .map_err(|err| format!("failed to bind cast stream server: {err}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("failed to set cast stream server non-blocking: {err}"))?;
        let listen_addr = listener
            .local_addr()
            .map_err(|err| format!("failed to read cast stream server address: {err}"))?;
        let resources: Arc<Mutex<HashMap<String, StreamResource>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let resources_worker = Arc::clone(&resources);

        thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let resources_clone = Arc::clone(&resources_worker);
                    thread::spawn(move || {
                        if let Err(err) = handle_stream_request(stream, peer, resources_clone) {
                            debug!("Cast stream request failed: {}", err);
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    prune_expired_stream_resources(&resources_worker);
                    thread::sleep(Duration::from_millis(30));
                }
                Err(err) => {
                    warn!("Cast stream server accept failed: {}", err);
                    thread::sleep(Duration::from_millis(120));
                }
            }
        });

        Ok(Self {
            listen_addr,
            resources,
        })
    }

    fn register_file(
        &self,
        path: PathBuf,
        content_type: String,
        allowed_ip: IpAddr,
    ) -> Result<String, String> {
        let token = random_token();
        let resource = StreamResource {
            path,
            content_type,
            allowed_ip,
            created_at: Instant::now(),
        };
        let mut resources = self
            .resources
            .lock()
            .map_err(|_| "cast stream resource lock poisoned".to_string())?;
        resources.insert(token.clone(), resource);
        Ok(token)
    }

    fn media_url(&self, token: &str, local_ip: IpAddr) -> String {
        format!(
            "http://{}:{}/cast/{}",
            local_ip,
            self.listen_addr.port(),
            token
        )
    }
}

fn prune_expired_stream_resources(resources: &Arc<Mutex<HashMap<String, StreamResource>>>) {
    let mut locked = match resources.lock() {
        Ok(locked) => locked,
        Err(poisoned) => poisoned.into_inner(),
    };
    locked.retain(|_, value| value.created_at.elapsed() <= STREAM_TOKEN_TTL);
}

fn parse_header_line(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    Some((name.trim(), value.trim()))
}

fn parse_http_request(
    reader: &mut BufReader<TcpStream>,
) -> Result<(String, String, HashMap<String, String>), String> {
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|err| format!("failed to read request line: {err}"))?;
    if request_line.trim().is_empty() {
        return Err("empty request".to_string());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing request method".to_string())?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| "missing request path".to_string())?
        .to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|err| format!("failed to read header line: {err}"))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = parse_header_line(&line) {
            headers.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }

    Ok((method, path, headers))
}

fn write_simple_response(
    stream: &mut TcpStream,
    status_line: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let header = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .map_err(|err| format!("failed to write response header: {err}"))?;
    stream
        .write_all(body)
        .map_err(|err| format!("failed to write response body: {err}"))?;
    Ok(())
}

fn parse_range_header(range_header: Option<&str>, file_size: u64) -> Option<(u64, u64)> {
    let value = range_header?;
    let bytes = value.strip_prefix("bytes=")?;
    let (start_raw, end_raw) = bytes.split_once('-')?;
    let start = if start_raw.trim().is_empty() {
        0
    } else {
        start_raw.trim().parse::<u64>().ok()?
    };
    let end = if end_raw.trim().is_empty() {
        file_size.saturating_sub(1)
    } else {
        end_raw.trim().parse::<u64>().ok()?
    };
    if start > end || end >= file_size {
        return None;
    }
    Some((start, end))
}

fn handle_stream_request(
    stream: TcpStream,
    peer: SocketAddr,
    resources: Arc<Mutex<HashMap<String, StreamResource>>>,
) -> Result<(), String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("failed to clone stream: {err}"))?,
    );
    let (method, path, headers) = parse_http_request(&mut reader)?;
    let mut stream = stream;

    if method != "GET" {
        return write_simple_response(
            &mut stream,
            "HTTP/1.1 405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method Not Allowed\n",
        );
    }
    let token = path
        .trim()
        .strip_prefix("/cast/")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "invalid cast stream path".to_string())?;

    let resource = {
        let locked = match resources.lock() {
            Ok(locked) => locked,
            Err(poisoned) => poisoned.into_inner(),
        };
        locked.get(token).cloned()
    };
    let Some(resource) = resource else {
        return write_simple_response(
            &mut stream,
            "HTTP/1.1 404 Not Found",
            "text/plain; charset=utf-8",
            b"Not Found\n",
        );
    };

    if peer.ip() != resource.allowed_ip {
        debug!(
            "Cast stream request source {} differs from discovered receiver {}; allowing by token",
            peer.ip(),
            resource.allowed_ip
        );
    }

    let mut file =
        File::open(&resource.path).map_err(|err| format!("failed to open stream file: {err}"))?;
    let file_size = file
        .metadata()
        .map_err(|err| format!("failed to stat stream file: {err}"))?
        .len();
    if file_size == 0 {
        return write_simple_response(
            &mut stream,
            "HTTP/1.1 204 No Content",
            &resource.content_type,
            b"",
        );
    }

    let range = parse_range_header(headers.get("range").map(String::as_str), file_size);
    let (start, end, status_line) = if let Some((start, end)) = range {
        (start, end, "HTTP/1.1 206 Partial Content")
    } else {
        (0, file_size - 1, "HTTP/1.1 200 OK")
    };
    let content_length = end.saturating_sub(start).saturating_add(1);

    file.seek(SeekFrom::Start(start))
        .map_err(|err| format!("failed to seek stream file: {err}"))?;

    let mut response_header = format!(
        "{status_line}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n",
        resource.content_type, content_length
    );
    if status_line.contains("206") {
        response_header.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            start, end, file_size
        ));
    }
    response_header.push_str("\r\n");
    stream
        .write_all(response_header.as_bytes())
        .map_err(|err| format!("failed to write stream response header: {err}"))?;

    let mut remaining = content_length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let read_cap = remaining.min(buffer.len() as u64) as usize;
        let read = file
            .read(&mut buffer[..read_cap])
            .map_err(|err| format!("failed to read stream file: {err}"))?;
        if read == 0 {
            break;
        }
        stream
            .write_all(&buffer[..read])
            .map_err(|err| format!("failed to write stream body: {err}"))?;
        remaining = remaining.saturating_sub(read as u64);
    }
    Ok(())
}

fn random_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{:032x}", nanos)
}

fn extension_to_content_type(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn local_ip_for_remote(remote_ip: IpAddr) -> Option<IpAddr> {
    let bind_addr = match remote_ip {
        IpAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        IpAddr::V6(_) => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)),
    };
    let socket = UdpSocket::bind(bind_addr).ok()?;
    socket.connect(SocketAddr::new(remote_ip, 9)).ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

fn cast_instance_name_from_fullname(fullname: &str) -> String {
    let suffix = "._googlecast._tcp.local.";
    fullname
        .trim()
        .strip_suffix(suffix)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fullname)
        .trim_matches('.')
        .to_string()
}

fn cast_device_from_resolved_service(service: &mdns_sd::ResolvedService) -> Option<CastDeviceInfo> {
    // Prefer IPv4 for compatibility with the current HTTP stream server bind model.
    let mut v4_addresses: Vec<_> = service.get_addresses_v4().iter().copied().collect();
    v4_addresses.sort();
    let address = v4_addresses.first().map(ToString::to_string)?;

    let host = service.get_hostname().trim_end_matches('.').to_string();
    let port = service.get_port();
    let fallback_name = cast_instance_name_from_fullname(service.get_fullname());
    let name = service
        .get_property_val_str("fn")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or(fallback_name);
    let model = service
        .get_property_val_str("md")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_default();
    let id = service
        .get_property_val_str("id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("{}:{}:{}", host, address, port));
    Some(CastDeviceInfo {
        id,
        name,
        model,
        host,
        address,
        port,
    })
}

fn discover_cast_devices_once() -> Vec<CastDeviceInfo> {
    const CAST_SERVICE_TYPE: &str = "_googlecast._tcp.local.";
    const DISCOVERY_WINDOW: Duration = Duration::from_millis(1800);
    const DISCOVERY_POLL: Duration = Duration::from_millis(250);

    let mdns = match ServiceDaemon::new() {
        Ok(mdns) => mdns,
        Err(err) => {
            warn!(
                "CastManager: failed to start mDNS discovery daemon: {}",
                err
            );
            return Vec::new();
        }
    };
    let browse_receiver = match mdns.browse(CAST_SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(err) => {
            warn!("CastManager: failed to browse cast mDNS service: {}", err);
            let _ = mdns.shutdown();
            return Vec::new();
        }
    };

    let deadline = Instant::now() + DISCOVERY_WINDOW;
    let mut devices_by_id: HashMap<String, CastDeviceInfo> = HashMap::new();
    while Instant::now() < deadline {
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(DISCOVERY_POLL);
        let Ok(event) = browse_receiver.recv_timeout(timeout) else {
            continue;
        };
        if let ServiceEvent::ServiceResolved(service) = event {
            if let Some(device) = cast_device_from_resolved_service(&service) {
                devices_by_id.insert(device.id.clone(), device);
            }
        }
    }

    if let Err(err) = mdns.stop_browse(CAST_SERVICE_TYPE) {
        debug!("CastManager: failed to stop mDNS browse cleanly: {}", err);
    }
    let _ = mdns.shutdown();

    let mut devices: Vec<CastDeviceInfo> = devices_by_id.into_values().collect();
    devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    devices.dedup_by(|a, b| a.id == b.id);
    devices
}

#[derive(Debug, Clone)]
struct MediaStatus {
    player_state: String,
    current_time_s: f64,
    duration_s: f64,
    idle_reason: Option<String>,
}

struct CastSession {
    stream: native_tls::TlsStream<TcpStream>,
    receiver_id: String,
    sender_id: String,
    media_transport_id: String,
    receiver_app_session_id: Option<String>,
    next_request_id: i64,
}

impl CastSession {
    fn connect(device: &CastDeviceInfo) -> Result<Self, String> {
        let address = format!("{}:{}", device.address, device.port);
        let tcp = TcpStream::connect_timeout(
            &address
                .parse()
                .map_err(|err| format!("invalid cast target address '{}': {err}", address))?,
            CAST_CONNECT_TIMEOUT,
        )
        .map_err(|err| format!("failed to connect to cast target {}: {err}", address))?;
        tcp.set_read_timeout(Some(CAST_READ_TIMEOUT))
            .map_err(|err| format!("failed to set cast read timeout: {err}"))?;
        tcp.set_write_timeout(Some(CAST_WRITE_TIMEOUT))
            .map_err(|err| format!("failed to set cast write timeout: {err}"))?;

        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|err| format!("failed to create cast tls connector: {err}"))?;
        let stream = connector
            .connect(&device.host, tcp)
            .map_err(|err| format!("failed cast tls handshake: {err}"))?;

        let mut session = Self {
            stream,
            receiver_id: "receiver-0".to_string(),
            sender_id: "sender-roqtune".to_string(),
            media_transport_id: "receiver-0".to_string(),
            receiver_app_session_id: None,
            next_request_id: 1,
        };

        session.send_json(
            CAST_NAMESPACE_CONNECTION,
            &session.receiver_id.clone(),
            serde_json::json!({"type":"CONNECT","origin":{}}),
        )?;

        let request_id = session.alloc_request_id();
        session.send_json(
            CAST_NAMESPACE_RECEIVER,
            &session.receiver_id.clone(),
            serde_json::json!({"type":"LAUNCH","appId":CAST_DEFAULT_MEDIA_RECEIVER_APP_ID,"requestId":request_id}),
        )?;
        let (transport_id, app_session_id) =
            session.await_media_transport_id(Duration::from_secs(8))?;
        session.media_transport_id = transport_id;
        session.receiver_app_session_id = Some(app_session_id);
        session.send_json(
            CAST_NAMESPACE_CONNECTION,
            &session.media_transport_id.clone(),
            serde_json::json!({"type":"CONNECT","origin":{}}),
        )?;

        Ok(session)
    }

    fn alloc_request_id(&mut self) -> i64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn send_json(
        &mut self,
        namespace: &str,
        destination_id: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let payload_text = payload.to_string();
        let frame = encode_cast_frame(
            &self.sender_id,
            destination_id,
            namespace,
            payload_text.as_str(),
        )?;
        self.stream
            .write_all(&frame)
            .map_err(|err| format!("failed to send cast frame: {err}"))?;
        Ok(())
    }

    fn read_next_message(&mut self) -> Result<Option<(String, String, String)>, String> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
            Err(err) => return Err(format!("failed to read cast frame length: {err}")),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            return Ok(None);
        }
        let mut payload = vec![0u8; len];
        self.stream
            .read_exact(&mut payload)
            .map_err(|err| format!("failed to read cast frame payload: {err}"))?;
        let decoded = decode_cast_frame(&payload)?;
        Ok(Some((
            decoded.namespace,
            decoded.source_id,
            decoded.payload_utf8,
        )))
    }

    fn await_media_transport_id(&mut self, timeout: Duration) -> Result<(String, String), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((namespace, _, payload)) = self.read_next_message()? {
                if namespace == CAST_NAMESPACE_HEARTBEAT {
                    if let Ok(value) = serde_json::from_str::<Value>(&payload) {
                        if value.get("type").and_then(Value::as_str) == Some("PING") {
                            let _ = self.send_json(
                                CAST_NAMESPACE_HEARTBEAT,
                                &self.receiver_id.clone(),
                                serde_json::json!({"type":"PONG"}),
                            );
                        }
                    }
                    continue;
                }
                if namespace != CAST_NAMESPACE_RECEIVER {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                if let Some(status) = value.get("status") {
                    if let Some(applications) = status.get("applications").and_then(Value::as_array)
                    {
                        for app in applications {
                            if app.get("appId").and_then(Value::as_str)
                                == Some(CAST_DEFAULT_MEDIA_RECEIVER_APP_ID)
                            {
                                if let Some(transport_id) =
                                    app.get("transportId").and_then(Value::as_str)
                                {
                                    if let Some(session_id) =
                                        app.get("sessionId").and_then(Value::as_str)
                                    {
                                        return Ok((
                                            transport_id.to_string(),
                                            session_id.to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Err("timed out waiting for media transport id".to_string())
    }

    fn load_media(
        &mut self,
        url: &str,
        content_type: &str,
        start_offset_ms: u64,
    ) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        let mut payload = serde_json::json!({
            "type":"LOAD",
            "requestId":request_id,
            "autoplay":true,
            "media":{
                "contentId":url,
                "streamType":"BUFFERED",
                "contentType":content_type,
                "metadata":{"metadataType":0}
            }
        });
        if start_offset_ms > 0 {
            payload["currentTime"] = serde_json::json!(start_offset_ms as f64 / 1000.0);
        }
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            payload,
        )?;
        Ok(())
    }

    fn play(&mut self) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            serde_json::json!({"type":"PLAY","requestId":request_id}),
        )
    }

    fn pause(&mut self) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            serde_json::json!({"type":"PAUSE","requestId":request_id}),
        )
    }

    fn stop(&mut self) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            serde_json::json!({"type":"STOP","requestId":request_id}),
        )
    }

    fn stop_receiver_app(&mut self) -> Result<(), String> {
        let Some(session_id) = self.receiver_app_session_id.clone() else {
            return Err("cast receiver session id is not available".to_string());
        };
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_RECEIVER,
            &self.receiver_id.clone(),
            serde_json::json!({"type":"STOP","requestId":request_id,"sessionId":session_id}),
        )
    }

    fn close_connection(&mut self, destination_id: &str) -> Result<(), String> {
        self.send_json(
            CAST_NAMESPACE_CONNECTION,
            destination_id,
            serde_json::json!({"type":"CLOSE"}),
        )
    }

    fn shutdown_receiver_and_close(&mut self) {
        if let Err(err) = self.stop() {
            debug!("CastSession: media STOP during disconnect failed: {}", err);
        }
        if let Err(err) = self.stop_receiver_app() {
            debug!(
                "CastSession: receiver app STOP during disconnect failed: {}",
                err
            );
        }
        let media_transport_id = self.media_transport_id.clone();
        if let Err(err) = self.close_connection(&media_transport_id) {
            debug!(
                "CastSession: media transport CLOSE during disconnect failed: {}",
                err
            );
        }
        let receiver_id = self.receiver_id.clone();
        if let Err(err) = self.close_connection(&receiver_id) {
            debug!(
                "CastSession: receiver CLOSE during disconnect failed: {}",
                err
            );
        }
        self.receiver_app_session_id = None;
    }

    fn seek_ms(&mut self, position_ms: u64) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            serde_json::json!({
                "type":"SEEK",
                "requestId":request_id,
                "currentTime":position_ms as f64 / 1000.0,
                "resumeState":"PLAYBACK_START"
            }),
        )
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_RECEIVER,
            &self.receiver_id.clone(),
            serde_json::json!({
                "type":"SET_VOLUME",
                "requestId":request_id,
                "volume":{"level":volume.clamp(0.0, 1.0)}
            }),
        )
    }

    fn request_media_status(&mut self) -> Result<(), String> {
        let request_id = self.alloc_request_id();
        self.send_json(
            CAST_NAMESPACE_MEDIA,
            &self.media_transport_id.clone(),
            serde_json::json!({"type":"GET_STATUS","requestId":request_id}),
        )
    }

    fn drain_messages(&mut self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        loop {
            let Some((namespace, _, payload)) = self.read_next_message()? else {
                break;
            };
            if namespace == CAST_NAMESPACE_HEARTBEAT {
                if let Ok(value) = serde_json::from_str::<Value>(&payload) {
                    if value.get("type").and_then(Value::as_str) == Some("PING") {
                        let _ = self.send_json(
                            CAST_NAMESPACE_HEARTBEAT,
                            &self.receiver_id.clone(),
                            serde_json::json!({"type":"PONG"}),
                        );
                    }
                }
                continue;
            }
            out.push((namespace, payload));
        }
        Ok(out)
    }
}

struct DecodedFrame {
    source_id: String,
    namespace: String,
    payload_utf8: String,
}

fn encode_cast_frame(
    source_id: &str,
    destination_id: &str,
    namespace: &str,
    payload_utf8: &str,
) -> Result<Vec<u8>, String> {
    let mut protobuf = Vec::new();
    write_varint_field(&mut protobuf, 1, 0); // CASTV2_1_0
    write_string_field(&mut protobuf, 2, source_id);
    write_string_field(&mut protobuf, 3, destination_id);
    write_string_field(&mut protobuf, 4, namespace);
    write_varint_field(&mut protobuf, 5, 0); // STRING
    write_string_field(&mut protobuf, 6, payload_utf8);

    let len = protobuf
        .len()
        .try_into()
        .map_err(|_| "cast frame too large".to_string())?;
    let mut frame = Vec::with_capacity(4 + protobuf.len());
    frame.extend_from_slice(&u32::to_be_bytes(len));
    frame.extend_from_slice(&protobuf);
    Ok(frame)
}

fn decode_cast_frame(bytes: &[u8]) -> Result<DecodedFrame, String> {
    let mut cursor = 0usize;
    let mut source_id = String::new();
    let mut namespace = String::new();
    let mut payload_utf8 = String::new();

    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)
            .ok_or_else(|| "invalid cast protobuf key".to_string())?;
        let field_number = (key >> 3) as u32;
        let wire_type = (key & 0x07) as u8;
        match (field_number, wire_type) {
            (_, 0) => {
                let _ = read_varint(bytes, &mut cursor)
                    .ok_or_else(|| "invalid cast protobuf varint field".to_string())?;
            }
            (_, 2) => {
                let len = read_varint(bytes, &mut cursor)
                    .ok_or_else(|| "invalid cast protobuf length".to_string())?
                    as usize;
                if cursor + len > bytes.len() {
                    return Err("cast protobuf string out of bounds".to_string());
                }
                let value = String::from_utf8(bytes[cursor..cursor + len].to_vec())
                    .map_err(|_| "cast protobuf invalid utf8".to_string())?;
                match field_number {
                    2 => source_id = value,
                    4 => namespace = value,
                    6 => payload_utf8 = value,
                    _ => {}
                }
                cursor += len;
            }
            _ => return Err("unsupported cast protobuf wire type".to_string()),
        }
    }

    Ok(DecodedFrame {
        source_id,
        namespace,
        payload_utf8,
    })
}

fn write_varint_field(out: &mut Vec<u8>, field_number: u32, value: u64) {
    let key = (field_number as u64) << 3;
    write_varint(out, key);
    write_varint(out, value);
}

fn write_string_field(out: &mut Vec<u8>, field_number: u32, value: &str) {
    let key = ((field_number as u64) << 3) | 2;
    write_varint(out, key);
    write_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut shift = 0u32;
    let mut value = 0u64;
    while *cursor < bytes.len() && shift <= 63 {
        let byte = bytes[*cursor];
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn parse_media_status(payload: &str) -> Option<MediaStatus> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
        return None;
    }
    let status = value.get("status")?.as_array()?.first()?.clone();
    Some(MediaStatus {
        player_state: status
            .get("playerState")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_string(),
        current_time_s: status
            .get("currentTime")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        duration_s: status
            .get("media")
            .and_then(|media| media.get("duration"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        idle_reason: status
            .get("idleReason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

fn transcode_to_wav_pcm(path: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    let metadata =
        std::fs::metadata(path).map_err(|err| format!("failed to stat source: {err}"))?;
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let key = format!("{}:{}:{}", path.display(), metadata.len(), modified);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    key.hash(&mut hasher);
    let file_name = format!("{:016x}.wav", hasher.finish());
    let output_path = cache_dir.join(file_name);
    if output_path.exists() {
        return Ok(output_path);
    }
    std::fs::create_dir_all(cache_dir)
        .map_err(|err| format!("failed to create cast transcode cache: {err}"))?;

    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let input = File::open(path).map_err(|err| format!("failed to open source: {err}"))?;
    let mss = MediaSourceStream::new(Box::new(input), Default::default());
    let hint = Hint::new();
    let mut format = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| format!("failed to probe source: {err}"))?
        .format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default audio track found".to_string())?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let channels = codec_params
        .channels
        .map(|channels| channels.count() as u16)
        .unwrap_or(2)
        .max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|err| format!("failed to create decoder: {err}"))?;

    let mut output =
        File::create(&output_path).map_err(|err| format!("failed to create wav file: {err}"))?;
    output
        .write_all(&[0u8; 44])
        .map_err(|err| format!("failed to write wav header placeholder: {err}"))?;
    let mut bytes_written: u64 = 0;

    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        let spec = decoded.spec();
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *spec);
        buffer.copy_interleaved_ref(decoded);
        for sample in buffer.samples() {
            let pcm = (*sample).clamp(-1.0, 1.0) * i16::MAX as f32;
            let value = pcm.round() as i16;
            output
                .write_all(&value.to_le_bytes())
                .map_err(|err| format!("failed to write wav data: {err}"))?;
            bytes_written += 2;
        }
    }

    let riff_chunk_size = 36u64.saturating_add(bytes_written);
    let byte_rate = sample_rate as u64 * channels as u64 * 2;
    let block_align = channels * 2;

    output
        .seek(SeekFrom::Start(0))
        .map_err(|err| format!("failed to seek wav header start: {err}"))?;
    output
        .write_all(b"RIFF")
        .map_err(|err| format!("failed to write wav RIFF: {err}"))?;
    output
        .write_all(&(riff_chunk_size as u32).to_le_bytes())
        .map_err(|err| format!("failed to write wav riff size: {err}"))?;
    output
        .write_all(b"WAVEfmt ")
        .map_err(|err| format!("failed to write wav fmt chunk: {err}"))?;
    output
        .write_all(&16u32.to_le_bytes())
        .map_err(|err| format!("failed to write wav fmt size: {err}"))?;
    output
        .write_all(&1u16.to_le_bytes())
        .map_err(|err| format!("failed to write wav audio format: {err}"))?;
    output
        .write_all(&channels.to_le_bytes())
        .map_err(|err| format!("failed to write wav channels: {err}"))?;
    output
        .write_all(&sample_rate.to_le_bytes())
        .map_err(|err| format!("failed to write wav sample rate: {err}"))?;
    output
        .write_all(&(byte_rate as u32).to_le_bytes())
        .map_err(|err| format!("failed to write wav byte rate: {err}"))?;
    output
        .write_all(&block_align.to_le_bytes())
        .map_err(|err| format!("failed to write wav block align: {err}"))?;
    output
        .write_all(&16u16.to_le_bytes())
        .map_err(|err| format!("failed to write wav bits-per-sample: {err}"))?;
    output
        .write_all(b"data")
        .map_err(|err| format!("failed to write wav data tag: {err}"))?;
    output
        .write_all(&(bytes_written as u32).to_le_bytes())
        .map_err(|err| format!("failed to write wav data length: {err}"))?;

    Ok(output_path)
}

/// Cast subsystem runtime manager.
pub struct CastManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    stream_server: CastStreamServer,
    devices: Vec<CastDeviceInfo>,
    connected_device: Option<CastDeviceInfo>,
    session: Option<CastSession>,
    allow_transcode_fallback: bool,
    transcode_cache_dir: PathBuf,
    current_track_id: Option<String>,
    current_track_source_path: Option<PathBuf>,
    current_path_kind: Option<CastPlaybackPathKind>,
    last_status_poll_at: Instant,
}

impl CastManager {
    /// Creates a cast manager bound to the shared bus.
    pub fn new(bus_consumer: Receiver<Message>, bus_producer: Sender<Message>) -> Self {
        let stream_server = CastStreamServer::new().expect("cast stream server should start");
        let transcode_cache_dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("roqtune")
            .join("cast_transcode");
        Self {
            bus_consumer,
            bus_producer,
            stream_server,
            devices: Vec::new(),
            connected_device: None,
            session: None,
            allow_transcode_fallback: false,
            transcode_cache_dir,
            current_track_id: None,
            current_track_source_path: None,
            current_path_kind: None,
            last_status_poll_at: Instant::now(),
        }
    }

    fn emit_connection_state(
        &self,
        state: CastConnectionState,
        reason: Option<String>,
        device: Option<CastDeviceInfo>,
    ) {
        let _ = self
            .bus_producer
            .send(Message::Cast(CastMessage::ConnectionStateChanged {
                state,
                device,
                reason,
            }));
    }

    fn emit_devices(&self) {
        let _ = self
            .bus_producer
            .send(Message::Cast(CastMessage::DevicesUpdated(
                self.devices.clone(),
            )));
    }

    fn discover_devices(&mut self) {
        self.emit_connection_state(CastConnectionState::Discovering, None, None);
        self.devices = discover_cast_devices_once();
        self.emit_devices();
        let state = if self.session.is_some() {
            CastConnectionState::Connected
        } else {
            CastConnectionState::Disconnected
        };
        self.emit_connection_state(state, None, self.connected_device.clone());
    }

    fn connect_device(&mut self, device_id: &str) {
        let Some(device) = self
            .devices
            .iter()
            .find(|device| device.id == device_id)
            .cloned()
        else {
            self.emit_connection_state(
                CastConnectionState::Disconnected,
                Some("Selected cast device was not found.".to_string()),
                None,
            );
            return;
        };
        self.emit_connection_state(CastConnectionState::Connecting, None, Some(device.clone()));
        match CastSession::connect(&device) {
            Ok(session) => {
                info!(
                    "CastManager: connected to cast target '{}' ({})",
                    device.name, device.address
                );
                self.session = Some(session);
                self.connected_device = Some(device.clone());
                self.emit_connection_state(CastConnectionState::Connected, None, Some(device));
            }
            Err(err) => {
                warn!("CastManager: connect failed: {}", err);
                self.session = None;
                self.connected_device = None;
                self.emit_connection_state(
                    CastConnectionState::Disconnected,
                    Some(format!("Failed to connect: {}", err)),
                    None,
                );
            }
        }
    }

    fn disconnect(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.shutdown_receiver_and_close();
        }
        self.session = None;
        self.current_track_id = None;
        self.current_track_source_path = None;
        self.current_path_kind = None;
        self.connected_device = None;
        self.emit_connection_state(
            CastConnectionState::Disconnected,
            Some("Cast session closed.".to_string()),
            None,
        );
    }

    fn load_track_with_mode(
        &mut self,
        track_id: &str,
        source_path: PathBuf,
        start_offset_ms: u64,
        mode: CastPlaybackPathKind,
    ) -> Result<(), String> {
        let Some(device) = self.connected_device.clone() else {
            return Err("No cast device connected".to_string());
        };
        let receiver_ip = device
            .address
            .parse::<IpAddr>()
            .map_err(|err| format!("invalid cast receiver IP '{}': {err}", device.address))?;
        let local_ip = local_ip_for_remote(receiver_ip)
            .ok_or_else(|| "Unable to determine local sender IP for cast receiver".to_string())?;

        let (stream_path, content_type, path_description) = match mode {
            CastPlaybackPathKind::Direct => (
                source_path.clone(),
                extension_to_content_type(&source_path),
                "Casting: Direct (unmodified source stream)".to_string(),
            ),
            CastPlaybackPathKind::TranscodeWavPcm => {
                let wav_path = transcode_to_wav_pcm(&source_path, &self.transcode_cache_dir)?;
                (
                    wav_path,
                    "audio/wav".to_string(),
                    "Casting: Transcode (WAV PCM)".to_string(),
                )
            }
        };

        let token = self.stream_server.register_file(
            stream_path.clone(),
            content_type.clone(),
            receiver_ip,
        )?;
        let url = self.stream_server.media_url(&token, local_ip);
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "No cast session available".to_string())?;
        let load_start_offset_ms = 0;
        if start_offset_ms > 0 {
            info!(
                "CastManager: forcing LOAD start offset to 0ms for receiver stability (requested {}ms for track {})",
                start_offset_ms, track_id
            );
        }
        session.load_media(&url, &content_type, load_start_offset_ms)?;
        self.current_track_id = Some(track_id.to_string());
        self.current_path_kind = Some(mode);
        let _ = self
            .bus_producer
            .send(Message::Cast(CastMessage::PlaybackPathChanged {
                kind: mode,
                description: path_description,
            }));
        let _ = self
            .bus_producer
            .send(Message::Playback(PlaybackMessage::TrackStarted(
                TrackStarted {
                    id: track_id.to_string(),
                    start_offset_ms: load_start_offset_ms,
                },
            )));
        Ok(())
    }

    fn load_track(&mut self, track_id: &str, path: PathBuf, start_offset_ms: u64) {
        self.current_track_source_path = Some(path.clone());
        let direct_result = self.load_track_with_mode(
            track_id,
            path.clone(),
            start_offset_ms,
            CastPlaybackPathKind::Direct,
        );
        if direct_result.is_ok() {
            return;
        }
        let direct_error = direct_result
            .err()
            .unwrap_or_else(|| "Direct cast failed".to_string());
        if !self.allow_transcode_fallback {
            let _ = self
                .bus_producer
                .send(Message::Cast(CastMessage::PlaybackError {
                    track_id: Some(track_id.to_string()),
                    message: format!(
                        "Direct cast failed: {}. Enable 'Cast transcode fallback' in Settings > Audio to improve compatibility.",
                        direct_error
                    ),
                    can_retry_with_transcode: true,
                }));
            let _ = self
                .bus_producer
                .send(Message::Playback(PlaybackMessage::TrackFinished(
                    track_id.to_string(),
                )));
            self.current_track_source_path = None;
            return;
        }

        match self.load_track_with_mode(
            track_id,
            path,
            start_offset_ms,
            CastPlaybackPathKind::TranscodeWavPcm,
        ) {
            Ok(()) => {}
            Err(err) => {
                let _ = self
                    .bus_producer
                    .send(Message::Cast(CastMessage::PlaybackError {
                        track_id: Some(track_id.to_string()),
                        message: format!(
                            "Cast failed after direct and WAV fallback attempts: {}",
                            err
                        ),
                        can_retry_with_transcode: false,
                    }));
                let _ = self
                    .bus_producer
                    .send(Message::Playback(PlaybackMessage::TrackFinished(
                        track_id.to_string(),
                    )));
                self.current_track_source_path = None;
            }
        }
    }

    fn handle_media_status(&mut self, status: MediaStatus) {
        if let Some(track_id) = self.current_track_id.clone() {
            let elapsed_ms = (status.current_time_s.max(0.0) * 1000.0).round() as u64;
            let total_ms = (status.duration_s.max(0.0) * 1000.0).round() as u64;
            let _ = self
                .bus_producer
                .send(Message::Playback(PlaybackMessage::PlaybackProgress {
                    elapsed_ms,
                    total_ms,
                }));
            if status.player_state == "IDLE" {
                match status.idle_reason.as_deref() {
                    Some("FINISHED") => {
                        self.current_track_id = None;
                        self.current_track_source_path = None;
                        let _ = self
                            .bus_producer
                            .send(Message::Playback(PlaybackMessage::TrackFinished(track_id)));
                    }
                    Some("ERROR") | Some("CANCELLED") => {
                        if self.current_path_kind == Some(CastPlaybackPathKind::Direct)
                            && self.allow_transcode_fallback
                        {
                            let source_path = self.current_track_source_path.clone();
                            let retry_track_id = track_id.clone();
                            if let Some(source_path) = source_path {
                                let retry_offset_ms =
                                    (status.current_time_s.max(0.0) * 1000.0).round() as u64;
                                match self.load_track_with_mode(
                                    &retry_track_id,
                                    source_path,
                                    retry_offset_ms,
                                    CastPlaybackPathKind::TranscodeWavPcm,
                                ) {
                                    Ok(()) => {
                                        let _ = self.bus_producer.send(Message::Cast(
                                            CastMessage::PlaybackError {
                                                track_id: Some(retry_track_id),
                                                message: "Direct cast failed on receiver. Retrying with WAV PCM."
                                                    .to_string(),
                                                can_retry_with_transcode: false,
                                            },
                                        ));
                                        return;
                                    }
                                    Err(err) => {
                                        warn!(
                                            "CastManager: fallback retry after direct receiver error failed: {}",
                                            err
                                        );
                                    }
                                }
                            }
                        }
                        self.current_track_id = None;
                        self.current_track_source_path = None;
                        let _ = self
                            .bus_producer
                            .send(Message::Cast(CastMessage::PlaybackError {
                            track_id: Some(track_id.clone()),
                            message:
                                "Cast receiver failed to play this track. Skipping to next track."
                                    .to_string(),
                            can_retry_with_transcode: !self.allow_transcode_fallback,
                        }));
                        let _ = self
                            .bus_producer
                            .send(Message::Playback(PlaybackMessage::TrackFinished(track_id)));
                    }
                    _ => {}
                }
            }
        }
    }

    fn pump_cast_messages(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let drained = match session.drain_messages() {
            Ok(messages) => messages,
            Err(err) => {
                warn!("CastManager: cast socket read failed: {}", err);
                self.disconnect();
                return;
            }
        };
        for (namespace, payload) in drained {
            if namespace == CAST_NAMESPACE_MEDIA {
                if let Some(status) = parse_media_status(&payload) {
                    self.handle_media_status(status);
                }
            }
        }
    }

    fn poll_status_if_needed(&mut self) {
        if self.session.is_none() {
            return;
        }
        if self.last_status_poll_at.elapsed() < DEFAULT_POLL_INTERVAL {
            return;
        }
        self.last_status_poll_at = Instant::now();
        if let Some(session) = self.session.as_mut() {
            if let Err(err) = session.request_media_status() {
                warn!("CastManager: failed to request media status: {}", err);
                self.disconnect();
            }
        }
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::Config(crate::protocol::ConfigMessage::ConfigChanged(config)) => {
                self.allow_transcode_fallback = config.cast.allow_transcode_fallback;
            }
            Message::Cast(CastMessage::DiscoverDevices) => self.discover_devices(),
            Message::Cast(CastMessage::Connect { device_id }) => self.connect_device(&device_id),
            Message::Cast(CastMessage::Disconnect) => self.disconnect(),
            Message::Cast(CastMessage::LoadTrack {
                track_id,
                path,
                start_offset_ms,
            }) => self.load_track(&track_id, path, start_offset_ms),
            Message::Cast(CastMessage::Play) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.play() {
                        warn!("CastManager: play command failed: {}", err);
                    }
                }
            }
            Message::Cast(CastMessage::Pause) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.pause() {
                        warn!("CastManager: pause command failed: {}", err);
                    }
                }
            }
            Message::Cast(CastMessage::Stop) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.stop() {
                        warn!("CastManager: stop command failed: {}", err);
                    }
                }
            }
            Message::Cast(CastMessage::SeekMs(position_ms)) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.seek_ms(position_ms) {
                        warn!("CastManager: seek command failed: {}", err);
                    }
                }
            }
            Message::Cast(CastMessage::SetVolume(volume)) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.set_volume(volume) {
                        warn!("CastManager: set volume command failed: {}", err);
                    }
                }
            }
            Message::Playback(PlaybackMessage::SetVolume(volume)) => {
                if let Some(session) = self.session.as_mut() {
                    if let Err(err) = session.set_volume(volume) {
                        warn!("CastManager: mirrored volume command failed: {}", err);
                    }
                }
            }
            Message::Cast(
                CastMessage::DevicesUpdated(_)
                | CastMessage::ConnectionStateChanged { .. }
                | CastMessage::PlaybackPathChanged { .. }
                | CastMessage::PlaybackError { .. },
            ) => {}
            _ => {}
        }
    }

    fn process_pending_bus_messages(&mut self) -> bool {
        loop {
            match self.bus_consumer.try_recv() {
                Ok(message) => self.handle_message(message),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return false,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    warn!("CastManager: bus lagged by {} messages", skipped);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return true,
            }
        }
    }

    /// Starts the blocking cast manager loop.
    pub fn run(&mut self) {
        info!("CastManager: started");
        loop {
            if self.process_pending_bus_messages() {
                break;
            }
            self.pump_cast_messages();
            self.poll_status_if_needed();
            if self.process_pending_bus_messages() {
                break;
            }
            thread::sleep(IDLE_LOOP_SLEEP);
        }
    }
}
