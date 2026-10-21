use std::str::FromStr;

/// A misconfigured knob. Startup fails with it rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

type Lookup = Box<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// Strict environment parsing for the anchoring and signer knobs: a value
/// that does not parse, a secret given both inline and as `<KEY>_FILE`, or an
/// unreadable or empty secret file is an error. An empty value counts as
/// unset, so `KEY=` in an env file switches a feature off.
pub struct Env(Lookup);

impl Env {
    pub fn process() -> Self {
        Self(Box::new(|key| match std::env::var(key) {
            Ok(v) => Ok(Some(v)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(e) => Err(format!("{key}: {e}")),
        }))
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let map: std::collections::HashMap<String, String> = pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Self(Box::new(move |key| Ok(map.get(key).cloned())))
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, ConfigError> {
        let value = (self.0)(key).map_err(ConfigError)?;
        Ok(value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()))
    }

    pub fn parse<T: FromStr>(&self, key: &str) -> Result<Option<T>, ConfigError> {
        self.get(key)?
            .map(|raw| {
                raw.parse::<T>()
                    .map_err(|_| ConfigError(format!("{key}={raw:?} is not a valid value")))
            })
            .transpose()
    }

    /// `KEY` inline or `KEY_FILE` naming a file that holds it, never both.
    pub fn secret(&self, key: &str) -> Result<Option<String>, ConfigError> {
        let file_key = format!("{key}_FILE");
        match (self.get(key)?, self.get(&file_key)?) {
            (Some(_), Some(_)) => Err(ConfigError(format!("set {key} or {file_key}, not both"))),
            (Some(v), None) => Ok(Some(v)),
            (None, Some(path)) => read_secret(&file_key, &path).map(Some),
            (None, None) => Ok(None),
        }
    }

    /// Comma-separated, no empty items, no duplicates.
    pub fn list(&self, key: &str) -> Result<Vec<String>, ConfigError> {
        let Some(raw) = self.get(key)? else {
            return Ok(Vec::new());
        };
        let mut items: Vec<String> = Vec::new();
        for item in raw.split(',').map(str::trim) {
            if item.is_empty() {
                return Err(ConfigError(format!("{key}: empty item in {raw:?}")));
            }
            if items.iter().any(|i| i == item) {
                return Err(ConfigError(format!("{key}: {item} is listed twice")));
            }
            items.push(item.to_string());
        }
        Ok(items)
    }
}

pub fn read_secret(key: &str, path: &str) -> Result<String, ConfigError> {
    let value =
        std::fs::read_to_string(path).map_err(|e| ConfigError(format!("{key}={path}: {e}")))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError(format!("{key}={path}: the file is empty")));
    }
    Ok(value.to_string())
}

/// An http(s) base URL with a host, normalized without a trailing slash.
pub fn http_url(key: &str, raw: &str) -> Result<String, ConfigError> {
    let bad = || ConfigError(format!("{key}: {raw:?} is not an http(s) URL"));
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or_else(bad)?;
    let host = rest.split('/').next().unwrap_or_default();
    let valid = |c: char| c.is_ascii_alphanumeric() || "-._:/[]%~".contains(c);
    if host.is_empty() || host.contains('@') || !rest.chars().all(valid) {
        return Err(bad());
    }
    Ok(raw.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_values() {
        let dir = std::env::temp_dir().join(format!("env-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret");
        let empty = dir.join("empty");
        std::fs::write(&secret, "s3cret\n").unwrap();
        std::fs::write(&empty, "\n").unwrap();
        let (secret, empty) = (secret.to_str().unwrap(), empty.to_str().unwrap());
        let env = Env::from_pairs([
            ("N", " 42 "),
            ("BAD", "4x"),
            ("BLANK", "  "),
            ("A", "inline"),
            ("A_FILE", secret),
            ("B_FILE", secret),
            ("C_FILE", empty),
            ("D_FILE", "/nonexistent/secret"),
            ("L", "x, y"),
            ("L_EMPTY_ITEM", "x,,y"),
            ("L_DUP", "x,x"),
        ]);
        assert_eq!(env.parse::<u64>("N").unwrap(), Some(42));
        assert!(env.parse::<u64>("BAD").is_err());
        assert_eq!(env.get("BLANK").unwrap(), None);
        assert_eq!(env.parse::<u64>("MISSING").unwrap(), None);
        assert!(env.secret("A").is_err(), "inline and _FILE together");
        assert_eq!(env.secret("B").unwrap().as_deref(), Some("s3cret"));
        assert!(env.secret("C").is_err());
        assert!(env.secret("D").is_err());
        assert_eq!(env.secret("E").unwrap(), None);
        assert_eq!(env.list("L").unwrap(), ["x", "y"]);
        assert!(env.list("L_EMPTY_ITEM").is_err());
        assert!(env.list("L_DUP").is_err());
        assert!(env.list("MISSING").unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn urls() {
        assert_eq!(
            http_url("K", "https://a.example/tsr/").unwrap(),
            "https://a.example/tsr"
        );
        assert_eq!(
            http_url("K", "http://127.0.0.1:8200").unwrap(),
            "http://127.0.0.1:8200"
        );
        for bad in [
            "ftp://x",
            "https://",
            "https://u:p@host",
            "https://a b",
            "a.example",
        ] {
            assert!(http_url("K", bad).is_err(), "{bad}");
        }
    }
}
