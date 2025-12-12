#[cfg(feature = "cql")]
const DRIVER_PKG_NAME: &str = "scylla";
#[cfg(feature = "alternator")]
const DRIVER_PKG_NAME: &str = "aws-sdk-dynamodb";

#[derive(Debug)]
pub struct VersionInfo {
    pub latte_version: &'static str,
    pub latte_build_date: &'static str,
    pub latte_git_sha: &'static str,
    pub db_driver_version: &'static str,
    pub db_driver_date: &'static str,
    pub db_driver_sha: &'static str,
}

mod version_info {
    include!(concat!(env!("OUT_DIR"), "/version_info.rs"));
}

pub fn get_version_info() -> VersionInfo {
    VersionInfo {
        latte_version: version_info::PKG_VERSION,
        latte_build_date: version_info::COMMIT_DATE,
        latte_git_sha: version_info::GIT_SHA,
        db_driver_version: version_info::DRIVER_VERSION,
        db_driver_date: version_info::DRIVER_RELEASE_DATE,
        db_driver_sha: version_info::DRIVER_SHA,
    }
}

#[allow(clippy::uninlined_format_args)]
pub fn format_version_info_json() -> String {
    let info = get_version_info();
    let latte_version = format!(r#""version":"{}""#, info.latte_version);
    let latte_build_date = format!(r#""commit_date":"{}""#, info.latte_build_date);
    let latte_git_sha = format!(r#""commit_sha":"{}""#, info.latte_git_sha);
    let db_driver_version = format!(r#""version":"{}""#, info.db_driver_version);
    let db_driver_date = format!(r#""commit_date":"{}""#, info.db_driver_date);
    let db_driver_sha = format!(r#""commit_sha":"{}""#, info.db_driver_sha);
    format!(
        r#"{{"latte":{{{},{},{}}},"{DRIVER_PKG_NAME}-driver":{{{},{},{}}}}}"#,
        latte_version,
        latte_build_date,
        latte_git_sha,
        db_driver_version,
        db_driver_date,
        db_driver_sha,
    )
}

pub fn format_version_info_human() -> String {
    let info = get_version_info();
    format!(
        "latte:\n\
         - Version: {}\n\
         - Build Date: {}\n\
         - Git SHA: {}\n\
         {DRIVER_PKG_NAME}-driver:\n\
         - Version: {}\n\
         - Build Date: {}\n\
         - Git SHA: {}",
        info.latte_version,
        info.latte_build_date,
        info.latte_git_sha,
        info.db_driver_version,
        info.db_driver_date,
        info.db_driver_sha
    )
}

pub fn get_formatted_version_info(as_json: bool) -> String {
    if as_json {
        format_version_info_json()
    } else {
        format_version_info_human()
    }
}
