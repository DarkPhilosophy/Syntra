'use strict';
// Evaluation of SPDX licence expressions against a set of acceptable licences.
//
//   expression := and ( 'OR' and )*
//   and        := term ( 'AND' term )*
//   term       := '(' expression ')' | id [ 'WITH' exception ]
//
// `AND` binds tighter than `OR`, and parentheses group. An expression is
// satisfied when the licensee may choose one `OR` alternative and must meet
// every `AND` term of it. That is why precedence matters: reading
// `(MIT OR GPL-2.0-only) AND GPL-2.0-only` as "MIT" would accept a package that
// also requires GPL-2.0-only.

// Licences that may be combined into a GPL-3.0-or-later program.
const compatible = new Set([
  'MIT', 'MIT-0', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause',
  'BSL-1.0', 'ISC', 'Zlib', 'Unlicense', 'CC0-1.0', '0BSD', 'Unicode-3.0', 'Unicode-DFS-2016',
  'MPL-2.0', 'LGPL-2.1-or-later', 'LGPL-3.0', 'LGPL-3.0-only', 'LGPL-3.0-or-later', 'GPL-3.0',
  'GPL-3.0-only', 'GPL-3.0-or-later', 'GPL-2.0-or-later', 'CDLA-Permissive-2.0', 'bzip2-1.0.6',
  'NCSA', 'OFL-1.1', 'Ubuntu-font-1.0', 'LicenseRef-Slint-Royalty-free-2.0',
  'LicenseRef-Slint-Software-3.0', 'LicenseRef-Slint-commercial',
]);

// Exceptions that only ever widen what a licence permits, so `X WITH <these>`
// is acceptable exactly when `X` is.
const permissiveExceptions = new Set(['LLVM-exception']);

const operators = new Set(['AND', 'OR', 'WITH']);

function tokenize(expression) {
  // Cargo metadata still carries the legacy `/` separator, meaning OR.
  const text = expression.replace(/\//g, ' OR ');
  const tokens = text.match(/\(|\)|[^\s()]+/g) || [];
  return tokens.map((token) => {
    const upper = token.toUpperCase();
    return operators.has(upper) ? upper : token;
  });
}

function parse(tokens) {
  let at = 0;
  const peek = () => tokens[at];
  const take = () => tokens[at++];

  function term() {
    const token = take();
    if (token === undefined) throw new Error('unexpected end of expression');
    if (token === '(') {
      const inner = orExpression();
      if (take() !== ')') throw new Error('missing closing parenthesis');
      return inner;
    }
    if (token === ')' || operators.has(token)) throw new Error(`unexpected "${token}"`);
    // Only an identifier can carry an exception. An operator, a parenthesis or
    // a second WITH after it is a malformed expression, never a licence.
    const node = { id: token };
    if (peek() === 'WITH') {
      take();
      const exception = take();
      if (exception === undefined || exception === '(' || exception === ')' || operators.has(exception)) {
        throw new Error('WITH must be followed by an exception identifier');
      }
      node.exception = exception;
    }
    return node;
  }

  function andExpression() {
    const terms = [term()];
    while (peek() === 'AND') {
      take();
      terms.push(term());
    }
    return terms.length === 1 ? terms[0] : { and: terms };
  }

  function orExpression() {
    const alternatives = [andExpression()];
    while (peek() === 'OR') {
      take();
      alternatives.push(andExpression());
    }
    return alternatives.length === 1 ? alternatives[0] : { or: alternatives };
  }

  const tree = orExpression();
  if (at !== tokens.length) throw new Error(`unexpected "${tokens[at]}"`);
  return tree;
}

function satisfied(node, allowed, exceptions) {
  if (node.or) return node.or.some((child) => satisfied(child, allowed, exceptions));
  if (node.and) return node.and.every((child) => satisfied(child, allowed, exceptions));
  if (node.exception !== undefined && !exceptions.has(node.exception)) return false;
  return allowed.has(node.id);
}

// Whether `expression` can be satisfied using only licences in `allowed`. An
// expression that cannot be read cannot be approved.
function acceptable(expression, allowed = compatible, exceptions = permissiveExceptions) {
  if (!expression) return false;
  try {
    return satisfied(parse(tokenize(expression)), allowed, exceptions);
  } catch {
    return false;
  }
}

module.exports = { acceptable, compatible, parse, tokenize };
