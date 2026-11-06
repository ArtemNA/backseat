#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "h264" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            _ => Err(format!("unknown codec {s}, use h264 or h265")),
        }
    }

    /// Lower-case name used in element names: h264, h265.
    pub fn short(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
        }
    }

    pub fn rtp_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl VideoMode {
    /// Parses "720p30", "1080p60", "1440p60".
    pub fn parse(s: &str) -> Result<Self, String> {
        let (res, fps) = s.split_once('p').ok_or_else(|| format!("bad mode {s}, e.g. 1080p60"))?;
        let (width, height) = match res {
            "720" => (1280, 720),
            "1080" => (1920, 1080),
            "1440" => (2560, 1440),
            _ => return Err(format!("unsupported resolution {res}p, use 720, 1080 or 1440")),
        };
        let fps: u32 = fps.parse().map_err(|_| format!("bad frame rate in {s}"))?;
        if fps != 30 && fps != 60 {
            return Err(format!("unsupported frame rate {fps}, use 30 or 60"));
        }
        Ok(Self { width, height, fps })
    }
}

#[derive(Clone, Debug)]
pub enum Source {
    /// Monitor capture; -1 is the primary monitor.
    Capture { monitor: i32 },
    /// Moving colour bars and test ticks, for repeatable measurements.
    Bars,
    /// Random noise: the hardest content for an encoder, for bitrate ceilings.
    Noise,
}

/// How peers find a path to each other.
#[derive(Clone, Debug)]
pub struct IceConfig {
    /// Public STUN server, as stun://host:port, or None for local candidates only.
    pub stun: Option<String>,
    /// The user's own TURN server, as turn(s)://user:pass@host:port. Never logged.
    pub turn: Option<String>,
}

impl Default for IceConfig {
    fn default() -> Self {
        Self { stun: Some("stun://stun.l.google.com:19302".into()), turn: None }
    }
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub mode: VideoMode,
    pub kbit: u32,
    pub codec: Codec,
    /// Periodic keyframe interval; viewers never request keyframes (decision D1).
    pub keyframe_secs: u32,
    pub audio: bool,
    pub source: Source,
    pub ice: IceConfig,
}

#[derive(Clone, Debug)]
pub struct ViewerConfig {
    /// Jitterbuffer budget in ms. 40 was the lowest that recovered losses well in stage 0.
    pub latency_ms: u32,
    pub audio: bool,
    /// After a loss, show nothing until the next keyframe instead of corrupted frames.
    pub wait_keyframe: bool,
    /// Decoder element override, for comparisons.
    pub decoder: Option<String>,
    /// Audio sink follows stream timing (drift correction). Video never syncs.
    pub audio_sync: bool,
    /// Tests only: receive video but do not decode or show it.
    pub headless: bool,
    /// Tests only: loss, a slow link or delay applied to traffic from the host.
    pub impair: crate::netsim::Impairment,
    pub ice: IceConfig,
}
