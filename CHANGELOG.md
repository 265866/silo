# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5](https://github.com/265866/silo/compare/v0.1.4...v0.1.5) - 2026-06-08

### Other

- gitignore the whole .claude/ directory ([#183](https://github.com/265866/silo/pull/183))
- untrack the local scheduled-tasks lock ([#182](https://github.com/265866/silo/pull/182))
- *(deps)* bump rusqlite 0.40, sha2 0.11, hmac 0.13, ratatui 0.30.1 ([#180](https://github.com/265866/silo/pull/180))

## [0.1.4](https://github.com/265866/silo/compare/v0.1.3...v0.1.4) - 2026-06-08

### Added

- *(main)* restore the terminal on SIGTERM/SIGHUP instead of leaving it in raw mode ([#164](https://github.com/265866/silo/pull/164))

### Fixed

- *(worker)* move broadcast confirmation polling off the serial ordered-command task ([#176](https://github.com/265866/silo/pull/176))
- *(solana)* reject recipients colliding with instruction program ids in transfer message builder ([#175](https://github.com/265866/silo/pull/175))
- *(input)* debounce large-send re-confirm against held-Enter autorepeat ([#174](https://github.com/265866/silo/pull/174))
- *(input)* make prepare_send idempotent while a send prepare/confirm is outstanding ([#173](https://github.com/265866/silo/pull/173))
- *(app)* guard SendPrepared on Route::Send so confirm modal cannot open off-Send ([#172](https://github.com/265866/silo/pull/172))
- *(app)* reset inflight balance-refresh counter when generation is bumped on RPC change ([#171](https://github.com/265866/silo/pull/171))
- *(reconcile)* isolate per-intent RPC failures so one transient probe error does not abort the whole reconcile batch ([#169](https://github.com/265866/silo/pull/169))
- *(db)* run PRAGMA foreign_key_check on open to detect dangling tx_intents.from_wallet ([#168](https://github.com/265866/silo/pull/168))
- *(worker)* surface compensating mark_terminal failure so a wedged-open intent isn't silent ([#167](https://github.com/265866/silo/pull/167))
- *(db)* run integrity_check before any write in Db::open on existing databases ([#166](https://github.com/265866/silo/pull/166))
- *(worker)* treat definitive sendTransaction preflight rejections as failed instead of polling them as uncertain ([#165](https://github.com/265866/silo/pull/165))
- *(vault)* raise Argon2id at-rest defaults for the cold seed-phrase vault ([#163](https://github.com/265866/silo/pull/163))
- *(worker)* surface DB write failure when finalizing a confirmed/failed transfer instead of silently dropping it ([#162](https://github.com/265866/silo/pull/162))
- *(db)* fail verify_audit_chain when an initialized vault has an empty audit log ([#161](https://github.com/265866/silo/pull/161))
- *(main)* disable bracketed paste on panic, not only on clean exit ([#160](https://github.com/265866/silo/pull/160))
- *(clipboard)* reap Linux clipboard-persist daemon children to avoid zombie accumulation ([#155](https://github.com/265866/silo/pull/155))
- *(ui)* replace fullwidth confetti glyph that overflows one terminal cell ([#159](https://github.com/265866/silo/pull/159))
- *(profiles)* recover from a corrupt profiles.json instead of refusing to boot ([#158](https://github.com/265866/silo/pull/158))
- *(ui)* format fiat amounts with per-currency minor-unit precision ([#157](https://github.com/265866/silo/pull/157))
- *(price)* reject implausible SOL prices and FX rates with absolute sanity bounds ([#156](https://github.com/265866/silo/pull/156))
- *(solana-rpc)* honor HTTP-date form of Retry-After on 429/408/5xx ([#151](https://github.com/265866/silo/pull/151))
- *(vault)* validate decrypted mnemonic UTF-8 without an unprotected heap copy ([#148](https://github.com/265866/silo/pull/148))

### Other

- *(db)* run SQLite on a dedicated actor thread so it never blocks a runtime thread or the UI loop ([#179](https://github.com/265866/silo/pull/179))
- *(ui)* gate redraw on a dirty flag and idle the tick loop ([#178](https://github.com/265866/silo/pull/178))
- *(app)* centralize the stale-event generation guard in apply_app_event ([#177](https://github.com/265866/silo/pull/177))
- *(db)* cover the WAL guard's refuse-to-run branch, not just the happy path ([#170](https://github.com/265866/silo/pull/170))
- *(money)* derive parse_sol_to_lamports from parse_decimal_scaled to remove duplicated decimal parsing ([#154](https://github.com/265866/silo/pull/154))
- *(dependabot)* add cargo ecosystem to monitor crate security updates ([#147](https://github.com/265866/silo/pull/147))
- *(msrv)* run the test suite on the 1.89 toolchain, not only cargo check ([#146](https://github.com/265866/silo/pull/146))
- *(profile)* enable overflow-checks in release and dist profiles ([#145](https://github.com/265866/silo/pull/145))
- *(deps)* bump actions/checkout from 4.3.1 to 6.0.3 ([#101](https://github.com/265866/silo/pull/101))
- *(deps)* update dtolnay/rust-toolchain requirement to 193d6aa1dbbc28bd2c0a6b0e327cfdce68baaf6e ([#102](https://github.com/265866/silo/pull/102))
- *(deps)* bump amannn/action-semantic-pull-request from 5 to 6 ([#100](https://github.com/265866/silo/pull/100))

## [0.1.3](https://github.com/265866/silo/compare/v0.1.2...v0.1.3) - 2026-06-07

### Fixed

- harden storage rpc and profile lifecycle ([#109](https://github.com/265866/silo/pull/109))

### Other

- *(ui)* move profile open/switch/new off the UI event-loop thread ([#112](https://github.com/265866/silo/pull/112))
- *(ui)* move blocking setup, storage, and clipboard work out of input handling ([#111](https://github.com/265866/silo/pull/111))
- *(input)* cover send confirmation persistence ([#108](https://github.com/265866/silo/pull/108))
- *(reconcile)* cover persisted intent recovery ([#107](https://github.com/265866/silo/pull/107))
- *(prop)* cover money transaction and vault invariants ([#106](https://github.com/265866/silo/pull/106))
- *(test)* run checks with locked dependencies ([#105](https://github.com/265866/silo/pull/105))
- *(deps)* run dependency policy on a schedule ([#104](https://github.com/265866/silo/pull/104))
- *(deny)* enforce bincode advisory guard ([#103](https://github.com/265866/silo/pull/103))
- *(actions)* pin GitHub Actions to SHAs ([#99](https://github.com/265866/silo/pull/99))
- *(release)* verify release bootstrap downloads ([#98](https://github.com/265866/silo/pull/98))
- *(install)* document checksum verification ([#97](https://github.com/265866/silo/pull/97))
- *(runtime)* disclose mainnet-only defaults ([#96](https://github.com/265866/silo/pull/96))
- *(first-run)* document passphrase setup and recovery ([#95](https://github.com/265866/silo/pull/95))
- *(support)* document support and bug-reporting paths ([#94](https://github.com/265866/silo/pull/94))
- *(security)* add fallback vulnerability contact ([#93](https://github.com/265866/silo/pull/93))
- *(operations)* add first-run and runtime troubleshooting ([#92](https://github.com/265866/silo/pull/92))
- *(keyboard)* document history and settings shortcuts ([#91](https://github.com/265866/silo/pull/91))
- *(deps)* remove unused rand dependency ([#89](https://github.com/265866/silo/pull/89))

## [0.1.2](https://github.com/265866/silo/compare/v0.1.1...v0.1.2) - 2026-06-07

### Fixed

- *(db)* harden safety checks and audited metadata ([#53](https://github.com/265866/silo/pull/53))
- resolve open issue sweep

## [0.1.1](https://github.com/265866/silo/compare/v0.1.0...v0.1.1) - 2026-06-05

### Other

- document install methods and contributor workflow ([#3](https://github.com/265866/silo/pull/3))
