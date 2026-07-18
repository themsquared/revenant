//! The provider catalog: the set of model providers a user can pick during
//! setup, and how each maps to a full tier config (fast / balanced / deep).
//!
//! Wider provider support is deliberate — a horde running Anthropic, OpenAI,
//! Grok, Gemini, and local models surfaces a far richer stream of improvements
//! than a monoculture would. The gateway already speaks all of these; this is
//! the human-facing menu + the sensible default model per tier for each.
//!
//! Model IDs here are reasonable defaults as of authoring — they change often.
//! They're written into `config.toml` where the owner can edit freely, and a
//! wrong id fails loudly at the gateway (never silently), so they're a starting
//! point, not a promise.

use crate::config::{Provider, RouteStrategy, TierConfig, TierTarget};
use std::collections::BTreeMap;

/// One selectable provider and its default fast/balanced/deep models.
#[derive(Debug, Clone)]
pub struct ProviderChoice {
    /// Wizard id (also the config-friendly slug): "anthropic", "openai", …
    pub key: &'static str,
    /// Human label for the picker.
    pub label: &'static str,
    /// One-line orientation (where to get a key, what it's good for).
    pub blurb: &'static str,
    pub provider: Provider,
    /// Secrets.env var holding the key; None for a keyless local provider.
    pub key_env: Option<&'static str>,
    /// Where a key is issued (shown in the picker); empty for local.
    pub key_url: &'static str,
    /// Base URL override (OpenAI-compatible providers like xAI/Grok); None = provider default.
    pub base_url: Option<&'static str>,
    /// Default model per tier.
    pub fast: &'static str,
    pub balanced: &'static str,
    pub deep: &'static str,
}

/// The catalog, in the order shown to the user. Anthropic first (most capable
/// tool use today); local last.
pub fn catalog() -> Vec<ProviderChoice> {
    vec![
        ProviderChoice {
            key: "anthropic",
            label: "Anthropic (Claude)",
            blurb: "most capable tool use · console.anthropic.com/settings/keys",
            provider: Provider::Anthropic,
            key_env: Some("ANTHROPIC_API_KEY"),
            key_url: "https://console.anthropic.com/settings/keys",
            base_url: None,
            fast: "claude-haiku-4-5-20251001",
            balanced: "claude-sonnet-5",
            deep: "claude-opus-4-8",
        },
        ProviderChoice {
            key: "openai",
            label: "OpenAI (GPT)",
            blurb: "widely used · platform.openai.com/api-keys",
            provider: Provider::OpenAI,
            key_env: Some("OPENAI_API_KEY"),
            key_url: "https://platform.openai.com/api-keys",
            base_url: None,
            fast: "gpt-4o-mini",
            balanced: "gpt-4o",
            deep: "o1",
        },
        ProviderChoice {
            key: "grok",
            label: "Grok (xAI)",
            blurb: "OpenAI-compatible · console.x.ai",
            provider: Provider::OpenAI, // xAI speaks the OpenAI API
            key_env: Some("XAI_API_KEY"),
            key_url: "https://console.x.ai",
            base_url: Some("https://api.x.ai/v1"),
            fast: "grok-2-latest",
            balanced: "grok-2-latest",
            deep: "grok-2-latest",
        },
        ProviderChoice {
            key: "kimi",
            label: "Kimi (Moonshot)",
            blurb: "K3 flagship, 1M ctx, strong agentic/tool use · OpenAI-compatible · platform.moonshot.ai",
            provider: Provider::OpenAI, // Moonshot speaks the OpenAI API
            key_env: Some("MOONSHOT_API_KEY"),
            key_url: "https://platform.moonshot.ai/console/api-keys",
            base_url: Some("https://api.moonshot.ai/v1"),
            fast: "kimi-k3",
            balanced: "kimi-k3",
            deep: "kimi-k3",
        },
        ProviderChoice {
            key: "gemini",
            label: "Google (Gemini)",
            blurb: "fast + long context · aistudio.google.com/apikey",
            provider: Provider::Gemini,
            key_env: Some("GEMINI_API_KEY"),
            key_url: "https://aistudio.google.com/apikey",
            base_url: None,
            fast: "gemini-2.0-flash",
            balanced: "gemini-2.0-flash",
            deep: "gemini-1.5-pro",
        },
        ProviderChoice {
            key: "openrouter",
            label: "OpenRouter (many models)",
            blurb: "one key, hundreds of models · openrouter.ai/keys",
            provider: Provider::OpenRouter,
            key_env: Some("OPENROUTER_API_KEY"),
            key_url: "https://openrouter.ai/keys",
            base_url: None,
            fast: "anthropic/claude-haiku-4.5",
            balanced: "anthropic/claude-sonnet-5",
            deep: "openai/o1",
        },
        ProviderChoice {
            key: "ollama",
            label: "Ollama (local, free)",
            blurb: "runs on your machine · great for chat, limited for agentic/coding",
            provider: Provider::Ollama,
            key_env: None,
            key_url: "",
            base_url: None,
            fast: "llama3.1:8b",
            balanced: "llama3.1:8b",
            deep: "qwen2.5-coder:14b",
        },
    ]
}

pub fn find(key: &str) -> Option<ProviderChoice> {
    catalog().into_iter().find(|c| c.key == key)
}

impl ProviderChoice {
    fn target(&self, model: &str) -> TierTarget {
        TierTarget {
            provider: self.provider,
            model: model.to_string(),
            api_key_env: self.key_env.map(|s| s.to_string()),
            base_url: self.base_url.map(|s| s.to_string()),
            weight: None,
        }
    }

    /// The fast/balanced/deep tier map for this provider — everything a fresh
    /// config needs to route all three tiers to the chosen provider. (A `local`
    /// Ollama tier is added separately so local testing is always available.)
    pub fn tiers(&self) -> BTreeMap<String, TierConfig> {
        let mut t = BTreeMap::new();
        for (name, model) in [("fast", self.fast), ("balanced", self.balanced), ("deep", self.deep)] {
            t.insert(
                name.to_string(),
                TierConfig { targets: vec![self.target(model)], strategy: RouteStrategy::Failover },
            );
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_well_formed() {
        for c in catalog() {
            assert!(!c.label.is_empty() && !c.fast.is_empty() && !c.balanced.is_empty() && !c.deep.is_empty());
            // Keyless provider ⇒ local (Ollama); everything else needs a key env.
            if c.key_env.is_none() {
                assert!(matches!(c.provider, Provider::Ollama));
            }
        }
        // Each choice produces exactly the three cloud tiers.
        let t = find("anthropic").unwrap().tiers();
        assert_eq!(t.len(), 3);
        assert!(t.contains_key("fast") && t.contains_key("balanced") && t.contains_key("deep"));
        // Grok is OpenAI-compatible with a base_url.
        let grok = find("grok").unwrap();
        assert_eq!(grok.provider, Provider::OpenAI);
        assert!(grok.base_url.is_some());
        // Kimi (Moonshot) is likewise OpenAI-compatible, keyed + base_url'd.
        let kimi = find("kimi").unwrap();
        assert_eq!(kimi.provider, Provider::OpenAI);
        assert_eq!(kimi.base_url.as_deref(), Some("https://api.moonshot.ai/v1"));
        assert_eq!(kimi.key_env, Some("MOONSHOT_API_KEY"));
    }
}
