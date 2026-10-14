use super::address::normalize_addresses;
use super::config::{AlternatorKeyRouteAffinity, AlternatorRequestCompressionMode};
use alternator_driver::{
    AlternatorClient, AlternatorConfig, CompressionAlgorithm, CompressionLevel,
    KeyRouteAffinityConfig, KeyRouteAffinityType, RequestCompression, RoutingScope,
};

pub fn create_client(
    sdk_config: &aws_config::SdkConfig,
    conf: &crate::config::ConnectionConf,
) -> AlternatorClient {
    AlternatorClient::from_conf(create_config(sdk_config, conf))
}

fn create_config(
    sdk_config: &aws_config::SdkConfig,
    conf: &crate::config::ConnectionConf,
) -> AlternatorConfig {
    let opts = &conf.alternator;

    // The client no longer imports shared SDK config. Preserve the supported
    // SDK settings while deriving endpoints from the Alternator seed hosts.
    let mut builder = AlternatorConfig::builder();
    builder.set_region(sdk_config.region().cloned());
    builder.set_credentials_provider(sdk_config.credentials_provider());
    builder.set_retry_config(sdk_config.retry_config().cloned());
    builder.set_timeout_config(sdk_config.timeout_config().cloned());
    builder.set_http_client(sdk_config.http_client());
    builder.set_sleep_impl(sdk_config.sleep_impl());
    if let Some(identity_cache) = sdk_config.identity_cache() {
        builder.set_identity_cache(identity_cache);
    }
    builder.set_app_name(sdk_config.app_name().cloned());

    let normalized = normalize_addresses(&conf.addresses);
    let mut seed_hosts = Vec::with_capacity(normalized.len());
    let mut detected_scheme = None;
    let mut detected_port = None;

    for url in &normalized {
        if let Some(host) = url.host_str() {
            seed_hosts.push(host.to_string());
        }
        if detected_scheme.is_none() {
            detected_scheme = Some(url.scheme().to_string());
        }
        if detected_port.is_none() {
            detected_port = url.port();
        }
    }

    if !seed_hosts.is_empty() {
        builder = builder.seed_hosts(seed_hosts);
    }
    if let Some(scheme) = detected_scheme {
        builder = builder.scheme(scheme);
    }
    if let Some(port) = detected_port {
        builder = builder.port(port);
    }

    if let Some(optimize) = opts.optimize_headers {
        builder = builder.optimize_headers(optimize);
    }
    match opts.request_compression {
        AlternatorRequestCompressionMode::DriverDefault => {}
        AlternatorRequestCompressionMode::Off => {
            builder = builder.request_compression(RequestCompression::disabled());
        }
        AlternatorRequestCompressionMode::Gzip => {
            let level = opts
                .compression_level
                .map(|n| CompressionLevel::new(n as u32))
                .unwrap_or_default();
            builder = builder.request_compression(RequestCompression::enabled(
                CompressionAlgorithm::Gzip,
                level,
                opts.compression_threshold,
            ));
        }
        AlternatorRequestCompressionMode::Zlib => {
            // The client calls zlib-wrapped DEFLATE "Deflate"; keep the CLI name.
            let level = opts
                .compression_level
                .map(|n| CompressionLevel::new(n as u32))
                .unwrap_or_default();
            builder = builder.request_compression(RequestCompression::enabled(
                CompressionAlgorithm::Deflate,
                level,
                opts.compression_threshold,
            ));
        }
    }

    if let Some(active) = opts.active_interval {
        builder = builder.active_interval(tokio::time::Duration::from_millis(active));
    }
    if let Some(idle) = opts.idle_interval {
        builder = builder.idle_interval(tokio::time::Duration::from_millis(idle));
    }

    let routing_scope = match (&conf.datacenter, &conf.rack) {
        (Some(dc), Some(rack)) => {
            if opts.routing_fallback.unwrap_or(true) {
                RoutingScope::from_rack(dc.clone(), rack.clone())
                    .with_fallback(RoutingScope::from_datacenter(dc.clone()))
                    .with_fallback(RoutingScope::from_cluster())
            } else {
                RoutingScope::from_rack(dc.clone(), rack.clone())
            }
        }
        (Some(dc), None) => {
            if opts.routing_fallback.unwrap_or(true) {
                RoutingScope::from_datacenter(dc.clone())
                    .with_fallback(RoutingScope::from_cluster())
            } else {
                RoutingScope::from_datacenter(dc.clone())
            }
        }
        (None, Some(_)) => {
            panic!("Datacenter must also be defined when rack is defined");
        }
        _ => RoutingScope::from_cluster(),
    };
    builder = builder.routing_scope(routing_scope);

    let affinity_mode =
        opts.key_route_affinity
            .unwrap_or(if !opts.key_route_affinity_tables.is_empty() {
                AlternatorKeyRouteAffinity::Rmw
            } else {
                AlternatorKeyRouteAffinity::None
            });

    if affinity_mode != AlternatorKeyRouteAffinity::None {
        let affinity_type = match affinity_mode {
            AlternatorKeyRouteAffinity::None => KeyRouteAffinityType::None,
            AlternatorKeyRouteAffinity::Rmw => KeyRouteAffinityType::Rmw,
            AlternatorKeyRouteAffinity::AnyWrite => KeyRouteAffinityType::AnyWrite,
        };
        let mut affinity_builder = KeyRouteAffinityConfig::builder().with_type(affinity_type);
        for (table, pk) in &opts.key_route_affinity_tables {
            affinity_builder = affinity_builder.with_pk_info(table, pk);
        }
        builder = builder.key_route_affinity(affinity_builder.build());
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::create_config;
    use crate::config::ConnectionConf;
    use alternator_driver::{CompressionAlgorithm, CompressionLevel, RequestCompression};
    use aws_config::retry::RetryConfig;
    use aws_config::timeout::TimeoutConfig;
    use aws_sdk_dynamodb::config::Region;
    use clap::Parser;
    use std::time::Duration;

    #[test]
    fn preserves_sdk_settings_and_uses_seed_endpoints() {
        let conf = ConnectionConf::try_parse_from([
            "latte-alternator",
            "https://node1:8043",
            "https://node2:8043",
        ])
        .unwrap();
        let sdk_config = aws_config::SdkConfig::builder()
            .region(Region::new("eu-west-1"))
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .timeout_config(
                TimeoutConfig::builder()
                    .operation_timeout(Duration::from_secs(7))
                    .build(),
            )
            .endpoint_url("https://unused.example.com")
            .build();

        let config = create_config(&sdk_config, &conf);

        assert_eq!(config.region().unwrap().as_ref(), "eu-west-1");
        assert_eq!(config.retry_config().unwrap().max_attempts(), 1);
        assert_eq!(
            config.timeout_config().unwrap().operation_timeout(),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            config.seed_hosts().unwrap(),
            vec!["node1".to_string(), "node2".to_string()]
        );
        assert_eq!(config.endpoint_url().unwrap(), "https://node1:8043/");
    }

    #[test]
    fn zlib_option_uses_deflate_compression() {
        let conf = ConnectionConf::try_parse_from([
            "latte-alternator",
            "--request-compression",
            "zlib",
            "--compression-level",
            "7",
            "--compression-threshold",
            "2048",
        ])
        .unwrap();
        let config = create_config(&aws_config::SdkConfig::builder().build(), &conf);

        assert_eq!(
            config.request_compression(),
            Some(RequestCompression::enabled(
                CompressionAlgorithm::Deflate,
                CompressionLevel::new(7),
                2048,
            ))
        );
    }
}
