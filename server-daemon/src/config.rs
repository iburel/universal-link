// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Configuration of the server binary, read from the environment (12-factor —
//! this is how you configure a container).
//!
//! Philosophy **opposite to the Core's**: the Core always starts, even if it has
//! to run unpaired, because IPC is its only channel to tell the GUI what's
//! wrong. A server has no one to talk to: an invalid configuration makes it
//! **refuse to start**. And all errors are reported at once — not one per
//! restart.
//!
//! Required: `UNIVERSALLINK_SERVER_BIND`, `UNIVERSALLINK_OIDC_ISSUER`,
//! `UNIVERSALLINK_OIDC_CLIENT_ID`. The other settings have defaults aligned with
//! the guidance in `doc/server-api.md`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use universallink_server::{Config, OidcConfig};

const DEFAULT_HEARTBEAT_SECS: u64 = 30;
const DEFAULT_HEARTBEAT_MAX_MISSED: u32 = 2;
const DEFAULT_NONCE_TTL_SECS: u64 = 60;
const DEFAULT_FRESH_TOKEN_MAX_AGE_SECS: u64 = 300;
const DEFAULT_MAX_REQUESTS_PER_MINUTE: u32 = 120;

/// Reads the configuration from the process environment.
pub fn load() -> Result<Config, String> {
    load_from(&|key| std::env::var(key).ok())
}

/// Testable core: the environment source is injected.
fn load_from(env: &dyn Fn(&str) -> Option<String>) -> Result<Config, String> {
    let mut errors: Vec<String> = Vec::new();

    let bind_addr = required(env, "UNIVERSALLINK_SERVER_BIND", &mut errors)
        .and_then(|v| parse::<SocketAddr>("UNIVERSALLINK_SERVER_BIND", &v, &mut errors));

    // The scheme is checked here rather than discovered on the first OIDC
    // connection: otherwise a typo would give an opaque authentication failure.
    let issuer_url = required(env, "UNIVERSALLINK_OIDC_ISSUER", &mut errors).filter(|v| {
        if v.starts_with("http://") || v.starts_with("https://") {
            true
        } else {
            errors.push(format!(
                "UNIVERSALLINK_OIDC_ISSUER must start with http:// or https:// : {v}"
            ));
            false
        }
    });

    let client_id = required(env, "UNIVERSALLINK_OIDC_CLIENT_ID", &mut errors);

    let heartbeat_interval = optional_secs(
        env,
        "UNIVERSALLINK_HEARTBEAT_SECS",
        DEFAULT_HEARTBEAT_SECS,
        &mut errors,
    );
    let heartbeat_max_missed = optional_u32(
        env,
        "UNIVERSALLINK_HEARTBEAT_MAX_MISSED",
        DEFAULT_HEARTBEAT_MAX_MISSED,
        &mut errors,
    );
    let nonce_ttl = optional_secs(
        env,
        "UNIVERSALLINK_NONCE_TTL_SECS",
        DEFAULT_NONCE_TTL_SECS,
        &mut errors,
    );
    let max_fresh_token_age = optional_secs(
        env,
        "UNIVERSALLINK_FRESH_TOKEN_MAX_AGE_SECS",
        DEFAULT_FRESH_TOKEN_MAX_AGE_SECS,
        &mut errors,
    );
    let max_requests_per_minute = optional_rate_limit(env, &mut errors);

    if !errors.is_empty() {
        return Err(errors.join(" ; "));
    }

    // `errors` empty ⟹ all required fields have been validated.
    Ok(Config {
        bind_addr: bind_addr.expect("validated"),
        oidc: OidcConfig {
            issuer_url: issuer_url.expect("validated"),
            client_id: client_id.expect("validated"),
            max_fresh_token_age: max_fresh_token_age.expect("validated"),
        },
        heartbeat_interval: heartbeat_interval.expect("validated"),
        heartbeat_max_missed: heartbeat_max_missed.expect("validated"),
        nonce_ttl: nonce_ttl.expect("validated"),
        max_requests_per_minute: max_requests_per_minute.expect("validated"),
    })
}

/// Required variable: present and non-empty. A variable SET BUT EMPTY
/// (`export FOO=`) counts as absent — same rule as on the Core side.
fn required(
    env: &dyn Fn(&str) -> Option<String>,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match non_empty(env, key) {
        Some(value) => Some(value),
        None => {
            errors.push(format!("{key} is required"));
            None
        }
    }
}

fn parse<T>(key: &str, raw: &str, errors: &mut Vec<String>) -> Option<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match raw.parse() {
        Ok(value) => Some(value),
        Err(e) => {
            errors.push(format!("{key} invalid ({raw}): {e}"));
            None
        }
    }
}

/// Optional setting in seconds: absent → default; present → parsed.
fn optional_secs(
    env: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: u64,
    errors: &mut Vec<String>,
) -> Option<Duration> {
    match non_empty(env, key) {
        None => Some(Duration::from_secs(default)),
        Some(raw) => parse::<u64>(key, &raw, errors).map(Duration::from_secs),
    }
}

fn optional_u32(
    env: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: u32,
    errors: &mut Vec<String>,
) -> Option<u32> {
    match non_empty(env, key) {
        None => Some(default),
        Some(raw) => parse::<u32>(key, &raw, errors),
    }
}

/// Request limit: absent → protective default; `0` → unlimited (`None`).
/// The outer `Option` distinguishes "no error" from "parse error".
fn optional_rate_limit(
    env: &dyn Fn(&str) -> Option<String>,
    errors: &mut Vec<String>,
) -> Option<Option<u32>> {
    const KEY: &str = "UNIVERSALLINK_MAX_REQUESTS_PER_MINUTE";
    match non_empty(env, KEY) {
        None => Some(Some(DEFAULT_MAX_REQUESTS_PER_MINUTE)),
        Some(raw) => match parse::<u32>(KEY, &raw, errors) {
            Some(0) => Some(None),
            Some(n) => Some(Some(n)),
            None => None,
        },
    }
}

/// A variable's value, `None` if absent or empty after trimming.
fn non_empty(env: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Path of the persisted directory file (`UNIVERSALLINK_SERVER_STATE`).
/// Optional: defaults to `universallink-directory.json` in the current folder.
/// On a deployment, point it at a volume
/// (`UNIVERSALLINK_SERVER_STATE=/data/directory.json`). Any path is valid — so
/// never a configuration error.
pub fn state_path() -> PathBuf {
    state_path_from(&|key| std::env::var(key).ok())
}

fn state_path_from(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    non_empty(env, "UNIVERSALLINK_SERVER_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("universallink-directory.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    const REQUIRED: &[(&str, &str)] = &[
        ("UNIVERSALLINK_SERVER_BIND", "0.0.0.0:8080"),
        ("UNIVERSALLINK_OIDC_ISSUER", "https://accounts.google.com"),
        ("UNIVERSALLINK_OIDC_CLIENT_ID", "abc.apps.googleusercontent.com"),
    ];

    #[test]
    fn minimal_config_uses_defaults() {
        let config = load_from(&env_of(REQUIRED)).expect("valid config");
        assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.oidc.issuer_url, "https://accounts.google.com");
        assert_eq!(config.oidc.client_id, "abc.apps.googleusercontent.com");
        assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
        assert_eq!(config.heartbeat_max_missed, 2);
        assert_eq!(config.nonce_ttl, Duration::from_secs(60));
        assert_eq!(config.oidc.max_fresh_token_age, Duration::from_secs(300));
        assert_eq!(config.max_requests_per_minute, Some(120));
    }

    #[test]
    fn all_missing_required_are_reported_together() {
        // All errors at once: we don't want to discover them one per restart.
        let err = load_from(&env_of(&[])).expect_err("must fail");
        assert!(err.contains("UNIVERSALLINK_SERVER_BIND"), "{err}");
        assert!(err.contains("UNIVERSALLINK_OIDC_ISSUER"), "{err}");
        assert!(err.contains("UNIVERSALLINK_OIDC_CLIENT_ID"), "{err}");
    }

    #[test]
    fn an_empty_variable_counts_as_missing() {
        let mut vars = REQUIRED.to_vec();
        vars[0] = ("UNIVERSALLINK_SERVER_BIND", "   ");
        let err = load_from(&env_of(&vars)).expect_err("must fail");
        assert!(err.contains("UNIVERSALLINK_SERVER_BIND"), "{err}");
    }

    #[test]
    fn a_bad_bind_address_is_refused() {
        let mut vars = REQUIRED.to_vec();
        vars[0] = ("UNIVERSALLINK_SERVER_BIND", "not-an-address");
        let err = load_from(&env_of(&vars)).expect_err("must fail");
        assert!(err.contains("UNIVERSALLINK_SERVER_BIND"), "{err}");
    }

    #[test]
    fn the_issuer_scheme_is_checked() {
        let mut vars = REQUIRED.to_vec();
        vars[1] = ("UNIVERSALLINK_OIDC_ISSUER", "accounts.google.com");
        let err = load_from(&env_of(&vars)).expect_err("must fail");
        assert!(err.contains("http"), "{err}");
    }

    #[test]
    fn tuning_knobs_can_be_overridden() {
        let mut vars = REQUIRED.to_vec();
        vars.push(("UNIVERSALLINK_HEARTBEAT_SECS", "10"));
        vars.push(("UNIVERSALLINK_HEARTBEAT_MAX_MISSED", "5"));
        vars.push(("UNIVERSALLINK_NONCE_TTL_SECS", "15"));
        vars.push(("UNIVERSALLINK_FRESH_TOKEN_MAX_AGE_SECS", "600"));
        vars.push(("UNIVERSALLINK_MAX_REQUESTS_PER_MINUTE", "300"));
        let config = load_from(&env_of(&vars)).expect("valid config");
        assert_eq!(config.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(config.heartbeat_max_missed, 5);
        assert_eq!(config.nonce_ttl, Duration::from_secs(15));
        assert_eq!(config.oidc.max_fresh_token_age, Duration::from_secs(600));
        assert_eq!(config.max_requests_per_minute, Some(300));
    }

    #[test]
    fn a_zero_rate_limit_means_unlimited() {
        let mut vars = REQUIRED.to_vec();
        vars.push(("UNIVERSALLINK_MAX_REQUESTS_PER_MINUTE", "0"));
        let config = load_from(&env_of(&vars)).expect("valid config");
        assert_eq!(config.max_requests_per_minute, None);
    }

    #[test]
    fn a_bad_numeric_is_refused() {
        let mut vars = REQUIRED.to_vec();
        vars.push(("UNIVERSALLINK_NONCE_TTL_SECS", "many"));
        let err = load_from(&env_of(&vars)).expect_err("must fail");
        assert!(err.contains("UNIVERSALLINK_NONCE_TTL_SECS"), "{err}");
    }

    #[test]
    fn the_state_path_defaults_and_can_be_overridden() {
        assert_eq!(
            state_path_from(&env_of(&[])),
            PathBuf::from("universallink-directory.json")
        );
        assert_eq!(
            state_path_from(&env_of(&[("UNIVERSALLINK_SERVER_STATE", "/data/a.json")])),
            PathBuf::from("/data/a.json")
        );
    }
}
