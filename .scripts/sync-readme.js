#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const root = path.resolve(__dirname, '..');
const readme = path.join(root, '.github', 'README.md');
const check = process.argv.includes('--check');
function region(name, body) {
  const re = new RegExp(`<!-- ${name}-START -->[\\s\\S]*?<!-- ${name}-END -->`);
  return `<!-- ${name}-START -->\\n${body.trim()}\\n<!-- ${name}-END -->`;
}
const cargo = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1'], {cwd: root, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024}));
const workspace = new Set(cargo.workspace_members);
const packages = cargo.packages.filter(p => workspace.has(p.id)).sort((a,b) => a.name.localeCompare(b.name));
const version = cargo.resolve && cargo.packages.find(p => p.name === 'syntra-core')?.version || '0.11.0';
const crates = packages.map(p => `- [${p.name}](../${p.manifest_path.replace(root + '/', '').replace('/Cargo.toml','')}) — ${p.description || 'Workspace crate.'}`).join('\n');
let text = fs.readFileSync(readme, 'utf8');
text = text.replace(/<!-- VERSION-START -->[\\s\\S]*?<!-- VERSION-END -->/, region('VERSION', `Current workspace version: **${version}**.`));
text = text.replace(/<!-- CRATES-START -->[\\s\\S]*?<!-- CRATES-END -->/, region('CRATES', crates));
if (check) {
  const current = fs.readFileSync(readme, 'utf8');
  if (current !== text) { console.error('README is out of date; run node .scripts/sync-readme.js'); process.exit(1); }
  console.log('README marker regions are up to date.');
} else {
  fs.writeFileSync(readme, text);
  console.log(`Updated README marker regions for ${packages.length} workspace crates (version ${version}).`);
}
