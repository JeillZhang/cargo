//! Schemas for JSON messages emitted by Cargo.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// File information of a package archive generated by `cargo package --list`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PackageList {
    /// The Package ID Spec of the package.
    pub id: crate::core::PackageIdSpec,
    /// A map of relative paths in the archive to their detailed file information.
    pub files: BTreeMap<PathBuf, PackageFile>,
}

/// Where the file is from.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PackageFile {
    /// File being copied from another location.
    Copy {
        /// An absolute path to the actual file content
        path: PathBuf,
    },
    /// File being generated during packaging
    Generate {
        /// An absolute path to the original file the generated one is based on.
        /// if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
}
