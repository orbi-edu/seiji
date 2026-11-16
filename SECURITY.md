# Security policy

## Supported versions

Only the latest release on the `main` branch gets security fixes.

## Report a vulnerability

Do not open a public issue for a security problem.

1. On the repository page, open **Security**, then **Report a vulnerability**. This opens a
   private report that only the maintainers can see.
2. Describe the problem, the affected component and the steps to reproduce it.
3. Do not include real tokens. If a token is part of the problem, rotate it first with
   `seiji user rotate-token`.

The maintainers answer each report. They tell you if they accept it, and they agree a
disclosure date with you.

## Scope

These problems are in scope:

- A way for the daemon account `seiji`, or a user with a token, to run code as root or as an
  account that is not a tenant account (not in `seiji-tenants`, an admin, or uid below 501).
- A way for a root helper (`seiji exec-as`, `seiji exec-drop`, `seiji kill-as`) to act on input
  that the daemon account controls, other than the target account and the command.
- A user who can see, drive, install on, release or delete another user's device, build or
  workspace.
- A user who can hold devices beyond their quota, or use devices without the time being billed,
  except a failed boot.
- A token that appears in a log, a command line, a tool result or the database in clear text.
- An MCP or HTTP route that accepts a request without a valid token, other than `/healthz`.
- Seiji that deletes or shuts down a device or a file that it did not create.

These are known limits, not vulnerabilities. [docs/security.md](docs/security.md) explains them:

- In `shared` isolation, Maestro flows run as the daemon account and can read its files.
- Android emulators share one adb server, so `adb devices` lists all users' emulators.

The security model is in [docs/security.md](docs/security.md).
