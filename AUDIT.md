Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Start the audit log with the CodeRabbit follow-ups to #64 and #66.

# Audit

Known issues, technical debt and their status, from test runs and code reading. Each entry names the code it concerns.

## Fixed

- **Conformance record could carry an earlier run's cases** (CodeRabbit on #64). `record()` in `tests/common/mod.rs` cleared stale shards only when `THALOVANT_CONFORMANCE_OUT` did not exist yet. The documented target, `contracts/conformance-results.json`, is committed, so it always exists. Shards are now scoped to a run, and `tests/conformance_record.rs` fails if an earlier run's shard reaches the record.

## Open

- **`usual_form` differs from the Python reference on two kinds of tag** (found while checking CodeRabbit on #66, by running both on the same inputs; not changed here):
  - A tag with an empty subtag: `"en-"`, `"en--US"`, `"en-US-"`. The reference returns `None` because any empty subtag ends it (`thalovant/_language.py`, `usual_form`). `src/language_matching.rs` `usual_form` returns `Some("en-us")`.
  - A language CLDR knows only through its macrolanguage: `"arb"`, `"cmn"`. The reference returns `arb-eg` / `cmn-cn`, checks the macrolanguage's likely subtags and keeps the language's own code. Rust returns `None` because `DATA.likely` has no direct entry.
  - The Go port gives the same answers as Rust on these inputs. No shared vector covers `usual_form`, so the parity gate does not see either difference.
  - Dangling singletons (`"en-u"`, `"en-x"`) already match the reference (`en-us`).
- **Conformance shards on platforms without a parent process id**. Outside Unix, a run is told apart only by `THALOVANT_CONFORMANCE_RUN` or a deleted target, as before. CI records conformance on Linux.
