use cargo_lock::Lockfile;
use chrono::{DateTime, Utc};
use git2::Repository;
use std::env;
use std::fs;
use std::path::Path;

const UNKNOWN: &str = "unknown";
const SCYLLA_PKG_NAME: &str = "scylla";

#[cfg(fetch_extended_version_info)]
mod scylla_date_utils {
    use chrono::{DateTime, Utc};
    use reqwest::blocking::Client;
    use serde_json::Value;

    const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn get_scylla_commit_date_from_github(sha: &str) -> Option<String> {
        let client = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent("latte (github.com/scylladb/latte)")
            .build()
            .ok()?;
        let resp = client
            .get(format!(
                "https://api.github.com/repos/scylladb/scylla-rust-driver/commits/{sha}",
            ))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            println!(
                "cargo:warning=Failed to fetch commit date: {} - {}",
                resp.status(),
                resp.text().unwrap_or_default()
            );
            return None;
        }
        DateTime::parse_from_rfc3339(
            resp.json::<Value>()
                .ok()?
                .get("commit")?
                .get("author")?
                .get("date")?
                .as_str()?,
        )
        .ok()
        .map(|dt| {
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
    }

    pub fn get_crate_release_date(name: &str, version: &str) -> Option<String> {
        let client = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent("latte (github.com/scylladb/latte)")
            .build()
            .ok()?;
        let resp = client
            .get(format!("https://crates.io/api/v1/crates/{name}/versions"))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            println!(
                "cargo:warning=Failed to fetch crate release date: {} - {}",
                resp.status(),
                resp.text().unwrap_or_default()
            );
            return None;
        }
        let json: Value = resp.json().ok()?;
        json.get("versions")
            .and_then(|versions| versions.as_array())
            .and_then(|array| {
                array.iter().find(|v| {
                    v.get("num")
                        .and_then(|num| num.as_str())
                        .map(|s| s == version)
                        .unwrap_or(false)
                })
            })
            .and_then(|v| v.get("created_at"))
            .and_then(|d| d.as_str())
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|dt| {
                dt.with_timezone(&Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            })
    }
}

#[cfg(not(fetch_extended_version_info))]
mod scylla_date_utils {
    const UNKNOWN_EXTENDED: &str = "unknown (build with --cfg fetch_extended_version_info)";

    pub fn get_crate_release_date(_name: &str, _version: &str) -> Option<String> {
        Some(UNKNOWN_EXTENDED.into())
    }

    pub fn get_scylla_commit_date_from_github(_sha: &str) -> Option<String> {
        Some(UNKNOWN_EXTENDED.into())
    }
}

fn get_git_info() -> Option<(String, String)> {
    let repo = Repository::open(".").ok()?;
    let head = repo.head().ok()?;
    let commit = head.peel_to_commit().ok()?;
    let dt = DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0)?;
    Some((
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        commit.id().to_string(),
    ))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=.git/HEAD");

    let out_dir = env::var_os("OUT_DIR").unwrap();
    let (commit_date, sha) = get_git_info().unwrap_or_else(|| (UNKNOWN.into(), UNKNOWN.into()));
    let lockfile = Lockfile::load("Cargo.lock").unwrap();
    let scylla_pkg = lockfile
        .packages
        .iter()
        .find(|p| p.name.as_str() == SCYLLA_PKG_NAME)
        .expect("scylla package not found in Cargo.lock");
    let scylla_source = scylla_pkg
        .source
        .as_ref()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut scylla_version = scylla_pkg.version.to_string();
    let (scylla_commit_date, scylla_sha) = if scylla_source.is_empty() {
        scylla_version = format!("{scylla_version}-dev");
        (
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            UNKNOWN.into(),
        )
    } else if scylla_source.contains("git+") {
        scylla_version = format!("{scylla_version}-dev");
        let commit_sha = scylla_source
            .split('#')
            .nth(1)
            .unwrap_or(UNKNOWN)
            .to_string();
        (
            scylla_date_utils::get_scylla_commit_date_from_github(&commit_sha)
                .unwrap_or(UNKNOWN.into()),
            commit_sha,
        )
    } else {
        (
            scylla_date_utils::get_crate_release_date(SCYLLA_PKG_NAME, &scylla_version)
                .unwrap_or(UNKNOWN.into()),
            "official_release".into(),
        )
    };

    fs::write(
        Path::new(&out_dir).join("version_info.rs"),
        format!(
            "pub const PKG_VERSION: &str = \"{}\";\n\
             pub const COMMIT_DATE: &str = \"{}\";\n\
             pub const GIT_SHA: &str = \"{}\";\n\
             pub const SCYLLA_DRIVER_VERSION: &str = \"{}\";\n\
             pub const SCYLLA_DRIVER_RELEASE_DATE: &str = \"{}\";\n\
             pub const SCYLLA_DRIVER_SHA: &str = \"{}\";\n",
            env!("CARGO_PKG_VERSION"),
            commit_date,
            sha,
            scylla_version,
            scylla_commit_date,
            scylla_sha
        ),
    )
    .unwrap();
}
