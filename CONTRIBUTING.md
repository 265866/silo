# Contributing to silo

Thanks for your interest in contributing! silo is a wallet that people trust with real funds, so
correctness and safety come first — small, well-tested changes are the most welcome kind.

## Development setup

You'll need a Rust toolchain (edition 2024, **MSRV 1.96**).

```sh
git clone https://github.com/265866/silo
cd silo
cargo run          # build and launch the TUI (needs a real terminal)
```

Install the commit-message hook once per clone — it checks that your messages follow the format
below before they're committed:

```sh
cargo install cocogitto
cog install-hook --all
```

## Commit messages

silo uses [Conventional Commits](https://www.conventionalcommits.org/). Versioning and the
changelog are generated from commit history, so the prefix matters:

- `feat: ...` — a new feature
- `fix: ...` — a bug fix
- `feat!: ...` or a `BREAKING CHANGE:` footer — a breaking change
- `chore:`, `docs:`, `refactor:`, `test:`, `ci:`, `perf:`, `build:` — everything else

An optional scope names the area, e.g. `feat(ui): add archive hotkey`.

## Before opening a pull request

Run the same checks CI will:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these on Linux, macOS, and Windows. Please add or update tests for behavior you change —
especially anything touching key derivation, the intent log, or the audit chain.

## Opening a pull request

1. Fork the repo and create a branch off `main`.
2. Give the **PR a Conventional Commit title** (e.g. `fix(db): …`). PRs are squash-merged, so the
   title becomes the commit on `main`, and it's checked automatically.
3. Make sure CI is green.
4. Don't bump the version or edit `CHANGELOG.md` — that's automated (see below).

A maintainer will review and merge it.

## Reporting security issues

Please **don't** open a public issue for security vulnerabilities. Report them privately through the
repository's **Security** tab (Private vulnerability reporting), or email `github@coltons.space` if
GitHub private vulnerability reporting is unavailable. Do not include recovery phrases, private keys,
API tokens, or live wallet secrets.

## How releases work

Contributors don't need to do anything for releases. Once changes are on `main`,
[release-plz](https://release-plz.dev) maintains a release PR that bumps the version and updates the
changelog; merging it triggers [cargo-dist](https://opensource.axo.dev/cargo-dist/) to build the
cross-platform binaries, publish a GitHub Release, and update the Homebrew tap.

## License

By contributing, you agree that your contributions are licensed under the project's
[**GPL-3.0-or-later**](LICENSE) license.
