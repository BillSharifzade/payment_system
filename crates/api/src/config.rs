use std::str::FromStr;

pub fn env_parse<T: FromStr>(key: &str) -> Result<Option<T>, String> {
    match std::env::var(key) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(format!("{key}: {e}")),
        Ok(raw) => raw
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("{key}={raw:?} is not a valid value")),
    }
}

pub fn env_or<T: FromStr>(key: &str, default: T) -> Result<T, String> {
    Ok(env_parse(key)?.unwrap_or(default))
}

pub fn env_or_file(key: &str) -> Result<Option<String>, String> {
    if let Ok(v) = std::env::var(key) {
        return Ok(Some(v));
    }
    let file_key = format!("{key}_FILE");
    match std::env::var(&file_key) {
        Ok(path) => std::fs::read_to_string(&path)
            .map(|s| Some(s.trim().to_string()))
            .map_err(|e| format!("{file_key}={path}: {e}")),
        Err(_) => Ok(None),
    }
}

pub fn env_bool(key: &str, default: bool) -> Result<bool, String> {
    match std::env::var(key) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("{key}: {e}")),
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("{key}={raw:?} must be true or false")),
        },
    }
}

pub fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false)
}

/// How money-moving requests are bound to a registered device (`DEVICE_BINDING`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceBinding {
    /// Every transfer, FX and app check payment must carry a valid device signature.
    Required,
    /// Signatures are verified when present (a bad one is refused); unsigned requests pass.
    Optional,
    /// Device headers are ignored.
    Off,
}

impl DeviceBinding {
    pub fn name(&self) -> &'static str {
        match self {
            DeviceBinding::Required => "required",
            DeviceBinding::Optional => "optional",
            DeviceBinding::Off => "off",
        }
    }
}

impl FromStr for DeviceBinding {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "required" => Ok(DeviceBinding::Required),
            "optional" => Ok(DeviceBinding::Optional),
            "off" => Ok(DeviceBinding::Off),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceConfig {
    pub binding: DeviceBinding,
    /// Active (non-revoked) devices one user may have at once (`DEVICE_MAX_ACTIVE`).
    pub max_active: i64,
}

impl DeviceConfig {
    pub const DEFAULT_MAX_ACTIVE: i64 = 3;

    /// Dev defaults: signatures are verified when sent, not demanded.
    pub fn dev() -> Self {
        Self {
            binding: DeviceBinding::Optional,
            max_active: Self::DEFAULT_MAX_ACTIVE,
        }
    }

    // `required` unless APP_ENV=dev; anything weaker is refused outside dev, and an
    // unrecognised value never falls back to a default.
    pub fn from_env(is_prod: bool) -> Result<Self, String> {
        let binding = match std::env::var("DEVICE_BINDING") {
            Err(std::env::VarError::NotPresent) if is_prod => DeviceBinding::Required,
            Err(std::env::VarError::NotPresent) => DeviceBinding::Optional,
            Err(e) => return Err(format!("DEVICE_BINDING: {e}")),
            Ok(raw) => raw
                .trim()
                .parse()
                .map_err(|_| format!("DEVICE_BINDING={raw:?} must be required, optional or off"))?,
        };
        if is_prod && binding != DeviceBinding::Required {
            return Err(format!(
                "DEVICE_BINDING={} is only allowed when APP_ENV=dev",
                binding.name()
            ));
        }
        let max_active: i64 = env_or("DEVICE_MAX_ACTIVE", Self::DEFAULT_MAX_ACTIVE)?;
        if !(1..=100).contains(&max_active) {
            return Err("DEVICE_MAX_ACTIVE must be between 1 and 100".to_string());
        }
        Ok(Self {
            binding,
            max_active,
        })
    }
}
