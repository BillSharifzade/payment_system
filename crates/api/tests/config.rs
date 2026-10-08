// Boot-time configuration rules. The environment is process-global, so these live in their own
// test binary and serialise on a lock.

use api::config::env_bool;
use api::{BiometricConfig, DepositConfig, MatcherBackend};

static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    for (k, v) in vars {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    let out = f();
    for (k, _) in vars {
        std::env::remove_var(k);
    }
    out
}

const KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn exact_matcher_is_refused_outside_dev() {
    let err = with_env(
        &[
            ("BIOMETRIC_TEMPLATE_KEY", Some(KEY)),
            ("BIOMETRIC_MATCHER", None),
        ],
        || BiometricConfig::from_env(true).err(),
    )
    .expect("an unset matcher defaults to exact, which prod refuses");
    assert!(err.contains("BIOMETRIC_MATCHER=exact"), "{err}");
    let err = with_env(
        &[
            ("BIOMETRIC_TEMPLATE_KEY", Some(KEY)),
            ("BIOMETRIC_MATCHER", Some("exact")),
        ],
        || BiometricConfig::from_env(true).err(),
    );
    assert!(err.is_some());

    let prod = with_env(
        &[
            ("BIOMETRIC_TEMPLATE_KEY", Some(KEY)),
            ("BIOMETRIC_MATCHER", Some("http")),
            ("BIOMETRIC_MATCHER_URL", Some("http://matcher:8080")),
        ],
        || BiometricConfig::from_env(true),
    )
    .unwrap();
    assert!(matches!(prod.matcher, MatcherBackend::Http(_)));
    let dev = with_env(&[("BIOMETRIC_MATCHER", None)], || {
        BiometricConfig::from_env(false)
    })
    .unwrap();
    assert!(matches!(dev.matcher, MatcherBackend::Exact));
    assert!(!dev.identify);
    assert_eq!(dev.max_attempts, 5);
    assert_eq!(dev.identify_scale, 10.0);
}

#[test]
fn biometric_identify_and_attempts_parse_strictly() {
    let cfg = with_env(
        &[
            ("BIOMETRIC_IDENTIFY", Some("true")),
            ("BIOMETRIC_MAX_ATTEMPTS", Some("3")),
            ("BIOMETRIC_IDENTIFY_SCALE", Some("12.5")),
        ],
        || BiometricConfig::from_env(false),
    )
    .unwrap();
    assert!(cfg.identify);
    assert_eq!(cfg.max_attempts, 3);
    assert_eq!(cfg.identify_scale, 12.5);
    for (k, v) in [
        ("BIOMETRIC_IDENTIFY", "ture"),
        ("BIOMETRIC_MAX_ATTEMPTS", "0"),
        ("BIOMETRIC_MAX_ATTEMPTS", "five"),
        ("BIOMETRIC_IDENTIFY_SCALE", "-1"),
    ] {
        let res = with_env(&[(k, Some(v))], || BiometricConfig::from_env(false));
        assert!(res.is_err(), "{k}={v} must refuse to boot");
    }
}

#[test]
fn single_admin_deposits_are_dev_only() {
    let d = with_env(&[("DEPOSIT_DUAL_CONTROL", None)], || {
        DepositConfig::from_env(true)
    })
    .unwrap();
    assert!(d.dual_control);
    assert_eq!(d.max_minor, 100_000_000);
    let err = with_env(&[("DEPOSIT_DUAL_CONTROL", Some("false"))], || {
        DepositConfig::from_env(true).err()
    });
    assert!(
        err.is_some(),
        "DEPOSIT_DUAL_CONTROL=false must be refused in prod"
    );
    let dev = with_env(&[("DEPOSIT_DUAL_CONTROL", Some("false"))], || {
        DepositConfig::from_env(false)
    })
    .unwrap();
    assert!(!dev.dual_control);
    for (k, v) in [
        ("DEPOSIT_DUAL_CONTROL", "flase"),
        ("DEPOSIT_MAX_MINOR", "0"),
        ("DEPOSIT_MAX_MINOR", "1e9"),
    ] {
        let res = with_env(&[(k, Some(v))], || DepositConfig::from_env(false));
        assert!(res.is_err(), "{k}={v} must refuse to boot");
    }
    let capped = with_env(&[("DEPOSIT_MAX_MINOR", Some("5000"))], || {
        DepositConfig::from_env(true)
    })
    .unwrap();
    assert_eq!(capped.max_minor, 5_000);
}

#[test]
fn env_bool_accepts_only_unambiguous_values() {
    with_env(&[("API_TEST_FLAG", None)], || {
        assert_eq!(env_bool("API_TEST_FLAG", true), Ok(true));
    });
    for (raw, want) in [
        ("1", true),
        ("YES", true),
        (" on ", true),
        ("0", false),
        ("Off", false),
    ] {
        let got = with_env(&[("API_TEST_FLAG", Some(raw))], || {
            env_bool("API_TEST_FLAG", !want)
        });
        assert_eq!(got, Ok(want), "{raw:?}");
    }
    let bad = with_env(&[("API_TEST_FLAG", Some("enabled"))], || {
        env_bool("API_TEST_FLAG", false)
    });
    assert!(bad.is_err());
}
