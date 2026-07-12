//! Child-process supervision for the bundled agentgateway.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Env vars every child process should inherit regardless of provider
/// secrets, so stdio subprocesses (e.g. MCP servers) can find the user's
/// home directory, kubeconfig, ssh config, etc. The gateway's own env is
/// still cleared first; this is a small, deliberate allowlist rather than
/// a full passthrough.
pub fn base_passthrough_env() -> Vec<(String, String)> {
    const KEYS: &[&str] = &["HOME", "USER", "USERPROFILE", "SHELL", "LANG", "TMPDIR", "TZ"];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

pub struct GatewaySupervisor {
    pub binary: PathBuf,
    pub config_path: PathBuf,
    /// Environment for the child: provider secrets + a minimal base.
    /// The child env is cleared first — the gateway sees only what we pass.
    pub env: Vec<(String, String)>,
    pub llm_port: u16,
}

pub struct SupervisorHandle {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl SupervisorHandle {
    /// Stop the gateway and wait for the supervise loop to exit.
    pub async fn shutdown(self) {
        self.token.cancel();
        let _ = self.task.await;
    }
}

impl GatewaySupervisor {
    /// Spawn the gateway, wait until the LLM data path answers, then keep
    /// it alive in the background (restart with exponential backoff).
    pub async fn start(self) -> Result<SupervisorHandle> {
        // Pre-flight: if something already answers on our LLM port, a stale
        // gateway or second revenant instance is running. Readiness checks
        // against a foreign process would falsely pass — refuse instead.
        let probe = revenant_llm::LlmClient::new(format!("http://127.0.0.1:{}", self.llm_port));
        if probe.models_ready().await {
            bail!(
                "port {} is already serving an LLM endpoint — another gateway or revenant \
                 instance is running (pkill agentgateway / check `revenant up`)",
                self.llm_port
            );
        }

        let token = CancellationToken::new();
        let child_token = token.clone();

        let mut child = self.spawn().context("spawning agentgateway")?;
        self.wait_ready(&mut child).await?;

        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                tokio::select! {
                    status = child.wait() => {
                        if child_token.is_cancelled() {
                            break;
                        }
                        tracing::warn!(?status, "agentgateway exited; restarting in {backoff:?}");
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {},
                            _ = child_token.cancelled() => break,
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        match self.spawn() {
                            Ok(new_child) => {
                                child = new_child;
                                // Reset backoff only after it stays up a while.
                                let healthy_after = tokio::time::Instant::now()
                                    + Duration::from_secs(600);
                                tokio::select! {
                                    _ = tokio::time::sleep_until(healthy_after) => {
                                        backoff = Duration::from_secs(1);
                                    }
                                    _ = child_token.cancelled() => break,
                                    _ = child.wait() => {
                                        // Exited quickly; loop keeps the backoff.
                                        continue;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::error!(%err, "failed to respawn agentgateway");
                            }
                        }
                    }
                    _ = child_token.cancelled() => break,
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            tracing::info!("agentgateway stopped");
        });

        Ok(SupervisorHandle { token, task })
    }

    fn spawn(&self) -> Result<tokio::process::Child> {
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("-f")
            .arg(&self.config_path)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .envs(base_passthrough_env())
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;

        // Forward child output into our logs, tagged.
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(forward_lines(stdout, "gateway"));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_lines(stderr, "gateway"));
        }
        Ok(child)
    }

    async fn wait_ready(&self, child: &mut tokio::process::Child) -> Result<()> {
        let llm = revenant_llm::LlmClient::new(format!("http://127.0.0.1:{}", self.llm_port));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!("agentgateway exited during startup: {status}");
            }
            if llm.models_ready().await {
                tracing::info!("agentgateway ready on port {}", self.llm_port);
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                bail!("agentgateway did not become ready within 30s");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn forward_lines(reader: impl tokio::io::AsyncRead + Unpin, tag: &'static str) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(target: "gateway", "[{tag}] {line}");
    }
}
