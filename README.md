# Game Translator Lite

A very lightweight **native desktop AI game translator** with excellent context and
glossary support — starting with **Ren'Py** while keeping the translation core
engine-independent.

Built with **Rust + Slint + SQLite**. No WebView, no Node.js, no Python runtime.

```
เลือกเกม → ตรวจจับ Engine → Extract text → แปลด้วย AI → Review / Edit → Export กลับเข้าเกม
```

See [DESIGN.md](DESIGN.md) for the full architecture.

## Build

```bat
cargo build --release
```

The binary lands in `target\release\translator-game-lite.exe`. Requires the Rust
MSVC toolchain (SQLite is bundled; nothing else to install).

## Features (v1)

- **Game projects** — pick a folder; the Ren'Py engine is auto-detected
  (loose `*.rpy` anywhere under `game/`, or packed `*.rpa` archives).
- **Extraction** — say statements (with speaker, character names resolved from
  `define e = Character("...")`), narrator lines, menu choices, and `old`/`new`
  translation pairs — from loose scripts **and directly out of `.rpa`
  archives** (RPA-3.0, compressed or raw members, both handled read-only).
  Python code, labels, jumps, conditions, comments and formatting are never
  touched. Empty skeleton lines from generated translation files are skipped.
- **Incremental scan** — sources are hashed (SHA-256). Unchanged lines keep their
  translation, changed lines reset to Pending, and text that moved is refilled
  from the translation memory automatically.
- **Context** — every entry is sent to the AI with its speaker, scene/label,
  previous N and next N lines (configurable window in Settings) and shown in the
  editor panel.
- **Glossary** — project glossary with enable/disable and search. Terms are matched
  on word boundaries (no blind substring replacement) and enforced during response
  validation.
- **Translation memory** — `(source_hash, target_language)`; repeated text never
  calls the AI twice. Priority: Glossary → Memory → AI.
- **Batch translation** — 10–30 entries per request (configurable), JSON responses
  mapped back by stable id, bounded concurrency (2–4, configurable), cancelable.
- **Validation** — protected tokens (`[player_name]`, `{b}`, `{color=#fff}`, ...),
  glossary terms, response format, empty translations and unexpected ids are all
  checked. Failed entries keep the AI attempt visible but are never exported
  until they pass or you fix them by hand (status `Failed` → retry).
- **Manual editing** — saving an edit sets status `Edited`; automatic translation
  never overwrites it unless you press *Re-translate*.
- **Export** — rewrites only the translated string literal in place (or the
  adjacent `new` line), preserving indentation, line endings and every untouched
  line. Entries whose game text no longer matches the scan are skipped, never
  overwritten blindly. Translations for scripts that live **inside an `.rpa`
  archive** are exported as `translate <language> strings:` old/new pairs into
  `tl/<language>/` — Ren'Py applies those at runtime (including to dialogue
  without its own translate block), so archive-packed games translate fully
  without ever modifying the archive.

## First run

1. **Settings** tab → pick a **Provider Preset**:
   - *Ollama Cloud (API key)* — endpoint `https://ollama.com/v1`, paste the
     API key from ollama.com (Account → API Keys). Thinking/reasoning is
     disabled automatically (the app talks to Ollama's native API with
     `think: false`).
   - *Ollama (local)* — endpoint `http://localhost:11434/v1`, API key stays
     **empty** (nothing is sent). Nothing leaves your machine.
   - *LM Studio (local)* — endpoint `http://localhost:1234/v1`, key empty.
   - *OpenAI (cloud)* / *OpenRouter* — paste your API key.
2. Click the **⟳** button next to **Model** to list the models the server
   offers and pick one. The selection is remembered **per provider** in the
   local SQLite database (e.g. Ollama Cloud and OpenAI each keep their own).
   Settings take effect immediately; **Save Settings** persists them for the
   next app start. The key is stored locally only — nothing is hardcoded.
3. The default translation style is a casual, friendly, conversational tone —
   edit the **Translation Prompt** to change it.

## First project

1. **Project** tab → *Open...* → select a Ren'Py project folder (the folder
   that contains `game/`). The app scans it and shows Total / Translated /
   Pending counts.
2. *Translate pending* → progress runs in the background; *Cancel* stops after the
   current batch. Click any row to review the original, its context and the
   translation; edit and *Save* (status becomes `Edited`).
3. **Glossary** tab → add terms (e.g. `Alice → อลิซ`). They are applied to new
   translations immediately.
4. *Export* writes the translations back into the game files.

A ready-to-scan example lives in [`examples/sample-renpy-game`](examples/sample-renpy-game).

## Project layout

```
src/
├── main.rs            # composition root
├── core/              # engine-independent models, GameEngine trait, scan
├── engine/renpy/      # parser, extractor, exporter (the only Ren'Py code)
├── ai/                # TranslationProvider trait + OpenAI-compatible impl
├── database/          # SQLite schema + repositories (bundled rusqlite)
├── translation/       # pipeline (batch + validation), translation memory
└── ui/                # Slint window + glue (background threads, pagination)
```

Adding another engine (Unity, RPG Maker, Godot) later means implementing one
`GameEngine` and registering it in `core::engine::registry()` — the translation
core does not know which engine produced the text.

## Tests

```bat
cargo test
```

54 tests cover the Ren'Py parser, extraction (including `.rpa` archives),
placeholder preservation, glossary matching, translation validation, source
hashing, incremental scanning, the translation pipeline (memory hits,
validation failures, cancellation) and the exporter (in-place rewrites,
old/new handling, stale-line safety, CRLF, tl-file generation).
