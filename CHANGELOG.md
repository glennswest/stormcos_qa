# Changelog

## [Unreleased]

### 2026-09-24
- **docs:** `docs/presentation.md`, a 13-slide Marp deck on purpose, place in stormcos, how it works, what works today, interfaces, shipping, status and planned work; linked from README (#7)
- **docs:** note that the deck's PDF export needs a browser and that marp needs `</dev/null` when stdin is not a terminal (#7)
- **docs:** qa-runner and must-gather module docs match behaviour (no self-tombstoning, one-level discovery, manifest outside tarball, collector env) (#6)
- **docs:** STANDARD.md corrected against qa-runner: one-level discovery, 40-line metadata scan, severity/scope/timeout fallbacks, real issue marker format, env set unconditionally vs per flag; `.qa.toml`, auto-close, nested test dirs and tombstoning marked not implemented (#8, #9, #10, #14) (#6)
- **docs:** README rewritten from the code: every qa-runner and must-gather flag with its default, discovery, scope gating, issue filing, report schema, exit code, built-in and component collectors; states that nothing runs the suite since stormcos-builder was retired and that it ships no golden (#6)
- **docs:** Add `CLAUDE.md` (work plan, version locations, shipping) and this changelog (#6)
- **feat:** stormblock-csi cluster tests + must-gather collector
- **fix:** root-fs checks were false-negative on a correct image
- **feat:** assert `authorized_keys` is storm-owned/readable
- **feat:** network reachability tests + QE-key-present guard
- **feat:** `tests/topology/single/` boot checks (#3)
- **feat:** ironprom and fastetcd tests + must-gather collectors
- **feat:** test standard, qa-runner, must-gather, example tests
