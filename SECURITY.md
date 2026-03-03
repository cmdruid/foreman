# Security Policy

## Supported Versions

`foreman` is in active development (alpha). Security fixes and hardening
are targeted first at:

- the latest `main` branch state
- the currently published/latest release

## Reporting a Vulnerability

If you discover a security issue, please report it privately and do **not**
open a public issue before we have a chance to investigate.

Preferred reporting methods:

- GitHub Security Advisories on this repository (if enabled)
- Private direct report to the maintainers if advisories are unavailable

Please include:

- affected version(s)
- reproduction steps
- impact assessment
- proof-of-concept details (if available)
- any relevant logs or traces

## Response and Disclosure

- We acknowledge initial reports as soon as we can.
- We will triage, fix, and publish a patch in the latest release path.
- We aim for coordinated disclosure and will credit reporters unless requested
  otherwise.

## Security Best Practices

- Keep a dedicated secret store for token and webhook secrets.
- Restrict network egress for worker processes unless callbacks require it.
- Restrict filesystem access to the foreman Unix socket path in non-local environments.
- Use local process ownership and user permissions for Codex execution.
- Enable authentication in the service config (`--service-config`) for any non-local deployment.
