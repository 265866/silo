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

**Network:** silo currently operates on Solana **mainnet-beta only**. The default RPC
endpoint is `https://api.mainnet-beta.solana.com`. Editing the RPC URL changes the
endpoint/provider used for mainnet-beta access; it does not add a supported devnet or
testnet mode for end users.

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
- **Encrypted vault.** The seed is sealed with XChaCha20-Poly1305 under an Argon2id-derived key
  from your passphrase. New vaults use cold-storage Argon2id parameters (64 MiB memory, 3 passes)
  rather than the lighter interactive-login minimum, because the vault file is a cold, high-value
  secret an attacker could exfiltrate and brute-force fully offline — so the KDF, not online rate
  limiting, is the work factor that matters. Each vault stores the parameters it was created with,
  so older vaults keep unlocking unchanged. Auto-lock kicks in after an idle timeout.
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

### Verify release downloads first

The [latest release](https://github.com/265866/silo/releases/latest) publishes these prebuilt archives and checksum files:

- `silo-aarch64-apple-darwin.tar.xz` and `.sha256`
- `silo-x86_64-apple-darwin.tar.xz` and `.sha256`
- `silo-x86_64-unknown-linux-gnu.tar.xz` and `.sha256`
- `silo-x86_64-pc-windows-msvc.zip` and `.sha256`
- `sha256.sum`

Manual archive installation is the most verification-friendly path: download the archive and its checksum, verify the digest, then extract and place `silo` on your `PATH`.

Linux:

```sh
curl -LO https://github.com/265866/silo/releases/latest/download/silo-x86_64-unknown-linux-gnu.tar.xz
curl -LO https://github.com/265866/silo/releases/latest/download/silo-x86_64-unknown-linux-gnu.tar.xz.sha256
sha256sum -c silo-x86_64-unknown-linux-gnu.tar.xz.sha256
```

macOS:

```sh
curl -LO https://github.com/265866/silo/releases/latest/download/silo-aarch64-apple-darwin.tar.xz
curl -LO https://github.com/265866/silo/releases/latest/download/silo-aarch64-apple-darwin.tar.xz.sha256
shasum -a 256 -c silo-aarch64-apple-darwin.tar.xz.sha256
```

Windows PowerShell:

```powershell
Invoke-WebRequest https://github.com/265866/silo/releases/latest/download/silo-x86_64-pc-windows-msvc.zip -OutFile silo-x86_64-pc-windows-msvc.zip
Invoke-WebRequest https://github.com/265866/silo/releases/latest/download/silo-x86_64-pc-windows-msvc.zip.sha256 -OutFile silo-x86_64-pc-windows-msvc.zip.sha256
$expected = (Get-Content silo-x86_64-pc-windows-msvc.zip.sha256).Split()[0]
$actual = (Get-FileHash silo-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
```

Releases are checksum-only today. There are no documented maintainer signatures, cosign signatures, or provenance attestations.

### Install script

The convenience installer scripts are `silo-installer.sh` and `silo-installer.ps1`. `curl | sh` and `irm | iex` rely on HTTPS and GitHub Releases as the trust boundary, and are less verification-friendly than manual archive installation unless you manually verify checksums or script digests first.

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/265866/silo/releases/latest/download/silo-installer.sh | sh
```

```powershell
irm https://github.com/265866/silo/releases/latest/download/silo-installer.ps1 | iex
```

### Prebuilt binaries

Download a build for your platform from the [latest release](https://github.com/265866/silo/releases/latest)
— macOS (Apple Silicon & Intel), Linux x86_64, and Windows x86_64.

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

- **First run** opens a setup wizard that creates your first profile ("Wallet 1"). Choose create or import, record the generated recovery phrase or enter your existing one, confirm the generated phrase word-by-word if creating, choose and confirm a vault passphrase, then unlock and use the wallet. Funded wallets are mainnet-beta wallets: real SOL can be transferred.
- **Later runs** open the profile picker.

Before funding a wallet:

- Make sure your recovery phrase is written down, stored offline, and never shared.
- Make sure you know your vault passphrase and do not share it.
- Check that you can unlock the vault and that the receive address is correct.
- Send a tiny test amount first when using a new address or setup, before larger funding or sends.

silo cannot recover lost recovery phrases or forgotten vault passphrases. The vault passphrase encrypts the recovery phrase on disk; use a strong, non-empty passphrase. The app allows an empty passphrase only after an explicit warning and confirmation, because anyone with access to your files could read the recovery phrase. For support, never include recovery phrases, passphrases, private keys, tokens, or live wallet secrets in issues, logs, screenshots, or diagnostics; see [Support and bug reports](#support-and-bug-reports) and [SECURITY.md](SECURITY.md).

Configuration lives in your platform config directory, overridable with `SILO_CONFIG_DIR`:

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/silo/` |
| Linux | `$XDG_CONFIG_HOME/silo/` or `~/.config/silo/` |
| Windows | `%APPDATA%\silo\` |

Each profile stores its vault and database under `<config>/profiles/<id>/`.

## Operations and troubleshooting

- **TTY required.** `silo` is an interactive TUI and must be started from a real terminal. It is not designed for cron, pipes, redirected stdio, or non-interactive service runners; if terminal setup fails, run it directly in a local terminal or an attached SSH session with a TTY.
- **Config directory failures.** On startup, silo creates the config directory and the active profile directory, then sets Unix permissions to `0700`. Errors such as `creating ...` or `setting permissions on ...` mean the configured location is missing permissions, owned by another user, read-only, or on a filesystem that cannot apply those permissions. Fix ownership/permissions or choose another config directory.
- **Single-instance lock.** The config directory contains `silo.lock`. If startup says `another silo instance is already running`, another silo process is holding that config directory's lock. Exit the other process, or use a separate config directory for an isolated run.
- **Temporary config directories.** `SILO_CONFIG_DIR` overrides the platform default. For safe testing, use a private throwaway directory, for example `SILO_CONFIG_DIR="$(mktemp -d)" silo`. Do not point `SILO_CONFIG_DIR` at a real config directory unless you intend to use those profiles; each config directory has its own lock, `profiles.json`, and `profiles/` tree.
- **Network dependencies.** SOL balances, blockhashes, transaction broadcast, and confirmation polling use the configured Solana JSON-RPC endpoint, defaulting to `https://api.mainnet-beta.solana.com`. The Settings screen can save another `http` or `https` RPC URL with a host and no username/password. Fiat pricing uses CoinGecko first; if that fails, silo falls back to Jupiter's SOL price and Frankfurter FX rates for non-USD currencies. If these services are unavailable, rate-limited, or return invalid data, expect refresh/send preparation errors or stale/missing fiat prices; already-created local vault/profile data remains on disk.
- **Linux clipboard persistence.** Clipboard support depends on the desktop clipboard backend. On Linux, silo tries to keep copied text available after the main process exits by spawning a small clipboard helper. Some sessions cannot persist clipboard contents, notably GNOME Wayland without data-control support, so copied addresses may disappear when silo exits or may only be available while the session keeps them.
- **Backups and migration.** Back up the whole config directory, especially `profiles.json` and each `<config>/profiles/<id>/vault.json` and `silo.db`. The mnemonic is the ultimate recovery material, but the vault/profile database contain labels, notes, audit history, settings, and cached metadata. To move machines, install the same or newer silo, copy the config directory to the platform default location or set `SILO_CONFIG_DIR` to it, preserve file ownership/private permissions, then start silo. SQLite migrations run automatically.

## Keyboard shortcuts

| Screen | Keys |
| --- | --- |
| Profiles | `↑↓` move · `enter` open · `n` new · `r` rename · `d` delete · `q` quit |
| Wallet list | `↑↓` move · `enter` open · `s` send · `M` →master · `F` fund · `n` new sub · `c` copy · `l` label · `t` note · `x` archive · `h` history · `a` audit · `g` settings · `r` refresh · `^L` lock · `q` quit |
| Wallet detail | `s` send · `M` →master · `F` fund · `c` copy · `h` history · `esc` back |
| Send | `tab` next field · `c` SOL/fiat · `m` max · `a` all · `enter` review · `^V` paste · `esc` cancel |
| History | `↑↓` scroll · `c` copy txid · `t` note · `q/esc` back |
| Settings | `e` edit RPC · `u` currency · `p` priority · `+/-` auto-lock · `L` lock now · `q/esc` back |

## Testing

```sh
cargo test
cargo test -- --ignored     # live tests hit price APIs and devnet RPC
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs `fmt`, `clippy`, dependency policy checks, an MSRV check, and the full test suite on Linux, macOS, and Windows for every pull request.

## Support and bug reports

For ordinary bugs, install failures, and runtime problems, open a public GitHub issue using the bug report or install/runtime support template. Include your OS, `silo` version, install method, terminal, relevant settings such as RPC endpoint type or provider name, reproduction steps, expected behavior, actual behavior, and sanitized logs or screenshots.

Never include recovery phrases, private keys, passphrases, API tokens, RPC tokens, or live wallet secrets in public issues, logs, screenshots, or diagnostics. Maintainers will not ask for secrets, cannot recover lost mnemonics or passphrases, cannot reverse transactions, and cannot guarantee support from third-party RPC/provider services.

For suspected security vulnerabilities, do not open a public issue. Report them privately through [SECURITY.md](SECURITY.md).

## Security

Please report vulnerabilities privately through GitHub private vulnerability reporting, or email `github@coltons.space` if that is unavailable. See [SECURITY.md](SECURITY.md).

## License

Copyright (c) 2026 Colton.

silo is free software, licensed under the **GNU General Public License v3.0 or later**
(GPL-3.0-or-later). It comes with NO WARRANTY. See [LICENSE](LICENSE) for the full text.