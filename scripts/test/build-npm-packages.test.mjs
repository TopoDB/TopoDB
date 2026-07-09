import { test } from 'node:test';
import assert from 'node:assert';
import { execFileSync, execSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const KEYS = ['darwin-arm64', 'darwin-x64', 'linux-x64', 'linux-arm64', 'win32-x64'];
const binName = (key) => (key === 'win32-x64' ? 'topodb-mcp.exe' : 'topodb-mcp');

function stageFakeBinaries() {
  const dir = mkdtempSync(join(tmpdir(), 'topodb-bins-'));
  for (const key of KEYS) {
    mkdirSync(join(dir, key), { recursive: true });
    writeFileSync(join(dir, key, binName(key)), `#!fake ${key}\n`);
  }
  return dir;
}

test('generator produces a stamped main package and five os/cpu-scoped sub-packages', () => {
  const binaries = stageFakeBinaries();
  const out = mkdtempSync(join(tmpdir(), 'topodb-out-'));
  execFileSync('node', [
    'scripts/build-npm-packages.mjs',
    '--version', '1.2.3',
    '--binaries', binaries,
    '--out', out,
  ], { stdio: 'inherit' });

  // Main package: version + optionalDependency pins rewritten to 1.2.3.
  const main = JSON.parse(readFileSync(join(out, 'topodb-mcp', 'package.json'), 'utf8'));
  assert.equal(main.name, '@topodb/topodb-mcp');
  assert.equal(main.version, '1.2.3');
  for (const key of KEYS) {
    assert.equal(main.optionalDependencies[`@topodb/topodb-mcp-${key}`], '1.2.3');
  }
  assert.ok(existsSync(join(out, 'topodb-mcp', 'bin', 'topodb-mcp.js')));

  // Each sub-package: correct name/version/os/cpu + exactly its one binary.
  const osFor = { darwin: 'darwin', linux: 'linux', win32: 'win32' };
  const cpuFor = { arm64: 'arm64', x64: 'x64' };
  for (const key of KEYS) {
    const [plat, arch] = key.split('-');
    const pkgDir = join(out, `topodb-mcp-${key}`);
    const pkg = JSON.parse(readFileSync(join(pkgDir, 'package.json'), 'utf8'));
    assert.equal(pkg.name, `@topodb/topodb-mcp-${key}`);
    assert.equal(pkg.version, '1.2.3');
    assert.deepEqual(pkg.os, [osFor[plat]]);
    assert.deepEqual(pkg.cpu, [cpuFor[arch]]);
    assert.ok(existsSync(join(pkgDir, binName(key))), `${key} binary present`);
  }

  // npm does NOT auto-include LICENSE-MIT/-APACHE (glob matches license(.ext)? only),
  // so the generator must copy them into every package.
  for (const f of ['LICENSE-MIT', 'LICENSE-APACHE']) {
    assert.ok(existsSync(join(out, 'topodb-mcp', f)), `main ${f}`);
    assert.ok(existsSync(join(out, 'topodb-mcp-linux-x64', f)), `sub ${f}`);
  }
});

test('npm pack --dry-run bundles exactly one binary per sub-package', () => {
  const binaries = stageFakeBinaries();
  const out = mkdtempSync(join(tmpdir(), 'topodb-out-'));
  execFileSync('node', [
    'scripts/build-npm-packages.mjs',
    '--version', '1.2.3', '--binaries', binaries, '--out', out,
  ], { stdio: 'inherit' });

  const pkgDir = join(out, 'topodb-mcp-linux-x64');
  // npm is `npm.cmd` on Windows: execFileSync('npm.cmd') throws EINVAL
  // (CVE-2024-27980 hardening) and execFileSync+shell:true warns (DEP0190).
  // execSync takes a full command string and always uses a shell, resolving
  // npm cleanly on both platforms with pristine output.
  const json = execSync('npm pack --dry-run --json', { cwd: pkgDir, encoding: 'utf8' });
  const files = JSON.parse(json)[0].files.map((f) => f.path);
  assert.ok(files.includes('topodb-mcp'), 'binary is packed');
  assert.ok(files.includes('package.json'), 'manifest is packed');
  const binaries2 = files.filter((f) => f === 'topodb-mcp' || f === 'topodb-mcp.exe');
  assert.equal(binaries2.length, 1, 'exactly one binary');
  assert.ok(files.includes('LICENSE-MIT') && files.includes('LICENSE-APACHE'),
    'license files are packed into the sub-package');
});

test('generator does not mutate the committed main package source', () => {
  const src = 'npm/topodb-mcp/package.json';
  const before = readFileSync(src, 'utf8');
  const binaries = stageFakeBinaries();
  const out = mkdtempSync(join(tmpdir(), 'topodb-out-'));
  execFileSync('node', [
    'scripts/build-npm-packages.mjs',
    '--version', '9.9.9', '--binaries', binaries, '--out', out,
  ], { stdio: 'inherit' });
  const after = readFileSync(src, 'utf8');
  assert.equal(after, before,
    'committed npm/topodb-mcp/package.json must be untouched by a generate run');
});
