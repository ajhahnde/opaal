//! Explicit-path host frontend for non-executing project inspection and checks.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::project::{
    MAX_PROJECT_DOCUMENT_BYTES, ProjectError, ProjectProgram, check_project, load_project_program,
    parse_authority_document, parse_project_manifest, parse_tool_lock,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectProjectRequest {
    manifest: PathBuf,
    task: String,
}

impl InspectProjectRequest {
    #[must_use]
    pub fn new(manifest: PathBuf, task: String) -> Self {
        Self { manifest, task }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckProjectRequest {
    manifest: PathBuf,
    task: String,
    environment: String,
    authority: PathBuf,
    tools: PathBuf,
    inputs: Vec<(String, String)>,
}

impl CheckProjectRequest {
    #[must_use]
    pub fn new(
        manifest: PathBuf,
        task: String,
        environment: String,
        authority: PathBuf,
        tools: PathBuf,
        inputs: Vec<(String, String)>,
    ) -> Self {
        Self {
            manifest,
            task,
            environment,
            authority,
            tools,
            inputs,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectFrontendRun {
    output: Vec<u8>,
}

impl ProjectFrontendRun {
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }
}

#[derive(Debug)]
pub struct ProjectFrontendError {
    rendered: String,
}

impl ProjectFrontendError {
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

impl std::fmt::Display for ProjectFrontendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl std::error::Error for ProjectFrontendError {}

pub fn inspect_project(
    request: &InspectProjectRequest,
) -> Result<ProjectFrontendRun, ProjectFrontendError> {
    let (project, _) = load_explicit_project(&request.manifest)?;
    let task = project.task(&request.task).map_err(frontend_contract)?;
    let callable = task.action().callable();
    let mut output = format!(
        "task {}\naction {}\nsignature {}(",
        task.id().qualified_name(),
        task.action().id().qualified_name(),
        callable.name(),
    );
    for (index, parameter) in callable.parameters().iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        output.push_str(parameter.name());
        output.push_str(": ");
        output.push_str(&parameter.value_type().to_string());
    }
    output.push_str(") -> ");
    output.push_str(&callable.result().to_string());
    output.push('\n');
    output.push_str("contract sha256:");
    output.push_str(task.action().id().contract_digest());
    output.push('\n');
    if let Some(documentation) = callable.documentation()
        && !documentation.is_empty()
    {
        output.push_str("documentation\n");
        for line in documentation.text().lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push_str("effects\n");
    for effect in task.effects() {
        output.push_str("  ");
        output.push_str(effect.capability());
        output.push(' ');
        output.push_str(effect.scope());
        output.push('\n');
    }
    output.push_str("tools\n");
    for tool in project.manifest().tools().values() {
        output.push_str("  ");
        output.push_str(&tool.id().qualified_name());
        output.push(' ');
        output.push_str(tool.adapter().name());
        output.push(' ');
        output.push_str(&tool.version().to_string());
        output.push('\n');
    }
    output.push_str("environments\n");
    for environment in project.manifest().environments().values() {
        output.push_str("  ");
        output.push_str(&environment.id().qualified_name());
        output.push('\n');
    }
    Ok(ProjectFrontendRun {
        output: output.into_bytes(),
    })
}

pub fn check_explicit_project(
    request: &CheckProjectRequest,
) -> Result<ProjectFrontendRun, ProjectFrontendError> {
    let (project, filesystem) = load_explicit_project(&request.manifest)?;
    let environment = project
        .manifest()
        .environment(&request.environment)
        .map_err(frontend_contract)?;
    let authority_path = filesystem
        .resolve_existing_file(&request.authority)
        .map_err(frontend_contract)?;
    let tools_path = filesystem
        .resolve_existing_file(&request.tools)
        .map_err(frontend_contract)?;
    if authority_path != environment.authority() || tools_path != environment.tool_lock() {
        return Err(frontend_contract(ProjectError::new(
            "CHECK006",
            "--authority and --tools must name the selected environment's exact files",
        )));
    }
    let authority_file = filesystem
        .open_existing_file(&authority_path)
        .map_err(frontend_contract)?;
    let tools_file = filesystem
        .open_existing_file(&tools_path)
        .map_err(frontend_contract)?;
    let authority_bytes =
        read_bounded(authority_file, MAX_PROJECT_DOCUMENT_BYTES).map_err(frontend_contract)?;
    let tools_bytes =
        read_bounded(tools_file, MAX_PROJECT_DOCUMENT_BYTES).map_err(frontend_contract)?;
    let authority =
        parse_authority_document(project.manifest(), &request.environment, &authority_bytes)
            .map_err(frontend_contract)?;
    let tools = parse_tool_lock(project.manifest(), &request.environment, &tools_bytes)
        .map_err(frontend_contract)?;
    check_project(
        &project,
        &request.task,
        &request.environment,
        &authority,
        &tools,
        request.inputs.clone(),
    )
    .map_err(frontend_contract)?;
    Ok(ProjectFrontendRun { output: Vec::new() })
}

fn load_explicit_project(
    requested: &Path,
) -> Result<(ProjectProgram, HostProjectFilesystem), ProjectFrontendError> {
    if requested.file_name().and_then(|name| name.to_str()) != Some("opaal.toml") {
        return Err(frontend_contract(ProjectError::new(
            "PROJECT023",
            "--project must name an explicit opaal.toml file",
        )));
    }
    let manifest_path = absolute_lexical(requested).map_err(frontend_contract)?;
    let (manifest_parent, manifest_file) =
        open_absolute_file_with_parent_nofollow(&manifest_path).map_err(frontend_contract)?;
    let manifest_bytes =
        read_bounded(manifest_file, MAX_PROJECT_DOCUMENT_BYTES).map_err(frontend_contract)?;
    let manifest =
        parse_project_manifest(&manifest_path, &manifest_bytes).map_err(frontend_contract)?;
    let manifest_directory = manifest_path
        .parent()
        .expect("an absolute manifest path has a parent");
    let root_relative = manifest
        .root()
        .strip_prefix(manifest_directory)
        .map_err(|_| {
            frontend_contract(ProjectError::new(
                "PROJECT006",
                "project root escapes the retained manifest directory",
            ))
        })?;
    let root_directory =
        open_relative_directory_nofollow(manifest_parent, root_relative, manifest.root())
            .map_err(frontend_contract)?;
    let filesystem = HostProjectFilesystem::new(manifest.root().to_path_buf(), root_directory);
    filesystem
        .resolve_existing_file(manifest.root_module())
        .map_err(frontend_contract)?;
    filesystem
        .validate_output_path(manifest.evidence())
        .map_err(frontend_contract)?;
    for endpoint in manifest.endpoints().values() {
        if let Some(ca) = endpoint.ca() {
            filesystem
                .resolve_existing_file(ca)
                .map_err(frontend_contract)?;
        }
    }
    let program = load_project_program(manifest, &filesystem, &filesystem).map_err(|error| {
        let rendered = match error {
            opaal_runtime::project::ProjectProgramError::Module(error) => error.render().to_owned(),
            opaal_runtime::project::ProjectProgramError::Contract(error) => {
                format!("opaal: {error}\n")
            }
        };
        ProjectFrontendError { rendered }
    })?;
    Ok((program, filesystem))
}

#[derive(Debug)]
struct HostProjectFilesystem {
    root: PathBuf,
    root_directory: File,
}

impl HostProjectFilesystem {
    fn new(root: PathBuf, root_directory: File) -> Self {
        Self {
            root,
            root_directory,
        }
    }

    fn resolve_existing_file(&self, candidate: &Path) -> Result<PathBuf, ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        if !candidate.starts_with(&self.root) {
            return Err(ProjectError::new(
                "PROJECT006",
                "path escapes the explicit project root",
            ));
        }
        self.open_existing_file(&candidate)?;
        Ok(candidate)
    }

    fn open_existing_file(&self, candidate: &Path) -> Result<File, ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            ProjectError::new("PROJECT006", "path escapes the explicit project root")
        })?;
        open_relative_file_nofollow(&self.root_directory, relative, &candidate)
    }

    fn validate_output_path(&self, candidate: &Path) -> Result<(), ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            ProjectError::new("PROJECT006", "path escapes the explicit project root")
        })?;
        validate_relative_output_path(&self.root_directory, relative, &candidate)
    }
}

impl ModuleCanonicalizer for HostProjectFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.resolve_existing_file(candidate)
            .map_err(|error| ModulePathError::new(error.to_string()))
    }
}

impl ModuleSourceLoader for HostProjectFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let file = self
            .open_existing_file(module.path())
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take(maximum as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        Ok(bytes)
    }
}

fn read_bounded(file: File, maximum: usize) -> Result<Vec<u8>, ProjectError> {
    let metadata = file
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(ProjectError::new(
            "PROJECT024",
            "path is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    if bytes.len() > maximum {
        return Err(ProjectError::new("PROJECT002", "document exceeds 1 MiB"));
    }
    Ok(bytes)
}

fn open_absolute_file_with_parent_nofollow(path: &Path) -> Result<(File, File), ProjectError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectError::new("PROJECT024", "path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| ProjectError::new("PROJECT024", "path has no file name"))?;
    let directory = open_absolute_directory_nofollow(parent)?;
    let file = open_file_at_nofollow(&directory, name, path)?;
    Ok((directory, file))
}

fn open_relative_directory_nofollow(
    mut directory: File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let mut traversed = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectError::new(
                    "PROJECT024",
                    format!("{} is not relative to the manifest", display_path.display()),
                ));
            }
        }
    }
    Ok(directory)
}

fn open_absolute_directory_nofollow(path: &Path) -> Result<File, ProjectError> {
    if !path.is_absolute() {
        return Err(ProjectError::new(
            "PROJECT024",
            "project directory path is not absolute",
        ));
    }
    let descriptor = rustix::fs::open(
        Path::new("/"),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, Path::new("/")))?;
    let mut directory = File::from(descriptor);
    let mut traversed = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectError::new(
                    "PROJECT024",
                    "project directory path is not lexically normalized",
                ));
            }
        }
    }
    Ok(directory)
}

fn open_relative_file_nofollow(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectError::new(
            "PROJECT024",
            "path is not a regular file",
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectError::new(
            "PROJECT024",
            "project file path is not lexically normalized",
        ));
    };
    let mut directory = None;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectError::new(
                "PROJECT024",
                "project file path is not lexically normalized",
            ));
        };
        traversed.push(name);
        let parent = directory.as_ref().unwrap_or(root);
        directory = Some(open_directory_at_nofollow(parent, name, &traversed)?);
    }
    open_file_at_nofollow(directory.as_ref().unwrap_or(root), file_name, display_path)
}

fn validate_relative_output_path(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<(), ProjectError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path has no file name",
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path is not lexically normalized",
        ));
    };
    let mut directory = None;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectError::new(
                "PROJECT024",
                "output path is not lexically normalized",
            ));
        };
        traversed.push(name);
        let parent = directory.as_ref().unwrap_or(root);
        directory = Some(open_directory_at_nofollow(parent, name, &traversed)?);
    }
    let parent = directory.as_ref().unwrap_or(root);
    let metadata =
        match rustix::fs::statat(parent, *file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(project_open_error(error, display_path)),
        };
    match rustix::fs::FileType::from_raw_mode(metadata.st_mode) {
        rustix::fs::FileType::RegularFile => Ok(()),
        rustix::fs::FileType::Symlink => Err(ProjectError::new(
            "PROJECT025",
            format!(
                "project path contains symbolic link `{}`",
                display_path.display()
            ),
        )),
        _ => Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a regular output file", display_path.display()),
        )),
    }
}

fn open_directory_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let directory = File::from(descriptor);
    if !directory
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
        .is_dir()
    {
        return Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a directory", display_path.display()),
        ));
    }
    Ok(directory)
}

fn open_file_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
        .is_file()
    {
        return Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a regular file", display_path.display()),
        ));
    }
    Ok(file)
}

fn project_open_error(error: rustix::io::Errno, path: &Path) -> ProjectError {
    if error == rustix::io::Errno::LOOP {
        ProjectError::new(
            "PROJECT025",
            format!("project path contains symbolic link `{}`", path.display()),
        )
    } else {
        ProjectError::new(
            "PROJECT024",
            format!("cannot open `{}`: {error}", path.display()),
        )
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, ProjectError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
            .join(path)
    };
    lexical_normalize(&candidate)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, ProjectError> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(ProjectError::new(
                        "PROJECT006",
                        "path escapes its lexical root",
                    ));
                }
            }
            Component::Normal(value) => output.push(value),
        }
    }
    Ok(output)
}

fn frontend_contract(error: ProjectError) -> ProjectFrontendError {
    ProjectFrontendError {
        rendered: format!("opaal: {error}\n"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = absolute_lexical(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target")
                    .join(format!(
                        "opaal-retained-project-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    )),
            )
            .unwrap();
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn retained_manifest_parent_anchors_later_source_reads() {
        let temporary = TempDirectory::new();
        let project_path = temporary.0.join("project");
        let moved_path = temporary.0.join("moved");
        fs::create_dir(&project_path).unwrap();
        let manifest_path = project_path.join("opaal.toml");
        fs::write(&manifest_path, "manifest").unwrap();
        fs::write(project_path.join("tasks.opaal"), "original").unwrap();

        let (manifest_parent, manifest_file) =
            open_absolute_file_with_parent_nofollow(&manifest_path).unwrap();
        drop(manifest_file);
        fs::rename(&project_path, &moved_path).unwrap();
        fs::create_dir(&project_path).unwrap();
        fs::write(project_path.join("tasks.opaal"), "replacement").unwrap();

        let root = open_relative_directory_nofollow(manifest_parent, Path::new("."), &project_path)
            .unwrap();
        let filesystem = HostProjectFilesystem::new(project_path.clone(), root);
        let mut source = String::new();
        filesystem
            .open_existing_file(&project_path.join("tasks.opaal"))
            .unwrap()
            .read_to_string(&mut source)
            .unwrap();
        assert_eq!(source, "original");
    }
}
