#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');

// process.platform + process.arch -> our platform key, or null if unsupported.
// Pure function, exported for unit testing.
function platformKey(platform, arch) {
  const table = {
    'darwin arm64': 'darwin-arm64',
    'darwin x64': 'darwin-x64',
    'linux x64': 'linux-x64',
    'linux arm64': 'linux-arm64',
    'win32 x64': 'win32-x64',
  };
  return table[`${platform} ${arch}`] || null;
}

function subPackageName(key) {
  return `@topodb/topodb-mcp-${key}`;
}

function binaryFileName(platform) {
  return platform === 'win32' ? 'topodb-mcp.exe' : 'topodb-mcp';
}

// Resolve the absolute path of the platform binary, or throw a clear error.
// `resolve` is injectable so tests need not stage real sub-packages.
function resolveBinary(platform, arch, resolve = require.resolve) {
  const key = platformKey(platform, arch);
  if (!key) {
    throw new Error(
      `topodb-mcp: unsupported platform ${platform}/${arch}. ` +
        `Supported: darwin-arm64, darwin-x64, linux-x64, linux-arm64, win32-x64. ` +
        `Install from source instead: cargo install topodb-mcp`,
    );
  }
  const specifier = `${subPackageName(key)}/${binaryFileName(platform)}`;
  try {
    return resolve(specifier);
  } catch (err) {
    if (err && err.code === 'MODULE_NOT_FOUND') {
      throw new Error(
        `topodb-mcp: the prebuilt binary package ${subPackageName(key)} is not installed. ` +
          `The platform-specific optional dependency was likely skipped (e.g. --no-optional, a ` +
          `registry error, or a corrupted cache). Reinstall the package, or build from source: ` +
          `cargo install topodb-mcp`,
      );
    }
    throw err;
  }
}

function main() {
  let binPath;
  try {
    binPath = resolveBinary(process.platform, process.arch);
  } catch (err) {
    process.stderr.write(`${err.message}\n`);
    process.exit(1);
    return;
  }
  // stdio: 'inherit' hands the child the real stdin/stdout/stderr fds. Mandatory:
  // topodb-mcp speaks newline-delimited JSON-RPC on stdout, so the launcher must
  // never touch those streams. All args are forwarded verbatim.
  const result = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
  if (result.error) {
    process.stderr.write(`topodb-mcp: failed to launch binary: ${result.error.message}\n`);
    process.exit(1);
    return;
  }
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }
  process.exit(result.status === null ? 1 : result.status);
}

module.exports = { platformKey, subPackageName, binaryFileName, resolveBinary, main };

if (require.main === module) {
  main();
}
