use super::*;

macro_rules! asset {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../../../examples/image-check/", $name)).as_slice(),
        )
    };
}

macro_rules! repository_asset {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../../../examples/repository-check/", $name)).as_slice(),
        )
    };
}

const REPOSITORY_ASSETS: &[(&str, &[u8])] = &[
    repository_asset!("README.md"),
    repository_asset!("policy.opaal"),
    repository_asset!("policy-tests.opaal"),
    repository_asset!("source.opaal"),
    repository_asset!("saved-tasks.opaal"),
    repository_asset!("tasks.opaal"),
    repository_asset!("fixtures/check.sh"),
    repository_asset!("fixtures/repository.toml"),
    repository_asset!("fixtures/pass.json"),
    repository_asset!("fixtures/findings.json"),
    repository_asset!("fixtures/negative-manifest.json"),
    repository_asset!("fixtures/invalid.json"),
    repository_asset!("fixtures/acquisition.json"),
];

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
    let (profile_name, profile_bytes) =
        if host_platform_triple() == "aarch64-unknown-linux-gnu" {
            (
                "docker-cli-profile-aarch64.json",
                include_bytes!("../../../../examples/image-check/docker-cli-profile-aarch64.json")
                    .as_slice(),
            )
        } else {
            (
                "docker-cli-profile.json",
                include_bytes!("../../../../examples/image-check/docker-cli-profile.json")
                    .as_slice(),
            )
        };
    let mut assets = ASSETS
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
        .collect::<Vec<_>>();
    assets.push((profile_name.to_owned(), profile_bytes.to_vec()));
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
                "aarch64-unknown-linux-gnu" => {
                    include_bytes!(
                        "../../../../examples/image-check/offline-tools-linux-aarch64.toml"
                    )
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
        "ci"
            if matches!(
                host_platform_triple(),
                "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
            ) =>
        {
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
                .replace("x86_64-unknown-linux-gnu", host_platform_triple())
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
    Ok(assets)
}

/// Create the fixed example exclusively. Failed creations remain for inspection.
pub fn init_project(request: &InitProjectRequest) -> Result<(), ProjectFrontendError> {
    init_project_with_writer(request, |file, bytes| {
        file.write_all(bytes).and_then(|()| file.sync_all())
    })
}

/// Create the fixed saved-policy or explicit Linux native-tool example, without execution.
pub fn init_repository_project(
    destination: &Path,
    environment: &str,
    tool: Option<&Path>,
) -> Result<(), ProjectFrontendError> {
    init_repository_project_with_writer(destination, environment, tool, |file, bytes| {
        file.write_all(bytes).and_then(|()| file.sync_all())
    })
}

fn init_repository_project_with_writer(
    destination: &Path,
    environment: &str,
    tool: Option<&Path>,
    persist: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
) -> Result<(), ProjectFrontendError> {
    let destination = setup_path(destination)?;
    let mut assets = REPOSITORY_ASSETS
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
        .collect::<Vec<_>>();
    let (manifest, authority, lock) = match (environment, host_platform_triple(), tool) {
        (
            "offline",
            host @ ("aarch64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"),
            None,
        ) => (
            include_bytes!("../../../../examples/repository-check/offline.toml").to_vec(),
            include_bytes!("../../../../examples/repository-check/offline-authority.toml").to_vec(),
            if host == "aarch64-apple-darwin" {
                include_bytes!("../../../../examples/repository-check/offline-tools-macos.toml")
                    .to_vec()
            } else if host == "aarch64-unknown-linux-gnu" {
                include_bytes!(
                    "../../../../examples/repository-check/offline-tools-linux-aarch64.toml"
                )
                    .to_vec()
            } else {
                include_bytes!("../../../../examples/repository-check/offline-tools-linux.toml")
                    .to_vec()
            },
        ),
        (
            "ci",
            "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu",
            Some(tool),
        ) if tool.is_absolute() => {
            let tool = setup_path(tool)?;
            let (_, file) =
                open_absolute_file_with_parent_nofollow(&tool).map_err(|_| setup_error(false))?;
            let mode = file.metadata().map_err(|_| setup_error(false))?.mode();
            if mode & 0o111 == 0 || mode & 0o6000 != 0 {
                return Err(setup_error(false));
            }
            let bytes = read_bounded(file, process::MAX_TOOL_EXECUTABLE_BYTES)
                .map_err(|_| setup_error(false))?;
            process::validate_native_image(&bytes, &|| Ok(())).map_err(|_| setup_error(false))?;
            let lock =
                include_str!("../../../../examples/repository-check/live-tools.template.toml")
                    .replace("x86_64-unknown-linux-gnu", host_platform_triple())
                    .replace(
                        "@TOOL@",
                        native_path(&tool)["value"]
                            .as_str()
                            .expect("native path encoding"),
                    )
                    .replace("@DIGEST@", &digest_bytes(&bytes));
            (
                include_bytes!("../../../../examples/repository-check/live.toml").to_vec(),
                include_bytes!("../../../../examples/repository-check/live-authority.toml")
                    .to_vec(),
                lock.into_bytes(),
            )
        }
        _ => return Err(setup_error(false)),
    };
    let parsed = parse_project_manifest(&destination.join("opaal.toml"), &manifest)
        .map_err(|_| setup_error(false))?;
    parse_authority_document(&parsed, environment, &authority).map_err(|_| setup_error(false))?;
    parse_tool_lock(&parsed, environment, &lock).map_err(|_| setup_error(false))?;
    let prefix = if environment == "ci" {
        "live"
    } else {
        "offline"
    };
    assets.extend([
        ("opaal.toml".to_owned(), manifest),
        (format!("{prefix}-authority.toml"), authority),
        (format!("{prefix}-tools.toml"), lock),
    ]);
    create_assets(&destination, assets, &["fixtures"], persist, File::sync_all)
}

fn init_project_with_writer(
    request: &InitProjectRequest,
    persist: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
) -> Result<(), ProjectFrontendError> {
    let destination = setup_path(&request.destination)?;
    let assets = prepared_assets(request, &destination)?;
    let directories = ["fixtures", "decode", "operations", "live"]
        .into_iter()
        .chain(if request.environment == "ci" {
            LIVE_DIRECTORIES.iter().copied()
        } else {
            [].iter().copied()
        })
        .collect::<Vec<_>>();
    create_assets(&destination, assets, &directories, persist, File::sync_all)
}

fn create_assets(
    destination: &Path,
    assets: Vec<(String, Vec<u8>)>,
    directories: &[&str],
    mut persist: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
    mut sync_directory: impl FnMut(&File) -> std::io::Result<()>,
) -> Result<(), ProjectFrontendError> {
    if assets.len() > 64
        || assets.iter().map(|(_, bytes)| bytes.len()).sum::<usize>() > 16 * 1024 * 1024
    {
        return Err(execute_error("INIT002", "setup asset limit exceeded"));
    }
    let parent =
        open_absolute_directory_nofollow(destination.parent().ok_or_else(|| setup_error(false))?)
            .map_err(|_| setup_error(false))?;
    let name = destination.file_name().ok_or_else(|| setup_error(false))?;
    rustix::fs::mkdirat(&parent, name, rustix::fs::Mode::from_raw_mode(0o700))
        .map_err(|_| setup_error(false))?;
    let create = || -> Result<(), ProjectFrontendError> {
        let root = open_directory_at_nofollow(&parent, name, destination)
            .map_err(|_| setup_error(true))?;
        for &directory in directories {
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
            sync_directory(&directory).map_err(|_| setup_error(true))?;
        }
        let filesystem = HostProjectFilesystem::new(
            destination.to_path_buf(),
            root.try_clone().map_err(|_| setup_error(true))?,
            ControlReadBudget::default(),
        );
        let manifest_bytes = filesystem
            .read_existing_bounded(&destination.join("opaal.toml"), MAX_PROJECT_DOCUMENT_BYTES)
            .map_err(|_| setup_error(true))?;
        let manifest = parse_project_manifest(&destination.join("opaal.toml"), &manifest_bytes)
            .map_err(|_| setup_error(true))?;
        load_project_program(manifest, &filesystem, &filesystem).map_err(|_| setup_error(true))?;
        sync_directory(&root)
            .and_then(|()| sync_directory(&parent))
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

    fn frozen_result(name: &str, observed: Value) {
        let bytes =
            include_bytes!("../../../../tests/golden/controlled-external-tools/prepare-cases.json");
        let inventory: Value = serde_json::from_slice(bytes).unwrap();
        let case = inventory["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .expect("frozen preparation case");
        assert_eq!(observed, case["expected"], "{name}");
        println!(
            "OPAAL_PREPARATION_CASE {}",
            serde_json::json!({
                "name": name, "observed": observed,
                "host": host_platform_triple(), "inventory_digest": digest_bytes(bytes),
            })
        );
    }

    fn initialize(
        destination: &Path,
        repository: bool,
        persist: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), ProjectFrontendError> {
        if repository {
            init_repository_project_with_writer(destination, "offline", None, persist)
        } else {
            init_project_with_writer(
                &InitProjectRequest {
                    destination: destination.to_path_buf(),
                    environment: "offline".to_owned(),
                    docker: None,
                    socket: None,
                    qualified_version: None,
                },
                persist,
            )
        }
    }

    #[test]
    fn failed_write_or_sync_keeps_every_partial_path_and_refuses_retry() {
        for (repository, written) in [
            (false, 0),
            (false, 2),
            (false, usize::MAX),
            (true, 0),
            (true, 2),
            (true, usize::MAX),
        ] {
            let parent = std::env::temp_dir().canonicalize().unwrap();
            let destination = parent.join(format!(
                "opaal-init-fault-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let expected = if repository {
                REPOSITORY_ASSETS[0].1
            } else {
                ASSETS[0].1
            };
            let expected = &expected[..written.min(expected.len())];
            let error = initialize(&destination, repository, |file, bytes| {
                file.write_all(&bytes[..written.min(bytes.len())])?;
                Err(std::io::Error::other("injected write/sync failure"))
            })
            .unwrap_err();
            assert!(error.rendered().contains("setup incomplete"));
            assert!(!error.rendered().contains("injected"));
            assert!(destination.join("fixtures").is_dir());
            assert_eq!(
                std::fs::read(destination.join("README.md")).unwrap(),
                expected
            );
            assert!(!destination.join("policy.opaal").exists());
            let retry_refused =
                initialize(&destination, repository, |_, _| unreachable!()).is_err();
            assert!(retry_refused);
            if repository {
                frozen_result(
                    match written {
                        0 => "failure-zero-write",
                        2 => "failure-partial-write",
                        _ => "failure-file-sync",
                    },
                    serde_json::json!({
                        "exit": 2, "partial_retained": destination.join("README.md").is_file(),
                        "retry_refused": retry_refused, "launches": 0,
                    }),
                );
            }
            std::fs::remove_dir_all(destination).unwrap();
        }
    }

    #[test]
    fn frozen_asset_bounds_admit_exact_limits_and_refuse_first_excess_before_creation() {
        let cases: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/golden/controlled-external-tools/prepare-cases.json"
        ))
        .unwrap();
        let mut checked = 0;
        for case in cases["cases"].as_array().unwrap() {
            let Some(bound) = case["input"]["asset_bound"].as_str() else {
                continue;
            };
            let destination = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "opaal-init-bound-{}-{}",
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
            let mut assets = prepared_assets(&request, &destination).unwrap();
            let limit = usize::try_from(case["input"]["value"].as_u64().unwrap()).unwrap();
            if bound == "count" {
                while assets.len() < limit {
                    assets.push((format!("padding-{}", assets.len()), Vec::new()));
                }
            } else {
                let bytes = assets.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
                assets.push(("padding".to_owned(), vec![0; limit - bytes]));
            }
            let result = create_assets(
                &destination,
                assets,
                &["fixtures", "decode", "operations", "live"],
                |file, bytes| file.write_all(bytes).and_then(|()| file.sync_all()),
                File::sync_all,
            );
            let admitted = case["expected"]["asset_limit_admitted"].as_bool().unwrap();
            assert_eq!(result.is_ok(), admitted, "{}: {result:?}", case["name"]);
            assert_eq!(destination.exists(), admitted);
            frozen_result(
                case["name"].as_str().unwrap(),
                serde_json::json!({"asset_limit_admitted": result.is_ok()}),
            );
            if admitted {
                std::fs::remove_dir_all(destination).unwrap();
            } else {
                assert!(result.unwrap_err().rendered().contains("INIT002"));
            }
            checked += 1;
        }
        assert_eq!(checked, 4);
    }

    #[test]
    fn directory_root_and_parent_sync_failures_retain_created_assets() {
        for point in ["asset-directory", "root", "parent"] {
            let destination = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "opaal-init-directory-sync-{}-{}",
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
            let assets = prepared_assets(&request, &destination).unwrap();
            let count = assets.len();
            let fail_at = match point {
                "asset-directory" => 0,
                "root" => count,
                _ => count + 1,
            };
            let mut calls = 0;
            let error = create_assets(
                &destination,
                assets,
                &["fixtures", "decode", "operations", "live"],
                |file, bytes| file.write_all(bytes).and_then(|()| file.sync_all()),
                |directory| {
                    let call = calls;
                    calls += 1;
                    if call == fail_at {
                        Err(std::io::Error::other("injected directory sync failure"))
                    } else {
                        directory.sync_all()
                    }
                },
            )
            .unwrap_err();
            assert!(error.rendered().contains("setup incomplete"), "{point}");
            assert!(!error.rendered().contains("injected"));
            assert_eq!(calls, fail_at + 1, "{point}");
            assert_eq!(
                std::fs::read(destination.join("README.md")).unwrap(),
                ASSETS[0].1
            );
            assert_eq!(
                destination.join("opaal.toml").exists(),
                point != "asset-directory"
            );
            let retry_refused = init_project(&request).is_err();
            assert!(retry_refused);
            frozen_result(
                match point {
                    "asset-directory" => "failure-directory-sync",
                    "root" => "failure-root-sync",
                    _ => "failure-parent-sync",
                },
                serde_json::json!({
                    "exit": 2, "partial_retained": destination.join("README.md").is_file(),
                    "retry_refused": retry_refused, "launches": 0,
                }),
            );
            std::fs::remove_dir_all(destination).unwrap();
        }
    }

    #[test]
    fn concurrent_asset_collision_is_retained_without_overwrite() {
        for (repository, collision) in [
            (false, "file"),
            (false, "symlink"),
            (false, "fifo"),
            (true, "file"),
            (true, "symlink"),
            (true, "fifo"),
        ] {
            let parent = std::env::temp_dir().canonicalize().unwrap();
            let destination = parent.join(format!(
                "opaal-init-collision-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut first = true;
            let error = initialize(&destination, repository, |file, bytes| {
                if first {
                    match collision {
                        "file" => {
                            std::fs::write(destination.join("policy.opaal"), b"concurrent canary")?
                        }
                        "symlink" => {
                            std::fs::write(destination.join("canary"), b"concurrent canary")?;
                            std::os::unix::fs::symlink("canary", destination.join("policy.opaal"))?;
                        }
                        "fifo" => assert!(
                            std::process::Command::new("mkfifo")
                                .arg(destination.join("policy.opaal"))
                                .status()?
                                .success()
                        ),
                        _ => unreachable!(),
                    }
                    first = false;
                }
                file.write_all(bytes).and_then(|()| file.sync_all())
            })
            .unwrap_err();
            assert!(error.rendered().contains("setup incomplete"));
            if collision == "fifo" {
                use std::os::unix::fs::FileTypeExt as _;
                assert!(
                    std::fs::symlink_metadata(destination.join("policy.opaal"))
                        .unwrap()
                        .file_type()
                        .is_fifo()
                );
            } else {
                assert_eq!(
                    std::fs::read(destination.join("policy.opaal")).unwrap(),
                    b"concurrent canary"
                );
                assert_eq!(
                    destination.join("policy.opaal").is_symlink(),
                    collision == "symlink"
                );
            }
            assert_eq!(
                std::fs::read(destination.join("README.md")).unwrap(),
                if repository {
                    REPOSITORY_ASSETS[0].1
                } else {
                    ASSETS[0].1
                }
            );
            let retry_refused =
                initialize(&destination, repository, |_, _| unreachable!()).is_err();
            assert!(retry_refused);
            if repository {
                frozen_result(
                    match collision {
                        "file" => "concurrent-asset-file",
                        "symlink" => "concurrent-asset-symlink",
                        _ => "concurrent-asset-fifo",
                    },
                    serde_json::json!({
                        "exit": 2, "preserved": true,
                        "partial_retained": destination.join("README.md").is_file(),
                        "retry_refused": retry_refused, "launches": 0,
                    }),
                );
            }
            std::fs::remove_dir_all(destination).unwrap();
        }
    }
}
