#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const { readFileSync } = require('node:fs');

// This launcher's own version. `optionalDependencies` pins every platform
// package to exactly this string, so it is also the version the platform
// package we resolve MUST report. See `resolveBinary`.
const OWN_VERSION = require('../package.json').version;

// Reads the version of an installed platform package, resolved the same way its
// binary is. Injectable so tests need not stage real sub-packages on disk.
function installedVersion(subPkg, resolve) {
  return JSON.parse(readFileSync(resolve(`${subPkg}/package.json`), 'utf8')).version;
}

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
//
// A SUCCESSFUL RESOLVE IS NOT PROOF WE FOUND OUR OWN BINARY, and assuming it was
// shipped a server two format generations old to a real user. `require.resolve`
// does not stop at this package's own `node_modules`: it WALKS UP the directory
// tree. On a Windows host where npm had installed the wrong platform package
// (`topodb-mcp-linux-x64`), `topodb-mcp-win32-x64` was absent from the plugin's
// data dir — so the walk-up carried on and found a stale
// `topodb-mcp-win32-x64@0.0.3` lying elsewhere on the machine. It resolved
// cleanly. The MODULE_NOT_FOUND branch below — the loud, actionable error whose
// entire job is this situation — therefore never ran, and a 0.0.3 server was
// launched while every version check in the stack read 0.0.7.
//
// `optionalDependencies` pins each platform package to this launcher's EXACT
// version, so "the resolved package reports a different version" means, with no
// ambiguity, that we escaped our own install. Checking it turns a silent
// wrong-binary execution into the error that was always supposed to fire.
function resolveBinary(platform, arch, resolve = require.resolve, deps = {}) {
  const { ownVersion = OWN_VERSION, versionOf = installedVersion } = deps;
  const key = platformKey(platform, arch);
  if (!key) {
    throw new Error(
      `topodb-mcp: unsupported platform ${platform}/${arch}. ` +
        `Supported: darwin-arm64, darwin-x64, linux-x64, linux-arm64, win32-x64. ` +
        `Install from source instead: cargo install topodb-mcp`,
    );
  }
  const subPkg = subPackageName(key);
  const specifier = `${subPkg}/${binaryFileName(platform)}`;

  let binPath;
  try {
    binPath = resolve(specifier);
  } catch (err) {
    if (err && err.code === 'MODULE_NOT_FOUND') {
      throw new Error(
        `topodb-mcp: the prebuilt binary package ${subPkg} is not installed. ` +
          `The platform-specific optional dependency was likely skipped (e.g. --no-optional, a ` +
          `registry error, or an npm install that ran on a different platform than this one). ` +
          `Reinstall the package, or build from source: cargo install topodb-mcp`,
      );
    }
    throw err;
  }

  let found;
  try {
    found = versionOf(subPkg, resolve);
  } catch {
    // The binary resolved but its package.json did not. That is not a layout we
    // ship, and we cannot establish the version — so we cannot establish that
    // this binary is ours. Refuse rather than run it.
    throw new Error(
      `topodb-mcp: resolved a binary at ${binPath} but could not read ${subPkg}'s package.json to ` +
        `confirm it belongs to this install (expected version ${ownVersion}). Refusing to launch an ` +
        `unverifiable binary. Reinstall @topodb/topodb-mcp, or build from source: cargo install topodb-mcp`,
    );
  }

  if (found !== ownVersion) {
    throw new Error(
      `topodb-mcp: refusing to launch a foreign binary. Resolved ${subPkg} version ${found} at ` +
        `${binPath}, but this launcher is version ${ownVersion} and pins that package to its own ` +
        `version exactly. This means Node resolved a copy OUTSIDE this install (require.resolve ` +
        `walks up the directory tree), almost certainly because ${subPkg} is missing here — check ` +
        `whether npm installed a different platform's package. Delete the stale copy at the path ` +
        `above, or reinstall @topodb/topodb-mcp so its own ${subPkg}@${ownVersion} is present.`,
    );
  }

  return binPath;
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
