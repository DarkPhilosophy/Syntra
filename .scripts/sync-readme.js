#!/usr/bin/env node
// Regenerates the marker regions of .github/README.md from Cargo metadata.
// `--check` fails instead of writing, for CI.
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const root = path.resolve(__dirname, '..');
const readme = path.join(root, '.github', 'README.md');
const check = process.argv.includes('--check');

const cargo = JSON.parse(
  execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  }),
);
const members = new Set(cargo.workspace_members);
const packages = cargo.packages
  .filter((p) => members.has(p.id))
  .sort((a, b) => a.name.localeCompare(b.name));
const version = packages.find((p) => p.name === 'syntra-core')?.version ?? packages[0].version;

const relative = (manifest) => path.relative(path.join(root, '.github'), path.dirname(manifest)).split(path.sep).join('/');
const crates = packages
  .map((p) => `- [\`${p.name}\`](${relative(p.manifest_path)}) — ${p.description || 'Workspace crate.'}`)
  .join('\n');

const regions = {
  VERSION: `<p align="center"><b>Current version: ${version}</b></p>`,
  CRATES: crates,
};

const current = fs.readFileSync(readme, 'utf8');
let next = current;
for (const [name, body] of Object.entries(regions)) {
  const pattern = new RegExp(`<!-- ${name}-START -->[\\s\\S]*?<!-- ${name}-END -->`);
  if (!pattern.test(next)) {
    console.error(`README is missing the ${name} marker region`);
    process.exit(1);
  }
  next = next.replace(pattern, () => `<!-- ${name}-START -->\n${body}\n<!-- ${name}-END -->`);
}

if (check) {
  if (next !== current) {
    console.error('README is out of date; run: node .scripts/sync-readme.js');
    process.exit(1);
  }
  console.log('README marker regions are up to date.');
} else {
  fs.writeFileSync(readme, next);
  console.log(`README updated: ${packages.length} crates, version ${version}.`);
}
