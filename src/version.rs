#[derive(Debug)]
pub struct VersionInfo {
    pub latte_version: &'static str,
    pub latte_build_date: &'static str,
    pub latte_git_sha: &'static str,
    pub scylla_driver_version: &'static str,
    pub scylla_driver_date: &'static str,
    pub scylla_driver_sha: &'static str,
}

mod version_info {
    include!(concat!(env!("OUT_DIR"), "/version_info.rs"));
}

pub fn get_version_info() -> VersionInfo {
    VersionInfo {
        latte_version: version_info::PKG_VERSION,
        latte_build_date: version_info::COMMIT_DATE,
        latte_git_sha: version_info::GIT_SHA,
        scylla_driver_version: version_info::SCYLLA_DRIVER_VERSION,
        scylla_driver_date: version_info::SCYLLA_DRIVER_RELEASE_DATE,
        scylla_driver_sha: version_info::SCYLLA_DRIVER_SHA,
    }
}

#[allow(clippy::uninlined_format_args)]
pub fn format_version_info_json() -> String {
    let info = get_version_info();
    let latte_version = format!(r#""version":"{}""#, info.latte_version);
    let latte_build_date = format!(r#""commit_date":"{}""#, info.latte_build_date);
    let latte_git_sha = format!(r#""commit_sha":"{}""#, info.latte_git_sha);
    let scylla_driver_version = format!(r#""version":"{}""#, info.scylla_driver_version);
    let scylla_driver_date = format!(r#""commit_date":"{}""#, info.scylla_driver_date);
    let scylla_driver_sha = format!(r#""commit_sha":"{}""#, info.scylla_driver_sha);
    format!(
        r#"{{"latte":{{{},{},{}}},"scylla-driver":{{{},{},{}}}}}"#,
        latte_version,
        latte_build_date,
        latte_git_sha,
        scylla_driver_version,
        scylla_driver_date,
        scylla_driver_sha,
    )
}

pub fn format_version_info_human() -> String {
    let info = get_version_info();
    format!(
        "latte:\n\
         - Version: {}\n\
         - Build Date: {}\n\
         - Git SHA: {}\n\
         scylla-driver:\n\
         - Version: {}\n\
         - Build Date: {}\n\
         - Git SHA: {}",
        info.latte_version,
        info.latte_build_date,
        info.latte_git_sha,
        info.scylla_driver_version,
        info.scylla_driver_date,
        info.scylla_driver_sha
    )
}

pub fn get_formatted_version_info(as_json: bool) -> String {
    if as_json {
        format_version_info_json()
    } else {
        format_version_info_human()
    }
}
