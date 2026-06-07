# silo

[![CI](https://github.com/265866/silo/actions/workflows/ci.yml/badge.svg)](https://github.com/265866/silo/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/265866/silo)](https://github.com/265866/silo/releases/latest)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPLv3-blue.svg)](LICENSE)

A **SOL-only** Solana wallet manager that runs entirely in your terminal.

`silo` manages many Solana wallets derived from a single BIP39 mnemonic, with a focus on
**not losing money**: an encrypted seed vault, a write-ahead intent log that survives the
process being killed in any state, and an append-only HMAC-chained audit log. It speaks to
the Solana network over plain JSON-RPC with a hand-rolled, ecosystem-interoperable
transaction serializer.

---

## What it does

- **One mnemonic, many wallets.** Account index `0` is your **master** wallet; subwallets are
  indices `1, 2, …` on the Phantom-compatible path `m/44'/501'/<index>'/0'`.
- **SOL only, always.** No SPL tokens, no associated token accounts.
- **Fiat equivalents.** Live SOL price with selectable currency (USD/EUR/GBP/JPY/CAD/AUD/CHF/CNY),
  and amounts you can enter denominated in either SOL or your chosen fiat.
- **Multiple profiles.** Each profile has its own mnemonic, vault, and database; switch between
  them from the picker screen.
- **Notes & audit.** Attach notes to wallets and transfers; every state change is recorded in an
  audit log.

## Safety model

- **The mnemonic is the only source of truth.** Private keys are *never* persisted. The seed
  is decrypted into memory only while unlocked, signing keys are derived at sign time and
  dropped immediately, and secrets are zeroized on lock/exit.
- **Encrypted vault.** The seed is sealed with XChaCha20-Poly1305 under an Argon2-derived key
  from your passphrase. Auto-lock kicks in after an idle timeout.
- **Crash-safe by construction.** Every money operation is a single `IMMEDIATE` SQLite
  transaction (`WAL` + `synchronous=FULL`) that mutates the row *and* appends its audit row
  atomically. A write-ahead **intent log** records transfers before they are signed/sent, and a
  **reconcile-on-boot** pass repairs any operation that was interrupted. silo refuses to run on a
  filesystem where WAL doesn't take.
- **Double-spend guards.** At most one open intent per source wallet; a single-instance lock
  prevents two processes from racing on one vault.
- **Tamper-evident audit log.** Append-only, HMAC-SHA256 hash-chained. The HMAC key is derived
  via HKDF from the vault key and never stored on disk (so the chain can't be forged offline).

## Architecture

A single async (`tokio`) `select!` loop owns the app state; workers communicate only by message,
so there are no locks on UI state. Terminal rendering is [ratatui](https://ratatui.rs) +
crossterm; storage is bundled SQLite via `rusqlite`; network I/O is one shared `reqwest` client
(rustls) for both RPC and the price feed.

```
src/
├── crypto.rs            # BIP39 mnemonic, seed, ed25519 + SLIP-0010 derivation, HKDF
├── vault.rs             # Argon2 + XChaCha20-Poly1305 seed vault, atomic writes
├── types.rs, money.rs   # shared domain types; exact (no-float) lamport ⇄ SOL math, fee/rent
├── db/                  # SQLite actor: wallets, intents (WAL log), audit chain
├── solana/              # JSON-RPC client, hand-rolled tx serializer, reconcile-on-boot
├── price.rs             # SOL price feed with multi-source fallback
├── clipboard.rs         # cross-platform copy (+ Linux clipboard-persist daemon)
├── profiles.rs          # multiple wallet profiles
├── app.rs, input.rs     # app state + command/event model; key handling & routing rules
├── worker.rs            # background tasks: RPC, price refresher, sends, DB actor
├── ui/                  # screens, theme, formatting, render
└── main.rs              # startup, single-instance lock, the select loop
```

The transaction wire format and key derivation are **cross-checked in tests** against Solana's
official `solana-keypair` / `solana-message` crates, so addresses and signed transactions are
provably interoperable with Phantom and the rest of the ecosystem.

## Install

### Homebrew (macOS & Linux)

```sh
brew install 265866/silo/silo
```

### Install script

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/265866/silo/releases/latest/download/silo-installer.sh | sh
```

```powershell
# Windows
irm https://github.com/265866/silo/releases/latest/download/silo-installer.ps1 | iex
```

### Prebuilt binaries

Download a build for your platform from the [latest release](https://github.com/265866/silo/releases/latest)
— macOS (Apple Silicon & Intel), Linux x86_64, and Windows x86_64, each with a `.sha256` checksum.

### From source

Requires a Rust toolchain (edition 2024, **MSRV 1.89**).

```sh
cargo install --locked --git https://github.com/265866/silo
# or, from a clone:
cargo run --locked       # build and launch
cargo build --release --locked
```

## Running silo

`silo` needs a TTY — run it in a real terminal.

- **First run** opens a setup wizard that creates your first profile ("Wallet 1") — generate a new
  mnemonic or import an existing one, then confirm the phrase word-by-word.
- **Later runs** open the profile picker.

Configuration lives in your platform config directory, overridable with `SILO_CONFIG_DIR`:

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/silo/` |
| Linux | `$XDG_CONFIG_HOME/silo/` or `~/.config/silo/` |
| Windows | `%APPDATA%\silo\` |

Each profile stores its vault and database under `<config>/profiles/<id>/`.

## Keyboard shortcuts

| Screen | Keys |
| --- | --- |
| Profiles | `↑↓` move · `enter` open · `n` new · `r` rename · `d` delete · `q` quit |
| Wallet list | `↑↓` move · `enter` open · `s` send · `M` →master · `F` fund · `n` new sub · `c` copy · `l` label · `t` note · `x` archive · `h` history · `a` audit · `g` settings · `r` refresh · `^L` lock · `q` quit |
| Wallet detail | `s` send · `M` →master · `F` fund · `c` copy · `h` history · `esc` back |
| Send | `tab` next field · `c` SOL/fiat · `m` max · `a` all · `enter` review · `^V` paste · `esc` cancel |
| History | `↑↓` scroll · `t` note · `esc` back |
| Settings | `e` edit RPC · `u` currency · `+/-` auto-lock · `L` lock now · `esc` back |

## Testing

```sh
cargo test
cargo test -- --ignored     # live tests hit price APIs and devnet RPC
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs `fmt`, `clippy`, dependency policy checks, an MSRV check, and the full test suite on Linux, macOS, and Windows for every pull request.

## License

Copyright (c) 2026 Colton.

silo is free software, licensed under the **GNU General Public License v3.0 or later**
(GPL-3.0-or-later). It comes with NO WARRANTY. See [LICENSE](LICENSE) for the full text.