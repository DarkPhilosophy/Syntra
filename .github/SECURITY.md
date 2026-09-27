# Security policy

Syntra controls keyboard and mouse input and carries clipboard contents and
files between machines, so security reports are taken seriously.

## Supported versions

Only the latest release and the `main` branch receive fixes. Older releases
are not patched; please upgrade before reporting.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub:
[Report a vulnerability](https://github.com/DarkPhilosophy/Syntra/security/advisories/new).

Please include:

- the affected version or commit and platform;
- what an attacker can achieve, and under which conditions (same network,
  authorised peer, local user, …);
- steps or a proof of concept that reproduce it;
- any suggested fix.

Remove personal data, real addresses and credentials from the report.

## What to expect

This is a volunteer project without a guaranteed response time. Reports are
acknowledged and investigated on a best-effort basis; confirmed issues are
fixed in a new release and credited to the reporter unless anonymity is
requested. Please allow reasonable time for a fix before public disclosure.

## Scope

In scope: the service, the dashboard, the plugins and the network, IPC and
plugin protocols in this repository. Out of scope: vulnerabilities in
third-party dependencies (report those upstream), attacks that require an
already-authorised peer to act maliciously within its intended permissions,
and issues requiring physical access or root on the target machine.

Syntra is provided without warranty; see the [disclaimer](DISCLAIMER.md).
