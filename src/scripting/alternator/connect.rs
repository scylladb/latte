use super::address::normalize_address;
use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use super::context::Context;
use super::driver::create_client;
use crate::config::ConnectionConf;
use aws_config::retry::RetryConfig;
use aws_config::BehaviorVersion;
use aws_sdk_dynamodb::config::{Credentials, Region};
use aws_sdk_dynamodb::error::DisplayErrorContext;

pub async fn connect(conf: &ConnectionConf) -> Result<Context, AlternatorError> {
    let address = conf
        .addresses
        .iter()
        .find_map(|addr| normalize_address(addr))
        .ok_or_else(|| {
            AlternatorError(AlternatorErrorKind::FailedToConnect(
                conf.addresses.join(", "),
                "no valid address".to_string(),
            ))
        })?
        .to_string();

    let mut config_loader = aws_config::defaults(BehaviorVersion::latest())
        .endpoint_url(&address)
        .retry_config(RetryConfig::standard().with_max_attempts(1))
        .timeout_config(
            aws_config::timeout::TimeoutConfig::builder()
                .operation_timeout(conf.request_timeout)
                .build(),
        );

    // We only specify custom credentials if aws_credentials flag is not set.
    // If aws_credentials flag is set, the SDK will automatically use credentials from the environment.
    if !conf.db.aws_credentials {
        let creds = Credentials::new(
            &conf.db.access_key_id,
            &conf.db.secret_access_key,
            None,
            None,
            "",
        );

        config_loader = config_loader
            .credentials_provider(creds)
            .region(Region::new(conf.db.region.clone()));
    }

    let config = config_loader.load().await;

    let client = create_client(&config, conf);

    // Validate connection by making a test request
    client.list_tables().limit(1).send().await.map_err(|e| {
        AlternatorError(AlternatorErrorKind::FailedToConnect(
            address,
            DisplayErrorContext(&e).to_string(),
        ))
    })?;

    Ok(Context::new(
        Some(client),
        conf.retry_number,
        conf.retry_interval,
        conf.validation_strategy,
        conf.page_size.get() as u64,
    ))
}
