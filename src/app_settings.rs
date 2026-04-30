use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const EQ_BANDS: [EqualizerBand; 10] = [
    EqualizerBand {
        label: "60 Hz",
        frequency_hz: 60.0,
    },
    EqualizerBand {
        label: "170 Hz",
        frequency_hz: 170.0,
    },
    EqualizerBand {
        label: "310 Hz",
        frequency_hz: 310.0,
    },
    EqualizerBand {
        label: "600 Hz",
        frequency_hz: 600.0,
    },
    EqualizerBand {
        label: "1 kHz",
        frequency_hz: 1000.0,
    },
    EqualizerBand {
        label: "3 kHz",
        frequency_hz: 3000.0,
    },
    EqualizerBand {
        label: "6 kHz",
        frequency_hz: 6000.0,
    },
    EqualizerBand {
        label: "12 kHz",
        frequency_hz: 12000.0,
    },
    EqualizerBand {
        label: "14 kHz",
        frequency_hz: 14000.0,
    },
    EqualizerBand {
        label: "16 kHz",
        frequency_hz: 16000.0,
    },
];

#[derive(Clone, Copy)]
pub struct EqualizerBand {
    pub label: &'static str,
    pub frequency_hz: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqualizerSettings {
    pub enabled: bool,
    pub preamp_db: f32,
    pub bands_db: [f32; 10],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    pub equalizer: EqualizerSettings,
}

impl Default for EqualizerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            preamp_db: 0.0,
            bands_db: [0.0; 10],
        }
    }
}

impl EqualizerSettings {
    pub fn preset_flat() -> Self {
        Self::default()
    }

    pub fn preset_bass_boost() -> Self {
        Self {
            enabled: true,
            preamp_db: -1.5,
            bands_db: [5.0, 4.0, 2.5, 1.0, 0.0, -0.5, -1.0, -1.0, -1.0, -1.0],
        }
    }

    pub fn preset_treble_boost() -> Self {
        Self {
            enabled: true,
            preamp_db: -1.5,
            bands_db: [-1.5, -1.0, -0.5, 0.0, 0.5, 2.0, 3.0, 4.5, 5.0, 5.0],
        }
    }

    pub fn preset_vocal() -> Self {
        Self {
            enabled: true,
            preamp_db: -1.0,
            bands_db: [-2.0, -1.0, 0.0, 2.0, 3.5, 3.0, 1.0, -0.5, -1.0, -1.5],
        }
    }
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            equalizer: EqualizerSettings::default(),
        }
    }
}

impl UserSettings {
    pub fn load() -> Self {
        let path = settings_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create settings directory")?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).context("Failed to write settings")?;
        Ok(())
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("onyx")
        .join("settings.json")
}
