# Contributing to Seiji

Thank you for your help. This file tells you how to make a change that the maintainers can
accept.

## Before you start

1. Read [AGENTS.md](AGENTS.md). Its rules apply to people and to AI agents.
2. For a large change, open an issue first. Describe the problem and the change that you
   propose. Wait for an answer before you write the code.
3. For a security problem, do not open an issue. Follow [SECURITY.md](SECURITY.md).

## Set up

You need Rust 1.88 or later and git. The test suite needs no simulator and no Xcode. To run
Seiji against real devices, you also need macOS with Xcode, Java 17 and the Maestro CLI.

```sh
git clone https://github.com/wiljafor1/seiji
cd seiji
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

[docs/testing.md](docs/testing.md) describes the test suite and the checks on a real host.

## Make a change

1. Make a branch from `main`.
2. Make the smallest change that solves the problem.
3. Add or change tests. A bug fix needs a test that fails before the fix.
4. Update the documentation that your change makes wrong.
5. Run `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`. All
   must pass.
6. If you changed `src/devices.rs`, `src/exec.rs`, `src/tenant.rs` or `src/maestro.rs`, run
   `seiji doctor <user>` on a Mac and put the output in the pull request.

## Open a pull request

- Use one pull request for one purpose.
- Use a title in the imperative mood with a type prefix, for example
  `fix: release a lease when its owner is removed`.
- In the description, say what changed, why, how you verified it, and what is risky.
- Link the issue.

A maintainer reviews the pull request. Answer the comments with a change or with evidence.

## License

You agree that your contribution is licensed under the [MIT License](LICENSE).
