# Repository Guidelines

## Project Structure & Module Organization

This is a native Rust desktop translator built with Slint and SQLite. Keep engine-independent behavior in `src/core/`; engine adapters belong in `src/engine/` (currently `renpy/` holds parsing, extraction, RPA reading, and export). Put provider code in `src/ai/`, persistence in `src/database/`, and batching/validation in `src/translation/`. UI markup is `ui/app.slint`; Rust UI wiring is `src/ui/mod.rs`. Use `examples/sample-renpy-game/` for safe manual scan/export checks. Read `DESIGN.md` before changing module boundaries.

## Build, Test, and Development Commands

Run these from the repository root:

```bat
make run       # build and launch the desktop app
make check     # fast compile/type check
make test      # run unit tests
make fmt       # format Rust sources
make lint      # Clippy; warnings fail the check
make release   # optimized binary: target\release\translator-game-lite.exe
```

`cargo test real_game_probe -- --ignored --nocapture` is an opt-in check against a configured real game; never rely on it as the only verification.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and let `cargo fmt --all` determine indentation and layout. Name modules and functions in `snake_case`, types and traits in `PascalCase`, and constants/statics in `SCREAMING_SNAKE_CASE`. Keep UI callbacks thin: place business rules in `core`, `translation`, or the appropriate engine module. Preserve Ren'Py source conservatively—do not rewrite untouched text, line endings, comments, or packed `.rpa` archives.

## Required Change Planning

**Plan before changing code—always.** Before editing any source, test, UI, build, or configuration code, write a concise implementation plan that states the problem, affected files/modules, intended behavior, risks, and verification commands. Get confirmation when the plan changes scope or affects game files, export behavior, data migration, or user settings. Do not start implementation until this planning step is complete.

## Testing Guidelines

Place unit tests beside the code they exercise under `#[cfg(test)]`; use descriptive `snake_case` names such as `skips_strings_already_translated_by_sibling_files`. Add regression coverage for parser and exporter edge cases, especially escaped strings, duplicate `old` entries, CRLF, stale source text, and archive-backed scripts. Run `make fmt`, `make lint`, and `make test` before proposing changes.

## Commit & Pull Request Guidelines

Recent history uses concise imperative subjects, for example `Add QA checks and bulk retranslate` and `Fix pending reset and duplicate Ren'Py strings`. Keep commits narrowly scoped. PRs should explain user-visible behavior, list validation commands, link relevant issues, and include screenshots for Slint UI changes. Call out any change that writes into a game directory or alters export behavior.

## Security & Configuration

Never commit API keys, local SQLite data, game saves, or copied game assets. Provider credentials are user-local settings; use placeholders in examples and logs.
