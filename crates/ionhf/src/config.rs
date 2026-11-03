//! `ionhf.toml`, in [`Config::dir`] with the window state and the log.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const FILE: &str = "ionhf.toml";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub call: String,
    pub grid: String,
    /// Answer calls whenever no link is up.
    pub answer: bool,
    pub target: String,
    /// Newest first.
    pub recent: Vec<String>,
    pub mini: bool,
    /// The net to join (empty: the first heard).
    pub net: String,
    /// The net we run, kept between nets.
    pub net_profile: NetProfile,
    /// Our check-in line (the call comes from `call`).
    pub net_line: String,
    /// Percent.
    pub drive: u32,
    /// OFDM level above the single-tone signals, 0 to 3 dB.
    pub ofdm_db: f32,
    /// How the link picks its level (`ion::link::Gear`).
    pub gear: Gear,
    /// How OFDM bursts are clipped (`ion::phy::set_td_clip`).
    pub clipper: Clipper,
    /// Offer the stream gear (`ion::link::frame::caps::STREAM`); off: plain ARQ.
    pub stream: bool,
    pub host_enabled: bool,
    /// Command port; data is the next one up.
    pub host_port: u16,
    pub tci_url: String,
    pub tci_sample_rate: u32,
    pub loopback_snr_db: f32,
    pub radio: RadioSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            call: String::new(),
            grid: String::new(),
            answer: true,
            target: String::new(),
            recent: Vec::new(),
            mini: false,
            net: String::new(),
            net_profile: NetProfile::default(),
            net_line: String::new(),
            drive: 50,
            ofdm_db: ion::link::audio::OFDM_AIR_GAIN_DB,
            gear: Gear::Goodput,
            clipper: Clipper::Classic,
            stream: true,
            host_enabled: false,
            host_port: 8300,
            tci_url: "ws://127.0.0.1:50001".into(),
            tci_sample_rate: 48_000,
            loopback_snr_db: 10.0,
            radio: RadioSettings::default(),
        }
    }
}

impl Config {
    /// The program's own folder when an `ionhf.toml` is already there
    /// (portable), else the platform's place for application settings.
    pub fn dir() -> PathBuf {
        if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from))
            && d.join(FILE).is_file()
        {
            return d;
        }
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        if cfg!(windows) {
            if let Some(a) = env("APPDATA") {
                return a.join("Ion HF");
            }
        } else if cfg!(target_os = "macos") {
            if let Some(h) = env("HOME") {
                return h.join("Library").join("Application Support").join("Ion HF");
            }
        } else if let Some(x) = env("XDG_CONFIG_HOME") {
            return x.join("ionhf");
        } else if let Some(h) = env("HOME") {
            return h.join(".config").join("ionhf");
        }
        PathBuf::from(".")
    }

    pub fn path() -> PathBuf {
        Self::dir().join(FILE)
    }

    pub fn load() -> Config {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::warn!("{}: {e}; using defaults", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        let result = std::fs::create_dir_all(Self::dir())
            .and_then(|_| toml::to_string_pretty(self).map_err(|e| std::io::Error::other(e.to_string())))
            .and_then(|text| std::fs::write(&path, text));
        if let Err(e) = result {
            log::warn!("could not save {}: {e}", path.display());
        }
    }

    /// TCI's rate on the wire; anything but 12 or 24 kHz is 48 kHz.
    pub fn tci_rate(&self) -> u32 {
        match self.tci_sample_rate {
            12_000 | 24_000 => self.tci_sample_rate,
            _ => 48_000,
        }
    }
}

/// Net control's profile: the announcement's fields as typed, and the
/// regulars.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NetProfile {
    pub name: String,
    /// As typed ("3.975", "7071.5 kHz"): `net_doc::parse_freq`.
    pub freq: String,
    pub backup: String,
    /// "01:00" UTC, on the day it opens.
    pub start: String,
    pub end: String,
    /// "2026-11-10 01:00" UTC.
    pub next: String,
    pub mode: crate::net_doc::Mode,
    pub purpose: crate::net_doc::Purpose,
    pub alt: String,
    /// Calls, by spaces or commas.
    pub regulars: String,
    /// Markdown.
    pub body: String,
}

impl NetProfile {
    pub fn regulars(&self) -> Vec<String> {
        self.regulars
            .split([' ', ',', ';', '\n'])
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| !c.is_empty())
            .collect()
    }

    /// The announcement as it stands, net control `ncs`, for a net opening
    /// at `day` (UTC seconds).
    pub fn announcement(&self, ncs: &str, day: i64) -> crate::net_doc::Announcement {
        use crate::net_doc::{minutes_on, parse_freq, parse_minutes};
        let start = minutes_on(day, &self.start);
        let end = minutes_on(day, &self.end).map(|e| match start {
            Some(s) if e < s => e + 24 * 60,
            _ => e,
        });
        crate::net_doc::Announcement {
            net: self.name.trim().to_ascii_uppercase(),
            ncs: ncs.to_string(),
            alt: self.alt.trim().to_ascii_uppercase(),
            freq_hz: parse_freq(&self.freq),
            backup_hz: parse_freq(&self.backup),
            start,
            end,
            next: parse_minutes(&self.next),
            mode: Some(self.mode),
            purpose: Some(self.purpose),
            body: self.body.clone(),
        }
    }
}

/// `gear` in `ionhf.toml` and `--gear`: the link's choice of level.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Gear {
    /// By expected throughput.
    #[default]
    Goodput,
    /// Up and down the ladder, as Ion HF 0.1.0-alpha.4 (for an A/B).
    Ladder,
}

impl Gear {
    pub fn parse(s: &str) -> Option<Gear> {
        match s.to_ascii_lowercase().as_str() {
            "goodput" => Some(Gear::Goodput),
            "ladder" => Some(Gear::Ladder),
            _ => None,
        }
    }

    pub fn link(self) -> ion::link::Gear {
        match self {
            Gear::Goodput => ion::link::Gear::Goodput,
            Gear::Ladder => ion::link::Gear::Ladder,
        }
    }
}

/// `clipper` in `ionhf.toml`: how OFDM bursts are clipped. Both stations
/// should use the same one (the header does not tell plain QAM apart).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Clipper {
    /// Per-symbol clip and the low-PAPR mode, as 0.1.0-alpha.5.
    #[default]
    Classic,
    /// Data2G's whole-burst clipper: more mean power at the same peak.
    Burst,
}

impl Clipper {
    pub const ALL: [Clipper; 2] = [Clipper::Classic, Clipper::Burst];

    pub fn name(self) -> &'static str {
        match self {
            Clipper::Classic => "Classic",
            Clipper::Burst => "Burst",
        }
    }
}

/// `--stream on|off`.
pub fn parse_on_off(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Tci,
    Rigctld,
    Icom,
    Cat,
    None,
}

impl Backend {
    pub const ALL: [Backend; 5] = [
        Backend::Tci,
        Backend::Icom,
        Backend::Cat,
        Backend::Rigctld,
        Backend::None,
    ];

    pub fn short(self) -> &'static str {
        match self {
            Backend::Tci => "TCI",
            Backend::Rigctld => "rigctld",
            Backend::Icom => "CI-V",
            Backend::Cat => "CAT",
            Backend::None => "Sound card",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PttMethod {
    #[default]
    Cat,
    Rts,
    Dtr,
    Vox,
}

impl PttMethod {
    pub fn name(self) -> &'static str {
        match self {
            PttMethod::Cat => "CAT command",
            PttMethod::Rts => "RTS",
            PttMethod::Dtr => "DTR",
            PttMethod::Vox => "VOX (none)",
        }
    }
}

/// Which audio input a CAT or rigctld PTT keys, on radios with both.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TxAudio {
    /// The rear data input (USB, ACC).
    #[default]
    Data,
    Mic,
}

impl TxAudio {
    pub const ALL: [TxAudio; 2] = [TxAudio::Data, TxAudio::Mic];

    pub fn name(self) -> &'static str {
        match self {
            TxAudio::Data => "Data",
            TxAudio::Mic => "Mic",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RadioSettings {
    pub backend: Backend,
    pub rigctld_addr: String,
    pub port: String,
    pub baud: u32,
    pub icom_model: String,
    pub civ_addr: u8,
    /// One of `rig::cat::MODELS`.
    pub cat_model: String,
    pub ptt: PttMethod,
    pub tx_audio: TxAudio,
    /// Empty: the CAT port.
    pub ptt_port: String,
    /// Empty: the system default.
    pub audio_in: String,
    pub audio_out: String,
    /// 0 to 1.
    pub tx_level: f32,
    pub ptt_lead_ms: u32,
    pub poll_ms: u32,
}

impl Default for RadioSettings {
    fn default() -> Self {
        Self {
            backend: Backend::Tci,
            rigctld_addr: crate::rig::rigctld::DEFAULT_ADDR.into(),
            port: String::new(),
            baud: 19_200,
            icom_model: "IC-7300".into(),
            civ_addr: 0x94,
            cat_model: "TS-590S/SG".into(),
            ptt: PttMethod::Cat,
            tx_audio: TxAudio::Data,
            ptt_port: String::new(),
            audio_in: String::new(),
            audio_out: String::new(),
            tx_level: 0.5,
            ptt_lead_ms: 50,
            poll_ms: 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let c = Config {
            call: "JA1ABC".into(),
            radio: RadioSettings {
                backend: Backend::Icom,
                civ_addr: 0xA4,
                ptt: PttMethod::Rts,
                ..RadioSettings::default()
            },
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("backend = \"icom\""), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn the_net_profile_round_trips_and_makes_the_announcement() {
        let c = Config {
            net_line: "Trousdale County ARES".into(),
            net_profile: NetProfile {
                name: "tnares".into(),
                freq: "3.975".into(),
                backup: "7262 kHz".into(),
                start: "23:30".into(),
                end: "00:30".into(),
                next: "2026-11-10 23:30".into(),
                mode: crate::net_doc::Mode::Lsb,
                purpose: crate::net_doc::Purpose::Training,
                alt: "kj5et".into(),
                regulars: "KJ5ET, ag5b\nW5XYZ".into(),
                body: "# Tonight\n\n- one\n".into(),
            },
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
        let p = &c.net_profile;
        assert_eq!(p.regulars(), ["KJ5ET", "AG5B", "W5XYZ"]);
        // 2026-11-03 12:00Z.
        let a = p.announcement("N0JLF", 1_793_707_200);
        assert_eq!(a.net, "TNARES");
        assert_eq!(a.alt, "KJ5ET");
        assert_eq!(a.freq_hz, Some(3_975_000));
        assert_eq!(a.backup_hz, Some(7_262_000));
        assert_eq!(a.start.map(crate::net_doc::minutes_text).unwrap(), "2026-11-03 23:30Z");
        assert_eq!(a.end.map(crate::net_doc::minutes_text).unwrap(), "2026-11-04 00:30Z");
        assert_eq!(a.next.map(crate::net_doc::minutes_text).unwrap(), "2026-11-10 23:30Z");
        let old: Config = toml::from_str("call = \"W1AW\"\n").unwrap();
        assert_eq!(old.net_profile, NetProfile::default());
    }

    #[test]
    fn missing_fields_take_defaults() {
        let c: Config = toml::from_str("call = \"W1AW\"\n[radio]\nbackend = \"cat\"\n").unwrap();
        assert_eq!(c.call, "W1AW");
        assert_eq!(c.tci_url, "ws://127.0.0.1:50001");
        assert_eq!(c.radio.backend, Backend::Cat);
        assert_eq!(c.radio.rigctld_addr, "127.0.0.1:4532");
        assert_eq!(c.gear, Gear::Goodput);
        let c: Config = toml::from_str("gear = \"ladder\"\n").unwrap();
        assert_eq!(c.gear, Gear::Ladder);
        assert_eq!(c.clipper, Clipper::Classic);
        let c: Config = toml::from_str("clipper = \"burst\"\n").unwrap();
        assert_eq!(c.clipper, Clipper::Burst);
        assert!(Config::default().stream);
        let c: Config = toml::from_str(
            "stream = false
",
        )
        .unwrap();
        assert!(!c.stream);
    }
}
