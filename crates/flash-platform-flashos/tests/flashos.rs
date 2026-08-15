//! Acceptance tests for the FlashOS adapter boundary and directory policy.

use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use flash_platform::{
    Capabilities, Capability, Platform, PlatformError, StandardDirectoryEnvironment,
};
use flash_platform_flashos::{FlashOsDirectoryPolicy, FlashOsPlatform};

struct DirectoryEnvironment(Vec<(OsString, OsString)>);

impl StandardDirectoryEnvironment for DirectoryEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        self.0
            .iter()
            .find_map(|(key, value)| (key == name).then(|| value.clone()))
    }
}

#[test]
fn the_adapter_advertises_nothing_before_target_qualification() {
    let platform = FlashOsPlatform::new();

    assert_eq!(platform.capabilities(), Capabilities::empty());
    for capability in Capability::ALL {
        assert_eq!(
            platform.require(capability),
            Err(PlatformError::Unsupported { capability }),
        );
    }
}

#[test]
fn flashos_standard_directories_preserve_absolute_native_overrides() {
    let environment = DirectoryEnvironment(vec![
        (OsString::from("HOME"), OsString::from("/users/test")),
        (
            OsString::from("XDG_CONFIG_HOME"),
            OsString::from("/native/config"),
        ),
        (
            OsString::from("XDG_CACHE_HOME"),
            OsString::from("/native/cache"),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            OsString::from("/native/state"),
        ),
    ]);

    let directories = FlashOsDirectoryPolicy::select(&environment);

    assert_eq!(directories.home(), Path::new("/users/test"));
    assert_eq!(directories.config(), Path::new("/native/config"));
    assert_eq!(directories.cache(), Path::new("/native/cache"));
    assert_eq!(directories.state(), Path::new("/native/state"));
}

#[cfg(unix)]
#[test]
fn flashos_standard_directories_preserve_non_utf8_path_bytes() {
    let native = OsString::from_vec(b"/native/\xffconfig".to_vec());
    let environment = DirectoryEnvironment(vec![
        (OsString::from("HOME"), OsString::from("/users/test")),
        (OsString::from("XDG_CONFIG_HOME"), native.clone()),
    ]);

    let directories = FlashOsDirectoryPolicy::select(&environment);

    assert_eq!(directories.config().as_os_str(), native);
}

#[test]
fn flashos_standard_directories_apply_target_owned_fallbacks() {
    let environment = DirectoryEnvironment(vec![
        (OsString::from("HOME"), OsString::from("relative/home")),
        (OsString::from("USER"), OsString::from("flash-user")),
        (
            OsString::from("XDG_CONFIG_HOME"),
            OsString::from("relative/config"),
        ),
    ]);

    let directories = FlashOsDirectoryPolicy::select(&environment);

    assert_eq!(directories.home(), Path::new("/home/flash-user"));
    assert_eq!(directories.config(), Path::new("/home/flash-user/.config"));
    assert_eq!(directories.cache(), Path::new("/home/flash-user/.cache"));
    assert_eq!(
        directories.state(),
        Path::new("/home/flash-user/.local/state")
    );
}

#[test]
fn flashos_root_and_unknown_users_have_deterministic_home_fallbacks() {
    let root = DirectoryEnvironment(vec![(OsString::from("USER"), OsString::from("root"))]);
    let unknown = DirectoryEnvironment(Vec::new());

    assert_eq!(
        FlashOsDirectoryPolicy::select(&root).home(),
        Path::new("/root")
    );
    assert_eq!(
        FlashOsDirectoryPolicy::select(&unknown).home(),
        Path::new("/home/user")
    );
}
