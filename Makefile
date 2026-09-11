# Game Translator Lite — Rust + Slint + SQLite
#
# GNU Make on Windows: winget install GnuWin32.Make   (or: choco install make)
# Usage: make <target> — run `make` alone to list targets.

CARGO := cargo

.PHONY: help build release run test check fmt lint probe clean

help:
	@echo 'Game Translator Lite targets:'
	@echo '  make build    - debug build'
	@echo '  make release  - optimized build -> target\release\translator-game-lite.exe'
	@echo '  make run      - build and start the app'
	@echo '  make test     - unit tests'
	@echo '  make check    - fast type check'
	@echo '  make fmt      - format all code'
	@echo '  make lint     - clippy with warnings as errors'
	@echo "  make probe    - extraction probe against a real Ren'Py game (ignored test)"
	@echo '  make clean    - remove target directory'

build:
	$(CARGO) build

release:
	$(CARGO) build --release

run:
	$(CARGO) run

test:
	$(CARGO) test

check:
	$(CARGO) check

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --all-targets -- -D warnings

probe:
	$(CARGO) test real_game_probe -- --ignored --nocapture

clean:
	$(CARGO) clean
