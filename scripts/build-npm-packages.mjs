#!/usr/bin/env node
// Generates the @topodb/topodb-mcp npm packages from prebuilt binaries.
// Usage: node scripts/build-npm-packages.mjs --version <v> --binaries <dir> --out <dir> [--name <pkg>] [--bin <binary>]
//   <binaries-dir>/<key>/<binary>  for each of the 5 platform keys.
import { mkdirSync, cpSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const KEYS = ['darwin-arm64', 'darwin-x64', 'linux-x64', 'linux-arm64', 'win32-x64'];
const OS_FOR = { darwin: 'darwin', linux: 'linux', win32: 'win32' };
const CPU_FOR = { arm64: 'arm64', x64: 'x64' };
const binName = (key, bin) => (key === 'win32-x64' ? `${bin}.exe` : bin);
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const LICENSES = ['LICENSE-MIT', 'LICENSE-APACHE'];
function copyLicenses(dstDir) {
  for (const f of LICENSES) cpSync(join(repoRoot, f), join(dstDir, f));
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    const flag = argv[i].replace(/^--/, '');
    out[flag] = argv[i + 1];
  }
  for (const req of ['version', 'binaries', 'out']) {
    if (!out[req]) throw new Error(`missing required --${req}`);
  }
  out.name = out.name ?? 'topodb-mcp';
  out.bin = out.bin ?? out.name;
  return out;
}

function buildMainPackage(name, version, outDir) {
  const src = join(repoRoot, 'npm', name);
  const dst = join(outDir, name);
  rmSync(dst, { recursive: true, force: true });
  mkdirSync(dst, { recursive: true });
  cpSync(join(src, 'bin'), join(dst, 'bin'), { recursive: true });
  cpSync(join(src, 'README.md'), join(dst, 'README.md'));
  copyLicenses(dst);
  const pkg = JSON.parse(readFileSync(join(src, 'package.json'), 'utf8'));
  pkg.version = version;
  for (const key of KEYS) pkg.optionalDependencies[`@topodb/${name}-${key}`] = version;
  writeFileSync(join(dst, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
}

function buildSubPackage(name, bin, key, version, binariesDir, outDir) {
  const [plat, arch] = key.split('-');
  const dst = join(outDir, `${name}-${key}`);
  rmSync(dst, { recursive: true, force: true });
  mkdirSync(dst, { recursive: true });
  cpSync(join(binariesDir, key, binName(key, bin)), join(dst, binName(key, bin)));
  copyLicenses(dst);
  const pkg = {
    name: `@topodb/${name}-${key}`,
    version,
    description: `Prebuilt ${bin} binary for ${key}.`,
    license: 'MIT OR Apache-2.0',
    repository: { type: 'git', url: 'https://github.com/TopoDB/TopoDB' },
    os: [OS_FOR[plat]],
    cpu: [CPU_FOR[arch]],
    files: [binName(key, bin), ...LICENSES],
  };
  writeFileSync(join(dst, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
}

function main() {
  const { version, binaries, out, name, bin } = parseArgs(process.argv.slice(2));
  mkdirSync(out, { recursive: true });
  buildMainPackage(name, version, out);
  for (const key of KEYS) buildSubPackage(name, bin, key, version, binaries, out);
  console.error(`built @topodb/${name}@${version} + ${KEYS.length} sub-packages -> ${out}`);
}

main();
