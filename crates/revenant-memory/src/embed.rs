//! Embedders. Builtin = model2vec static embeddings (potion-retrieval-32M):
//! pure Rust, no transformer, microseconds per text — the reason the read
//! path stays under budget on a Pi. Gateway = /v1/embeddings for users who
//! want transformer quality and accept the dependency.

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const BUILTIN_MODEL: &str = "potion-retrieval-32M";
pub const BUILTIN_FILES: &[&str] = &["model.safetensors", "tokenizer.json", "config.json"];
pub const BUILTIN_HF_BASE: &str =
    "https://huggingface.co/minishlab/potion-retrieval-32M/resolve/main";

/// Download the builtin model into `<models_dir>/potion-retrieval-32M/`
/// (same pattern as the pinned gateway binary). Verified by test-loading.
pub async fn ensure_builtin_model(models_dir: &Path) -> Result<std::path::PathBuf> {
    let dir = models_dir.join(BUILTIN_MODEL);
    if BUILTIN_FILES.iter().all(|f| dir.join(f).exists()) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let http = reqwest::Client::new();
    for file in BUILTIN_FILES {
        let target = dir.join(file);
        if target.exists() {
            continue;
        }
        tracing::info!("downloading embedding model file {file}");
        let bytes = http
            .get(format!("{BUILTIN_HF_BASE}/{file}"))
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("downloading {file}"))?
            .bytes()
            .await?;
        if bytes.is_empty() {
            bail!("downloaded {file} is empty");
        }
        let tmp = dir.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &target)?;
    }
    // The real verification: the model must load and produce vectors.
    let embedder = BuiltinEmbedder::load(&dir)?;
    tracing::info!(
        "embedding model ready: {} ({} dims)",
        embedder.id(),
        embedder.dim()
    );
    Ok(dir)
}

pub trait Embedder: Send + Sync {
    /// Stable identity — stored in mem_meta; a mismatch triggers re-embed.
    fn id(&self) -> String;
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.embed(std::slice::from_ref(&text.to_string()))?
            .into_iter()
            .next()
            .context("embedder returned nothing")
    }
}

pub struct BuiltinEmbedder {
    model: model2vec_rs::model::StaticModel,
    dim: usize,
}

impl BuiltinEmbedder {
    /// Load from `~/.revenant/models/potion-retrieval-32M/` (downloaded by
    /// `revenant init`).
    pub fn load(dir: &Path) -> Result<Self> {
        for file in BUILTIN_FILES {
            if !dir.join(file).exists() {
                bail!(
                    "embedding model file missing: {} — run `revenant init` to download it",
                    dir.join(file).display()
                );
            }
        }
        let model = model2vec_rs::model::StaticModel::from_pretrained(
            dir.to_string_lossy().as_ref(),
            None,
            None,
            None,
        )
        .map_err(|e| anyhow::anyhow!("loading model2vec model: {e}"))?;
        let probe = model.encode(&["dim probe".to_string()]);
        let dim = probe.first().map(|v| v.len()).context("model produced no embedding")?;
        Ok(BuiltinEmbedder { model, dim })
    }
}

impl Embedder for BuiltinEmbedder {
    fn id(&self) -> String {
        format!("builtin/{BUILTIN_MODEL}")
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(self.model.encode(texts))
    }
}

/// Gateway-routed embeddings (OpenAI shape via agentgateway).
pub struct GatewayEmbedder {
    llm: revenant_llm::LlmClient,
    model: String,
    dim: std::sync::OnceLock<usize>,
    runtime: tokio::runtime::Handle,
}

impl GatewayEmbedder {
    pub fn new(llm: revenant_llm::LlmClient, model: String) -> Self {
        GatewayEmbedder {
            llm,
            model,
            dim: std::sync::OnceLock::new(),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl Embedder for GatewayEmbedder {
    fn id(&self) -> String {
        format!("gateway/{}", self.model)
    }
    fn dim(&self) -> usize {
        *self.dim.get().unwrap_or(&0)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // The Embedder trait is sync (builtin is CPU-bound); bridge to async.
        let llm = self.llm.clone();
        let model = self.model.clone();
        let texts = texts.to_vec();
        let vectors = tokio::task::block_in_place(|| {
            self.runtime
                .block_on(async move { llm.embeddings(&model, &texts).await })
        })?;
        if let Some(first) = vectors.first() {
            let _ = self.dim.set(first.len());
        }
        Ok(vectors)
    }
}

/// Cosine similarity via simsimd (NEON/AVX), with a scalar fallback.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    use simsimd::SpatialSimilarity;
    match f32::cosine(a, b) {
        // simsimd returns cosine DISTANCE (0 = identical).
        Some(distance) => (1.0 - distance) as f32,
        None => {
            let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
            for (x, y) in a.iter().zip(b) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot / (na.sqrt() * nb.sqrt())
            }
        }
    }
}

pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_sanity() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        let c = vec![0.0f32, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
        assert!(cosine(&a, &c).abs() < 1e-5);
    }

    #[test]
    fn blob_round_trip() {
        let v = vec![0.5f32, -1.25, 3.0];
        assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
    }
}
