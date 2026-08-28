//! Compile-time gates for development and fixture-only behavior.

use std::path::PathBuf;

pub const DEV_PLACEBO_REGISTRY_ENV: &str = "ACP_STACK_DEV_PLACEBO_REGISTRY";
pub const FIXTURE_AGENT_CAPABILITIES_ENV: &str = "ACP_STACK_AGENT_CAPABILITIES_PATH";
pub const FIXTURE_CONFIG_OPTIONS_ENV: &str = "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH";
pub const FIXTURE_NEW_SESSION_RESPONSE_ENV: &str = "ACP_STACK_AGENT_NEW_SESSION_RESPONSE_PATH";
pub const GITHUB_API_BASE_ENV: &str = "ACP_STACK_GITHUB_API_BASE";
pub const PROVIDER_MODELS_BASE_ENV: &str = "ACP_STACK_PROVIDER_MODELS_BASE";
pub const INSTALL_BINARY_DIR_ENV: &str = "ACP_STACK_INSTALL_BINARY_DIR";
pub const S3_ENDPOINT_OVERRIDE_ENV: &str = "ACP_STACK_S3_ENDPOINT_OVERRIDE";
pub const TEST_INSECURE_HTTPS_ENV: &str = "ACP_STACK_TEST_INSECURE_HTTPS";
pub const TEST_SKIP_AGENT_INSTALL_ENV: &str = "ACP_STACK_TEST_SKIP_AGENT_INSTALL";
/// A fixture build on a throwaway host (docker test image, CI runner) may use the real HOME and
/// the real network; unset, `fs_util::home_dir` and `http_client` refuse both.
pub const TEST_DISPOSABLE_HOST_ENV: &str = "ACP_STACK_TEST_DISPOSABLE_HOST";

#[cfg(feature = "test-fixtures")]
pub fn fixture_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[cfg(not(feature = "test-fixtures"))]
pub fn fixture_path(name: &str) -> Option<PathBuf> {
    let _ = name;
    None
}

#[cfg(feature = "test-fixtures")]
pub fn fixture_string(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(not(feature = "test-fixtures"))]
pub fn fixture_string(name: &str) -> Option<String> {
    let _ = name;
    None
}

#[cfg(feature = "test-fixtures")]
pub fn fixture_enabled(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

#[cfg(not(feature = "test-fixtures"))]
pub fn fixture_enabled(name: &str) -> bool {
    let _ = name;
    false
}

/// The disposable-host opt-out demands an exact "1": with presence semantics a stray `VAR=0`
/// or empty assignment in a developer shell would silently disable every fixture guard.
#[cfg(feature = "test-fixtures")]
pub fn disposable_host_enabled() -> bool {
    value_marks_host_disposable(std::env::var_os(TEST_DISPOSABLE_HOST_ENV).as_deref())
}

#[cfg(not(feature = "test-fixtures"))]
pub fn disposable_host_enabled() -> bool {
    false
}

/// Single decision point for every fixture guard (HOME isolation, egress proxy, websocket and
/// git restrictions) so a guard added later cannot drift from the disposable-host opt-out.
#[cfg(feature = "test-fixtures")]
pub fn fixture_guards_active() -> bool {
    !disposable_host_enabled()
}

#[cfg(not(feature = "test-fixtures"))]
pub fn fixture_guards_active() -> bool {
    false
}

#[cfg(feature = "test-fixtures")]
fn value_marks_host_disposable(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

#[cfg(all(test, feature = "test-fixtures"))]
mod guard_value_tests {
    use super::*;

    #[test]
    fn only_exact_one_marks_host_disposable() {
        assert!(value_marks_host_disposable(Some(std::ffi::OsStr::new("1"))));
        for rejected in ["0", "", "true", " 1", "1 "] {
            assert!(
                !value_marks_host_disposable(Some(std::ffi::OsStr::new(rejected))),
                "{rejected}"
            );
        }
        assert!(!value_marks_host_disposable(None));
    }
}

#[cfg(all(test, not(feature = "test-fixtures")))]
mod tests {
    use super::*;

    #[cfg(not(feature = "test-fixtures"))]
    #[test]
    fn fixture_envs_are_ignored_without_feature() {
        let _guard = EnvGuard::set(GITHUB_API_BASE_ENV, "http://127.0.0.1:1");
        assert_eq!(fixture_string(GITHUB_API_BASE_ENV), None);
        assert_eq!(fixture_path(FIXTURE_CONFIG_OPTIONS_ENV), None);
        assert_eq!(fixture_path(FIXTURE_AGENT_CAPABILITIES_ENV), None);
        assert!(!fixture_enabled(TEST_INSECURE_HTTPS_ENV));
    }

    struct EnvGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            // SAFETY: this unit test is single-threaded with respect to this
            // process-global variable and restores it before returning.
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `set`.
            unsafe {
                if let Some(previous) = self.previous.take() {
                    std::env::set_var(self.name, previous);
                } else {
                    std::env::remove_var(self.name);
                }
            }
        }
    }
}
