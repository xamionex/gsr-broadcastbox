//! `watch`: play the live WHEP stream in mpv through a local MPEG-TS bridge.
//!
//! GStreamer's WHEP client advertises a constrained baseline H264 profile that
//! the Broadcast Box endpoint rejects. A small HTTP proxy on loopback rewrites
//! that offer and adds the stream key, so the key never appears in the
//! gst-launch or mpv arguments. The bridge remuxes the WebRTC RTP streams into
//! MPEG-TS and a loopback relay serves mpv a current program table before the
//! live packets. The relay is needed because tcpserversink sends every new
//! client the original caps streamheader, which can predate the video stream.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::process;

const PROXY_TIMEOUT: Duration = Duration::from_secs(20);
const BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(15);
const BRIDGE_STREAMS_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_READ_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const PMT_TABLE_ID: u8 = 0x02;
const SDP_PROFILE_FROM: &str = "profile-level-id=42e016";
const SDP_PROFILE_TO: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// Play the live stream described by the configuration.
pub fn run(config: &Config, stream_key: &str) -> Result<(), String> {
    let whep_url = whep_url(config);
    let video = VideoChain::for_codec(&config.codec)?;
    let audio = config.audio_argument().is_some();
    if audio && config.audio_codec != "opus" {
        return Err(format!(
            "watch needs Opus audio, but audio_codec is {:?}. Set audio_codec = \"opus\" or remove audio_sources.",
            config.audio_codec
        ));
    }

    ensure_tools(&video, audio)?;

    let proxy = start_proxy(&whep_url, stream_key)?;
    let bridge_port = free_tcp_port()?;

    println!(
        "Watching {}",
        crate::config::watch_url(&config.watch_base_url, stream_key)
    );
    if audio {
        println!("Bridge: {} video and Opus audio over MPEG-TS", video.label);
    } else {
        println!("Bridge: {} video over MPEG-TS", video.label);
    }

    install_signals();

    let mut gst = Command::new("gst-launch-1.0")
        .args(gst_args(&proxy.endpoint(), bridge_port, &video, audio))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to start gst-launch-1.0: {e}"))?;

    let ready = wait_for_listener(&mut gst, bridge_port);
    if ready.is_err() || SHUTDOWN.load(Ordering::Relaxed) {
        stop_child(&mut gst, Duration::from_secs(2));
        return ready;
    }

    let expected_streams = if audio { 2 } else { 1 };
    let relay = match Relay::start(bridge_port, expected_streams) {
        Ok(relay) => relay,
        Err(error) => {
            stop_child(&mut gst, Duration::from_secs(2));
            return Err(error);
        }
    };

    let ready = relay.wait_ready(&mut gst);
    if ready.is_err() || SHUTDOWN.load(Ordering::Relaxed) {
        relay.stop();
        stop_child(&mut gst, Duration::from_secs(2));
        return ready;
    }

    let mut mpv = Command::new("mpv")
        .args(mpv_args(relay.port, config.watch_hardware_decode))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            relay.stop();
            stop_child(&mut gst, Duration::from_secs(2));
            format!("failed to start mpv: {e}")
        })?;

    let result = monitor(&mut gst, &mut mpv, &relay);
    relay.stop();
    stop_child(&mut gst, Duration::from_secs(2));
    stop_child(&mut mpv, Duration::from_secs(2));
    result
}

/// Derive the WHEP endpoint from the configured WHIP endpoint.
///
/// `WHEP_URL` overrides the derived value.
fn whep_url(config: &Config) -> String {
    if let Some(value) = non_empty_env("WHEP_URL") {
        return value;
    }
    derived_whep_url(config)
}

fn derived_whep_url(config: &Config) -> String {
    let whip = config.whip_url.trim().trim_end_matches('/');
    if let Some(prefix) = whip.strip_suffix("/whip") {
        return format!("{prefix}/whep");
    }
    format!(
        "{}/api/whep",
        config.watch_base_url.trim().trim_end_matches('/')
    )
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug)]
struct VideoChain {
    label: &'static str,
    caps: &'static str,
    depay: &'static str,
    parse: &'static [&'static str],
    parsed_caps: &'static str,
}

impl VideoChain {
    fn for_codec(codec: &str) -> Result<Self, String> {
        match codec {
            // gpu-screen-recorder resolves "auto" to H264.
            "h264" | "auto" => Ok(VideoChain {
                label: "H264",
                caps: "application/x-rtp,media=video,encoding-name=H264",
                depay: "rtph264depay",
                parse: &["h264parse", "config-interval=-1"],
                parsed_caps: "video/x-h264,stream-format=byte-stream,alignment=au",
            }),
            "hevc" => Ok(VideoChain {
                label: "HEVC",
                caps: "application/x-rtp,media=video,encoding-name=H265",
                depay: "rtph265depay",
                parse: &["h265parse", "config-interval=1"],
                parsed_caps: "video/x-h265,stream-format=byte-stream,alignment=au",
            }),
            "av1" => Ok(VideoChain {
                label: "AV1",
                caps: "application/x-rtp,media=video,encoding-name=AV1",
                depay: "rtpav1depay",
                parse: &["av1parse"],
                parsed_caps: "video/x-av1,stream-format=obu-stream,alignment=frame",
            }),
            "vp9" => Ok(VideoChain {
                label: "VP9",
                caps: "application/x-rtp,media=video,encoding-name=VP9",
                depay: "rtpvp9depay",
                parse: &["vp9parse"],
                parsed_caps: "video/x-vp9,alignment=frame",
            }),
            "vp8" => Err(
                "watch cannot mux VP8 into MPEG-TS. Set codec to h264, hevc, av1 or vp9."
                    .to_string(),
            ),
            other => Err(format!(
                "watch does not support the {other:?} video codec. Set codec to h264, hevc, av1 or vp9."
            )),
        }
    }

    fn parse_element(&self) -> &'static str {
        self.parse[0]
    }
}

fn ensure_tools(video: &VideoChain, audio: bool) -> Result<(), String> {
    let missing_programs: Vec<&str> = ["gst-launch-1.0", "gst-inspect-1.0", "mpv"]
        .into_iter()
        .filter(|program| !command_exists(program))
        .collect();
    if !missing_programs.is_empty() {
        return Err(format!(
            "watch needs these programs in PATH: {}. Install GStreamer and mpv.",
            missing_programs.join(", ")
        ));
    }

    let mut elements = vec![
        "whepclientsrc",
        video.depay,
        video.parse_element(),
        "mpegtsmux",
        "tcpserversink",
    ];
    if audio {
        elements.extend(["rtpopusdepay", "opusparse"]);
    }

    let missing_elements: Vec<&str> = elements
        .into_iter()
        .filter(|element| !gst_element_exists(element))
        .collect();
    if !missing_elements.is_empty() {
        return Err(format!(
            "watch is missing GStreamer elements: {}. Install gst-plugins-base, gst-plugins-good and gst-plugins-bad, plus the rswebrtc plugin (gst-plugin-rswebrtc on Arch, gstreamer1.0-plugins-rs on Debian and Ubuntu).",
            missing_elements.join(", ")
        ));
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        directory
            .join(name)
            .metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn gst_element_exists(name: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn gst_args(whep_endpoint: &str, bridge_port: u16, video: &VideoChain, audio: bool) -> Vec<String> {
    let mut args = vec![
        "-q".to_string(),
        "-e".to_string(),
        "whepclientsrc".to_string(),
        "name=src".to_string(),
        format!("signaller::whep-endpoint={whep_endpoint}"),
        "src.video_0".to_string(),
        "!".to_string(),
        "queue".to_string(),
        "!".to_string(),
        video.caps.to_string(),
        "!".to_string(),
        video.depay.to_string(),
        "!".to_string(),
    ];
    args.extend(video.parse.iter().map(|token| token.to_string()));
    args.extend(
        [
            "!",
            video.parsed_caps,
            "!",
            "queue",
            "!",
            "mpegtsmux",
            "name=mux",
            "!",
            "tcpserversink",
            "host=127.0.0.1",
        ]
        .into_iter()
        .map(str::to_string),
    );
    args.push(format!("port={bridge_port}"));
    args.push("sync=false".to_string());

    if audio {
        args.extend(
            [
                "src.audio_0",
                "!",
                "queue",
                "!",
                "application/x-rtp,media=audio,encoding-name=OPUS",
                "!",
                "rtpopusdepay",
                "!",
                "opusparse",
                "!",
                "queue",
                "!",
                "mux.",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    args
}

fn mpv_args(relay_port: u16, hardware_decode: bool) -> Vec<String> {
    let mut args = vec!["--force-window=yes".to_string(), "--cache=no".to_string()];
    if hardware_decode {
        args.push("--hwdec=auto-safe".to_string());
        args.push("--vd-lavc-threads=1".to_string());
    } else {
        // Software decoding measured lower latency than VAAPI on the reference machine.
        args.push("--hwdec=no".to_string());
        args.push("--vd-lavc-threads=4".to_string());
    }
    args.push(format!("tcp://127.0.0.1:{relay_port}"));
    args
}

fn free_tcp_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to reserve a loopback port: {e}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|e| format!("failed to read the reserved port: {e}"))
}

/// Wait until gst-launch has opened the MPEG-TS listener.
fn wait_for_listener(gst: &mut Child, port: u16) -> Result<(), String> {
    let deadline = Instant::now() + BRIDGE_READY_TIMEOUT;
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(status) = gst
            .try_wait()
            .map_err(|e| format!("failed to check gst-launch-1.0: {e}"))?
        {
            return Err(format!(
                "gst-launch-1.0 exited before the bridge was ready ({status})"
            ));
        }
        if tcp_port_listening(port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the MPEG-TS bridge".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A loopback relay that serves mpv a current program table and live packets.
///
/// tcpserversink sends the caps streamheader to every new client. mpegtsmux
/// builds that streamheader once, before the video caps are known, so it can
/// advertise audio only. mpv then probes an audio-only program table and never
/// selects the video track. The relay keeps a single connection to GStreamer,
/// remembers the newest complete program table, and starts each mpv connection
/// with that table.
struct Relay {
    port: u16,
    state: Arc<RelayState>,
}

#[derive(Default)]
struct RelayState {
    ready: AtomicBool,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Relay {
    fn start(gst_port: u16, expected_streams: usize) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("failed to start the MPEG-TS relay: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed to configure the MPEG-TS relay: {e}"))?;
        let port = listener
            .local_addr()
            .map(|address| address.port())
            .map_err(|e| format!("failed to read the relay port: {e}"))?;
        let state = Arc::new(RelayState::default());
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("watch-relay".to_string())
            .spawn(move || relay_loop(listener, gst_port, expected_streams, thread_state))
            .map_err(|e| format!("failed to start the MPEG-TS relay thread: {e}"))?;
        Ok(Self { port, state })
    }

    fn wait_ready(&self, gst: &mut Child) -> Result<(), String> {
        let deadline = Instant::now() + BRIDGE_STREAMS_TIMEOUT;
        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(status) = gst
                .try_wait()
                .map_err(|e| format!("failed to check gst-launch-1.0: {e}"))?
            {
                return Err(format!(
                    "gst-launch-1.0 exited before the stream was ready ({status})"
                ));
            }
            if let Some(error) = self.error() {
                return Err(error);
            }
            if self.state.ready.load(Ordering::Acquire) {
                return Ok(());
            }
            if self.state.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "timed out waiting for the WHEP stream to announce every codec".to_string(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn error(&self) -> Option<String> {
        self.state.error.lock().ok().and_then(|error| error.clone())
    }

    fn stop(&self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

impl RelayState {
    fn fail(&self, message: String) {
        if !self.stop.load(Ordering::Relaxed) {
            if let Ok(mut error) = self.error.lock() {
                *error = Some(message);
            }
        }
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn relay_loop(
    listener: TcpListener,
    gst_port: u16,
    expected_streams: usize,
    state: Arc<RelayState>,
) {
    let mut gst = match TcpStream::connect(("127.0.0.1", gst_port)) {
        Ok(stream) => stream,
        Err(error) => {
            state.fail(format!("failed to connect to the MPEG-TS bridge: {error}"));
            return;
        }
    };
    let _ = gst.set_nodelay(true);
    let _ = gst.set_read_timeout(Some(RELAY_READ_TIMEOUT));

    let mut reader = RelayReader::new(expected_streams);
    let mut client = None;
    let mut chunk = [0u8; 64 * 1024];

    loop {
        if state.stop.load(Ordering::Relaxed) {
            return;
        }

        if client.is_none() && reader.ready() {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    let Some((pat, pmt)) = reader.headers() else {
                        continue;
                    };
                    let mut stream = stream;
                    if stream.write_all(&pat).is_err() || stream.write_all(&pmt).is_err() {
                        continue;
                    }
                    client = Some(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    state.fail(format!("failed to accept the mpv connection: {error}"));
                    return;
                }
            }
        }

        match gst.read(&mut chunk) {
            Ok(0) => {
                state.fail("the MPEG-TS bridge closed the connection".to_string());
                return;
            }
            Ok(read) => {
                let packets = reader.push(&chunk[..read]);
                if let Some(stream) = client.as_mut() {
                    if !packets.is_empty() && stream.write_all(&packets).is_err() {
                        state.stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                if reader.ready() {
                    state.ready.store(true, Ordering::Release);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                state.fail(format!("failed to read from the MPEG-TS bridge: {error}"));
                return;
            }
        }
    }
}

struct RelayReader {
    buffer: Vec<u8>,
    expected_streams: usize,
    pat: Option<[u8; TS_PACKET_SIZE]>,
    pmt: Option<[u8; TS_PACKET_SIZE]>,
}

impl RelayReader {
    fn new(expected_streams: usize) -> Self {
        Self {
            buffer: Vec::new(),
            expected_streams,
            pat: None,
            pmt: None,
        }
    }

    /// Add data and return the complete TS packets, retaining any partial tail.
    fn push(&mut self, data: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(data);
        let Some(start) = find_packet_start(&self.buffer) else {
            self.buffer.clear();
            return Vec::new();
        };
        if start > 0 {
            self.buffer.drain(..start);
        }

        let complete = self.buffer.len() / TS_PACKET_SIZE * TS_PACKET_SIZE;
        let packets = self.buffer[..complete].to_vec();
        for packet in packets.chunks_exact(TS_PACKET_SIZE) {
            self.observe(packet);
        }
        self.buffer.drain(..complete);
        packets
    }

    fn observe(&mut self, packet: &[u8]) {
        if packet_is_pat(packet) {
            let mut pat = [0u8; TS_PACKET_SIZE];
            pat.copy_from_slice(packet);
            self.pat = Some(pat);
        }
        if let Some(streams) = packet_pmt_streams(packet) {
            if streams >= self.expected_streams {
                let mut pmt = [0u8; TS_PACKET_SIZE];
                pmt.copy_from_slice(packet);
                self.pmt = Some(pmt);
            }
        }
    }

    fn ready(&self) -> bool {
        self.pat.is_some() && self.pmt.is_some()
    }

    fn headers(&self) -> Option<([u8; TS_PACKET_SIZE], [u8; TS_PACKET_SIZE])> {
        Some((self.pat?, self.pmt?))
    }
}

/// Find a TS packet boundary, checking the next packet when it is available.
fn find_packet_start(buffer: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while offset < buffer.len() {
        if buffer[offset] == TS_SYNC_BYTE {
            let next = offset + TS_PACKET_SIZE;
            let aligned = next >= buffer.len() || buffer[next] == TS_SYNC_BYTE;
            if aligned {
                return Some(offset);
            }
        }
        offset += 1;
    }
    None
}

/// Return the PID and payload of a TS packet whose payload starts a section.
fn packet_payload(packet: &[u8]) -> Option<(u16, &[u8])> {
    if packet.len() != TS_PACKET_SIZE || packet[0] != TS_SYNC_BYTE {
        return None;
    }
    if packet[1] & 0x40 == 0 {
        return None;
    }
    let pid = (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16;
    let adaptation = (packet[3] >> 4) & 0x03;
    if adaptation != 1 && adaptation != 3 {
        return None;
    }

    let mut offset = 4;
    if adaptation == 3 {
        offset += 1 + packet[4] as usize;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    offset += 1 + packet[offset] as usize;
    if offset > TS_PACKET_SIZE {
        return None;
    }
    Some((pid, &packet[offset..]))
}

/// Return true for a valid program association table on PID zero.
fn packet_is_pat(packet: &[u8]) -> bool {
    let Some((pid, section)) = packet_payload(packet) else {
        return false;
    };
    if pid != 0 || section.len() < 12 || section[0] != 0 {
        return false;
    }
    let section_length = (((section[1] & 0x0f) as usize) << 8) | section[2] as usize;
    section_length >= 9
        && (section_length - 9).is_multiple_of(4)
        && 3 + section_length <= section.len()
}

/// Count the elementary streams in a single-packet PMT section.
fn packet_pmt_streams(packet: &[u8]) -> Option<usize> {
    let (_, section) = packet_payload(packet)?;
    if section.len() < 12 || section[0] != PMT_TABLE_ID {
        return None;
    }

    let section_length = (((section[1] & 0x0f) as usize) << 8) | section[2] as usize;
    let end = 3 + section_length;
    let crc_start = end.checked_sub(4)?;
    if end > section.len() || crc_start < 12 {
        return None;
    }

    let program_info_length = (((section[10] & 0x0f) as usize) << 8) | section[11] as usize;
    let mut cursor = 12 + program_info_length;
    if cursor > crc_start {
        return None;
    }
    let mut streams = 0;
    while cursor + 5 <= crc_start {
        let es_info_length =
            (((section[cursor + 3] & 0x0f) as usize) << 8) | section[cursor + 4] as usize;
        streams += 1;
        cursor += 5 + es_info_length;
    }
    Some(streams)
}

fn tcp_port_listening(port: u16) -> bool {
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in contents.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let _slot = fields.next();
            let Some(local) = fields.next() else {
                continue;
            };
            let Some((_, local_port)) = local.rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(local_port, 16).ok() != Some(port) {
                continue;
            }
            let _remote = fields.next();
            if fields.next() == Some("0A") {
                return true;
            }
        }
    }
    false
}

fn monitor(gst: &mut Child, mpv: &mut Child, relay: &Relay) -> Result<(), String> {
    let mut gst_status = None;
    let mut mpv_status = None;

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(());
        }
        if gst_status.is_none() {
            gst_status = gst
                .try_wait()
                .map_err(|e| format!("failed to check gst-launch-1.0: {e}"))?;
        }
        if mpv_status.is_none() {
            mpv_status = mpv
                .try_wait()
                .map_err(|e| format!("failed to check mpv: {e}"))?;
        }
        if gst_status.is_some() || mpv_status.is_some() {
            break;
        }
        if let Some(error) = relay.error() {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    if let Some(status) = gst_status {
        if !status.success() {
            return Err(format!(
                "the WHEP bridge stopped ({status}). Check that the stream is live and that codec matches the stream."
            ));
        }
    }
    if let Some(status) = mpv_status {
        if !status.success() {
            return Err(format!("mpv exited ({status})"));
        }
    }
    Ok(())
}

fn stop_child(child: &mut Child, grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = child.id();
    process::signal(pid, libc::SIGINT);
    if wait_child(child, grace) {
        return;
    }
    process::signal(pid, libc::SIGTERM);
    if wait_child(child, Duration::from_secs(2)) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn install_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

#[derive(Debug, Clone)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Upstream {
    origin: String,
    path: String,
    agent: ureq::Agent,
    auth: ureq::http::HeaderValue,
}

struct Proxy {
    port: u16,
    endpoint_path: String,
}

impl Proxy {
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.endpoint_path)
    }
}

fn start_proxy(upstream_url: &str, stream_key: &str) -> Result<Proxy, String> {
    let (origin, path) = split_url(upstream_url)?;
    let auth = ureq::http::HeaderValue::from_str(&format!("Bearer {stream_key}"))
        .map_err(|_| "the stream key cannot be sent in an HTTP header".to_string())?;
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(PROXY_TIMEOUT))
        .build()
        .new_agent();

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to start the WHEP proxy: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read the WHEP proxy address: {e}"))?
        .port();

    let endpoint_path = path.clone();
    let upstream = Upstream {
        origin,
        path,
        agent,
        auth,
    };
    std::thread::Builder::new()
        .name("whep-proxy".to_string())
        .spawn(move || serve(listener, upstream))
        .map_err(|e| format!("failed to start the WHEP proxy: {e}"))?;

    Ok(Proxy {
        port,
        endpoint_path,
    })
}

fn split_url(url: &str) -> Result<(String, String), String> {
    let url = url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("invalid WHEP URL {url:?}"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "WHEP URL must start with http:// or https:// (got {url:?})"
        ));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(format!("WHEP URL has no host (got {url:?})"));
    }
    let tail = &rest[authority_end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];
    let path = if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    };
    Ok((format!("{scheme}://{authority}"), path))
}

fn serve(listener: TcpListener, upstream: Upstream) {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if let Err(error) = handle_connection(stream, &upstream) {
                    eprintln!("gsr-broadcastbox: WHEP proxy: {error}");
                }
            }
            Err(error) => {
                eprintln!("gsr-broadcastbox: WHEP proxy stopped: {error}");
                return;
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, upstream: &Upstream) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let local_origin = stream
        .local_addr()
        .map(|address| format!("http://{address}"))
        .map_err(|e| format!("failed to read the proxy address: {e}"))?;
    let reader_stream = stream
        .try_clone()
        .map_err(|e| format!("failed to read the proxy request: {e}"))?;
    let mut reader = BufReader::new(reader_stream);

    let request = match read_request(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(error) => {
            let _ = write_response(&mut stream, 400, &[], error.as_bytes());
            return Ok(());
        }
    };

    if request.method == "OPTIONS" {
        let headers = vec![
            ("Access-Control-Allow-Headers".to_string(), "*".to_string()),
            ("Access-Control-Allow-Methods".to_string(), "*".to_string()),
            ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
        ];
        return write_response(&mut stream, 204, &headers, &[]);
    }

    if !matches!(request.method.as_str(), "POST" | "PATCH" | "DELETE") {
        return write_response(&mut stream, 405, &[], b"method not allowed");
    }

    let body = if request.method == "POST" {
        rewrite_offer(&request.body)?
    } else {
        request.body.clone()
    };
    let url = format!("{}{}", upstream.origin, request.target);
    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&url);
    builder = builder.header("Authorization", upstream.auth.clone());
    for (name, value) in &request.headers {
        if forward_request_header(name) {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    let upstream_request = builder
        .body(body)
        .map_err(|e| format!("failed to build the WHEP request: {e}"))?;

    let mut response = match upstream.agent.run(upstream_request) {
        Ok(response) => response,
        Err(error) => {
            let message = format!("WHEP upstream request failed: {error}");
            eprintln!("gsr-broadcastbox: {message}");
            return write_response(&mut stream, 502, &[], message.as_bytes());
        }
    };

    let status = response.status().as_u16();
    let mut headers = Vec::new();
    for (name, value) in response.headers() {
        if skip_response_header(name.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        if name.as_str().eq_ignore_ascii_case("location") {
            headers.push((
                name.to_string(),
                rewrite_location(value, upstream, &local_origin),
            ));
        } else {
            headers.push((name.to_string(), value.to_string()));
        }
    }

    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES as u64)
        .read_to_vec()
        .map_err(|e| format!("failed to read the WHEP response: {e}"))?;
    write_response(&mut stream, status, &headers, &body)
}

fn read_request<R: BufRead>(reader: &mut R) -> Result<Option<Request>, String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) => return Err(format!("failed to read the request line: {error}")),
    }

    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Err("empty request line".to_string());
    }
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing request method".to_string())?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| "missing request target".to_string())?
        .to_string();
    if !target.starts_with('/') {
        return Err(format!("unsupported request target {target:?}"));
    }

    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|e| format!("failed to read a request header: {e}"))?;
        if read == 0 {
            return Err("connection closed while reading request headers".to_string());
        }
        total += read;
        if total > MAX_HEADER_BYTES {
            return Err("request headers are too large".to_string());
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(format!("malformed request header {header:?}"));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| {
            value
                .parse::<usize>()
                .map_err(|_| format!("invalid Content-Length {value:?}"))
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("request body is too large".to_string());
    }

    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("failed to read the request body: {e}"))?;

    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn rewrite_offer(body: &[u8]) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(body).map_err(|_| "the WHEP offer is not valid UTF-8".to_string())?;
    let mut out = String::with_capacity(body.len() + 64);
    let mut changed = false;

    for line in text.split_inclusive('\n') {
        let (content, ending) = if let Some(content) = line.strip_suffix("\r\n") {
            (content, "\r\n")
        } else if let Some(content) = line.strip_suffix('\n') {
            (content, "\n")
        } else {
            (line, "")
        };

        if content.starts_with("a=fmtp:") && content.contains(SDP_PROFILE_FROM) {
            let params = &content["a=fmtp:".len()..];
            let Some(payload) = params.split_whitespace().next() else {
                out.push_str(line);
                continue;
            };
            out.push_str("a=fmtp:");
            out.push_str(payload);
            out.push(' ');
            out.push_str(SDP_PROFILE_TO);
            out.push_str(ending);
            changed = true;
        } else {
            out.push_str(line);
        }
    }

    Ok(if changed {
        out.into_bytes()
    } else {
        body.to_vec()
    })
}

fn rewrite_location(value: &str, upstream: &Upstream, local_origin: &str) -> String {
    let path = if let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    {
        match rest.find('/') {
            Some(index) => &rest[index..],
            None => "/",
        }
    } else {
        value
    };

    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", upstream.path.trim_end_matches('/'), path)
    };
    format!("{local_origin}{path}")
}

fn forward_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-type" | "accept" | "if-match"
    )
}

fn skip_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-encoding"
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(), String> {
    let mut response = Vec::with_capacity(body.len() + 256);
    write!(
        &mut response,
        "HTTP/1.1 {status} {}\r\n",
        reason_phrase(status)
    )
    .map_err(|e| format!("failed to build the proxy response: {e}"))?;
    for (name, value) in headers {
        write!(&mut response, "{name}: {value}\r\n")
            .map_err(|e| format!("failed to build the proxy response: {e}"))?;
    }
    write!(
        &mut response,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .map_err(|e| format!("failed to build the proxy response: {e}"))?;
    response.extend_from_slice(body);
    stream
        .write_all(&response)
        .map_err(|e| format!("failed to write the proxy response: {e}"))?;
    let _ = stream.flush();
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn whep_url_replaces_the_whip_suffix() {
        let mut config = config();
        config.whip_url = "https://broadcast.example/api/whip".to_string();
        config.watch_base_url = "https://watch.example".to_string();
        assert_eq!(
            derived_whep_url(&config),
            "https://broadcast.example/api/whep"
        );

        config.whip_url = "https://broadcast.example/api/whip/".to_string();
        assert_eq!(
            derived_whep_url(&config),
            "https://broadcast.example/api/whep"
        );
    }

    #[test]
    fn whep_url_falls_back_to_the_watch_base() {
        let mut config = config();
        config.whip_url = "https://broadcast.example/custom-ingest".to_string();
        config.watch_base_url = "https://watch.example/".to_string();
        assert_eq!(derived_whep_url(&config), "https://watch.example/api/whep");
    }

    #[test]
    fn offer_profile_is_rewritten_and_line_endings_are_kept() {
        let offer =
            b"v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=fmtp:96 profile-level-id=42e016\r\n";
        let rewritten = rewrite_offer(offer).unwrap();
        let text = String::from_utf8(rewritten).unwrap();
        assert!(text.contains(
            "a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n"
        ));
    }

    #[test]
    fn an_offer_without_the_old_profile_is_unchanged() {
        let offer = b"v=0\r\na=fmtp:96 profile-level-id=42e01f\r\n";
        assert_eq!(rewrite_offer(offer).unwrap(), offer.to_vec());
    }

    #[test]
    fn urls_are_split_into_origin_and_path() {
        assert_eq!(
            split_url("https://broadcast.example/api/whep").unwrap(),
            (
                "https://broadcast.example".to_string(),
                "/api/whep".to_string()
            )
        );
        assert_eq!(
            split_url("http://127.0.0.1:8080/api/whep/").unwrap(),
            ("http://127.0.0.1:8080".to_string(), "/api/whep".to_string())
        );
        assert_eq!(
            split_url("https://broadcast.example").unwrap(),
            ("https://broadcast.example".to_string(), "/".to_string())
        );
    }

    #[test]
    fn locations_are_rewritten_to_the_local_proxy() {
        let upstream = Upstream {
            origin: "https://broadcast.example".to_string(),
            path: "/api/whep".to_string(),
            agent: ureq::Agent::config_builder().build().new_agent(),
            auth: ureq::http::HeaderValue::from_static("Bearer test"),
        };

        assert_eq!(
            rewrite_location("/api/whep/session", &upstream, "http://127.0.0.1:9999"),
            "http://127.0.0.1:9999/api/whep/session"
        );
        assert_eq!(
            rewrite_location(
                "https://broadcast.example/api/whep/session",
                &upstream,
                "http://127.0.0.1:9999"
            ),
            "http://127.0.0.1:9999/api/whep/session"
        );
        assert_eq!(
            rewrite_location("session", &upstream, "http://127.0.0.1:9999"),
            "http://127.0.0.1:9999/api/whep/session"
        );
    }

    #[test]
    fn gst_arguments_use_named_pads_and_never_include_the_key() {
        let video = VideoChain::for_codec("h264").unwrap();
        let args = gst_args("http://127.0.0.1:1234/api/whep", 4321, &video, true);
        let joined = args.join(" ");
        assert!(joined.contains("src.video_0"));
        assert!(joined.contains("src.audio_0"));
        assert!(joined.contains("signaller::whep-endpoint=http://127.0.0.1:1234/api/whep"));
        assert!(joined.contains("mpegtsmux name=mux"));
        assert!(joined.contains("tcpserversink host=127.0.0.1 port=4321 sync=false"));
        assert!(joined.contains("mux."));
        assert!(!joined.contains("auth-token"));
        assert!(!joined.contains("secret"));
        assert!(args.iter().all(|argument| !argument.contains(' ')));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "h264parse" && pair[1] == "config-interval=-1"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "tcpserversink" && pair[1] == "host=127.0.0.1"));
    }

    #[test]
    fn mpv_uses_software_decode_by_default() {
        let args = mpv_args(4321, false);
        assert!(args.contains(&"--hwdec=no".to_string()));
        assert!(args.contains(&"--vd-lavc-threads=4".to_string()));
        assert!(!args
            .iter()
            .any(|argument| argument.starts_with("--hwdec=auto")));
        assert_eq!(args.last().unwrap(), "tcp://127.0.0.1:4321");
    }

    #[test]
    fn mpv_can_use_hardware_decode() {
        let args = mpv_args(4321, true);
        assert!(args.contains(&"--hwdec=auto-safe".to_string()));
        assert!(args.contains(&"--vd-lavc-threads=1".to_string()));
        assert!(!args.contains(&"--vd-lavc-threads=4".to_string()));
    }

    #[test]
    fn vp8_is_rejected_with_a_clear_message() {
        let error = VideoChain::for_codec("vp8").unwrap_err();
        assert!(error.contains("VP8"));
        assert!(VideoChain::for_codec("h264").is_ok());
        assert!(VideoChain::for_codec("auto").is_ok());
    }

    #[test]
    fn a_listening_port_is_detected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp_port_listening(port));
    }

    #[test]
    fn a_complete_program_table_announces_both_streams() {
        let mut reader = RelayReader::new(2);
        let mut data = Vec::new();
        data.extend_from_slice(&pat_packet());
        data.extend_from_slice(&pmt_packet(2));

        let packets = reader.push(&data);

        assert_eq!(packets.len(), 2 * TS_PACKET_SIZE);
        assert!(reader.ready());
        let (pat, pmt) = reader.headers().unwrap();
        assert_eq!(pat[0], TS_SYNC_BYTE);
        assert_eq!(pmt[0], TS_SYNC_BYTE);
    }

    #[test]
    fn an_audio_only_table_is_not_ready_for_two_streams() {
        let mut reader = RelayReader::new(2);
        let mut data = Vec::new();
        data.extend_from_slice(&pat_packet());
        data.extend_from_slice(&pmt_packet(1));

        reader.push(&data);

        assert!(!reader.ready());
    }

    #[test]
    fn a_table_split_across_reads_is_reassembled() {
        let mut reader = RelayReader::new(2);
        let mut data = Vec::new();
        data.extend_from_slice(&pat_packet());
        data.extend_from_slice(&pmt_packet(2));

        let first = reader.push(&data[..TS_PACKET_SIZE + 100]);
        assert_eq!(first.len(), TS_PACKET_SIZE);
        assert!(!reader.ready());

        let second = reader.push(&data[TS_PACKET_SIZE + 100..]);
        assert_eq!(second.len(), TS_PACKET_SIZE);
        assert!(reader.ready());
    }

    #[test]
    fn a_partial_packet_is_kept_for_the_next_read() {
        let mut reader = RelayReader::new(2);
        let packet = pat_packet();

        assert!(reader.push(&packet[..100]).is_empty());
        assert_eq!(reader.push(&packet[100..]).len(), TS_PACKET_SIZE);
    }

    #[test]
    fn a_program_association_table_must_come_from_pid_zero() {
        let mut packet = pat_packet();
        packet[2] = 0x01;

        assert!(!packet_is_pat(&packet));
    }

    fn pat_packet() -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xff; TS_PACKET_SIZE];
        packet[0] = TS_SYNC_BYTE;
        packet[1] = 0x40;
        packet[2] = 0x00;
        packet[3] = 0x10;
        packet[4] = 0x00;
        packet[5] = 0x00;
        let section_length = 9 + 4;
        packet[6] = 0xb0 | ((section_length >> 8) as u8 & 0x0f);
        packet[7] = (section_length & 0xff) as u8;
        packet[8] = 0x00;
        packet[9] = 0x01;
        packet[10] = 0xc1;
        packet[11] = 0x00;
        packet[12] = 0x00;
        packet[13] = 0x00;
        packet[14] = 0x01;
        packet[15] = 0xf0;
        packet[16] = 0x00;
        packet
    }

    fn pmt_packet(streams: usize) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xff; TS_PACKET_SIZE];
        packet[0] = TS_SYNC_BYTE;
        packet[1] = 0x40;
        packet[2] = 0x00;
        packet[3] = 0x10;
        packet[4] = 0x00;
        packet[5] = PMT_TABLE_ID;
        let section_length = 9 + streams * 5 + 4;
        packet[6] = 0xb0 | ((section_length >> 8) as u8 & 0x0f);
        packet[7] = (section_length & 0xff) as u8;
        packet[8] = 0x00;
        packet[9] = 0x01;
        packet[10] = 0xc1;
        packet[13] = 0x01;
        packet[14] = 0x00;
        packet[15] = 0xf0;
        packet[16] = 0x00;
        for stream in 0..streams {
            let offset = 17 + stream * 5;
            packet[offset] = 0x1b;
            packet[offset + 1] = 0x01;
            packet[offset + 2] = stream as u8;
            packet[offset + 3] = 0xf0;
            packet[offset + 4] = 0x00;
        }
        packet
    }

    #[test]
    fn the_proxy_rewrites_the_offer_and_injects_auth() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (sender, receiver) = mpsc::channel();

        let upstream_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = upstream.accept().unwrap();
                let reader_stream = stream.try_clone().unwrap();
                let mut reader = BufReader::new(reader_stream);
                let request = read_request(&mut reader).unwrap().unwrap();
                sender.send(request.clone()).unwrap();

                let (status, headers, body) = if request.method == "POST" {
                    (
                        201,
                        vec![("Location".to_string(), "/api/whep/session-1".to_string())],
                        b"answer".to_vec(),
                    )
                } else {
                    (200, Vec::new(), Vec::new())
                };
                write_response(&mut stream, status, &headers, &body).unwrap();
            }
        });

        let proxy = start_proxy(
            &format!("http://127.0.0.1:{upstream_port}/api/whep"),
            "secret",
        )
        .unwrap();

        let offer = b"v=0\r\na=fmtp:97 profile-level-id=42e016\r\n";
        let post = proxy_request(&proxy.endpoint(), "POST", offer);
        assert!(post.starts_with("HTTP/1.1 201 Created"));
        assert!(post
            .to_ascii_lowercase()
            .contains("location: http://127.0.0.1:"));

        let session = format!("{}/session-1", proxy.endpoint());
        let patch = proxy_request(&session, "PATCH", b"fragment");
        assert!(patch.starts_with("HTTP/1.1 200 OK"));

        let post = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(post.method, "POST");
        assert_eq!(post.target, "/api/whep");
        assert_eq!(header(&post, "authorization"), Some("Bearer secret"));
        let body = String::from_utf8(post.body).unwrap();
        assert!(body.contains(
            "a=fmtp:97 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        ));

        let patch = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(patch.method, "PATCH");
        assert_eq!(patch.target, "/api/whep/session-1");
        assert_eq!(header(&patch, "authorization"), Some("Bearer secret"));
        assert_eq!(patch.body, b"fragment".to_vec());

        upstream_thread.join().unwrap();
    }

    fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn proxy_request(url: &str, method: &str, body: &[u8]) -> String {
        let (_, rest) = url.split_once("://").unwrap();
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, "/".to_string()),
        };
        let mut stream = TcpStream::connect(authority).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
