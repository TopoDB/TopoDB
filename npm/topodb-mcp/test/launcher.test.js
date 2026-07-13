'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const {
  platformKey,
  subPackageName,
  binaryFileName,
  resolveBinary,
} = require('../bin/topodb-mcp.js');

test('platformKey maps every supported platform', () => {
  assert.equal(platformKey('darwin', 'arm64'), 'darwin-arm64');
  assert.equal(platformKey('darwin', 'x64'), 'darwin-x64');
  assert.equal(platformKey('linux', 'x64'), 'linux-x64');
  assert.equal(platformKey('linux', 'arm64'), 'linux-arm64');
  assert.equal(platformKey('win32', 'x64'), 'win32-x64');
});

test('platformKey returns null for unsupported combos', () => {
  assert.equal(platformKey('freebsd', 'x64'), null);
  assert.equal(platformKey('linux', 'ia32'), null);
  assert.equal(platformKey('win32', 'arm64'), null);
});

test('binaryFileName is .exe only on win32', () => {
  assert.equal(binaryFileName('win32'), 'topodb-mcp.exe');
  assert.equal(binaryFileName('linux'), 'topodb-mcp');
  assert.equal(binaryFileName('darwin'), 'topodb-mcp');
});

test('subPackageName scopes under @topodb', () => {
  assert.equal(subPackageName('linux-x64'), '@topodb/topodb-mcp-linux-x64');
});

test('resolveBinary throws a clear, actionable error on unsupported platform', () => {
  assert.throws(
    () => resolveBinary('freebsd', 'x64', () => 'unused'),
    /unsupported platform freebsd\/x64/,
  );
});

test('resolveBinary requests the correct specifier and returns the resolved path', () => {
  const calls = [];
  const fakeResolve = (spec) => {
    calls.push(spec);
    return '/abs/' + spec;
  };
  // A matching version, so this test stays about the SPECIFIER. The
  // version-mismatch behaviour has its own tests below.
  const ok = { ownVersion: '9.9.9', versionOf: () => '9.9.9' };

  const p = resolveBinary('linux', 'x64', fakeResolve, ok);
  assert.equal(calls[0], '@topodb/topodb-mcp-linux-x64/topodb-mcp');
  assert.equal(p, '/abs/@topodb/topodb-mcp-linux-x64/topodb-mcp');

  const w = resolveBinary('win32', 'x64', fakeResolve, ok);
  assert.equal(calls[1], '@topodb/topodb-mcp-win32-x64/topodb-mcp.exe');
  assert.equal(w, '/abs/@topodb/topodb-mcp-win32-x64/topodb-mcp.exe');
});

test('resolveBinary gives an actionable error when the platform package is not installed', () => {
  const missing = () => { const e = new Error('Cannot find module x'); e.code = 'MODULE_NOT_FOUND'; throw e; };
  assert.throws(
    () => resolveBinary('linux', 'x64', missing),
    /prebuilt binary package @topodb\/topodb-mcp-linux-x64 is not installed/,
  );
});

// --- the ghost-binary bug ------------------------------------------------
//
// A real Windows install ran a 0.0.3 SERVER while every version check in the
// stack said 0.0.7. `npm` had installed the wrong platform package
// (topodb-mcp-linux-x64 on a win32 host), so topodb-mcp-win32-x64 was absent
// from the plugin's data dir -- and `require.resolve` does not stop there. It
// WALKED UP the directory tree, found a stale topodb-mcp-win32-x64@0.0.3 left
// somewhere else on that machine, and resolved successfully. Because it
// succeeded, the MODULE_NOT_FOUND path above -- the loud, actionable error --
// never fired, and a server two format generations old was executed silently.
//
// Resolution succeeding is therefore NOT proof we resolved OUR OWN binary. The
// invariant that makes it proof: `optionalDependencies` pins each platform
// package to this launcher's EXACT version, so a version that differs means we
// escaped our own install and must refuse.
test('resolveBinary refuses a platform package whose version is not the launcher\'s own', () => {
  const resolve = (spec) => '/ghost/node_modules/' + spec;
  const versionOf = () => '0.0.3'; // the stale package the tree-walk found
  assert.throws(
    () => resolveBinary('win32', 'x64', resolve, { ownVersion: '0.0.7', versionOf }),
    /0\.0\.3[\s\S]*0\.0\.7|0\.0\.7[\s\S]*0\.0\.3/,
    'must name both the version it found and the version it expected',
  );
  assert.throws(
    () => resolveBinary('win32', 'x64', resolve, { ownVersion: '0.0.7', versionOf }),
    /\/ghost\//,
    'must name WHERE the foreign binary was resolved from -- that path is the whole diagnosis',
  );
});

test('resolveBinary accepts the platform package when its version matches the launcher', () => {
  const resolve = (spec) => '/ok/node_modules/' + spec;
  const versionOf = () => '0.0.7';
  assert.equal(
    resolveBinary('win32', 'x64', resolve, { ownVersion: '0.0.7', versionOf }),
    '/ok/node_modules/@topodb/topodb-mcp-win32-x64/topodb-mcp.exe',
  );
});
