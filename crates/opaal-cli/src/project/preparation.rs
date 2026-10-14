use super::*;

macro_rules! asset {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../../../examples/image-check/", $name)).as_slice(),
        )
    };
}

const ASSETS: &[(&str, &[u8])] = &[
    asset!("README.md"),
    asset!("policy.opaal"),
    asset!("policy-tests.opaal"),
    asset!("source.opaal"),
    asset!("saved-tasks.opaal"),
    asset!("tasks.opaal"),
    asset!("fixtures/image.json"),
    asset!("fixtures/invalid.json"),
    asset!("decode/cases.json"),
    asset!("operations/cases.json"),
    asset!("live/cases.json"),
    asset!("docker-cli-profile.json"),
];
const OFFLINE: &[u8] = include_bytes!("../../../../examples/image-check/offline.toml");
const LIVE: &str = include_str!("../../../../examples/image-check/live.template.toml");
const LIVE_TOOLS: &str = include_str!("../../../../examples/image-check/live-tools.template.toml");
const LIVE_DIRECTORIES: &[&str] = &["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"];

/// Explicit new destination and independently qualified live facts, if selected.
#[derive(Clone, Debug)]
pub struct InitProjectRequest {
    pub destination: PathBuf,
    pub environment: String,
    pub docker: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub qualified_version: Option<String>,
}

fn setup_error(incomplete: bool) -> ProjectFrontendError {
    execute_error(
        "INIT001",
        if incomplete {
            "setup incomplete; partial destination retained; use a fresh destination"
        } else {
            "setup refused; require a fresh destination, supported host and valid explicit facts"
        },
    )
}

fn setup_path(path: &Path) -> Result<PathBuf, ProjectFrontendError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(setup_error(false));
    }
    absolute_lexical(path).map_err(|_| setup_error(false))
}

fn prepared_assets(
    request: &InitProjectRequest,
    destination: &Path,
) -> Result<Vec<(String, Vec<u8>)>, ProjectFrontendError> {
    let mut assets = ASSETS
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
        .collect::<Vec<_>>();
    let (manifest, authority, lock) = match request.environment.as_str() {
        "offline"
            if request.docker.is_none()
                && request.socket.is_none()
                && request.qualified_version.is_none() =>
        {
            let lock = match host_platform_triple() {
                "aarch64-apple-darwin" => {
                    include_bytes!("../../../../examples/image-check/offline-tools-macos.toml")
                        .as_slice()
                }
                "x86_64-unknown-linux-gnu" => {
                    include_bytes!("../../../../examples/image-check/offline-tools-linux.toml")
                        .as_slice()
                }
                _ => return Err(setup_error(false)),
            };
            (
                OFFLINE.to_vec(),
                include_bytes!("../../../../examples/image-check/offline-authority.toml").to_vec(),
                lock.to_vec(),
            )
        }
        "ci" if host_platform_triple() == "x86_64-unknown-linux-gnu" => {
            let executable = request
                .docker
                .as_deref()
                .ok_or_else(|| setup_error(false))?;
            let socket = request
                .socket
                .as_deref()
                .ok_or_else(|| setup_error(false))?;
            let profile: Value =
                serde_json::from_slice(docker::PROFILE_BYTES).map_err(|_| setup_error(false))?;
            if !executable.is_absolute()
                || !socket.is_absolute()
                || request.qualified_version.as_deref() != profile["cli_version"].as_str()
            {
                return Err(setup_error(false));
            }
            let executable = setup_path(executable)?;
            let (_, file) = open_absolute_file_with_parent_nofollow(&executable)
                .map_err(|_| setup_error(false))?;
            if file.metadata().map_err(|_| setup_error(false))?.mode() & 0o111 == 0 {
                return Err(setup_error(false));
            }
            let bytes = read_bounded(file, 64 * 1024 * 1024).map_err(|_| setup_error(false))?;
            PosixOperationalAdapter::new()
                .observe_local_path(socket, false)
                .map_err(|_| setup_error(false))?;
            let manifest = LIVE.replace(
                "@SOCKET@",
                native_path(socket)["value"]
                    .as_str()
                    .expect("native path encoding"),
            );
            let lock = LIVE_TOOLS
                .replace(
                    "@DOCKER@",
                    native_path(&executable)["value"]
                        .as_str()
                        .expect("native path encoding"),
                )
                .replace("@DIGEST@", &digest_bytes(&bytes));
            (
                manifest.into_bytes(),
                include_bytes!("../../../../examples/image-check/live-authority.toml").to_vec(),
                lock.into_bytes(),
            )
        }
        _ => return Err(setup_error(false)),
    };
    let parsed = parse_project_manifest(&destination.join("opaal.toml"), &manifest)
        .map_err(|_| setup_error(false))?;
    parse_authority_document(&parsed, &request.environment, &authority)
        .map_err(|_| setup_error(false))?;
    parse_tool_lock(&parsed, &request.environment, &lock).map_err(|_| setup_error(false))?;
    let (authority_name, lock_name) = if request.environment == "offline" {
        ("offline-authority.toml", "offline-tools.toml")
    } else {
        ("live-authority.toml", "live-tools.toml")
    };
    assets.extend([
        ("opaal.toml".to_owned(), manifest),
        (authority_name.to_owned(), authority),
        (lock_name.to_owned(), lock),
    ]);
    if assets.len() > 64
        || assets.iter().map(|(_, bytes)| bytes.len()).sum::<usize>() > 16 * 1024 * 1024
    {
        return Err(execute_error("INIT002", "setup asset limit exceeded"));
    }
    Ok(assets)
}

/// Create the fixed example exclusively. Failed creations remain for inspection.
pub fn init_project(request: &InitProjectRequest) -> Result<(), ProjectFrontendError> {
    init_project_with_writer(request, |file, bytes| {
        file.write_all(bytes).and_then(|()| file.sync_all())
    })
}

fn init_project_with_writer(
    request: &InitProjectRequest,
    mut persist: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
) -> Result<(), ProjectFrontendError> {
    let destination = setup_path(&request.destination)?;
    let assets = prepared_assets(request, &destination)?;
    let parent =
        open_absolute_directory_nofollow(destination.parent().ok_or_else(|| setup_error(false))?)
            .map_err(|_| setup_error(false))?;
    let name = destination.file_name().ok_or_else(|| setup_error(false))?;
    rustix::fs::mkdirat(&parent, name, rustix::fs::Mode::from_raw_mode(0o700))
        .map_err(|_| setup_error(false))?;
    let create = || -> Result<(), ProjectFrontendError> {
        let root = open_directory_at_nofollow(&parent, name, &destination)
            .map_err(|_| setup_error(true))?;
        for directory in ["fixtures", "decode", "operations", "live"]
            .into_iter()
            .chain(if request.environment == "ci" {
                LIVE_DIRECTORIES.iter().copied()
            } else {
                [].iter().copied()
            })
        {
            rustix::fs::mkdirat(&root, directory, rustix::fs::Mode::from_raw_mode(0o700))
                .map_err(|_| setup_error(true))?;
        }
        for (name, bytes) in assets {
            let path = Path::new(&name);
            let directory = open_relative_directory_nofollow(
                root.try_clone().map_err(|_| setup_error(true))?,
                path.parent().expect("asset parent"),
                path,
            )
            .map_err(|_| setup_error(true))?;
            let descriptor = rustix::fs::openat(
                &directory,
                path.file_name().expect("asset filename"),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|_| setup_error(true))?;
            let mut file = File::from(descriptor);
            persist(&mut file, &bytes).map_err(|_| setup_error(true))?;
            drop(file);
            directory.sync_all().map_err(|_| setup_error(true))?;
        }
        let filesystem = HostProjectFilesystem::new(
            destination.clone(),
            root.try_clone().map_err(|_| setup_error(true))?,
            ControlReadBudget::default(),
        );
        let manifest_bytes = filesystem
            .read_existing_bounded(&destination.join("opaal.toml"), MAX_PROJECT_DOCUMENT_BYTES)
            .map_err(|_| setup_error(true))?;
        let manifest = parse_project_manifest(&destination.join("opaal.toml"), &manifest_bytes)
            .map_err(|_| setup_error(true))?;
        load_project_program(manifest, &filesystem, &filesystem).map_err(|_| setup_error(true))?;
        root.sync_all()
            .and_then(|()| parent.sync_all())
            .map_err(|_| setup_error(true))?;
        Ok(())
    };
    create()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn failed_write_or_sync_keeps_every_partial_path_and_refuses_retry() {
        for partial_write in [true, false] {
            let parent = std::env::temp_dir().canonicalize().unwrap();
            let destination = parent.join(format!(
                "opaal-init-fault-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let request = InitProjectRequest {
                destination: destination.clone(),
                environment: "offline".to_owned(),
                docker: None,
                socket: None,
                qualified_version: None,
            };
            let error = init_project_with_writer(&request, |file, bytes| {
                if partial_write {
                    file.write_all(&bytes[..2])?;
                }
                Err(std::io::Error::other("injected write/sync failure"))
            })
            .unwrap_err();
            assert!(error.rendered().contains("setup incomplete"));
            assert!(!error.rendered().contains("injected"));
            assert!(destination.join("fixtures").is_dir());
            assert_eq!(
                std::fs::read(destination.join("README.md")).unwrap().len(),
                if partial_write { 2 } else { 0 }
            );
            assert!(init_project(&request).is_err());
            std::fs::remove_dir_all(destination).unwrap();
        }
    }

    #[test]
    fn concurrent_asset_collision_is_retained_without_overwrite() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let destination = parent.join(format!(
            "opaal-init-collision-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let request = InitProjectRequest {
            destination: destination.clone(),
            environment: "offline".to_owned(),
            docker: None,
            socket: None,
            qualified_version: None,
        };
        let mut first = true;
        let error = init_project_with_writer(&request, |file, bytes| {
            if first {
                std::fs::write(destination.join("policy.opaal"), b"concurrent canary")?;
                first = false;
            }
            file.write_all(bytes).and_then(|()| file.sync_all())
        })
        .unwrap_err();
        assert!(error.rendered().contains("setup incomplete"));
        assert_eq!(
            std::fs::read(destination.join("policy.opaal")).unwrap(),
            b"concurrent canary"
        );
        assert_eq!(
            std::fs::read(destination.join("README.md")).unwrap(),
            ASSETS[0].1
        );
        std::fs::remove_dir_all(destination).unwrap();
    }
}
