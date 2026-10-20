'use strict';
// The notices generator, run against synthetic Cargo metadata so no cargo, no
// network and no repository file is involved.
//
//   node --test scripts/test/*.test.js
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const script = path.resolve(__dirname, '..', 'sync-licenses.js');

function workspace(packages, resolve) {
  const own = { id: 'own 0.1.0', name: 'own', version: '0.1.0', license: 'GPL-3.0-or-later', source: null };
  return {
    workspace_members: [own.id],
    packages: [own, ...packages],
    resolve: { nodes: resolve ?? [own, ...packages].map((p) => ({ id: p.id })) },
  };
}

function crate(name, version, license, extra = {}) {
  return {
    id: `${name} ${version}`,
    name,
    version,
    license,
    source: 'registry+https://github.com/rust-lang/crates.io-index',
    ...extra,
  };
}

// Runs the generator in a fresh temporary directory.
function run(metadata, args, existingNotices) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'syntra-notices-'));
  const metadataFile = path.join(directory, 'metadata.json');
  const notices = path.join(directory, 'THIRD_PARTY_NOTICES.md');
  fs.writeFileSync(metadataFile, JSON.stringify(metadata));
  if (existingNotices !== undefined) fs.writeFileSync(notices, existingNotices);
  const result = spawnSync(process.execPath, [script, ...args], {
    encoding: 'utf8',
    env: { ...process.env, SYNTRA_CARGO_METADATA: metadataFile, SYNTRA_NOTICES_PATH: notices },
  });
  return {
    ...result,
    notices,
    written: fs.existsSync(notices) ? fs.readFileSync(notices, 'utf8') : undefined,
    cleanup: () => fs.rmSync(directory, { recursive: true, force: true }),
  };
}

test('writing lists third-party crates only, sorted, and not the workspace', () => {
  const run1 = run(workspace([crate('zeta', '1.0.0', 'MIT'), crate('alpha', '2.0.0', 'Apache-2.0')]), []);
  try {
    assert.equal(run1.status, 0, run1.stderr);
    assert.match(run1.written, /Third-party packages: \*\*2\*\*/);
    assert.ok(run1.written.indexOf('| alpha |') < run1.written.indexOf('| zeta |'));
    assert.ok(!run1.written.includes('| own |'));
  } finally {
    run1.cleanup();
  }
});

test('a local path package outside the workspace is third-party, not hidden', () => {
  const vendored = crate('vendored', '1.0.0', 'MIT', { source: null });
  const result = run(workspace([vendored]), []);
  try {
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.written, /\| vendored \| 1\.0\.0 \| MIT \| local \|/);
  } finally {
    result.cleanup();
  }
});

test('--check passes on current notices and never writes', () => {
  const metadata = workspace([crate('alpha', '1.0.0', 'MIT')]);
  const generated = run(metadata, []);
  try {
    const before = fs.statSync(generated.notices).mtimeMs;
    const content = generated.written;
    const checked = spawnSync(process.execPath, [script, '--check'], {
      encoding: 'utf8',
      env: {
        ...process.env,
        SYNTRA_CARGO_METADATA: path.join(path.dirname(generated.notices), 'metadata.json'),
        SYNTRA_NOTICES_PATH: generated.notices,
      },
    });
    assert.equal(checked.status, 0, checked.stderr);
    assert.equal(fs.readFileSync(generated.notices, 'utf8'), content);
    assert.equal(fs.statSync(generated.notices).mtimeMs, before);
  } finally {
    generated.cleanup();
  }
});

test('--check fails on stale notices and leaves them untouched', () => {
  const result = run(workspace([crate('alpha', '1.0.0', 'MIT')]), ['--check'], 'old contents\n');
  try {
    assert.equal(result.status, 1);
    assert.match(result.stderr, /out of date/);
    assert.equal(result.written, 'old contents\n', 'a check must not rewrite the file');
  } finally {
    result.cleanup();
  }
});

test('--check fails when the notices file is missing and does not create it', () => {
  const result = run(workspace([crate('alpha', '1.0.0', 'MIT')]), ['--check']);
  try {
    assert.equal(result.status, 1);
    assert.match(result.stderr, /is missing/);
    assert.equal(result.written, undefined);
  } finally {
    result.cleanup();
  }
});

test('a licence that cannot be satisfied fails the check even when notices are current', () => {
  const metadata = workspace([crate('copyleft', '1.0.0', '(MIT OR GPL-2.0-only) AND GPL-2.0-only')]);
  const generated = run(metadata, []);
  try {
    assert.equal(generated.status, 0, 'writing reports problems but still writes');
    const checked = spawnSync(process.execPath, [script, '--check'], {
      encoding: 'utf8',
      env: {
        ...process.env,
        SYNTRA_CARGO_METADATA: path.join(path.dirname(generated.notices), 'metadata.json'),
        SYNTRA_NOTICES_PATH: generated.notices,
      },
    });
    assert.equal(checked.status, 1);
    assert.match(checked.stderr, /copyleft@1\.0\.0/);
  } finally {
    generated.cleanup();
  }
});

test('a package with no licence is reported as UNKNOWN and fails the check', () => {
  const metadata = workspace([crate('mystery', '1.0.0', null)]);
  const generated = run(metadata, []);
  try {
    assert.match(generated.written, /\| mystery \| 1\.0\.0 \| UNKNOWN \|/);
    const checked = spawnSync(process.execPath, [script, '--check'], {
      encoding: 'utf8',
      env: {
        ...process.env,
        SYNTRA_CARGO_METADATA: path.join(path.dirname(generated.notices), 'metadata.json'),
        SYNTRA_NOTICES_PATH: generated.notices,
      },
    });
    assert.equal(checked.status, 1);
  } finally {
    generated.cleanup();
  }
});

test('--filter-platform checks coverage of the resolved graph, not equality', () => {
  const linux = crate('linux-only', '1.0.0', 'MIT');
  const windows = crate('windows-only', '1.0.0', 'MIT');
  const full = workspace([linux, windows]);
  const generated = run(full, []);
  try {
    // A one-target closure resolves only some packages, although `packages`
    // still lists both.
    const filtered = workspace([linux, windows], [{ id: 'own 0.1.0' }, { id: linux.id }]);
    const metadataFile = path.join(path.dirname(generated.notices), 'filtered.json');
    fs.writeFileSync(metadataFile, JSON.stringify(filtered));
    const env = { ...process.env, SYNTRA_CARGO_METADATA: metadataFile, SYNTRA_NOTICES_PATH: generated.notices };
    const covered = spawnSync(process.execPath, [script, '--check', '--filter-platform', 'x86_64-unknown-linux-gnu'], {
      encoding: 'utf8',
      env,
    });
    assert.equal(covered.status, 0, covered.stderr);

    // A resolved package the committed notices do not list is a failure.
    const stranger = crate('new-dependency', '1.0.0', 'MIT');
    fs.writeFileSync(
      metadataFile,
      JSON.stringify(workspace([linux, stranger], [{ id: 'own 0.1.0' }, { id: linux.id }, { id: stranger.id }])),
    );
    const uncovered = spawnSync(process.execPath, [script, '--check', '--filter-platform', 'x86_64-unknown-linux-gnu'], {
      encoding: 'utf8',
      env,
    });
    assert.equal(uncovered.status, 1);
    assert.match(uncovered.stderr, /new-dependency@1\.0\.0/);
  } finally {
    generated.cleanup();
  }
});

test('--filter-platform refuses to write, so a partial closure cannot become the notices', () => {
  const result = run(workspace([crate('alpha', '1.0.0', 'MIT')]), ['--filter-platform', 'x86_64-unknown-linux-gnu']);
  try {
    assert.equal(result.status, 2);
    assert.equal(result.written, undefined);
  } finally {
    result.cleanup();
  }
});
