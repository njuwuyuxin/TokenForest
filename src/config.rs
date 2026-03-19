use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_CONFIG_PATH: &str = "cyber_bonsai.toml";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub render: RenderConfig,
    pub network: NetworkConfig,
    pub smoothing: SmoothingConfig,
}

impl AppConfig {
    pub fn load_default() -> Result<Self> {
        Self::load_from(Path::new(DEFAULT_CONFIG_PATH))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed reading config file {}", path.display()))?;
        let parsed: Self = toml::from_str(&content)
            .with_context(|| format!("failed parsing config file {}", path.display()))?;
        Ok(parsed.sanitized())
    }

    fn sanitized(mut self) -> Self {
        self.render.max_token_rate = self.render.max_token_rate.max(1.0);

        self.network.poll_interval_ms = self.network.poll_interval_ms.max(60);
        self.network.bytes_per_token_estimate = self.network.bytes_per_token_estimate.max(0.5);

        self.smoothing.window_size = self.smoothing.window_size.max(3);
        self.smoothing.tau_rise_seconds = self.smoothing.tau_rise_seconds.max(0.2);
        self.smoothing.tau_fall_seconds = self.smoothing.tau_fall_seconds.max(0.2);
        self.smoothing.clip_percentile = self.smoothing.clip_percentile.clamp(0.5, 0.99);
        self.smoothing.clip_multiplier = self.smoothing.clip_multiplier.max(1.0);
        self.smoothing.clip_offset = self.smoothing.clip_offset.max(0.0);

        let weight_sum = self.smoothing.median_weight + self.smoothing.latest_weight;
        if weight_sum > f32::EPSILON {
            self.smoothing.median_weight /= weight_sum;
            self.smoothing.latest_weight /= weight_sum;
        } else {
            self.smoothing.median_weight = 0.58;
            self.smoothing.latest_weight = 0.42;
        }

        self
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct RenderConfig {
    pub max_token_rate: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            max_token_rate: 2400.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub poll_interval_ms: u64,
    pub bytes_per_token_estimate: f32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 400,
            bytes_per_token_estimate: 4.1,
        }
    }
}

impl NetworkConfig {
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SmoothingConfig {
    pub window_size: usize,
    pub tau_rise_seconds: f32,
    pub tau_fall_seconds: f32,
    pub median_weight: f32,
    pub latest_weight: f32,
    pub clip_percentile: f32,
    pub clip_multiplier: f32,
    pub clip_offset: f32,
}

impl Default for SmoothingConfig {
    fn default() -> Self {
        Self {
            window_size: 18,
            tau_rise_seconds: 2.8,
            tau_fall_seconds: 5.5,
            median_weight: 0.58,
            latest_weight: 0.42,
            clip_percentile: 0.8,
            clip_multiplier: 1.25,
            clip_offset: 120.0,
        }
    }
}
