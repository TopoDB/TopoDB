//! Acquisition of the ONNX Runtime dynamic library: pinned artifact table,
//! checksum-verified download + extraction, and the strict-precedence
//! resolver the embedder consults before any ort call. See
//! docs/superpowers/specs/2026-07-18-onnx-bundling-design.md (local, gitignored).

/// The exact ONNX Runtime version ort-sys 2.0.0-rc.12 distributes (its
/// build/download/dist.txt pins ms@1.24.2). Bumping ort => update this
/// version and every sha256 below, nothing else.
/// consumed from Task 3's resolver
#[allow(dead_code)]
pub(crate) const ORT_VERSION: &str = "1.24.2";

/// One official Microsoft release artifact and the dylib inside it.
/// sha256 is of the ARCHIVE, verified before extraction.
/// consumed from Task 3's resolver
#[allow(dead_code)]
pub(crate) struct OrtArtifact {
    pub archive_name: &'static str,
    pub sha256: &'static str,
    /// Path of the dylib inside the archive (below the top-level
    /// `onnxruntime-<platform>-<ver>/` directory).
    pub dylib_rel_path: &'static str,
    /// Bare filename the dylib is installed as.
    /// consumed from Task 3's resolver
    #[allow(dead_code)]
    pub dylib_file: &'static str,
    pub is_zip: bool,
}

#[allow(dead_code)]
static MACOS_ARM64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-osx-arm64-1.24.2.tgz",
    sha256: "0af4fa503e8ea285245b47ee42d0a7461b8156a81270857da0c1d4ecf858abde",
    dylib_rel_path: "lib/libonnxruntime.1.24.2.dylib",
    dylib_file: "libonnxruntime.1.24.2.dylib",
    is_zip: false,
};
#[allow(dead_code)]
static LINUX_X64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-linux-x64-1.24.2.tgz",
    sha256: "43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6",
    dylib_rel_path: "lib/libonnxruntime.so.1.24.2",
    dylib_file: "libonnxruntime.so.1.24.2",
    is_zip: false,
};
#[allow(dead_code)]
static LINUX_ARM64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-linux-aarch64-1.24.2.tgz",
    sha256: "6715b3d19965a2a6981e78ed4ba24f17a8c30d2d26420dbed10aac7ceca0085e",
    dylib_rel_path: "lib/libonnxruntime.so.1.24.2",
    dylib_file: "libonnxruntime.so.1.24.2",
    is_zip: false,
};
#[allow(dead_code)]
static WIN_X64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-win-x64-1.24.2.zip",
    sha256: "8e3e9c826375352e29cb2614fe44f3d7a4b0ff7b8028ad7a456af9d949a7e8b0",
    dylib_rel_path: "lib/onnxruntime.dll",
    dylib_file: "onnxruntime.dll",
    is_zip: true,
};

/// The artifact for the compile-time target, or None on targets this repo
/// doesn't ship (auto-download then simply never engages; resolution falls
/// through to Failed with the manual-install guidance).
///
/// Notably x86_64-apple-darwin: Microsoft ships no official 1.24.2 artifact
/// for Intel Macs (nor does ort's own binary table) — those hosts keep the
/// manual-install path (brew / ORT_DYLIB_PATH).
/// consumed from Task 3's resolver
#[allow(dead_code)]
pub(crate) fn current_artifact() -> Option<&'static OrtArtifact> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(&MACOS_ARM64)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(&LINUX_X64)
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(&LINUX_ARM64)
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(&WIN_X64)
    } else {
        None
    }
}

/// consumed from Task 3's resolver
#[allow(dead_code)]
pub(crate) fn artifact_url(a: &OrtArtifact) -> String {
    format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{ORT_VERSION}/{}",
        a.archive_name
    )
}

#[cfg(test)]
pub(crate) fn all_artifacts() -> [&'static OrtArtifact; 4] {
    [&MACOS_ARM64, &LINUX_X64, &LINUX_ARM64, &WIN_X64]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_table_is_fully_pinned() {
        for a in all_artifacts() {
            assert!(
                a.archive_name.contains(ORT_VERSION),
                "archive {} must embed the version pin",
                a.archive_name
            );
            assert_eq!(
                a.sha256.len(),
                64,
                "sha256 for {} must be 64 hex chars, got {:?}",
                a.archive_name,
                a.sha256
            );
            assert!(
                a.sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "sha256 for {} must be lowercase hex",
                a.archive_name
            );
            assert!(a.dylib_rel_path.starts_with("lib/"), "{}", a.dylib_rel_path);
            assert!(
                artifact_url(a).starts_with(
                    "https://github.com/microsoft/onnxruntime/releases/download/v1.24.2/"
                ),
                "{}",
                artifact_url(a)
            );
            assert_eq!(a.is_zip, a.archive_name.ends_with(".zip"));
        }
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn current_target_artifact_matches_platform_support() {
        match current_artifact() {
            Some(a) => assert!(a.archive_name.contains(ORT_VERSION)),
            None => assert!(
                cfg!(all(target_os = "macos", target_arch = "x86_64")),
                "only Intel macOS may lack a pinned artifact"
            ),
        }
    }
}
