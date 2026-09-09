#![forbid(unsafe_code)]

use std::path::Path;

use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityQuery, AuthorityScope, Platform,
};
use opaal_platform_posix::PosixPlatform;

#[test]
fn posix_reports_only_maintained_operational_enforcement_truth() {
    let filesystem = AuthorityQuery::new(
        AuthorityEffect::FilesystemRead,
        AuthorityScope::ProjectPath(Path::new("/project/input")),
    );
    let process = AuthorityQuery::new(AuthorityEffect::ProcessRun, AuthorityScope::Tool("cargo"));
    let network = AuthorityQuery::new(
        AuthorityEffect::NetworkHttp,
        AuthorityScope::Endpoint {
            endpoint: "readiness",
            method: "GET",
        },
    );

    assert_eq!(
        PosixPlatform.authority_enforcement(filesystem),
        AuthorityEnforcement::Enforced
    );
    assert_eq!(
        PosixPlatform.authority_enforcement(process),
        if cfg!(target_os = "linux") {
            AuthorityEnforcement::Unenforced
        } else {
            AuthorityEnforcement::Unsupported
        },
        "Linux retains exact executable identity; macOS must report the process route as unsupported"
    );
    assert_eq!(
        PosixPlatform.authority_enforcement(network),
        AuthorityEnforcement::Enforced,
        "the maintained HTTP adapter enforces endpoint and TLS policy"
    );
}
