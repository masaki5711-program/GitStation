//! `[clients]`: the `gstn` binaries the server distributes
//! (`GET /api/v1/clients`, runbook cli-distribution).
//!
//! | key | default | range |
//! |---|---|---|
//! | `clients.dir` | none: `forge serve` uses `<directory of the forge executable>/../share/forge/clients` | an absolute path; a directory holding `manifest.json` and the binaries the image build wrote |
//!
//! forge only reads the directory; it never builds or changes a binary.

use std::path::PathBuf;

use forge_errors::ForgeError;

use super::{database, invalid};

/// The key (named by errors and doctor).
pub const DIR_KEY: &str = "clients.dir";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientsConfig {
    /// `clients.dir` (`None`: the install directory's default).
    pub dir: Option<PathBuf>,
}

impl ClientsConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let dir = match get(DIR_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(DIR_KEY, "empty")),
            Some(p) => Some(database::password_path(DIR_KEY, p)?),
        };
        Ok(Self { dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_dir_is_optional_and_absolute() {
        let none = ClientsConfig::load(|_| None).expect("default");
        assert_eq!(none.dir, None);
        let abs = if cfg!(windows) {
            r"C:\forge\clients"
        } else {
            "/usr/local/share/forge/clients"
        };
        let set = ClientsConfig::load(|k| (k == DIR_KEY).then_some(abs)).expect("absolute");
        assert_eq!(set.dir, Some(PathBuf::from(abs)));
        for bad in ["", "relative/clients"] {
            let err = ClientsConfig::load(|k| (k == DIR_KEY).then_some(bad)).expect_err(bad);
            assert_eq!(err.code(), forge_errors::ErrorCode::ConfigInvalid);
        }
    }
}
