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
    pub net: String,
    /// Percent.
    pub drive: u32,
    /// OFDM level above the single-tone signals, 0 to 3 dB.
    pub ofdm_db: f32,
    /// How the link picks its level (`ion::link::Gear`).
    pub gear: Gear,
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
            drive: 50,
            ofdm_db: ion::link::audio::OFDM_AIR_GAIN_DB,
            gear: Gear::Goodput,
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
    fn missing_fields_take_defaults() {
        let c: Config = toml::from_str("call = \"W1AW\"\n[radio]\nbackend = \"cat\"\n").unwrap();
        assert_eq!(c.call, "W1AW");
        assert_eq!(c.tci_url, "ws://127.0.0.1:50001");
        assert_eq!(c.radio.backend, Backend::Cat);
        assert_eq!(c.radio.rigctld_addr, "127.0.0.1:4532");
        assert_eq!(c.gear, Gear::Goodput);
        let c: Config = toml::from_str("gear = \"ladder\"\n").unwrap();
        assert_eq!(c.gear, Gear::Ladder);
        assert!(Config::default().stream);
        let c: Config = toml::from_str(
            "stream = false
",
        )
        .unwrap();
        assert!(!c.stream);
    }
}
