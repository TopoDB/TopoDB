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
  const p = resolveBinary('linux', 'x64', fakeResolve);
  assert.equal(calls[0], '@topodb/topodb-mcp-linux-x64/topodb-mcp');
  assert.equal(p, '/abs/@topodb/topodb-mcp-linux-x64/topodb-mcp');

  const w = resolveBinary('win32', 'x64', fakeResolve);
  assert.equal(calls[1], '@topodb/topodb-mcp-win32-x64/topodb-mcp.exe');
  assert.equal(w, '/abs/@topodb/topodb-mcp-win32-x64/topodb-mcp.exe');
});
