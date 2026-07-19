//! Acquisition of the ONNX Runtime dynamic library: pinned artifact table,
//! checksum-verified download + extraction, and the strict-precedence
//! resolver the embedder consults before any ort call. See
//! docs/superpowers/specs/2026-07-18-onnx-bundling-design.md (local, gitignored).

/// The exact ONNX Runtime version ort-sys 2.0.0-rc.12 distributes (its
/// build/download/dist.txt pins ms@1.24.2). Bumping ort => update this
/// version and every sha256 below, nothing else.
pub(crate) const ORT_VERSION: &str = "1.24.2";

/// One official Microsoft release artifact and the dylib inside it.
/// sha256 is of the ARCHIVE, verified before extraction.
pub(crate) struct OrtArtifact {
    pub archive_name: &'static str,
    pub sha256: &'static str,
    /// Path of the dylib inside the archive (below the top-level
    /// `onnxruntime-<platform>-<ver>/` directory).
    pub dylib_rel_path: &'static str,
    /// Bare filename the dylib is installed as.
    pub dylib_file: &'static str,
    pub is_zip: bool,
}

static MACOS_ARM64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-osx-arm64-1.24.2.tgz",
    sha256: "0af4fa503e8ea285245b47ee42d0a7461b8156a81270857da0c1d4ecf858abde",
    dylib_rel_path: "lib/libonnxruntime.1.24.2.dylib",
    dylib_file: "libonnxruntime.1.24.2.dylib",
    is_zip: false,
};
static LINUX_X64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-linux-x64-1.24.2.tgz",
    sha256: "43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6",
    dylib_rel_path: "lib/libonnxruntime.so.1.24.2",
    dylib_file: "libonnxruntime.so.1.24.2",
    is_zip: false,
};
static LINUX_ARM64: OrtArtifact = OrtArtifact {
    archive_name: "onnxruntime-linux-aarch64-1.24.2.tgz",
    sha256: "6715b3d19965a2a6981e78ed4ba24f17a8c30d2d26420dbed10aac7ceca0085e",
    dylib_rel_path: "lib/libonnxruntime.so.1.24.2",
    dylib_file: "libonnxruntime.so.1.24.2",
    is_zip: false,
};
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

pub(crate) fn artifact_url(a: &OrtArtifact) -> String {
    format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{ORT_VERSION}/{}",
        a.archive_name
    )
}

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum OrtRuntime {
    /// ORT_DYLIB_PATH is set and loadable — ort resolves it itself.
    EnvOverride,
    /// A system runtime is on the loader search path — ort finds it.
    System,
    /// A cached/downloaded dylib at this path — point ort at it via init_from.
    Local(PathBuf),
    /// No runtime; the String is the one stderr line's reason text.
    Unavailable(String),
}

fn platform_soname() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// Probes whether a single candidate dylib is loadable. Returns `Ok` if
/// libloading can successfully load it, `Err` with the failure reason otherwise.
pub(crate) fn probe_loadable(candidate: &str) -> Result<(), String> {
    // SAFETY: loading a shared library runs its initializers; this is
    // the exact load ort itself performs immediately after a successful
    // probe, so no new code runs that wouldn't have run anyway.
    match unsafe { libloading::Library::new(candidate) } {
        Ok(lib) => {
            drop(lib);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Strict precedence per the design spec: env override (exclusive) →
/// system soname → cached download → fetch (if enabled) → Unavailable.
/// `fetch` is injected so unit tests never touch the network.
pub(crate) fn resolve(
    model_dir: &Path,
    download_enabled: bool,
    fetch: &dyn Fn(&str, &Path) -> Result<(), String>,
) -> OrtRuntime {
    // 1. Explicit override: the ONLY candidate, never download (matches
    //    ort's own resolution — no fallback past an explicit path).
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        if !p.is_empty() {
            return match probe_loadable(&p) {
                Ok(()) => OrtRuntime::EnvOverride,
                Err(e) => {
                    OrtRuntime::Unavailable(format!("ORT_DYLIB_PATH is set but not loadable: {e}"))
                }
            };
        }
    }
    // 2. System runtime on the loader search path.
    if probe_loadable(platform_soname()).is_ok() {
        return OrtRuntime::System;
    }
    let Some(artifact) = current_artifact() else {
        return OrtRuntime::Unavailable(
            "no ONNX Runtime found and no pinned artifact exists for this platform; \
             install ONNX Runtime or set ORT_DYLIB_PATH"
                .into(),
        );
    };
    let ort_root = model_dir.join("ort");
    let cached = ort_root.join(ORT_VERSION).join(artifact.dylib_file);
    // 3. Previously downloaded.
    if cached.exists() {
        return match probe_loadable(&cached.to_string_lossy()) {
            Ok(()) => OrtRuntime::Local(cached),
            Err(e) => OrtRuntime::Unavailable(format!(
                "cached ONNX Runtime at {} is not loadable: {e}; delete it to re-download",
                cached.display()
            )),
        };
    }
    // 4/5. Fetch, or explain why not.
    if !download_enabled {
        return OrtRuntime::Unavailable(
            "no ONNX Runtime found and auto-download is disabled (--no-ort-download); \
             install ONNX Runtime, set ORT_DYLIB_PATH, or drop the flag"
                .into(),
        );
    }
    let url = artifact_url(artifact);
    if let Err(e) = std::fs::create_dir_all(&ort_root) {
        return OrtRuntime::Unavailable(format!("cannot create {}: {e}", ort_root.display()));
    }
    let tmp = match tempfile::Builder::new()
        .prefix("download-")
        .tempfile_in(&ort_root)
    {
        Ok(t) => t,
        Err(e) => return OrtRuntime::Unavailable(format!("temp file: {e}")),
    };
    if let Err(e) = fetch(&url, tmp.path()) {
        return OrtRuntime::Unavailable(format!("download of {url} failed: {e}"));
    }
    match install_from_archive(tmp.path(), artifact, &ort_root) {
        Ok(dylib) => match probe_loadable(&dylib.to_string_lossy()) {
            Ok(()) => OrtRuntime::Local(dylib),
            Err(e) => OrtRuntime::Unavailable(format!(
                "downloaded ONNX Runtime at {} failed the load probe: {e}",
                dylib.display()
            )),
        },
        Err(e) => OrtRuntime::Unavailable(e),
    }
}

/// Streaming download to `dest` over the ureq stack hf-hub already uses.
pub(crate) fn http_fetch(url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let mut reader = resp.into_body().into_reader();
    let mut out =
        std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    std::io::copy(&mut reader, &mut out).map_err(|e| format!("stream {url}: {e}"))?;
    Ok(())
}

pub(crate) fn sha256_hex(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Verify `archive` against `a.sha256`, extract just the dylib, and install
/// it atomically as `<ort_root>/<ORT_VERSION>/<a.dylib_file>`.
///
/// Staging happens in a fresh temp dir INSIDE `ort_root` (same filesystem,
/// so the final `rename` is atomic); a concurrent winner is adopted, never
/// clobbered. Nothing ever sits partially written at the final path.
pub(crate) fn install_from_archive(
    archive: &Path,
    a: &OrtArtifact,
    ort_root: &Path,
) -> Result<PathBuf, String> {
    let final_dir = ort_root.join(ORT_VERSION);
    let final_dylib = final_dir.join(a.dylib_file);
    if final_dylib.exists() {
        return Ok(final_dylib);
    }
    let actual = sha256_hex(archive)?;
    if actual != a.sha256 {
        return Err(format!(
            "checksum mismatch for {}: expected {}, got {actual} — refusing to extract \
             (either the pinned version was re-released or the download was corrupted/tampered)",
            a.archive_name, a.sha256
        ));
    }
    std::fs::create_dir_all(ort_root).map_err(|e| format!("mkdir {}: {e}", ort_root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix("staging-")
        .tempdir_in(ort_root)
        .map_err(|e| format!("staging dir in {}: {e}", ort_root.display()))?;
    extract_dylib(archive, a, staging.path().join(a.dylib_file).as_path())?;
    match std::fs::rename(staging.path(), &final_dir) {
        Ok(()) => {
            // Renamed away: forget the TempDir so its Drop doesn't try to
            // delete the now-installed directory.
            std::mem::forget(staging);
            Ok(final_dylib)
        }
        Err(_) if final_dylib.exists() => Ok(final_dylib), // concurrent winner
        Err(e) => Err(format!("install rename to {}: {e}", final_dir.display())),
    }
}

/// Whether `entry_path` (forward-slash-normalized) names the artifact's
/// `dylib_rel_path` as a whole path segment — either exactly, or as a
/// suffix following a `/` boundary. A plain `ends_with` would also match an
/// unrelated entry that merely shares the tail, e.g. `evillib/libonnxruntime...`
/// matching `lib/libonnxruntime...`.
fn matches_dylib_entry(entry_path: &str, dylib_rel_path: &str) -> bool {
    entry_path == dylib_rel_path || entry_path.ends_with(&format!("/{dylib_rel_path}"))
}

/// Extract the single entry whose path ends with `a.dylib_rel_path` to
/// `dest`. Archives lay content under a top-level
/// `onnxruntime-<platform>-<ver>/` directory, so we match on suffix.
fn extract_dylib(archive: &Path, a: &OrtArtifact, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(archive).map_err(|e| format!("open {}: {e}", archive.display()))?;
    if a.is_zip {
        let mut z =
            zip::ZipArchive::new(f).map_err(|e| format!("zip {}: {e}", archive.display()))?;
        for i in 0..z.len() {
            let mut entry = z.by_index(i).map_err(|e| e.to_string())?;
            if matches_dylib_entry(&entry.name().replace('\\', "/"), a.dylib_rel_path) {
                let mut out = std::fs::File::create(dest)
                    .map_err(|e| format!("create {}: {e}", dest.display()))?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
    } else {
        let gz = flate2::read::GzDecoder::new(f);
        let mut t = tar::Archive::new(gz);
        for entry in t.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?.into_owned();
            if matches_dylib_entry(&path.to_string_lossy(), a.dylib_rel_path) {
                let mut out = std::fs::File::create(dest)
                    .map_err(|e| format!("create {}: {e}", dest.display()))?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
    }
    Err(format!(
        "archive {} contains no entry ending in {}",
        archive.display(),
        a.dylib_rel_path
    ))
}

#[cfg(test)]
pub(crate) fn all_artifacts() -> [&'static OrtArtifact; 4] {
    [&MACOS_ARM64, &LINUX_X64, &LINUX_ARM64, &WIN_X64]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// Builds a tiny .tgz laid out like a real release archive:
    /// onnxruntime-fake-1.24.2/lib/<dylib_file> containing `content`.
    fn fixture_tgz(dir: &std::path::Path, dylib_file: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join("fixture.tgz");
        let f = std::fs::File::create(&path).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let inner = format!("onnxruntime-fake-{ORT_VERSION}/lib/{dylib_file}");
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, inner, content).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    fn fixture_artifact(sha256: &'static str) -> OrtArtifact {
        OrtArtifact {
            archive_name: "onnxruntime-fake-1.24.2.tgz",
            sha256,
            dylib_rel_path: "lib/libfake.dylib",
            dylib_file: "libfake.dylib",
            is_zip: false,
        }
    }

    #[test]
    fn install_verifies_extracts_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = fixture_tgz(dir.path(), "libfake.dylib", b"not really a dylib");
        let sum = sha256_hex(&archive).unwrap();
        // Leak the checksum string to satisfy the &'static table shape.
        let a = fixture_artifact(Box::leak(sum.into_boxed_str()));

        let ort_root = dir.path().join("ort");
        let installed = install_from_archive(&archive, &a, &ort_root).unwrap();
        assert_eq!(installed, ort_root.join(ORT_VERSION).join("libfake.dylib"));
        assert_eq!(std::fs::read(&installed).unwrap(), b"not really a dylib");
        // Second install over an existing dir: adopts, no error.
        let again = install_from_archive(&archive, &a, &ort_root).unwrap();
        assert_eq!(again, installed);
        // No staging debris left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&ort_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != std::ffi::OsStr::new(ORT_VERSION))
            .collect();
        assert!(leftovers.is_empty(), "staging debris: {leftovers:?}");
    }

    #[test]
    fn checksum_mismatch_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let archive = fixture_tgz(dir.path(), "libfake.dylib", b"bytes");
        let a =
            fixture_artifact("0000000000000000000000000000000000000000000000000000000000000000");
        let ort_root = dir.path().join("ort");
        let err = install_from_archive(&archive, &a, &ort_root).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(
            !ort_root.join(ORT_VERSION).exists(),
            "nothing may reach the final path on mismatch"
        );
    }

    #[test]
    fn zip_archives_extract_too() {
        // Exercises the is_zip branch on every platform (the zip crate's
        // writer is not Windows-only), so the Windows artifact path isn't
        // trusted to CI alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.zip");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file(format!("onnxruntime-fake-{ORT_VERSION}/lib/fake.dll"), opts)
            .unwrap();
        zw.write_all(b"zip dylib bytes").unwrap();
        zw.finish().unwrap();

        let sum = sha256_hex(&path).unwrap();
        let a = OrtArtifact {
            archive_name: "onnxruntime-fake-1.24.2.zip",
            sha256: Box::leak(sum.into_boxed_str()),
            dylib_rel_path: "lib/fake.dll",
            dylib_file: "fake.dll",
            is_zip: true,
        };
        let ort_root = dir.path().join("ort");
        let installed = install_from_archive(&path, &a, &ort_root).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"zip dylib bytes");
    }

    #[test]
    fn concurrent_installs_race_safely() {
        let dir = tempfile::tempdir().unwrap();
        let archive = fixture_tgz(dir.path(), "libfake.dylib", b"race");
        let sum = sha256_hex(&archive).unwrap();
        let a: &'static OrtArtifact =
            Box::leak(Box::new(fixture_artifact(Box::leak(sum.into_boxed_str()))));
        let ort_root = dir.path().join("ort");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let archive = archive.clone();
                let ort_root = ort_root.clone();
                std::thread::spawn(move || install_from_archive(&archive, a, &ort_root))
            })
            .collect();
        let paths: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        assert!(paths.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"race");
    }

    fn no_fetch(_url: &str, _dest: &std::path::Path) -> Result<(), String> {
        panic!("fetch must not be called in this scenario");
    }

    #[test]
    fn env_override_wins_and_never_downloads() {
        // ORT_DYLIB_PATH pointing at garbage: resolver must report
        // Unavailable (probe fails) WITHOUT attempting a download —
        // an explicit override is never second-guessed. Env-var tests are
        // process-global: set/remove inside, and don't parallel-panic —
        // cargo runs #[test]s in threads, so serialize via a lock.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ORT_DYLIB_PATH", "/definitely/not/a/real/dylib");
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(dir.path(), true, &no_fetch);
        std::env::remove_var("ORT_DYLIB_PATH");
        assert!(matches!(r, OrtRuntime::Unavailable(_)), "{r:?}");
    }

    #[test]
    fn cached_dylib_short_circuits_the_fetch() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ORT_DYLIB_PATH");
        let Some(a) = current_artifact() else {
            // Platform without a pinned artifact (e.g., Intel Mac): skip this test
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cached_dir = dir.path().join("ort").join(ORT_VERSION);
        std::fs::create_dir_all(&cached_dir).unwrap();
        // A file that EXISTS but is not loadable: the resolver must find
        // it, probe it, fail the probe, and land Unavailable — still
        // without fetching. On a host that has a SYSTEM ONNX Runtime the
        // resolver legitimately stops earlier at System (also without
        // fetching); accept both — `no_fetch`'s panic is the invariant.
        std::fs::write(cached_dir.join(a.dylib_file), b"not a dylib").unwrap();
        let r = resolve(dir.path(), true, &no_fetch);
        assert!(
            matches!(r, OrtRuntime::Unavailable(_) | OrtRuntime::System),
            "{r:?}"
        );
    }

    #[test]
    fn disabled_downloads_never_fetch() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ORT_DYLIB_PATH");
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(dir.path(), false, &no_fetch);
        // On a host WITH a system ORT this legitimately resolves System;
        // on a host without one it must be Unavailable and the message must
        // name the flag. Either way no_fetch proves no download attempt.
        match r {
            OrtRuntime::System => {}
            OrtRuntime::Unavailable(msg) => {
                if current_artifact().is_some() {
                    assert!(msg.contains("--no-ort-download"), "{msg}");
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn fetch_failure_degrades_with_reason() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ORT_DYLIB_PATH");
        let dir = tempfile::tempdir().unwrap();
        let called = std::sync::atomic::AtomicUsize::new(0);
        let failing = |_url: &str, _dest: &std::path::Path| -> Result<(), String> {
            called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("simulated network failure".into())
        };
        let r = resolve(dir.path(), true, &failing);
        match r {
            // Host with a system ORT: resolver stops at System, fetch not called.
            OrtRuntime::System => assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0),
            OrtRuntime::Unavailable(msg) => {
                assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert!(msg.contains("simulated network failure"), "{msg}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
