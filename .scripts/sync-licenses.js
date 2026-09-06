#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const root = path.resolve(__dirname, '..');
const out = path.join(root, '.github', 'THIRD_PARTY_NOTICES.md');
const check = process.argv.includes('--check');
const data = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1'], {cwd: root, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024}));
const packages = [...new Map(data.packages.map(p => [p.name, p])).values()].filter(p => !p.source).sort((a,b) => a.name.localeCompare(b.name));
const lines = ['# Third-party notices', '', 'Syntra is distributed under GPL-3.0-or-later. Workspace package metadata declares these licences:', '', '| Package | Licence |', '|---|---|', ...packages.map(p => `| ${p.name} | ${p.license || 'UNKNOWN'} |`), '', 'This table is generated from Cargo metadata. Third-party packages retain their own licence terms; consult their source distributions for full notices.', ''];
const next = lines.join('\n');
if (check) { if (fs.readFileSync(out, 'utf8') !== next) { console.error('Third-party notices are out of date.'); process.exit(1); } console.log('Third-party notices are up to date.'); }
else { fs.writeFileSync(out, next); console.log(`Updated third-party notices for ${packages.length} packages.`); }
