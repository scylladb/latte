use clap::Parser;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug, Default, Serialize, Deserialize)]
pub struct DbConnectionConf {
    /// Use AWS credentials and region from the environment.
    /// Mutually exclusive with `access-key-id`, `secret-access-key` and `region`.
    #[clap(long("aws-credentials"), conflicts_with_all = &["access_key_id", "secret_access_key", "region"])]
    pub aws_credentials: bool,

    /// Access key ID.
    #[clap(long("access-key-id"), default_value = "")]
    pub access_key_id: String,

    /// Secret access key.
    #[serde(skip)] // Keep the secret out of generated reports; defaults to empty when a report is read back.
    #[clap(long("secret-access-key"), default_value = "")]
    pub secret_access_key: String,

    /// Region.
    #[clap(long("region"), default_value = "us-east-1")]
    pub region: String,
}

pub fn parse_compression_level(s: &str) -> Result<u8, String> {
    let n: u8 = s
        .parse()
        .map_err(|_| format!("Invalid compression level: {s} (expected integer 1-9)"))?;
    if (1..=9).contains(&n) {
        Ok(n)
    } else {
        Err("Compression level must be between 1 and 9".to_string())
    }
}

/// HTTP request body compression mode for the alternator-driver client.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AlternatorRequestCompressionMode {
    /// Use alternator-driver defaults.
    #[default]
    DriverDefault,
    Off,
    Gzip,
    Zlib,
}

const DEFAULT_COMPRESSION_THRESHOLD: usize = 1024;

/// Alternator-driver connection options.
#[derive(Parser, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AlternatorConnectionOpts {
    /// How to compress Alternator request bodies before signing.
    #[clap(
        long("request-compression"),
        required = false,
        default_value = "driver-default",
        value_name = "MODE",
        value_enum
    )]
    pub request_compression: AlternatorRequestCompressionMode,

    /// Minimum uncompressed body size in bytes before compression applies (`gzip` / `zlib` only).
    #[clap(
        long("compression-threshold"),
        required = false,
        default_value_t = DEFAULT_COMPRESSION_THRESHOLD,
        value_name = "BYTES"
    )]
    pub compression_threshold: usize,

    /// Deflate compression level 1–9 (`gzip` / `zlib` only). If omitted, the driver default level is used.
    #[clap(
        long("compression-level"),
        required = false,
        value_name = "1-9",
        value_parser = parse_compression_level
    )]
    pub compression_level: Option<u8>,

    /// Strip request headers not used by Alternator before transmit. If omitted, the driver default (true) applies.
    #[clap(long("optimize-headers"), required = false, value_name = "BOOL")]
    pub optimize_headers: Option<bool>,
}

/// Serde hook: missing `compression_threshold` in a report must not deserialize as `usize::default()` (0).
fn default_compression_threshold() -> usize {
    DEFAULT_COMPRESSION_THRESHOLD
}

impl Default for AlternatorConnectionOpts {
    fn default() -> Self {
        Self {
            request_compression: AlternatorRequestCompressionMode::default(),
            compression_threshold: default_compression_threshold(),
            compression_level: None,
            optimize_headers: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_without_secret_access_key_field() {
        // Simulates reading a report generated with serde(skip): the secret field is absent.
        let json = r#"{
            "aws_credentials": false,
            "access_key_id": "",
            "region": "us-east-1"
        }"#;
        let conf: DbConnectionConf = serde_json::from_str(json).unwrap();
        assert_eq!(conf.secret_access_key, "");
    }

    #[test]
    fn deserialize_with_secret_access_key_field_ignores_it() {
        // Backwards compatibility: if an older report contains the secret field,
        // deserialization still succeeds and the value is defaulted.
        let json = r#"{
            "aws_credentials": false,
            "access_key_id": "",
            "secret_access_key": "supersecret",
            "region": "us-east-1"
        }"#;
        let conf: DbConnectionConf = serde_json::from_str(json).unwrap();
        assert_eq!(conf.secret_access_key, "");
    }
}
