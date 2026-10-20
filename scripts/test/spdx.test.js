'use strict';
// Behaviour of the licence-expression evaluator: what is accepted for a
// GPL-3.0-or-later project, and, as important, what is not.
//
//   node --test scripts/test/*.test.js
const test = require('node:test');
const assert = require('node:assert/strict');
const { acceptable } = require('../lib/spdx');

const accepted = (expression) => assert.equal(acceptable(expression), true, `should accept: ${expression}`);
const refused = (expression) => assert.equal(acceptable(expression), false, `should refuse: ${expression}`);

test('a single compatible licence is accepted and an unknown one is not', () => {
  accepted('MIT');
  accepted('Apache-2.0');
  accepted('GPL-3.0-or-later');
  refused('GPL-2.0-only');
  refused('Proprietary-Thing');
  refused('');
  refused(undefined);
  refused(null);
});

test('OR is satisfied by one acceptable alternative', () => {
  accepted('MIT OR GPL-2.0-only');
  accepted('GPL-2.0-only OR Apache-2.0');
  accepted('MIT/Apache-2.0'); // the legacy separator means OR
  refused('GPL-2.0-only OR AGPL-3.0-only');
});

test('AND needs every term to be acceptable', () => {
  accepted('MIT AND Apache-2.0');
  refused('MIT AND GPL-2.0-only');
  refused('GPL-2.0-only AND MIT');
});

test('AND binds tighter than OR', () => {
  // MIT, or (Apache-2.0 and a refused licence): the first alternative suffices.
  accepted('MIT OR Apache-2.0 AND GPL-2.0-only');
  // (GPL-2.0-only and MIT), or Apache-2.0: the second alternative suffices.
  accepted('GPL-2.0-only AND MIT OR Apache-2.0');
  // Every alternative contains a refused licence.
  refused('GPL-2.0-only AND MIT OR AGPL-3.0-only AND Apache-2.0');
});

// The case that a parser which only strips parentheses and splits on OR/AND
// gets wrong: reading it as "MIT OR ..." would approve a package that also
// requires GPL-2.0-only.
test('parentheses group: a required licence is not escaped by an inner OR', () => {
  refused('(MIT OR GPL-2.0-only) AND GPL-2.0-only');
  accepted('(MIT OR GPL-2.0-only) AND Apache-2.0');
  accepted('MIT AND (Apache-2.0 OR GPL-2.0-only)');
  refused('MIT AND (GPL-2.0-only OR AGPL-3.0-only)');
  accepted('((MIT))');
});

test('WITH takes an exception only from the permissive list', () => {
  accepted('Apache-2.0 WITH LLVM-exception');
  accepted('(Apache-2.0 WITH LLVM-exception) OR MIT');
  refused('Apache-2.0 WITH Some-Other-exception');
  // An exception never turns a refused licence into an accepted one.
  refused('GPL-2.0-only WITH LLVM-exception');
});

test('malformed expressions are refused, never approved', () => {
  for (const expression of [
    'MIT AND',
    'AND MIT',
    'MIT OR',
    'OR MIT',
    'MIT MIT',
    '(MIT',
    'MIT)',
    '()',
    'MIT WITH',
    'MIT WITH AND',
    'MIT WITH OR Apache-2.0',
    'MIT WITH (Apache-2.0)',
    'MIT WITH WITH LLVM-exception',
    'WITH MIT',
    'MIT AND OR Apache-2.0',
  ]) {
    refused(expression);
  }
});

test('operator keywords are matched case-insensitively and licence ids are not', () => {
  accepted('mit'.toUpperCase());
  accepted('MIT or Apache-2.0');
  refused('mit'); // identifiers are case-sensitive in the allow list
});
