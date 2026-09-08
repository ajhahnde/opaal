//! Canonical Semantic Versioning operations.

use std::fmt;

use semver::{Version as SemVersion, VersionReq};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Version(SemVersion);

impl Version {
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let parsed = SemVersion::parse(text).map_err(|error| VersionError(error.to_string()))?;
        if parsed.to_string() != text {
            return Err(VersionError(
                "version is not in canonical SemVer form".to_owned(),
            ));
        }
        Ok(Self(parsed))
    }

    #[must_use]
    pub fn render(&self) -> String {
        self.0.to_string()
    }

    #[must_use]
    pub const fn as_semver(&self) -> &SemVersion {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionRange(VersionReq);

impl VersionRange {
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let range = VersionReq::parse(text).map_err(|error| VersionError(error.to_string()))?;
        Ok(Self(range))
    }

    #[must_use]
    pub fn matches(&self, version: &Version) -> bool {
        self.0.matches(&version.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionError(String);

impl fmt::Display for VersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VersionError {}
