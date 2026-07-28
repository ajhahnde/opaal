//! Acceptance tests for the platform capability foundation.

use std::ffi::OsString;
use std::path::Path;

use flashshell_platform::{
    Capabilities, Capability, ChildDescriptor, ChildProcess, DescriptorEndpoint,
    DescriptorReadError, DirectoryEntry, DirectoryEntryKind, DirectoryReadError,
    DirectoryReadRequest, FakeChild, FakePlatform, FileActionError, FileOpenMode, FileOpenRequest,
    PipeEndpoints, PipeError, Platform, PlatformError, ProcessGroup, ProcessGroupId, ProcessStatus,
    RecordingPlatform, SpawnError, SpawnRequest, SpawnRequestError, WorkingDirectoryError,
    WorkingDirectoryRequest,
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
    let mut file = FakePlatform::full()
        .open_file_io(request)
        .expect("the full fake opens an in-process file endpoint");
    let mut buffer = [0; 4];
    assert_eq!(file.read(&mut buffer).unwrap(), 0);
    assert_eq!(file.write(b"bytes").unwrap(), 5);
    drop((endpoint, inherited, file));

    assert_eq!(
        FakePlatform::none()
            .open_file(request)
            .expect_err("the empty fake rejects file actions"),
        FileActionError::Platform(PlatformError::Unsupported {
            capability: Capability::FileActions,
        })
    );
    assert_eq!(
        FakePlatform::none()
            .open_file_io(request)
            .expect_err("the empty fake rejects in-process file actions"),
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
    let sibling = platform.spawn(&request).expect("the fake spawn succeeds");

    assert!(child.id() > 0);
    assert_ne!(child.id(), sibling.id());
    assert_eq!(child.process_group(), None);
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

use flashshell_platform::TerminalSize;

#[test]
fn a_fake_platform_reports_its_scripted_terminal_size() {
    let platform =
        FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(100, 40));

    assert!(platform.is_terminal());
    let size = platform.terminal_size().expect("size is supported");
    assert_eq!(size.columns(), 100);
    assert_eq!(size.rows(), 40);
}

#[test]
fn a_default_fake_platform_is_not_a_terminal() {
    let platform = FakePlatform::full();

    assert!(!platform.is_terminal());
    let size = platform.terminal_size().expect("size is supported");
    assert_eq!(size.columns(), 80);
    assert_eq!(size.rows(), 24);
}

#[test]
fn terminal_size_requires_the_terminal_info_capability() {
    let platform = FakePlatform::new(Capabilities::empty());

    let error = platform.terminal_size().expect_err("capability is absent");

    assert_eq!(
        error,
        PlatformError::Unsupported {
            capability: Capability::TerminalInfo
        }
    );
}

#[test]
fn entering_raw_mode_requires_the_terminal_info_capability() {
    let platform = FakePlatform::new(Capabilities::empty());

    let error = platform.enter_raw_mode().expect_err("capability is absent");

    assert_eq!(
        error,
        PlatformError::Unsupported {
            capability: Capability::TerminalInfo
        }
    );
}

#[test]
fn a_fake_raw_mode_guard_restores_without_a_terminal() {
    let platform = FakePlatform::full();

    let mut guard = platform.enter_raw_mode().expect("raw mode is supported");

    assert!(guard.restore().is_ok());
    assert!(guard.restore().is_ok(), "restore is idempotent");
}

/// A platform that implements only what the trait requires.
///
/// `FakePlatform` overrides both terminal answers, so it cannot reach the trait
/// defaults; the adapters that do reach them are the ones that never think
/// about terminals at all, and this stands in for those.
#[derive(Debug)]
struct MinimalPlatform;

impl Platform for MinimalPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        Err(PipeError::Platform(unsupported(Capability::Pipes)))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Err(FileActionError::Platform(unsupported(
            Capability::FileActions,
        )))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Err(FileActionError::Platform(unsupported(
            Capability::FileActions,
        )))
    }

    fn spawn(&self, _request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        Err(SpawnError::Platform(unsupported(Capability::ProcessSpawn)))
    }
}

fn unsupported(capability: Capability) -> PlatformError {
    PlatformError::Unsupported { capability }
}

#[test]
fn a_platform_reports_neither_end_as_a_terminal_by_default() {
    // Both defaults have to be false. An implementor that forgets the output
    // end must fall back to the canonical editor, which is merely degraded —
    // the opposite default would write cursor escapes into a redirect target.
    let platform = MinimalPlatform;

    assert!(!platform.is_terminal());
    assert!(!platform.is_output_terminal());
}

#[test]
fn a_scripted_platform_can_answer_its_two_ends_differently() {
    // The mixed case is the whole point of the second method: a keyboard
    // session with its output redirected. A double backed by one flag could
    // not express it, so the distinction would be untestable everywhere.
    let redirected = FakePlatform::with_terminal_ends(
        Capabilities::full(),
        true,
        false,
        TerminalSize::new(80, 24),
    );

    assert!(redirected.is_terminal());
    assert!(!redirected.is_output_terminal());
}

#[test]
fn a_scripted_terminal_reports_both_ends() {
    let platform =
        FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(80, 24));

    assert!(platform.is_terminal());
    assert!(platform.is_output_terminal());
}

#[test]
fn a_recording_platform_counts_the_terminal_calls_after_it_has_moved() {
    let platform = RecordingPlatform::new(FakePlatform::full());
    let log = platform.log();

    // The consumer takes the platform by value; the log outlives that move,
    // which is the whole reason this double exists.
    let moved = platform;
    let _ = moved.enter_raw_mode();
    let _ = moved.enter_raw_mode();
    let _ = moved.terminal_size();

    assert_eq!(log.raw_mode_entries(), 2);
    assert_eq!(log.terminal_size_queries(), 1);
}

#[test]
fn a_recording_platform_answers_exactly_as_the_platform_it_wraps() {
    // The recorder overrides every method so it can observe two of them. That
    // makes it structurally able to drift: a method whose override is dropped
    // silently falls back to the trait default instead of the wrapped
    // platform, and the three methods below are exactly the ones whose
    // defaults differ from `FakePlatform`'s answers.
    let inner = FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(132, 43));
    let recording = RecordingPlatform::new(inner);

    assert_eq!(recording.capabilities(), inner.capabilities());
    assert_eq!(recording.is_terminal(), inner.is_terminal());
    assert_eq!(recording.is_output_terminal(), inner.is_output_terminal());
    assert_eq!(recording.terminal_size(), inner.terminal_size());
    assert_eq!(
        recording.resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new("/tmp"),
            Path::new("sub/../here"),
        )),
        inner.resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new("/tmp"),
            Path::new("sub/../here"),
        ))
    );

    // The fifth and last override that can be deleted without a compile error.
    // Its trait default returns InvalidEndpoint where the wrapped fake returns
    // Ok(0), so a dropped delegation would change the answer silently.
    let (reader, _writer) = inner.pipe().expect("the fake pipe exists").into_parts();
    let mut buffer = [0u8; 8];
    assert_eq!(
        recording.read_descriptor(reader.as_ref(), &mut buffer),
        inner.read_descriptor(reader.as_ref(), &mut buffer)
    );
}

#[test]
fn a_recording_platform_records_a_refused_call_and_still_refuses_it() {
    let platform = RecordingPlatform::new(FakePlatform::none());
    let log = platform.log();

    let error = platform.enter_raw_mode().expect_err("capability is absent");

    assert_eq!(
        error,
        PlatformError::Unsupported {
            capability: Capability::TerminalInfo
        }
    );
    assert_eq!(
        log.raw_mode_entries(),
        1,
        "the attempt is recorded even though it failed"
    );
}

#[test]
fn directory_read_is_a_capability_of_its_own() {
    // Directory enumeration is deliberately not folded into FileActions: an
    // adapter can provide descriptor plumbing without providing enumeration,
    // and the capability model has no partial support to express that with.
    let platform = FakePlatform::new(Capabilities::empty().with(Capability::FileActions));

    assert_eq!(
        platform.require(Capability::DirectoryRead),
        Err(PlatformError::Unsupported {
            capability: Capability::DirectoryRead,
        }),
    );
    assert!(Capability::ALL.contains(&Capability::DirectoryRead));
}

#[test]
fn reading_a_directory_without_the_capability_is_unsupported() {
    let platform = FakePlatform::new(Capabilities::empty().with(Capability::FileActions));
    let request = DirectoryReadRequest::new(Path::new("."), Path::new("/stage"));

    let error = platform
        .read_directory(request)
        .expect_err("the capability is absent");

    assert_eq!(
        error,
        DirectoryReadError::Platform(PlatformError::Unsupported {
            capability: Capability::DirectoryRead,
        }),
    );
}

#[test]
fn the_fake_platform_enumerates_an_empty_directory() {
    // The fake performs no host access, so a supported capability yields an
    // immediately exhausted stream rather than invented entries.
    let platform = FakePlatform::full();
    let request = DirectoryReadRequest::new(Path::new("."), Path::new("/stage"));

    let mut stream = platform
        .read_directory(request)
        .expect("the capability is present");

    assert_eq!(stream.next_entry(), Ok(None));
    assert_eq!(stream.next_entry(), Ok(None), "exhaustion is idempotent");
}

#[test]
fn a_directory_entry_carries_its_native_name_kind_and_file_size() {
    let entry = DirectoryEntry::new(
        OsString::from("notes.txt"),
        DirectoryEntryKind::File,
        Some(4096),
    );

    assert_eq!(entry.name(), OsString::from("notes.txt"));
    assert_eq!(entry.kind(), DirectoryEntryKind::File);
    assert_eq!(entry.size(), Some(4096));
}

#[test]
fn a_directory_entry_that_is_not_a_regular_file_has_no_size() {
    // Size is absent rather than zero: a directory has no byte length the shell
    // could honestly report, and inventing one would be a fabricated field.
    let entry = DirectoryEntry::new(OsString::from("src"), DirectoryEntryKind::Directory, None);

    assert_eq!(entry.kind(), DirectoryEntryKind::Directory);
    assert_eq!(entry.size(), None);
}

#[test]
fn a_process_group_identifier_rejects_the_reserved_zero() {
    // Zero means "the calling process" in setpgid, so a zero-valued identifier
    // would make joining an existing group indistinguishable from leading a
    // new one.
    assert_eq!(ProcessGroupId::new(0), None);
    assert_eq!(ProcessGroupId::new(41).map(ProcessGroupId::get), Some(41));
}

#[test]
fn a_spawn_request_inherits_the_shell_group_until_it_is_told_otherwise() {
    let argv = [OsString::from("fixture")];
    let environment = [];
    let request = SpawnRequest::new(Path::new("/fixture"), &argv, &environment, Path::new("/"))
        .expect("the request is valid");

    assert_eq!(request.process_group(), ProcessGroup::Inherit);
    assert!(!request.process_group().requires_capability());

    let group = ProcessGroupId::new(7).expect("seven is a usable group");
    for placement in [ProcessGroup::New, ProcessGroup::Join(group)] {
        let placed = request.in_process_group(placement);
        assert_eq!(placed.process_group(), placement);
        assert!(placed.process_group().requires_capability());
    }
}

#[test]
fn a_fake_child_leads_the_group_it_was_asked_to_create() {
    let leader = FakeChild::new(ProcessGroup::New);
    let group = leader
        .process_group()
        .expect("a new leader names a group after itself");

    assert_eq!(group.get(), leader.id());
    assert_eq!(
        FakeChild::new(ProcessGroup::Join(group)).process_group(),
        Some(group),
    );
    assert_eq!(FakeChild::new(ProcessGroup::Inherit).process_group(), None);
}

#[test]
fn placing_a_child_in_a_group_requires_the_process_group_capability() {
    // Spawn alone is not enough: a platform that can start a process but not
    // group it must refuse the placement instead of quietly dropping it.
    let platform = FakePlatform::new(Capabilities::empty().with(Capability::ProcessSpawn));
    let argv = [OsString::from("fixture")];
    let environment = [];
    let request = SpawnRequest::new(Path::new("/fixture"), &argv, &environment, Path::new("/"))
        .expect("the request is valid");

    assert!(
        platform.spawn(&request).is_ok(),
        "inheriting is always fine"
    );
    assert_eq!(
        platform
            .spawn(&request.in_process_group(ProcessGroup::New))
            .expect_err("the placement must be refused"),
        SpawnError::Platform(PlatformError::Unsupported {
            capability: Capability::ProcessGroups,
        }),
    );
}

#[test]
fn a_recording_platform_reports_the_group_placement_of_every_spawn() {
    let platform = RecordingPlatform::new(FakePlatform::full());
    let spawns = platform.spawn_log();
    let argv = [OsString::from("fixture")];
    let environment = [];
    let request = SpawnRequest::new(Path::new("/fixture"), &argv, &environment, Path::new("/"))
        .expect("the request is valid");

    let leader = platform
        .spawn(&request.in_process_group(ProcessGroup::New))
        .expect("the leader spawns");
    let group = leader.process_group().expect("the leader leads a group");
    platform
        .spawn(&request.in_process_group(ProcessGroup::Join(group)))
        .expect("the follower spawns");

    let records = spawns.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].requested(), ProcessGroup::New);
    assert_eq!(records[0].process_group(), Some(group));
    assert_eq!(records[1].requested(), ProcessGroup::Join(group));
    assert_eq!(records[1].process_group(), Some(group));
}

#[test]
fn entering_the_foreground_requires_the_foreground_terminal_capability() {
    let group = ProcessGroupId::new(11).expect("eleven is a usable group");
    let platform = FakePlatform::new(Capabilities::full_without(Capability::ForegroundTerminal));

    assert_eq!(
        platform
            .enter_foreground(group)
            .expect_err("the handover must be refused"),
        PlatformError::Unsupported {
            capability: Capability::ForegroundTerminal,
        },
    );
    assert_eq!(
        platform
            .foreground_process_group()
            .expect_err("querying the owner must be refused too"),
        PlatformError::Unsupported {
            capability: Capability::ForegroundTerminal,
        },
    );
}

#[test]
fn a_terminal_handover_is_released_once_whether_restored_or_dropped() {
    let platform = RecordingPlatform::new(FakePlatform::full());
    let log = platform.log();
    let first = ProcessGroupId::new(21).expect("the group is usable");
    let second = ProcessGroupId::new(22).expect("the group is usable");

    let mut guard = platform
        .enter_foreground(first)
        .expect("the full fake hands the terminal over");
    assert_eq!(guard.restore(), Ok(()));
    // Restoration is idempotent, and the drop that follows must not count a
    // second release: a doubled tally would hide a terminal that never returned.
    assert_eq!(guard.restore(), Ok(()));
    drop(guard);
    assert_eq!(log.foreground_releases(), 1);

    // The path a runtime error or a panic takes: the guard is never restored
    // explicitly and the terminal comes back through `Drop` alone.
    drop(
        platform
            .enter_foreground(second)
            .expect("the second handover succeeds"),
    );

    assert_eq!(log.foreground_handovers(), vec![first, second]);
    assert_eq!(log.foreground_releases(), 2);
}
