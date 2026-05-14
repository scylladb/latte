use super::config::AlternatorRequestCompressionMode;
use alternator_driver::{
    AlternatorClient, AlternatorConfig, CompressionAlgorithm, CompressionLevel, RequestCompression,
};

pub fn create_client(
    sdk_config: &aws_config::SdkConfig,
    conf: &crate::config::ConnectionConf,
) -> AlternatorClient {
    let opts = &conf.alternator;

    let mut builder = AlternatorConfig::new(sdk_config).to_builder();
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
            let level = opts
                .compression_level
                .map(|n| CompressionLevel::new(n as u32))
                .unwrap_or_default();
            builder = builder.request_compression(RequestCompression::enabled(
                CompressionAlgorithm::Zlib,
                level,
                opts.compression_threshold,
            ));
        }
    }
    AlternatorClient::from_conf(builder.build())
}
