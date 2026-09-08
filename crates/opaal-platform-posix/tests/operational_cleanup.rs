#![forbid(unsafe_code)]

use std::path::Path;

use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityQuery, AuthorityScope, Platform,
};
use opaal_platform_posix::PosixPlatform;

#[test]
fn posix_does_not_promote_low_level_capabilities_to_operational_authority() {
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
        AuthorityEnforcement::Unsupported
    );
    assert_eq!(
        PosixPlatform.authority_enforcement(process),
        AuthorityEnforcement::Unsupported,
        "process primitives alone do not implement the bounded maintained adapter"
    );
    assert_eq!(
        PosixPlatform.authority_enforcement(network),
        AuthorityEnforcement::Unsupported,
        "the bounded HTTP adapter is not part of this platform surface yet"
    );
}
