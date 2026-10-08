use std::str::FromStr;
use std::time::Duration;

/// `LEDGER_BACKEND`: where balances live. Anything but these two names is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    TigerBeetle,
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "postgres" => Ok(Backend::Postgres),
            "tigerbeetle" => Ok(Backend::TigerBeetle),
            _ => Err(format!(
                "LEDGER_BACKEND={s:?} must be postgres or tigerbeetle"
            )),
        }
    }
}

impl Backend {
    /// Unset means `postgres`.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("LEDGER_BACKEND") {
            Err(std::env::VarError::NotPresent) => Ok(Backend::Postgres),
            Err(e) => Err(format!("LEDGER_BACKEND: {e}")),
            Ok(raw) => raw.trim().parse(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbConfig {
    /// `TIGERBEETLE_CLUSTER_ID` (required): the id the cluster was formatted with.
    pub cluster_id: u128,
    /// `TIGERBEETLE_ADDRESSES` (required): every replica, comma-separated, `port` or
    /// `host:port`, in the same order the replicas were started with.
    pub addresses: String,
    /// `TIGERBEETLE_PENDING_TIMEOUT_SECS` (default 120; 0 = never): after this long the cluster
    /// voids a reservation by itself. Only the backstop for a dead recovery worker.
    pub pending_timeout_secs: u32,
    /// `TIGERBEETLE_REQUEST_TIMEOUT_MS` (default 5000): the client retries internally forever;
    /// this bounds how long a request waits before failing as unavailable.
    pub request_timeout: Duration,
    /// `TIGERBEETLE_RECOVERY_GRACE_SECS` (default 10): recovery leaves younger reservations to
    /// the request that made them. Correctness does not depend on it, only on whom it fails.
    pub recovery_grace: Duration,
}

impl TbConfig {
    pub const DEFAULT_PENDING_TIMEOUT_SECS: u32 = 120;
    pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5_000;
    pub const DEFAULT_RECOVERY_GRACE_SECS: u64 = 10;

    pub fn new(cluster_id: u128, addresses: impl Into<String>) -> Self {
        Self {
            cluster_id,
            addresses: addresses.into(),
            pending_timeout_secs: Self::DEFAULT_PENDING_TIMEOUT_SECS,
            request_timeout: Duration::from_millis(Self::DEFAULT_REQUEST_TIMEOUT_MS),
            recovery_grace: Duration::from_secs(Self::DEFAULT_RECOVERY_GRACE_SECS),
        }
    }

    /// A reservation must commit within half its timeout: the other half is the recovery
    /// worker's to post it before the cluster would void it.
    pub fn commit_budget(&self) -> Option<Duration> {
        (self.pending_timeout_secs > 0)
            .then(|| Duration::from_secs(self.pending_timeout_secs as u64) / 2)
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        fn parse<T: FromStr>(key: &str, raw: Option<String>) -> Result<Option<T>, String> {
            raw.map(|v| {
                v.trim()
                    .parse()
                    .map_err(|_| format!("{key}={v:?} is not a valid value"))
            })
            .transpose()
        }
        let cluster_id: u128 = parse("TIGERBEETLE_CLUSTER_ID", get("TIGERBEETLE_CLUSTER_ID"))?
            .ok_or("TIGERBEETLE_CLUSTER_ID is required when LEDGER_BACKEND=tigerbeetle")?;
        let addresses = get("TIGERBEETLE_ADDRESSES")
            .ok_or("TIGERBEETLE_ADDRESSES is required when LEDGER_BACKEND=tigerbeetle")?;
        let addresses = parse_addresses(&addresses)?;
        let mut cfg = Self::new(cluster_id, addresses);
        if let Some(v) = parse(
            "TIGERBEETLE_PENDING_TIMEOUT_SECS",
            get("TIGERBEETLE_PENDING_TIMEOUT_SECS"),
        )? {
            cfg.pending_timeout_secs = v;
        }
        if let Some(ms) = parse::<u64>(
            "TIGERBEETLE_REQUEST_TIMEOUT_MS",
            get("TIGERBEETLE_REQUEST_TIMEOUT_MS"),
        )? {
            cfg.request_timeout = Duration::from_millis(ms);
        }
        if let Some(s) = parse::<u64>(
            "TIGERBEETLE_RECOVERY_GRACE_SECS",
            get("TIGERBEETLE_RECOVERY_GRACE_SECS"),
        )? {
            cfg.recovery_grace = Duration::from_secs(s);
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        let t = self.pending_timeout_secs;
        if t != 0 && !(10..=86_400).contains(&t) {
            return Err(format!(
                "TIGERBEETLE_PENDING_TIMEOUT_SECS={t} must be 0 (never) or 10..=86400"
            ));
        }
        let ms = self.request_timeout.as_millis();
        if !(100..=60_000).contains(&ms) {
            return Err(format!(
                "TIGERBEETLE_REQUEST_TIMEOUT_MS={ms} must be 100..=60000"
            ));
        }
        let grace = self.recovery_grace.as_secs();
        if !(1..=600).contains(&grace) {
            return Err(format!(
                "TIGERBEETLE_RECOVERY_GRACE_SECS={grace} must be 1..=600"
            ));
        }
        if self
            .commit_budget()
            .is_some_and(|b| self.recovery_grace >= b)
        {
            return Err(format!(
                "TIGERBEETLE_RECOVERY_GRACE_SECS={grace} must be below half of \
                 TIGERBEETLE_PENDING_TIMEOUT_SECS={t}, or reservations could expire first"
            ));
        }
        Ok(())
    }
}

fn parse_addresses(raw: &str) -> Result<String, String> {
    let bad = || {
        format!("TIGERBEETLE_ADDRESSES={raw:?} must be a comma-separated list of port or host:port")
    };
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.is_empty() || parts.len() > 6 {
        return Err(bad());
    }
    for p in &parts {
        let port = match p.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => port,
            Some(_) => return Err(bad()),
            None => p,
        };
        if port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
            return Err(bad());
        }
    }
    Ok(parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> Result<TbConfig, String> {
        let m: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        TbConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn strict_parsing() {
        let base = [
            ("TIGERBEETLE_CLUSTER_ID", "7"),
            ("TIGERBEETLE_ADDRESSES", "3000, 10.0.0.2:3001"),
        ];
        let c = cfg(&base).unwrap();
        assert_eq!(
            (c.cluster_id, c.addresses.as_str()),
            (7, "3000,10.0.0.2:3001")
        );
        assert_eq!(c.commit_budget(), Some(Duration::from_secs(60)));

        assert!(cfg(&base[..1]).is_err());
        assert!(cfg(&base[1..]).is_err());
        for (k, v) in [
            ("TIGERBEETLE_CLUSTER_ID", "-1"),
            ("TIGERBEETLE_ADDRESSES", ""),
            ("TIGERBEETLE_ADDRESSES", "3000,"),
            ("TIGERBEETLE_ADDRESSES", ":3000"),
            ("TIGERBEETLE_ADDRESSES", "host:http"),
            ("TIGERBEETLE_PENDING_TIMEOUT_SECS", "5"),
            ("TIGERBEETLE_PENDING_TIMEOUT_SECS", "1m"),
            ("TIGERBEETLE_REQUEST_TIMEOUT_MS", "0"),
            ("TIGERBEETLE_RECOVERY_GRACE_SECS", "0"),
            ("TIGERBEETLE_RECOVERY_GRACE_SECS", "60"),
        ] {
            let mut vars = base.to_vec();
            vars.retain(|(key, _)| *key != k);
            vars.push((k, v));
            assert!(cfg(&vars).is_err(), "{k}={v} must be refused");
        }
        let mut never = base.to_vec();
        never.push(("TIGERBEETLE_PENDING_TIMEOUT_SECS", "0"));
        never.push(("TIGERBEETLE_RECOVERY_GRACE_SECS", "600"));
        assert_eq!(cfg(&never).unwrap().commit_budget(), None);

        assert_eq!("tigerbeetle".parse(), Ok(Backend::TigerBeetle));
        assert!("TigerBeetle".parse::<Backend>().is_err());
    }
}
