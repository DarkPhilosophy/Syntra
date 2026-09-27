#!/usr/bin/env node
// Regenerates .github/THIRD_PARTY_NOTICES.md from Cargo metadata: every
// third-party crate in the dependency graph with its declared licence, and
// a check that each licence is compatible with GPL-3.0-or-later.
// `--check` fails instead of writing, for CI.
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const root = path.resolve(__dirname, '..');
const out = path.join(root, '.github', 'THIRD_PARTY_NOTICES.md');
const check = process.argv.includes('--check');

const data = JSON.parse(
  execFileSync('cargo', ['metadata', '--format-version', '1'], {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  }),
);

// Licences that may be combined into a GPL-3.0-or-later program.
const compatible = new Set([
  'MIT', 'MIT-0', 'Apache-2.0', 'Apache-2.0 WITH LLVM-exception', 'BSD-2-Clause', 'BSD-3-Clause',
  'BSL-1.0', 'ISC', 'Zlib', 'Unlicense', 'CC0-1.0', '0BSD', 'Unicode-3.0', 'Unicode-DFS-2016',
  'MPL-2.0', 'LGPL-2.1-or-later', 'LGPL-3.0', 'LGPL-3.0-only', 'LGPL-3.0-or-later', 'GPL-3.0',
  'GPL-3.0-only', 'GPL-3.0-or-later', 'GPL-2.0-or-later', 'CDLA-Permissive-2.0', 'bzip2-1.0.6',
  'NCSA', 'OFL-1.1', 'Ubuntu-font-1.0', 'LicenseRef-Slint-Royalty-free-2.0',
  'LicenseRef-Slint-Software-3.0', 'LicenseRef-Slint-commercial',
]);

// An SPDX expression is acceptable when at least one OR-alternative consists
// only of compatible licences.
function acceptable(expression) {
  if (!expression) return false;
  const normalised = expression.replace(/\//g, ' OR ').replace(/[()]/g, ' ');
  return normalised.split(/\s+OR\s+/).some((alternative) =>
    alternative
      .split(/\s+AND\s+/)
      .map((term) => term.trim())
      .filter(Boolean)
      .every((term) => compatible.has(term)),
  );
}

const thirdParty = [...new Map(
  data.packages.filter((p) => p.source).map((p) => [`${p.name}@${p.version}`, p]),
).values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const problems = thirdParty.filter((p) => !acceptable(p.license));
const escape = (text) => String(text ?? '').replace(/\|/g, '\\|');
const repo = (p) => (p.repository ? `[source](${p.repository})` : '');

const lines = [
  '# Third-party notices',
  '',
  'Syntra is distributed under GPL-3.0-or-later. It is built on the open-source',
  'packages listed below, each of which remains under its own licence. The table',
  'is generated from Cargo metadata by `node .scripts/sync-licenses.js`; the full',
  'licence texts ship with each package\'s source distribution.',
  '',
  `Third-party packages: **${thirdParty.length}**.`,
  '',
  '| Package | Version | Licence | |',
  '|---|---|---|---|',
  ...thirdParty.map((p) => `| ${p.name} | ${p.version} | ${escape(p.license || p.license_file || 'UNKNOWN')} | ${repo(p)} |`),
  '',
];
const next = lines.join('\n');

if (problems.length) {
  console.error('Licences needing review (not recognised as GPL-3.0-compatible):');
  for (const p of problems) console.error(`  ${p.name}@${p.version}: ${p.license || p.license_file || 'UNKNOWN'}`);
}

if (check) {
  if (!fs.existsSync(out) || fs.readFileSync(out, 'utf8') !== next) {
    console.error('Third-party notices are out of date; run: node .scripts/sync-licenses.js');
    process.exit(1);
  }
  if (problems.length) process.exit(1);
  console.log(`Third-party notices are up to date (${thirdParty.length} packages).`);
} else {
  fs.writeFileSync(out, next);
  console.log(`Third-party notices updated (${thirdParty.length} packages, ${problems.length} to review).`);
}
