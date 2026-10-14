//! Build-time provisioning (`wifi.env`, compiled in by `option_env!`).

use crate::canvas::{D_COLS, D_ROWS};
use crate::http::{ChainCache, TlsVerify};
use crate::roots;
use alloc::{format, string::String};

const fn or(value: Option<&'static str>, default: &'static str) -> &'static str {
    match value {
        Some(v) => v,
        None => default,
    }
}

const fn set(value: Option<&'static str>) -> Option<&'static str> {
    match value {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// A bring-up pattern, then halt.
pub const TEST_PATTERN: Option<&str> = set(option_env!("INKPLATE_TEST_PATTERN"));
const _: () = assert!(
    matches!(or(TEST_PATTERN, "1").as_bytes(), b"1" | b"grey"),
    "INKPLATE_TEST_PATTERN is 1 or grey"
);

pub const WIFI_SSID: Option<&str> = set(option_env!("INKPLATE_WIFI_SSID"));
pub const WIFI_PASS: &str = or(option_env!("INKPLATE_WIFI_PASS"), "");
const URL: Option<&str> = set(option_env!("INKPLATE_SERVER_URL"));
const TOKEN: Option<&str> = set(option_env!("INKPLATE_SERVER_TOKEN"));
/// Under common 60s proxy idle cuts.
const WAIT_S: u32 = 55;

pub struct Server {
    pub host: &'static str,
    pub path: String,
    pub bearer: Option<&'static str>,
}

impl Server {
    /// `https://host/path`; always port 443.
    pub fn from_env() -> Result<Self, &'static str> {
        let url = URL.ok_or("NO SERVER URL")?;
        let rest = url.strip_prefix("https://").ok_or("URL NOT HTTPS")?;
        let (host, path) = rest
            .find('/')
            .and_then(|at| rest.split_at_checked(at))
            .unwrap_or((rest, "/"));
        if host.is_empty()
            || host.contains([':', '@', '?'])
            || !url.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err("BAD SERVER URL");
        }
        let join = if path.contains('?') { '&' } else { '?' };
        Ok(Self {
            host,
            path: format!("{path}{join}width={D_COLS}&height={D_ROWS}&tones=4&wait={WAIT_S}"),
            bearer: TOKEN,
        })
    }

    pub const fn tls<'s>(&'s self, cache: &'s mut ChainCache) -> TlsVerify<'s> {
        TlsVerify {
            roots: roots::ROOTS,
            name: self.host,
            cache,
        }
    }
}
