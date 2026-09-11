# Game Translator Lite — Design

A very lightweight native desktop AI game translator with excellent context and glossary
support, starting with Ren'Py while keeping the core engine-independent.

Stack: **Rust + Slint + SQLite** (local-first, no WebView / Node / Python runtime).

## 1. Architecture

```
                    Application (src/main.rs, src/ui/*)
                         │
              ┌──────────┴──────────┐
              │                     │
        Translation Core       Project Core
        (src/translation/)     (src/core/project.rs, src/core/scan.rs)
              │
       ┌──────┼───────┐
       │      │       │
    Context Glossary Memory
  (core/context) (core/glossary) (translation/memory)
       │      │       │
       └──────┼───────┘
              │
         AI Provider (src/ai/)
              │
              ▼
        Engine Adapter (src/engine/, trait: core/engine.rs)
              │
           Ren'Py (src/engine/renpy/)
```

- **Core never references Ren'Py.** The only engine-specific code lives in `src/engine/renpy/`.
- Adding Unity / RPG Maker / Godot later = implement one `GameEngine` + register it. No core rewrite.
- No plugin ABI, no dynamic loading, no event bus, no DI framework. Plain structs + traits.

## 2. Project structure

```
src/
├── main.rs                 # composition root: DB, engine registry, UI wiring
├── core/
│   ├── project.rs          # Project model
│   ├── source.rs           # SourceEntry
│   ├── translation.rs      # TranslationEntry + TranslationStatus
│   ├── context.rs          # ContextWindow, DialogueContext, prompt formatting
│   ├── glossary.rs         # GlossaryEntry + term matching logic
│   ├── engine.rs           # GameEngine trait
│   └── scan.rs             # project scan + incremental scan orchestration
├── engine/
│   ├── mod.rs              # engine registry
│   └── renpy/
│       ├── mod.rs          # RenpyEngine (detect/extract/export impl)
│       ├── parser.rs       # line-level .rpy parser (say/menu/old-new)
│       ├── extractor.rs    # walk .rpy files -> SourceEntry list
│       └── exporter.rs     # in-place string rewrite, minimal diffs
├── ai/
│   ├── mod.rs
│   └── provider.rs         # TranslationProvider trait + OpenAI-compatible impl
├── database/
│   ├── mod.rs
│   └── sqlite.rs           # schema + repositories (rusqlite, bundled)
├── translation/
│   ├── mod.rs
│   ├── pipeline.rs         # batch translation, validation, bounded concurrency, cancel
│   └── memory.rs           # translation memory lookup/store
└── ui/
    ├── mod.rs              # Slint <-> Rust glue (background threads, models)
    └── app.slint           # single window: Project / Glossary / Settings tabs
```

## 3. Core data models

```rust
pub struct SourceEntry {          // engine-independent (no renpy_* fields)
    pub id: String,               // "{project_id}|{rel_path}|{line}"
    pub engine_id: String,        // "renpy"
    pub file_path: String,        // relative to project root
    pub line: u32,
    pub speaker: Option<String>,
    pub source_text: String,      // unescaped dialogue text
    pub source_hash: String,      // sha256(source_text)
    pub context: Option<String>,  // scene/label name when known
}

pub enum TranslationStatus { Pending, Translated, Edited, Failed }

pub struct TranslationEntry {     // what the engine exporter consumes
    pub source: SourceEntry,
    pub translated_text: Option<String>,
    pub status: TranslationStatus,
    pub updated_at: i64,
}

pub struct GlossaryEntry { id, source, target, note: Option<String>, enabled: bool }
pub struct Project { id, name, path, engine_id, source_language, target_language,
                     created_at, updated_at }
```

Rules:
- `id` is stable per location; text identity is `source_hash`. If a game update moves text to
  another line, the new row is prefilled from Translation Memory by hash.
- Manual edits set status `Edited`; automatic translation never overwrites `Edited`/`Translated`.
- Failed validation keeps the AI attempt visible but status = `Failed`; export skips it.

## 4. SQLite schema

Per the spec, plus `settings` (key/value) and practical indexes. `rusqlite` with the
`bundled` feature; WAL journal; all batch writes in transactions with prepared statements.

```sql
CREATE TABLE IF NOT EXISTS projects (... per spec ...);
CREATE TABLE IF NOT EXISTS sources (... per spec ...);
CREATE TABLE IF NOT EXISTS translations (
    source_id TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    translated_text TEXT,
    status TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS glossary (... per spec ...);
CREATE TABLE IF NOT EXISTS translation_memory (... per spec ...);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_sources_project_file ON sources(project_id, file_path, line);
CREATE INDEX IF NOT EXISTS idx_sources_hash ON sources(source_hash);
CREATE INDEX IF NOT EXISTS idx_glossary_project ON glossary(project_id);
CREATE INDEX IF NOT EXISTS idx_tm_lang ON translation_memory(target_language);
```

`Db` wraps `Mutex<Connection>` (Connection is `Send`, guard makes it shareable across
worker threads without global mutable state).

## 5. Translation pipeline

```
pending entries (status Pending | Failed)
  → Translation Memory lookup by (source_hash, target_language)   [Glossary > TM > AI]
  → batch remaining (configurable, default 20/request)
  → worker pool (bounded concurrency, default 2, max 8)
      per batch: filter glossary terms that actually appear (word-boundary matching)
                 build context per item (speaker / previous N / next N from DB)
                 call provider, map response by stable id
                 validate → save (Translated) or mark Failed
  → every successful translation is stored into translation_memory
```

- Cancellation: `AtomicBool` checked between batches; UI exposes Cancel.
- Progress: done/total callbacks pushed to the UI thread via `upgrade_in_event_loop`.
- The provider trait is sync (workers are plain threads); bounded by a work queue.
  This keeps the app tokio-free and lightweight. Async can be introduced later behind
  the same trait without touching the core.

## 6. Context system

- `ContextWindow { before, after }` (default 1/1, configurable in Settings).
- Context is resolved from the DB: entries of the same file ordered by line; includes
  speaker, scene/label name, previous N texts, next N texts.
- Formatted into each batch item (`speaker`, `previous[]`, `next[]`) and shown in the
  editor panel; the prompt template is user-configurable.

## 7. Glossary system

- Project-scoped entries: source term, target term, note, enabled flag.
- **Matching** (not blind substring replace): Latin/alnum terms match on word boundaries
  (Unicode `\b`); terms containing punctuation/CJK match as substrings. Matching is used
  for (a) building the glossary block sent to the AI, (b) response validation.
- Priority: Glossary → Translation Memory → AI. Changing a glossary entry affects future
  translations + validation immediately; bulk retranslation stays manual (re-run on
  Failed/Pending), the data model needs no further support.

## 8. Ren'Py adapter design

- **Detect**: `<root>/game` (or root) contains `*.rpy` files.
- **Extract** (line-based parser, comments/indentation preserved by never rewriting files):
  - `e "Hello."` → say with speaker (also `e happy "..."` attributes, `extend "..."`)
  - `"Hello."` → narrator say
  - `"Choice text":` / `"Choice text" if cond:` (inside `menu:`) → menu choice
  - `old "Hello"` / `new "สวัสดี"` pairs (translate blocks) → source = old text;
    existing `new` text is imported as an `Edited` translation
  - speaker display names resolved from `define e = Character("Eileen")`
  - excluded statement keywords (if/return/show/play/...) never match as speakers
  - `label start:` → stored as scene context
  - escaped strings (`\"`, `\\`) unescaped on extract, re-escaped on export
- **Protected tokens**: `[player_name]`, `{b}`, `{/b}`, `{color=#fff}` — extracted by
  regex from each source text (via `GameEngine::protected_tokens`) and validated after AI.
- **Export**: rewrite only the string literal at the recorded line:
  - say/choice lines: replace the string in place, keep indentation + trailing text
  - `old` lines: rewrite/insert the adjacent `new` line
  - safety: if the line's current text no longer matches the recorded original, the
    entry is skipped (counted), never written blindly
  - CRLF/LF line endings preserved; files are touched only if something changed

## 9. Threading & performance

- UI thread never does IO: scan / translate / export run on `std::thread` workers,
  results pushed to Slint via `upgrade_in_event_loop`.
- SQLite: WAL, prepared statements, transactions; UI lists use pagination
  (never loads the whole project into the UI model).
- Translation: bounded concurrency (2–4), batched requests (10–30), cancelable.

## 10. Deliberate v1 deviations (kept lite)

- `TranslationProvider` is a sync trait executed on worker threads instead of
  `#[async_trait]` — avoids pulling tokio into a "lite" app; the trait boundary stays.
- Source entry id embeds the project id and location; text identity is the sha256 hash.
