//! HTTP client for the clone daemon REST API.
//!
//! Thin async wrapper around reqwest. Each method maps to one daemon endpoint;
//! errors carry the HTTP status code and response body for diagnostics.

use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct CloneClient {
    base_url: String,
    auth_token: Option<String>,
    http: Client,
}

impl CloneClient {
    pub fn new(base_url: String, auth_token: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("reqwest client build");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token,
            http,
        }
    }

    /// Quick connectivity probe used at controller startup.
    pub async fn health(&self) -> Result<()> {
        let resp = self.get_raw("/health").await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("daemon /health returned {}", resp.status()))
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_raw(&self, path: &str) -> Result<reqwest::Response> {
        let mut req = self.http.get(self.url(path));
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        Ok(req.send().await?)
    }

    async fn post_json<B: Serialize>(&self, path: &str, body: &B) -> Result<reqwest::Response> {
        let mut req = self.http.post(self.url(path)).json(body);
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        Ok(req.send().await?)
    }

    async fn delete_raw(&self, path: &str) -> Result<reqwest::Response> {
        let mut req = self.http.delete(self.url(path));
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        Ok(req.send().await?)
    }

    /// Drain response body into an error string when status is non-2xx.
    async fn ensure_ok(
        resp: reqwest::Response,
        context: &str,
    ) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(anyhow!("{context} failed: HTTP {status}: {body}"))
        }
    }
}

// ---- Request/Response DTOs ------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateVmReq {
    pub kernel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_mb: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CreateVmResp {
    pub vm_id: String,
    #[serde(default)]
    pub pid: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RestoreReq {
    pub snapshot_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_mb: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SaveReq {
    pub output_path: String,
}

#[derive(Debug, Serialize)]
pub struct BalloonReq {
    pub target_mb: u32,
}

#[derive(Debug, Serialize)]
pub struct ExecReq {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExecResp {
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

#[derive(Debug, Deserialize)]
pub struct VmStatusResp {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub uptime_secs: Option<f64>,
    #[serde(default)]
    pub memory_usage_bytes: Option<u64>,
    /// clone daemon may or may not populate this; we fall back to exec'ing
    /// `ip addr` inside the guest if missing.
    #[serde(default)]
    pub guest_ip: Option<String>,
}

// ---- Public API methods ---------------------------------------------------

impl CloneClient {
    pub async fn create_vm(&self, req: CreateVmReq) -> Result<CreateVmResp> {
        let resp = self.post_json("/v1/vms", &req).await?;
        let resp = Self::ensure_ok(resp, "create_vm").await?;
        Ok(resp.json().await?)
    }

    pub async fn restore_vm(&self, req: RestoreReq) -> Result<CreateVmResp> {
        let resp = self.post_json("/v1/restore", &req).await?;
        let resp = Self::ensure_ok(resp, "restore_vm").await?;
        Ok(resp.json().await?)
    }

    pub async fn save_vm(&self, vm_id: &str, output_path: &str) -> Result<()> {
        let resp = self
            .post_json(&format!("/v1/vms/{vm_id}/save"), &SaveReq {
                output_path: output_path.to_string(),
            })
            .await?;
        Self::ensure_ok(resp, "save_vm").await?;
        Ok(())
    }

    pub async fn balloon_vm(&self, vm_id: &str, target_mb: u32) -> Result<()> {
        let resp = self
            .post_json(&format!("/v1/vms/{vm_id}/balloon"), &BalloonReq { target_mb })
            .await?;
        Self::ensure_ok(resp, "balloon_vm").await?;
        Ok(())
    }

    pub async fn exec_vm(&self, vm_id: &str, command: &str, args: &[&str]) -> Result<ExecResp> {
        let resp = self
            .post_json(&format!("/v1/vms/{vm_id}/exec"), &ExecReq {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
            })
            .await?;
        let resp = Self::ensure_ok(resp, "exec_vm").await?;
        Ok(resp.json().await?)
    }

    pub async fn destroy_vm(&self, vm_id: &str) -> Result<()> {
        let resp = self.delete_raw(&format!("/v1/vms/{vm_id}")).await?;
        Self::ensure_ok(resp, "destroy_vm").await?;
        Ok(())
    }

    pub async fn get_vm(&self, vm_id: &str) -> Result<VmStatusResp> {
        let resp = self.get_raw(&format!("/v1/vms/{vm_id}")).await?;
        let resp = Self::ensure_ok(resp, "get_vm").await?;
        Ok(resp.json().await?)
    }

    /// Convenience: fetch the guest's eth0 IPv4 by exec'ing inside the VM.
    /// Retries for up to ~20s while the guest agent / network comes up.
    pub async fn fetch_guest_ip(&self, vm_id: &str) -> Result<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last_err: Option<anyhow::Error> = None;
        loop {
            match self
                .exec_vm(
                    vm_id,
                    "sh",
                    &["-c", "ip -4 -o addr show eth0 | awk '{print $4}' | cut -d/ -f1"],
                )
                .await
            {
                Ok(out) => {
                    let ip = out.stdout.trim().to_string();
                    if ip.is_empty() {
                        last_err = Some(anyhow!("guest eth0 has no IPv4 yet"));
                    } else {
                        return Ok(ip);
                    }
                }
                Err(e) => last_err = Some(e),
            }
            if std::time::Instant::now() >= deadline {
                return Err(last_err
                    .unwrap_or_else(|| anyhow!("fetch_guest_ip timed out without error")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// Probe a TCP port from inside the guest by exec'ing `nc -z`.
    /// Returns Ok(true) if connect succeeded within the timeout.
    pub async fn probe_port_in_guest(
        &self,
        vm_id: &str,
        port: u16,
        timeout_secs: u32,
    ) -> Result<bool> {
        let cmd = format!(
            "nc -z -w{timeout_secs} 127.0.0.1 {port} 2>/dev/null && echo OK || echo FAIL"
        );
        let out = self.exec_vm(vm_id, "sh", &["-c", &cmd]).await?;
        Ok(out.stdout.contains("OK"))
    }

    // Suppress unused warning for StatusCode import; kept for future per-status handling.
    #[allow(dead_code)]
    fn _status_hint(s: StatusCode) -> &'static str {
        if s.is_client_error() {
            "client error"
        } else {
            "server error"
        }
    }
}
