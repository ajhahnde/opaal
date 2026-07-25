//! Acceptance tests for the platform capability foundation.

use std::ffi::OsString;
use std::path::Path;

use flashshell_platform::{
    Capabilities, Capability, ChildDescriptor, DescriptorReadError, FakePlatform, FileActionError,
    FileOpenMode, FileOpenRequest, PipeError, Platform, PlatformError, ProcessStatus, SpawnError,
    SpawnRequest, SpawnRequestError, WorkingDirectoryError, WorkingDirectoryRequest,
};

#[test]
fn fake_platform_reports_its_scripted_capability_set() {
    let caps = Capabilities::empty()
        .with(Capability::ProcessSpawn)
        .with(Capability::Pipes);
    let platform = FakePlatform::new(caps);

    assert_eq!(platform.capabilities(), caps);
    assert!(platform.capabilities().supports(Capability::ProcessSpawn));
    assert!(platform.capabilities().supports(Capability::Pipes));
    assert!(
        !platform
            .capabilities()
            .supports(Capability::ForegroundTerminal)
    );
}

#[test]
fn require_accepts_a_supported_capability() {
    let platform = FakePlatform::new(Capabilities::empty().with(Capability::Environment));

    assert_eq!(platform.require(Capability::Environment), Ok(()));
}

#[test]
fn require_rejects_an_unsupported_capability_naming_it() {
    let platform = FakePlatform::new(Capabilities::empty());

    assert_eq!(
        platform.require(Capability::ProcessSpawn),
        Err(PlatformError::Unsupported {
            capability: Capability::ProcessSpawn,
        }),
    );
}

#[test]
fn full_supports_every_capability_and_empty_supports_none() {
    let full = FakePlatform::full();
    let none = FakePlatform::none();

    for capability in Capability::ALL {
        assert!(
            full.capabilities().supports(capability),
            "full is missing {capability:?}",
        );
        assert!(
            !none.capabilities().supports(capability),
            "empty unexpectedly has {capability:?}",
        );
    }
}

#[test]
fn fake_working_directory_resolution_is_host_free_and_capability_gated() {
    let resolved = FakePlatform::full()
        .resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new("../next"),
            Path::new("/work/current"),
        ))
        .expect("full fake should resolve lexically");
    assert_eq!(resolved, Path::new("/work/next"));

    let error = FakePlatform::none()
        .resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new("next"),
            Path::new("/work"),
        ))
        .expect_err("absent capability should fail before resolution");
    assert_eq!(
        error,
        WorkingDirectoryError::Platform(PlatformError::Unsupported {
            capability: Capability::WorkingDirectory,
        })
    );
}

#[test]
fn capability_construction_is_additive_and_order_independent() {
    let a = Capabilities::empty()
        .with(Capability::Pipes)
        .with(Capability::ProcessSpawn);
    let b = Capabilities::empty()
        .with(Capability::ProcessSpawn)
        .with(Capability::Pipes);

    assert_eq!(a, b);
    // Adding a capability already present is idempotent.
    assert_eq!(a.with(Capability::Pipes), a);
}

#[test]
fn unavailable_carries_the_capability_and_a_reason() {
    let error = PlatformError::Unavailable {
        capability: Capability::MonotonicClock,
        reason: "clock source not yet started".to_string(),
    };

    match error {
        PlatformError::Unavailable { capability, reason } => {
            assert_eq!(capability, Capability::MonotonicClock);
            assert!(reason.contains("clock source"));
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn spawn_requests_require_an_explicit_argv_zero() {
    let environment = [];
    let error = SpawnRequest::new(Path::new("/fixture"), &[], &environment, Path::new("/work"))
        .expect_err("an empty argv must be rejected");

    assert_eq!(error, SpawnRequestError::EmptyArgv);
}

#[test]
fn fake_pipes_are_host_free_and_require_the_pipe_capability() {
    let endpoints = FakePlatform::full()
        .pipe()
        .expect("the full fake creates opaque endpoints");
    let (_reader, _writer) = endpoints.into_parts();

    assert_eq!(
        FakePlatform::none()
            .pipe()
            .expect_err("the empty fake rejects pipes"),
        PipeError::Platform(PlatformError::Unsupported {
            capability: Capability::Pipes,
        })
    );
}

#[test]
fn fake_descriptor_reads_are_host_free_and_capability_gated() {
    let full = FakePlatform::full();
    let (reader, _writer) = full
        .pipe()
        .expect("the fake pipe should exist")
        .into_parts();
    let mut buffer = [0u8; 16];

    assert_eq!(full.read_descriptor(reader.as_ref(), &mut buffer), Ok(0));
    assert_eq!(
        FakePlatform::none().read_descriptor(reader.as_ref(), &mut buffer),
        Err(DescriptorReadError::Platform(PlatformError::Unsupported {
            capability: Capability::Pipes,
        }))
    );
}

#[test]
fn final_descriptor_maps_reject_duplicate_targets_but_allow_endpoint_aliasing() {
    let (reader, writer) = FakePlatform::full()
        .pipe()
        .expect("the fake pipe should succeed")
        .into_parts();
    let argv = [OsString::from("fixture")];
    let environment = [];
    let aliased = [
        ChildDescriptor::new(1, writer.as_ref()),
        ChildDescriptor::new(2, writer.as_ref()),
    ];
    let request = SpawnRequest::new(
        Path::new("/fixture"),
        &argv,
        &environment,
        Path::new("/work"),
    )
    .expect("the request is valid")
    .with_descriptors(&aliased)
    .expect("two targets may deliberately share one endpoint");
    assert_eq!(request.descriptors().len(), 2);

    let duplicate = [
        ChildDescriptor::new(0, reader.as_ref()),
        ChildDescriptor::new(0, writer.as_ref()),
    ];
    let error = SpawnRequest::new(
        Path::new("/fixture"),
        &argv,
        &environment,
        Path::new("/work"),
    )
    .expect("the request is valid")
    .with_descriptors(&duplicate)
    .expect_err("one child target may have only one final mapping");
    assert_eq!(error, SpawnRequestError::DuplicateDescriptor(0));

    let error = SpawnRequest::new(
        Path::new("/fixture"),
        &argv,
        &environment,
        Path::new("/work"),
    )
    .expect("the request is valid")
    .with_descriptors(&aliased)
    .expect("the map is valid")
    .with_closed_descriptors(&[2])
    .expect_err("one target cannot be both mapped and closed");
    assert_eq!(error, SpawnRequestError::MappedAndClosedDescriptor(2));
}

#[test]
fn fake_file_actions_are_host_free_and_capability_gated() {
    let request = FileOpenRequest::new(
        Path::new("target"),
        Path::new("/work"),
        FileOpenMode::WriteAppend,
    );
    let endpoint = FakePlatform::full()
        .open_file(request)
        .expect("the full fake opens an opaque endpoint");
    let inherited = FakePlatform::full()
        .inherit_descriptor(1)
        .expect("the full fake duplicates an inherited endpoint");
    drop((endpoint, inherited));

    assert_eq!(
        FakePlatform::none()
            .open_file(request)
            .expect_err("the empty fake rejects file actions"),
        FileActionError::Platform(PlatformError::Unsupported {
            capability: Capability::FileActions,
        })
    );
}

#[test]
fn fake_spawn_is_host_free_and_returns_an_owned_waitable_child() {
    let platform = FakePlatform::full();
    let argv = [
        OsString::from("fixture"),
        OsString::from("literal argument"),
    ];
    let environment = [(OsString::from("FLASH"), OsString::from("shell"))];
    let request = SpawnRequest::new(
        Path::new("/does/not/need/to/exist"),
        &argv,
        &environment,
        Path::new("/also/not/read"),
    )
    .expect("the request is valid");

    let mut child = platform.spawn(&request).expect("the fake spawn succeeds");

    assert_eq!(child.id(), 0);
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));
}

#[test]
fn spawn_rejects_an_absent_capability_before_host_access() {
    let platform = FakePlatform::none();
    let argv = [OsString::from("fixture")];
    let environment = [];
    let request = SpawnRequest::new(
        Path::new("/does/not/exist"),
        &argv,
        &environment,
        Path::new("/does/not/exist"),
    )
    .expect("the request is structurally valid");

    assert_eq!(
        platform
            .spawn(&request)
            .expect_err("spawn must be rejected"),
        SpawnError::Platform(PlatformError::Unsupported {
            capability: Capability::ProcessSpawn,
        }),
    );
}
