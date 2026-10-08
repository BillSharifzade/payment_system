// --server-bin: run payment-server as a child with the bench's knobs, so a run is one command
// and A/B comparisons can swap binaries. Dev mode (DESIGN §21): single-admin deposits
// (DEPOSIT_DUAL_CONTROL=false) and optional device binding (DEVICE_BINDING=optional, so
// unsigned and signed runs share a server). Limits a benchmark would otherwise trip (per-IP
// rate limit, AML caps) are lifted; the AML guard still sums its window on every post.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use crate::args::Config;

const NO_LIMIT: &str = "1000000000000000";

pub async fn spawn(cfg: &Config) -> Result<(Child, String), String> {
    let bin = cfg.server_bin.as_ref().expect("checked by the caller");
    let log = std::fs::File::create(&cfg.server_log)
        .map_err(|e| format!("{}: {e}", cfg.server_log.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let base = format!("http://127.0.0.1:{}", cfg.server_port);
    let mut cmd = Command::new(bin);
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("APP_ENV", "dev")
        .env("RUST_LOG", "info")
        .env("DATABASE_URL", &cfg.database_url)
        .env("BIND_ADDR", format!("127.0.0.1:{}", cfg.server_port))
        .env("METRICS_ADDR", format!("127.0.0.1:{}", cfg.server_port + 1))
        .env("DB_MAX_CONNECTIONS", cfg.pool_size.to_string())
        .env("TRANSFER_FEE_BPS", cfg.fee_bps.to_string())
        .env("DEPOSIT_DUAL_CONTROL", "false")
        .env("DEVICE_BINDING", "optional")
        .env("RATE_LIMIT_MAX", "4000000000")
        .env("LOGIN_LIMIT_MAX", "4000000000")
        .env(
            "DOCUMENT_STORE_DIR",
            std::env::temp_dir().join("payment-loadtest-kyc"),
        );
    for level in ["L1", "L2"] {
        for limit in ["PER_TX_MINOR", "DAILY_MINOR", "VELOCITY_PER_HOUR"] {
            cmd.env(format!("AML_{level}_{limit}"), NO_LIMIT);
        }
    }
    if let Some(redis) = &cfg.redis_url {
        cmd.env("REDIS_URL", redis);
    }
    for (k, v) in &cfg.server_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;

    let client = reqwest::Client::new();
    for _ in 0..600 {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "payment-server exited early ({status}); see --server-log"
            ));
        }
        if let Ok(r) = client.get(format!("{base}/ready")).send().await {
            if r.status().is_success() {
                return Ok((child, base));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("payment-server not ready after 60s".into())
}
