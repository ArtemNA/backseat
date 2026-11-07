#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    /// Needs a hardware AV1 encoder at the host (NVIDIA RTX 40+, AMD RX 7000+, Intel Arc);
    /// any viewer decodes it, on the GPU when it can, else with dav1d on the CPU.
    Av1,
}

impl Codec {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "h264" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            _ => Err(format!("unknown codec {s}, use h264, h265 or av1")),
        }
    }

    /// Lower-case name used in element names: h264, h265, av1.
    pub fn short(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
        }
    }

    pub fn rtp_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
            Self::Av1 => "AV1",
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

/// What one encoder produces: size, frame rate and bitrate. The host's settings are the
/// ceiling; a viewer may ask for less, never more (user requirement 2026-11-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Quality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbit: u32,
}

/// Bitrates a viewer may ask for, kbit/s. Few on purpose: viewers asking for similar
/// qualities share one encoder instead of each starting their own (user, 2026-11-06).
/// 15 is AV1's default: about the picture H.265 gives at 20.
pub const KBIT_PRESETS: [u32; 8] = [2000, 4000, 8000, 12000, 15000, 20000, 35000, 50000];

impl Quality {
    pub fn new(mode: VideoMode, kbit: u32) -> Self {
        Self { width: mode.width, height: mode.height, fps: mode.fps, kbit }
    }

    /// Parses a viewer's request: "720p30 4000" (mode and kbit/s), or just "720p30".
    pub fn parse_request(s: &str, ceiling: Quality) -> Result<Self, String> {
        let mut parts = s.split_whitespace();
        let mode = VideoMode::parse(parts.next().ok_or("empty quality request")?)?;
        let kbit = match parts.next() {
            Some(k) => k.parse().map_err(|_| format!("bad bitrate {k}"))?,
            None => ceiling.kbit,
        };
        Ok(Self::new(mode, kbit))
    }

    /// Each part at most the host's: a smaller size only if it is smaller in both
    /// dimensions, otherwise the host's own size. The bitrate is the host's, or the largest
    /// preset below it that does not exceed the request (the smallest preset at least).
    pub fn clamp_to(self, ceiling: Quality) -> Self {
        let smaller = self.width < ceiling.width && self.height < ceiling.height;
        let (width, height) = if smaller { (self.width, self.height) } else { (ceiling.width, ceiling.height) };
        let kbit = if self.kbit >= ceiling.kbit {
            ceiling.kbit
        } else {
            let below = KBIT_PRESETS.iter().copied().filter(|&k| k < ceiling.kbit);
            below.clone().filter(|&k| k <= self.kbit).max().or_else(|| below.min()).unwrap_or(ceiling.kbit)
        };
        Self { width, height, fps: self.fps.min(ceiling.fps), kbit }
    }
}

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}x{}@{} {} kbit/s", self.width, self.height, self.fps, self.kbit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_never_gets_more_than_the_host_sends() {
        let ceiling = Quality::new(VideoMode::parse("1080p60").unwrap(), 8000);
        let ask = |s: &str| Quality::parse_request(s, ceiling).unwrap().clamp_to(ceiling);
        assert_eq!(ask("1440p60 50000"), ceiling);
        assert_eq!(ask("1080p30"), Quality { fps: 30, ..ceiling });
        assert_eq!(ask("720p60 4000"), Quality { width: 1280, height: 720, fps: 60, kbit: 4000 });
        assert_eq!(ask("720p30 100").kbit, 2000);
        // Requests snap down to presets, so similar asks share an encoder.
        assert_eq!(ask("720p30 5000").kbit, 4000);
        assert_eq!(ask("720p30 7999").kbit, 4000);
        // A host below every preset is the only choice.
        let low = Quality { kbit: 1500, ..ceiling };
        assert_eq!(Quality::parse_request("720p30 1000", low).unwrap().clamp_to(low).kbit, 1500);
        // Between the top preset under the host and the host's own: still capped at the host.
        let odd = Quality { kbit: 10000, ..ceiling };
        assert_eq!(Quality::parse_request("1080p60 9000", odd).unwrap().clamp_to(odd).kbit, 8000);
        assert!(Quality::parse_request("900p60", ceiling).is_err());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// Monitor capture by DXGI output index; -1 is the primary monitor.
    Capture { monitor: i32 },
    /// Monitor capture by HMONITOR, as listed by `platform::monitors`.
    Monitor { handle: u64 },
    /// One window by HWND, through Windows Graphics Capture. A minimized window sends
    /// nothing (stage 6 probe A).
    Window { hwnd: u64 },
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
    /// Capture only this process's audio (and its children) instead of the whole system.
    pub audio_pid: Option<u32>,
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
    /// Tests only: never answer the offer, like a stuck peer.
    pub debug_never_answer: bool,
    /// Tests only: neither send nor accept ICE candidates, so no path can exist.
    pub debug_no_ice: bool,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            latency_ms: 40,
            audio: true,
            wait_keyframe: false,
            decoder: None,
            audio_sync: true,
            headless: false,
            impair: Default::default(),
            ice: IceConfig::default(),
            debug_never_answer: false,
            debug_no_ice: false,
        }
    }
}
