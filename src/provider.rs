//! Measured provider presets. Two measured tuning rounds earned these
//! defaults with ABBA runs; it lived in the CLI, so a host harness planning through the
//! library got the untuned path (host planning measured 26-80s vs 4.1s on
//! the CLI). This module moves the presets onto the public surface:
//! `provider_preset(name)` returns the measured defaults, and the `apply_*`
//! helpers stamp them onto `ArchitectOptions` and `SchedulerConfig` without
//! touching explicit host choices. Presets are data plus small helpers; the
//! engine core stays provider-neutral (core stays generic).

use crate::architect::{ArchitectOptions, SINGLE_RESPONSE_THRESHOLD_TOKENS};
use crate::controller::{ControllerConfig, DEFAULT_COLD_START_BURST_CAP};
use crate::scheduler::SchedulerConfig;

/// One provider's defaults. Measured presets cite their ABBA run in the field
/// docs; the Anthropic preset pins labeled starting points instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Lookup name for `--provider` and `provider_preset`.
    pub name: &'static str,
    /// OpenAI-compatible base URL.
    pub base_url: &'static str,
    /// Default model for the provider key.
    pub model: &'static str,
    /// Worker-call reasoning effort (`SchedulerConfig.reasoning_effort`).
    /// Muse rejects "none"; deepseek-flash rejects "minimal" (on it,
    /// "minimal" reasons MORE than "low"), so each preset pins what its
    /// provider accepts and what measured fastest.
    pub worker_effort: &'static str,
    /// Architect fragment effort. ABBA run: minimal vs low planning 12.5 vs
    /// 32.6s (Muse), 3.1k vs 8.6k planning output tokens, 0 vs 2 retries.
    pub architect_effort: &'static str,
    /// Strict json_schema `response_format` on architect fragments. ABBA
    /// run. deepseek-flash rejects json_schema, so its preset disables it.
    pub architect_schema: bool,
    /// Cold-start admission burst cap (`ControllerConfig.cold_burst_cap`).
    /// ABBA run x4 (15-task memory wave, DeepSeek): 16 vs 8 median wave
    /// 12.6 vs 15.3s. Muse keeps the conservative default: it sheds wide
    /// cold bursts with empty completions.
    pub cold_burst_cap: usize,
    /// Size-router cutoff: estimated deliverables above this take the
    /// manifest + wave, below go out as one direct call. ABBA run
    /// (DeepSeek): the wave beat one direct call 2-3x on wall at +0.4-2.9%
    /// tokens, so only trivial objectives stay direct.
    pub single_response_threshold: u64,
    /// Environment variable the provider API key lives in. Empty means the
    /// host wires the key itself.
    pub key_env: &'static str,
}

/// api.meta.ai, muse-spark-1.3-contributor. ABBA runs.
pub const MUSE_PRESET: ProviderPreset = ProviderPreset {
    name: "muse",
    base_url: "https://api.meta.ai/v1",
    model: "muse-spark-1.3-contributor",
    worker_effort: "minimal",
    architect_effort: "minimal",
    architect_schema: true,
    cold_burst_cap: DEFAULT_COLD_START_BURST_CAP,
    single_response_threshold: SINGLE_RESPONSE_THRESHOLD_TOKENS,
    key_env: "MUSE_API_KEY",
};

/// api.deepseek.com, deepseek-flash. Effort "none" = 0 reasoning tokens
/// (its fastest measured mode). Preset disables architect schema (rejected).
/// ABBA runs.
pub const DEEPSEEK_PRESET: ProviderPreset = ProviderPreset {
    name: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    model: "deepseek-flash",
    worker_effort: "none",
    architect_effort: "none",
    architect_schema: false,
    cold_burst_cap: 16,
    single_response_threshold: 10_000,
    key_env: "DEEPSEEK_API_KEY",
};

/// api.anthropic.com (OpenAI-compatible layer), claude-sonnet-5-5. Starting
/// points, not measurements: effort `low` maps to a small thinking budget on
/// the compat layer, schema stays off until a paired run proves it accepted,
/// and the burst cap is conservative for the standard rate tiers. Key from
/// ANTHROPIC_API_KEY.
pub const ANTHROPIC_PRESET: ProviderPreset = ProviderPreset {
    name: "anthropic",
    base_url: "https://api.anthropic.com/v1",
    model: "claude-sonnet-5-5",
    worker_effort: "low",
    architect_effort: "low",
    architect_schema: false,
    cold_burst_cap: 8,
    single_response_threshold: SINGLE_RESPONSE_THRESHOLD_TOKENS,
    key_env: "ANTHROPIC_API_KEY",
};

/// All known presets. Muse and DeepSeek presets carry ABBA-measured values.
/// The Anthropic preset pins conservative starting points: its knobs are not
/// measured on this engine yet, so cite no numbers for it until a paired run.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[MUSE_PRESET, DEEPSEEK_PRESET, ANTHROPIC_PRESET];

/// Look up a preset by name (`muse`, `deepseek`, `anthropic`). Unknown names
/// return None.
pub fn provider_preset(name: &str) -> Option<ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .find(|preset| preset.name == name)
        .copied()
}

impl ProviderPreset {
    /// Read the provider key from this preset's environment variable.
    /// `Err` carries a host-actionable message.
    pub fn api_key_from_env(&self) -> Result<String, String> {
        std::env::var(self.key_env)
            .map_err(|_| format!("environment variable {0} is not set", self.key_env))
    }

    /// Stamp the measured planning knobs onto architect options. Explicit
    /// host choices win: the fragment effort is only replaced while it
    /// equals the library default, and a preset can only disable schema
    /// (never force it on over a host that turned it off). A schema-off
    /// preset (deepseek) always disables: its provider rejects the schema.
    pub fn apply_architect(&self, options: &mut ArchitectOptions) {
        if options.fragment_effort == ArchitectOptions::default().fragment_effort {
            options.fragment_effort = self.architect_effort.to_string();
        }
        if !self.architect_schema {
            options.schema = false;
        }
    }

    /// Stamp the measured worker knobs onto a scheduler config. A host-set
    /// worker effort or a custom cold burst cap is left alone; defaults are
    /// replaced by the preset's measured values.
    pub fn apply_scheduler(&self, config: &mut SchedulerConfig) {
        if config.reasoning_effort.is_none() {
            config.reasoning_effort = Some(self.worker_effort.to_string());
        }
        if config.controller.cold_burst_cap == DEFAULT_COLD_START_BURST_CAP {
            config.controller.cold_burst_cap = self.cold_burst_cap;
        }
    }

    /// Size-router cutoff for this provider (see `single_response_threshold`).
    pub fn size_router_threshold(&self) -> u64 {
        self.single_response_threshold
    }
}

/// Controller knob passthrough so hosts can read the measured burst cap
/// without importing the controller module directly.
impl ProviderPreset {
    /// The cold-start burst cap as a `ControllerConfig` patch target.
    pub fn apply_controller(&self, controller: &mut ControllerConfig) {
        if controller.cold_burst_cap == DEFAULT_COLD_START_BURST_CAP {
            controller.cold_burst_cap = self.cold_burst_cap;
        }
    }
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)] // preset guards pin literal definitions against drift
mod tests {
    use super::*;

    #[test]
    fn lookup_resolves_known_names_and_rejects_unknown() {
        assert_eq!(provider_preset("muse"), Some(MUSE_PRESET));
        assert_eq!(provider_preset("deepseek"), Some(DEEPSEEK_PRESET));
        assert_eq!(provider_preset("anthropic"), Some(ANTHROPIC_PRESET));
        assert_eq!(provider_preset("openai"), None);
        assert_eq!(provider_preset(""), None);
        let names: Vec<&str> = PROVIDER_PRESETS.iter().map(|preset| preset.name).collect();
        assert_eq!(
            names,
            vec!["muse", "deepseek", "anthropic"],
            "names stay unique"
        );
    }

    #[test]
    fn presets_carry_measured_values() {
        assert_eq!(MUSE_PRESET.architect_effort, "minimal");
        assert!(MUSE_PRESET.architect_schema);
        assert_eq!(MUSE_PRESET.cold_burst_cap, DEFAULT_COLD_START_BURST_CAP);
        assert_eq!(
            MUSE_PRESET.single_response_threshold,
            SINGLE_RESPONSE_THRESHOLD_TOKENS
        );
        assert_eq!(MUSE_PRESET.worker_effort, "minimal");

        assert_eq!(DEEPSEEK_PRESET.architect_effort, "none");
        assert!(
            !DEEPSEEK_PRESET.architect_schema,
            "deepseek rejects json_schema"
        );
        assert_eq!(DEEPSEEK_PRESET.cold_burst_cap, 16);
        assert!(DEEPSEEK_PRESET.single_response_threshold < 13_000);
        assert_eq!(DEEPSEEK_PRESET.worker_effort, "none");

        // Anthropic pins starting points, not measurements: keep them
        // conservative until a paired run replaces them with measured values.
        assert_eq!(ANTHROPIC_PRESET.worker_effort, "low");
        assert_eq!(ANTHROPIC_PRESET.architect_effort, "low");
        assert!(
            !ANTHROPIC_PRESET.architect_schema,
            "schema stays off until the compat layer proves it in a paired run"
        );
        assert!(ANTHROPIC_PRESET.cold_burst_cap <= 8, "conservative start");
        assert_eq!(ANTHROPIC_PRESET.key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn apply_architect_stamps_defaults_and_respects_explicit_choices() {
        // DeepSeek on library defaults: effort minimal -> none, schema off.
        let mut options = ArchitectOptions::default();
        DEEPSEEK_PRESET.apply_architect(&mut options);
        assert_eq!(options.fragment_effort, "none");
        assert!(!options.schema);

        // Muse on defaults: already the measured values, must stay.
        let mut muse_options = ArchitectOptions::default();
        MUSE_PRESET.apply_architect(&mut muse_options);
        assert_eq!(muse_options.fragment_effort, "minimal");
        assert!(muse_options.schema);

        // Explicit host effort wins over the preset.
        let mut explicit = ArchitectOptions {
            fragment_effort: "low".to_string(),
            ..ArchitectOptions::default()
        };
        DEEPSEEK_PRESET.apply_architect(&mut explicit);
        assert_eq!(explicit.fragment_effort, "low", "explicit host effort wins");
        assert!(!explicit.schema, "schema-off preset still disables");
    }

    #[test]
    fn apply_scheduler_stamps_defaults_and_respects_explicit_choices() {
        let mut config = SchedulerConfig::default();
        assert!(config.reasoning_effort.is_none());
        DEEPSEEK_PRESET.apply_scheduler(&mut config);
        assert_eq!(config.reasoning_effort.as_deref(), Some("none"));
        assert_eq!(config.controller.cold_burst_cap, 16);

        // Host-set values win.
        let mut host = SchedulerConfig {
            reasoning_effort: Some("low".to_string()),
            ..SchedulerConfig::default()
        };
        host.controller.cold_burst_cap = 32;
        MUSE_PRESET.apply_scheduler(&mut host);
        assert_eq!(host.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(host.controller.cold_burst_cap, 32, "custom burst cap wins");
    }

    #[test]
    fn apply_controller_patches_only_the_default_cap() {
        let mut controller = ControllerConfig::default();
        DEEPSEEK_PRESET.apply_controller(&mut controller);
        assert_eq!(controller.cold_burst_cap, 16);
        let mut custom = ControllerConfig {
            cold_burst_cap: 48,
            ..ControllerConfig::default()
        };
        MUSE_PRESET.apply_controller(&mut custom);
        assert_eq!(custom.cold_burst_cap, 48);
    }
}
