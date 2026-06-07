# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
