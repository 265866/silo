# Contributing

## Commit messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/). Versioning, changelogs, and releases are automated from commit history, so the format matters:

- `feat: ...` — a new feature (bumps the minor version)
- `fix: ...` — a bug fix (bumps the patch version)
- `feat!: ...` or a `BREAKING CHANGE:` footer — a breaking change
- `chore:`, `docs:`, `refactor:`, `test:`, `ci:`, `perf:`, `build:` — housekeeping, no release on their own

An optional scope names the area, e.g. `feat(ui): add archive hotkey`.

## Local setup

Install the commit-message linter once per clone:

```sh
cargo install cocogitto
cog install-hook --all
```

Pull requests are **squash-merged**, so the PR title becomes the commit on `main`. PR titles must also follow Conventional Commits — this is enforced in CI.
