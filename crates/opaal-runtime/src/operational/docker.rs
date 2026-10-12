//! Fixed Docker profile and read-only local target binding.

use opaal_platform::operational::{OperationalAdapter, UnixIdentity, validate_child_environment};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::ModuleError;
use crate::project::{DockerTarget, ProjectManifest};
use crate::workflow::{digest_bytes, native_path};

pub const PROFILE: &str = "docker-cli-image-inspect.v1";
pub const PROFILE_BYTES: &[u8] =
    include_bytes!("../../../../tests/golden/docker-image-checks/docker-cli-profile.json");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundDockerTarget {
    pub target: DockerTarget,
    pub record: Value,
    pub environment: Vec<(OsString, OsString)>,
}

fn identity_value(identity: UnixIdentity) -> Value {
    json!({"device":identity.device,"inode":identity.inode,"mode":identity.mode,"uid":identity.uid,"gid":identity.gid})
}

pub fn child_environment(root: &Path) -> Result<Vec<(OsString, OsString)>, ModuleError> {
    let environment = vec![
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("TZ"), OsString::from("UTC")),
        (
            OsString::from("PATH"),
            root.join("empty-path").into_os_string(),
        ),
        (
            OsString::from("HOME"),
            root.join("sandbox-home").into_os_string(),
        ),
        (
            OsString::from("TMPDIR"),
            root.join("sandbox-tmp").into_os_string(),
        ),
        (
            OsString::from("DOCKER_API_VERSION"),
            OsString::from(crate::docker::API_VERSION),
        ),
    ];
    validate_child_environment(&environment)?;
    if environment
        .iter()
        .map(|(name, value)| name.as_encoded_bytes().len() + value.as_encoded_bytes().len())
        .sum::<usize>()
        > 65536
    {
        return Err(ModuleError::invalid(
            "DOCKER002",
            "Docker child environment exceeds 64 KiB",
        ));
    }
    Ok(environment)
}

pub fn bind_target(
    manifest: &ProjectManifest,
    target: &DockerTarget,
    adapter: &dyn OperationalAdapter,
) -> Result<BoundDockerTarget, ModuleError> {
    let environment = child_environment(manifest.root())?;
    let mut safe_directories = Vec::new();
    for (role, path) in [
        ("config", target.config_directory.clone()),
        ("home", manifest.root().join("sandbox-home")),
        ("path", manifest.root().join("empty-path")),
        ("tmp", manifest.root().join("sandbox-tmp")),
    ] {
        let mut record = identity_value(adapter.observe_local_path(&path, true)?);
        record["role"] = json!(role);
        record["path"] = native_path(&path);
        safe_directories.push(record);
    }
    let socket_identity = identity_value(adapter.observe_local_path(&target.socket, false)?);
    Ok(BoundDockerTarget {
        target: target.clone(),
        record: json!({"id":target.id,"adapter":"docker","tool":target.tool,"socket":native_path(&target.socket),"config_directory":native_path(&target.config_directory),"profile":PROFILE,"profile_digest":digest_bytes(PROFILE_BYTES),"safe_directories":safe_directories,"socket_identity":socket_identity}),
        environment,
    })
}

pub fn recheck_target(
    manifest: &ProjectManifest,
    bound: &BoundDockerTarget,
    adapter: &dyn OperationalAdapter,
) -> Result<(), ModuleError> {
    if bind_target(manifest, &bound.target, adapter)? != *bound {
        return Err(ModuleError::invalid(
            "EXECUTE_STALE",
            "Docker target identity changed",
        ));
    }
    Ok(())
}

pub fn environment_record(environment: &[(OsString, OsString)]) -> Value {
    Value::Array(environment.iter().map(|(name,value)| {
        let path = PathBuf::from(value);
        json!({"name":name.to_str().expect("fixed environment name"),"value":native_path(&path)})
    }).collect())
}

pub(crate) fn arguments(target: &DockerTarget, stage: usize, image: &str) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("--config"),
        target.config_directory.as_os_str().to_owned(),
        OsString::from("--host"),
        OsString::from(format!(
            "unix://{}",
            target.socket.to_str().expect("validated ASCII socket")
        )),
    ];
    let suffix: &[&OsStr] = match stage {
        0 => &[OsStr::new("--version")],
        1 | 3 => &[
            OsStr::new("version"),
            OsStr::new("--format"),
            OsStr::new("{{json .}}"),
        ],
        2 => &[
            OsStr::new("image"),
            OsStr::new("inspect"),
            OsStr::new("--format"),
            OsStr::new("{{json .}}"),
            OsStr::new("--"),
            OsStr::new(image),
        ],
        _ => unreachable!("four fixed stages"),
    };
    argv.extend(suffix.iter().map(|value| (*value).to_owned()));
    argv
}

pub(crate) fn validate_probe(bytes: &[u8]) -> Result<(), ModuleError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_profile())?;
    let build = text
        .strip_prefix(&format!(
            "Docker version {}, build ",
            crate::docker::TOOL_VERSION
        ))
        .and_then(|text| text.strip_suffix('\n'))
        .ok_or_else(invalid_profile)?;
    if build.is_empty() || build.len() > 512 || build.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err(invalid_profile());
    }
    Ok(())
}

fn invalid_profile() -> ModuleError {
    ModuleError::invalid(
        "DOCKER003",
        "Docker response does not match the fixed client/server/API profile",
    )
}

pub(crate) fn document(
    bytes: &[u8],
    budget: &mut crate::eval::ResourceBudget,
    cancel: crate::eval::CancellationToken,
) -> Result<crate::Record, ModuleError> {
    if bytes.len() > crate::docker::MAX_IMAGE_BYTES {
        return Err(ModuleError::invalid(
            "DOCKER001",
            "Docker response exceeds 1 MiB",
        ));
    }
    match crate::eval::decode_json_controlled(bytes, budget, cancel)? {
        crate::Value::Record(document) => Ok(document),
        _ => Err(ModuleError::invalid(
            "DOCKER001",
            "Docker requires one JSON object",
        )),
    }
}

pub(crate) fn version_fields(document: &crate::Record) -> Result<Vec<String>, ModuleError> {
    let mut fields = Vec::new();
    for (side, keys) in [
        ("Client", &["Version", "ApiVersion", "Os", "Arch"][..]),
        (
            "Server",
            &["Version", "ApiVersion", "MinAPIVersion", "Os", "Arch"][..],
        ),
    ] {
        let Some(crate::Value::Record(record)) = document.get(side) else {
            return Err(invalid_profile());
        };
        for key in keys {
            let Some(crate::Value::String(text)) = record.get(key) else {
                return Err(invalid_profile());
            };
            fields.push(text.to_string());
        }
    }
    if fields[0] != crate::docker::TOOL_VERSION
        || fields[1] != crate::docker::API_VERSION
        || fields[2] != "linux"
        || fields[3] != "amd64"
        || fields[4] != crate::docker::TOOL_VERSION
        || fields[7] != "linux"
        || fields[8] != "amd64"
    {
        return Err(invalid_profile());
    }
    let api = api_parts(crate::docker::API_VERSION)?;
    if api_parts(&fields[6])? > api || api > api_parts(&fields[5])? {
        return Err(invalid_profile());
    }
    Ok(fields)
}

fn api_parts(text: &str) -> Result<(u32, u32), ModuleError> {
    let (major, minor) = text.split_once('.').ok_or_else(invalid_profile)?;
    let parts = (
        major.parse::<u32>().map_err(|_| invalid_profile())?,
        minor.parse::<u32>().map_err(|_| invalid_profile())?,
    );
    if format!("{}.{}", parts.0, parts.1) != text {
        return Err(invalid_profile());
    }
    Ok(parts)
}
