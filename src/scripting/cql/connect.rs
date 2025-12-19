use super::cass_error::{CassError, CassErrorKind};
use super::context::Context;
use crate::config::ConnectionConf;
use openssl::ssl::{SslContextBuilder, SslFiletype, SslMethod, SslVerifyMode};
use scylla::client::session::TlsContext;
use scylla::client::PoolSize;
use scylla::policies::load_balancing::DefaultPolicy;

use scylla::client::execution_profile::ExecutionProfile;
use scylla::client::session_builder::SessionBuilder;

fn tls_context(conf: &&ConnectionConf) -> Result<Option<TlsContext>, Box<CassError>> {
    if conf.ssl {
        let mut ssl = SslContextBuilder::new(SslMethod::tls())?;
        if let Some(path) = &conf.ssl_ca_cert_file {
            ssl.set_ca_file(path)?;
        }
        if let Some(path) = &conf.ssl_cert_file {
            ssl.set_certificate_file(path, SslFiletype::PEM)?;
        }
        if let Some(path) = &conf.ssl_key_file {
            ssl.set_private_key_file(path, SslFiletype::PEM)?;
        }
        if conf.ssl_peer_verification {
            ssl.set_verify(SslVerifyMode::PEER);
        }
        Ok(Some(TlsContext::from(ssl.build())))
    } else {
        Ok(None)
    }
}

/// Configures connection to Cassandra.
pub async fn connect(conf: &ConnectionConf) -> Result<Context, CassError> {
    let mut policy_builder = DefaultPolicy::builder().token_aware(true);
    let mut datacenter: String = "".to_string();
    let mut rack: String = "".to_string();
    if let Some(dc) = &conf.datacenter {
        if let Some(current_rack) = &conf.rack {
            policy_builder = policy_builder
                .prefer_datacenter_and_rack(dc.to_owned(), current_rack.to_owned())
                .permit_dc_failover(true);
            rack = current_rack.clone();
        } else {
            policy_builder = policy_builder
                .prefer_datacenter(dc.to_owned())
                .permit_dc_failover(true);
        }
        datacenter = dc.clone();
    } else if let Some(_rack) = &conf.rack {
        panic!("Datacenter must also be defined when rack is defined");
    }
    let profile = ExecutionProfile::builder()
        .consistency(conf.consistency.consistency())
        .serial_consistency(Some(conf.serial_consistency.serial_consistency()))
        .load_balancing_policy(policy_builder.build())
        .request_timeout(Some(conf.request_timeout))
        .build();

    let scylla_session = SessionBuilder::new()
        .known_nodes(&conf.addresses)
        .pool_size(PoolSize::PerShard(conf.count))
        .user(&conf.user, &conf.password)
        .tls_context(tls_context(&conf)?)
        .default_execution_profile_handle(profile.into_handle())
        .build()
        .await
        .map_err(|e| CassError(CassErrorKind::FailedToConnect(conf.addresses.clone(), e)))?;
    Ok(Context::new(
        Some(scylla_session),
        conf.page_size.get() as u64,
        datacenter,
        rack,
        conf.retry_number,
        conf.retry_interval,
        conf.validation_strategy,
    ))
}
