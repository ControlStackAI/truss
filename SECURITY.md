# Security Policy

## Supported versions

Truss is under active development. Security fixes are applied to the latest
commit on the default branch; older tags are not maintained as separate support
lines.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private
vulnerability reporting for this repository:

<https://github.com/ControlStackAI/truss/security/advisories/new>

Include the affected revision, reproduction steps, impact, and any suggested
mitigation. You should receive an acknowledgment within seven days.

## Security boundary

Truss's default `NoSandbox` execution mode is not a security sandbox. Tool policy,
working-directory scoping, signed journals, and integrity verification do not
provide operating-system isolation for untrusted code.
