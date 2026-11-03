Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Record what the CodeRabbit review-body items on older PRs left open.

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
- **A `ws://` endpoint carries the access key in cleartext** (CodeRabbit on #30, outside the diff). `authorized_wss_url` in `src/transport.rs` accepts `ws://` as well as `wss://` and puts `authorization`, which is base64 of `user-agent:access_key`, in the upgrade URL's query, before Noise starts. The Python reference (`_hive.py`, "WSS endpoint must start with ws:// or wss://") and the Go port (`transport_wss.go`) accept `ws://` the same way, so refusing it only here would break parity. The access key alone does not authenticate: the Noise pre-shared key comes from the password (`derive_psk`, Argon2id), and the client key is pinned. Platform identities name `wss://` endpoints. Requiring `wss://` would be a cross-SDK decision, for every port at once.
- **Tests not yet written, because unit tests live under `src/`, a release path.** Each would publish a crate with no code change, so they wait for the next change to `src/`:
  - The usual-form retry after an empty first listing (CodeRabbit on #66). `en-CA` against a hub that loaded `en-us` keeps `languages == ["en-CA"]` and reports `listed_in == ["en-us"]`; with `nearest: false` it lists once and reports `["en-CA"]`. Checked on 2026-11-03 with a test that was run and not committed, against `FakeHub` in `src/intents.rs`; it passes on `main` (a81ec4a). The tests in place cover the keyword-only retry (`a_tag_with_only_keyword_rows_is_listed_under_its_usual_form`) and the nearest-off listing (`the_hubs_own_listing_is_not_asked_when_nearest_is_off`).
  - Single `read()` test servers (CodeRabbit on #21). The provisioning tests read whole requests through `read_full_request` (since 8a27279). About ten older test servers in `src/control.rs` still take one `read()` and then assert on the body, for example `login_sends_mfa_fields_only_when_provided`. They can fail if a request's body arrives in a later TCP segment. No CI failure has been seen from it.
- **Conformance shards on platforms without a parent process id**. Outside Unix, a run is told apart only by `THALOVANT_CONFORMANCE_RUN` or a deleted target, as before. CI records conformance on Linux.
