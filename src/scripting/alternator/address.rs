/// Default Alternator listen port (see ALTERNATOR.md).
pub(crate) const DEFAULT_ALTERNATOR_PORT: u16 = 8000;

/// Parse an Alternator address, applying `http://` when no scheme is present and
/// [`DEFAULT_ALTERNATOR_PORT`] when no port is specified.
pub(crate) fn normalize_address(addr: &str) -> Option<url::Url> {
    if addr.is_empty() {
        return None;
    }

    let url_str = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };

    let mut url = url::Url::parse(&url_str).ok()?;
    url.host_str()?;
    if url.port().is_none() {
        url.set_port(Some(DEFAULT_ALTERNATOR_PORT)).ok()?;
    }
    Some(url)
}

pub(crate) fn normalize_addresses(addresses: &[String]) -> Vec<url::Url> {
    addresses
        .iter()
        .filter_map(|addr| normalize_address(addr))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_empty_address() {
        assert!(normalize_address("").is_none());
    }

    #[test]
    fn applies_default_scheme_and_port() {
        let url = normalize_address("localhost").unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(DEFAULT_ALTERNATOR_PORT));
    }

    #[test]
    fn preserves_explicit_port() {
        let url = normalize_address("http://127.0.0.1:9000").unwrap();
        assert_eq!(url.port(), Some(9000));
    }

    #[test]
    fn applies_default_port_to_host_without_port() {
        let url = normalize_address("http://example.com").unwrap();
        assert_eq!(url.port(), Some(DEFAULT_ALTERNATOR_PORT));
    }

    #[test]
    fn host_port_without_scheme() {
        let url = normalize_address("127.0.0.1:8000").unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(8000));
    }

    #[test]
    fn normalize_addresses_filters_empty() {
        let urls = normalize_addresses(&[
            "".to_string(),
            "localhost".to_string(),
            "http://node2:8000".to_string(),
        ]);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].host_str(), Some("localhost"));
        assert_eq!(urls[1].host_str(), Some("node2"));
    }
}
