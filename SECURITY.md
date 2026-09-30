# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |
| 0.1.x   | :x:                |

## Reporting a Vulnerability

Please report security vulnerabilities by one of the following methods:

- **Private vulnerability reporting**: Use the "Security" tab on the repository
  (private vulnerability reporting) or file a GitHub Security Advisory.

Include:
- Description of the vulnerability.
- Steps to reproduce.
- Affected versions.
- Suggested fix if available.

We will acknowledge receipt within 72 hours and provide a detailed response within 7 days.

**Please do NOT open public GitHub issues for security vulnerabilities.**

## Security Practices

- We aim to have smart contracts formally verified where feasible.
- External audits are planned before any major release; none have been
  performed yet.
- Dependencies are audited automatically via CI using `cargo audit`.
- Critical and high severity vulnerabilities are patched within 14 days.
- Low/medium issues are scheduled for the next minor release.

## Disclosure Policy

- We follow coordinated disclosure.
- Fixes are released as patch versions.
- Public disclosure occurs after a fix is available.

Contact: via the repository's Security tab (GitHub private vulnerability reporting).
