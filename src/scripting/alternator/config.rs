use clap::Parser;
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
