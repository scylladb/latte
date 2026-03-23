use clap::builder::PossibleValue;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;
use std::path::PathBuf;

#[derive(Parser, Debug, Serialize, Deserialize)]
pub struct DbConnectionConf {
    /// Number of connections per Cassandra node / Scylla shard.
    #[clap(
        short('c'),
        long("connections"),
        default_value = "1",
        value_name = "COUNT"
    )]
    pub count: NonZeroUsize,

    /// Cassandra user name
    #[clap(long, env("CASSANDRA_USER"), default_value = "")]
    pub user: String,

    /// Password to use if password authentication is required by the server
    #[clap(long, env("CASSANDRA_PASSWORD"), default_value = "")]
    pub password: String,

    /// Enable SSL
    #[clap(long("ssl"))]
    pub ssl: bool,

    /// Path to the CA certificate file in PEM format
    #[clap(long("ssl-ca"), value_name = "PATH")]
    pub ssl_ca_cert_file: Option<PathBuf>,

    /// Path to the client SSL certificate file in PEM format
    #[clap(long("ssl-cert"), value_name = "PATH")]
    pub ssl_cert_file: Option<PathBuf>,

    /// Path to the client SSL private key file in PEM format
    #[clap(long("ssl-key"), value_name = "PATH")]
    pub ssl_key_file: Option<PathBuf>,

    /// Verify if the peer's certificate is trusted
    #[clap(long("ssl-peer-verification"))]
    pub ssl_peer_verification: bool,

    /// Datacenter name
    #[clap(long("datacenter"), required = false)]
    pub datacenter: Option<String>,

    /// Rack name
    #[clap(long("rack"), required = false)]
    pub rack: Option<String>,

    /// CQL query consistency level.
    /// 'SERIAL' and 'LOCAL_SERIAL' values are compatible only with SELECT statements
    /// and make Scylla use Paxos consensus algorithm
    #[clap(long("consistency"), required = false, default_value = "LOCAL_QUORUM")]
    pub consistency: Consistency,

    /// Serial consistency level for conditional (LWT) queries
    #[clap(
        long("serial-consistency"),
        required = false,
        default_value = "LOCAL_SERIAL"
    )]
    pub serial_consistency: SerialConsistency,
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Consistency {
    Any,
    One,
    Two,
    Three,
    Quorum,
    All,
    LocalOne,
    #[default]
    LocalQuorum,
    EachQuorum,
    // NOTE: 'Serial' and 'LocalSerial' values may be used in SELECT statements
    // to make them use Paxos consensus algorithm.
    Serial,
    LocalSerial,
}

impl Consistency {
    pub fn consistency(&self) -> scylla::frame::types::Consistency {
        match self {
            Self::Any => scylla::frame::types::Consistency::Any,
            Self::One => scylla::frame::types::Consistency::One,
            Self::Two => scylla::frame::types::Consistency::Two,
            Self::Three => scylla::frame::types::Consistency::Three,
            Self::Quorum => scylla::frame::types::Consistency::Quorum,
            Self::All => scylla::frame::types::Consistency::All,
            Self::LocalOne => scylla::frame::types::Consistency::LocalOne,
            Self::LocalQuorum => scylla::frame::types::Consistency::LocalQuorum,
            Self::EachQuorum => scylla::frame::types::Consistency::EachQuorum,
            Self::Serial => scylla::frame::types::Consistency::Serial,
            Self::LocalSerial => scylla::frame::types::Consistency::LocalSerial,
        }
    }
}

impl ValueEnum for Consistency {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self::Any,
            Self::One,
            Self::Two,
            Self::Three,
            Self::Quorum,
            Self::All,
            Self::LocalOne,
            Self::LocalQuorum,
            Self::EachQuorum,
            Self::Serial,
            Self::LocalSerial,
        ]
    }

    fn from_str(s: &str, _ignore_case: bool) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "any" => Ok(Self::Any),
            "one" | "1" => Ok(Self::One),
            "two" | "2" => Ok(Self::Two),
            "three" | "3" => Ok(Self::Three),
            "quorum" | "q" => Ok(Self::Quorum),
            "all" => Ok(Self::All),
            "local_one" | "localone" | "l1" => Ok(Self::LocalOne),
            "local_quorum" | "localquorum" | "lq" => Ok(Self::LocalQuorum),
            "each_quorum" | "eachquorum" | "eq" => Ok(Self::EachQuorum),
            "serial" | "s" => Ok(Self::Serial),
            "local_serial" | "localserial" | "ls" => Ok(Self::LocalSerial),
            s => Err(format!("Unknown consistency level {s}")),
        }
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        match self {
            Self::Any => Some(PossibleValue::new("ANY")),
            Self::One => Some(PossibleValue::new("ONE")),
            Self::Two => Some(PossibleValue::new("TWO")),
            Self::Three => Some(PossibleValue::new("THREE")),
            Self::Quorum => Some(PossibleValue::new("QUORUM")),
            Self::All => Some(PossibleValue::new("ALL")),
            Self::LocalOne => Some(PossibleValue::new("LOCAL_ONE")),
            Self::LocalQuorum => Some(PossibleValue::new("LOCAL_QUORUM")),
            Self::EachQuorum => Some(PossibleValue::new("EACH_QUORUM")),
            Self::Serial => Some(PossibleValue::new("SERIAL")),
            Self::LocalSerial => Some(PossibleValue::new("LOCAL_SERIAL")),
        }
    }
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SerialConsistency {
    Serial,
    #[default]
    LocalSerial,
}

impl SerialConsistency {
    pub fn serial_consistency(&self) -> scylla::frame::types::SerialConsistency {
        match self {
            Self::Serial => scylla::frame::types::SerialConsistency::Serial,
            Self::LocalSerial => scylla::frame::types::SerialConsistency::LocalSerial,
        }
    }
}

impl ValueEnum for SerialConsistency {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Serial, Self::LocalSerial]
    }

    fn from_str(s: &str, _ignore_case: bool) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "serial" | "s" => Ok(Self::Serial),
            "local_serial" | "localserial" | "ls" => Ok(Self::LocalSerial),
            s => Err(format!("Unknown serial consistency level {s}")),
        }
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        match self {
            Self::Serial => Some(PossibleValue::new("SERIAL")),
            Self::LocalSerial => Some(PossibleValue::new("LOCAL_SERIAL")),
        }
    }
}
