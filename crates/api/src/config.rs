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
