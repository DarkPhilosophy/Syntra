#!/usr/bin/env node
// Regenerates licenses/THIRD_PARTY_NOTICES.md from Cargo metadata: every
// third-party crate in the dependency graph with its declared licence, and a
// check that each licence expression can be satisfied with licences compatible
// with GPL-3.0-or-later.
//
//   node scripts/sync-licenses.js            write the notices
//   node scripts/sync-licenses.js --check    fail instead of writing (CI)
//   --offline                                forwarded to cargo metadata
//   --filter-platform <triple>               restrict the closure to one target;
//                                            --check only, because the committed
//                                            notices cover every platform
//
// Metadata is always resolved with `--locked`, so the result depends only on
// Cargo.lock and never silently updates it.
//
// For tests, SYNTRA_CARGO_METADATA may name a JSON file that is read instead of
// running cargo, and SYNTRA_NOTICES_PATH may name the notices file, so the
// generator can be exercised on synthetic data without touching the repository.
'use strict';
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const { acceptable } = require('./lib/spdx');

const root = path.resolve(__dirname, '..');
const out = process.env.SYNTRA_NOTICES_PATH
  ? path.resolve(process.env.SYNTRA_NOTICES_PATH)
  : path.join(root, 'licenses', 'THIRD_PARTY_NOTICES.md');
const args = process.argv.slice(2);
const flags = new Set(args);
const check = flags.has('--check');
const platformAt = args.indexOf('--filter-platform');
const platform = platformAt >= 0 ? args[platformAt + 1] : undefined;
if (platformAt >= 0 && !platform) {
  console.error('--filter-platform needs a target triple');
  process.exit(2);
}
if (platform && !check) {
  console.error('--filter-platform is for --check only: it would write notices missing the other platforms');
  process.exit(2);
}

function loadMetadata() {
  if (process.env.SYNTRA_CARGO_METADATA) {
    return JSON.parse(fs.readFileSync(process.env.SYNTRA_CARGO_METADATA, 'utf8'));
  }
  const cargoArgs = ['metadata', '--format-version', '1', '--locked'];
  if (flags.has('--offline')) cargoArgs.push('--offline');
  if (platform) cargoArgs.push('--filter-platform', platform);
  return JSON.parse(
    execFileSync('cargo', cargoArgs, {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
    }),
  );
}
const data = loadMetadata();

// Workspace members are this project's own code and are not listed. Every
// other package is third-party: registry and git crates (`source` set) and
// also any path package outside the workspace, such as a vendored copy, which
// has no `source` but is not ours either.
const members = new Set(data.workspace_members);
const thirdParty = [...new Map(
  data.packages
    .filter((p) => !members.has(p.id))
    .map((p) => [`${p.name}@${p.version}`, p]),
).values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const licenceOf = (p) => p.license || p.license_file || 'UNKNOWN';
const problems = thirdParty.filter((p) => !acceptable(p.license));
const escape = (text) => String(text ?? '').replace(/\|/g, '\\|');
const origin = (p) => {
  if (p.repository) return `[source](${p.repository})`;
  return p.source ? '' : 'local';
};

const lines = [
  '# Third-party notices',
  '',
  'Syntra is distributed under GPL-3.0-or-later. It is built on the open-source',
  'packages listed below, each of which remains under its own licence. The table',
  'is generated from Cargo metadata by `node scripts/sync-licenses.js`; the full',
  'licence texts ship with each package\'s source distribution.',
  '',
  `Third-party packages: **${thirdParty.length}**.`,
  '',
  '| Package | Version | Licence | |',
  '|---|---|---|---|',
  ...thirdParty.map((p) => `| ${p.name} | ${p.version} | ${escape(licenceOf(p))} | ${origin(p)} |`),
  '',
];
const next = lines.join('\n');

// A filtered run judges only the packages it resolved (below); reporting every
// platform's problems there would blame crates that were not selected.
if (!platform && problems.length) {
  console.error('Licences needing review (not recognised as GPL-3.0-compatible):');
  for (const p of problems) console.error(`  ${p.name}@${p.version}: ${licenceOf(p)}`);
}

// What a filtered run may claim. `cargo metadata --filter-platform` narrows the
// *resolve graph*, but `packages` can still list crates that belong only to
// other targets, so the selection is taken from `resolve.nodes`, which is the
// graph that was actually resolved.
function resolvedPackages() {
  const ids = new Set((data.resolve?.nodes ?? []).map((node) => node.id));
  return thirdParty.filter((p) => ids.has(p.id));
}

function committedRows() {
  const rows = new Map();
  for (const line of fs.readFileSync(out, 'utf8').split('\n')) {
    // `| name | version | licence | source |`; a `|` inside a field is escaped.
    const cells = line.split(/(?<!\\)\|/).map((cell) => cell.trim().replace(/\\\|/g, '|'));
    if (cells.length >= 5 && cells[0] === '' && /^\d/.test(cells[2])) {
      rows.set(`${cells[1]}@${cells[2]}`, cells[3]);
    }
  }
  return rows;
}

const shown = (file) => path.relative(root, file) || file;

if (platform) {
  // Coverage validation, not equality: the committed notices span every
  // platform, so a one-target closure is checked to be a subset of them, with
  // the same version and licence for each package, and to pass the policy.
  if (!fs.existsSync(out)) {
    console.error(`${shown(out)} is missing; run: node scripts/sync-licenses.js`);
    process.exit(1);
  }
  const committed = committedRows();
  const selected = resolvedPackages();
  const missing = selected.filter((p) => committed.get(`${p.name}@${p.version}`) !== licenceOf(p));
  const failed = selected.filter((p) => !acceptable(p.license));
  for (const p of missing) {
    console.error(`  not covered by the committed notices: ${p.name}@${p.version} (${p.license || 'UNKNOWN'})`);
  }
  for (const p of failed) {
    console.error(`  licence needs review: ${p.name}@${p.version}: ${licenceOf(p)}`);
  }
  if (missing.length || failed.length) process.exit(1);
  console.log(`Notices cover all ${selected.length} packages resolved for ${platform} (coverage check, not a full-output comparison).`);
} else if (check) {
  if (!fs.existsSync(out)) {
    console.error(`${shown(out)} is missing; run: node scripts/sync-licenses.js`);
    process.exit(1);
  }
  if (fs.readFileSync(out, 'utf8') !== next) {
    console.error('Third-party notices are out of date; run: node scripts/sync-licenses.js');
    process.exit(1);
  }
  if (problems.length) process.exit(1);
  console.log(`Third-party notices are up to date (${thirdParty.length} packages).`);
} else {
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.writeFileSync(out, next);
  console.log(`Third-party notices updated (${thirdParty.length} packages, ${problems.length} to review).`);
}
