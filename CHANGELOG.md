# Changelog

## 0.14.0 — 2026-09-28

The Python reference's 0.9.1 round: sign in as a registered app, read a code before approving it, tell a hub that refused this client's own key from any other refusal, and keep an identity file's key beside it. Everything is additive. No public function changed its signature, no public struct gained a field, and no error that existed changed its variant: a hub that refuses this client's key is still the `ThalovantError::Connection` refusal it was, with a new classifier to tell it apart.

- `ControlPlane::begin_device_login_as(scopes, client_name, client_id)` signs in as a registered app, such as `HOME_ASSISTANT_CLIENT_ID` (`thalovant-home-assistant`). The approval screen then shows the platform's own name for the app as verified, and approving it again replaces the token the app already holds instead of counting another against the plan. `None` leaves `client_id` out, and `begin_device_login` is that call. An id the API does not know is refused with 400 `unknown_client`, returned as the `ApiResponse` it is. `login_with_browser_as(options, client_id)` does the whole flow the same way: a method rather than a field on `DeviceLoginOptions`, whose public fields a literal names.
- `ControlPlane::describe_device_login(user_code)` reads a pending code the way the approval screen does, signed in as the person approving it: `GET /v1/auth/device/codes/{user_code}`, with the code percent-encoded. The new `DeviceLoginRequest` (`#[non_exhaustive]`) carries `scopes`, `client_name`, `client_id`, `client_verified`, `device_name` and `expires_at`. `client_verified` is true only when the API says so and names the app; an unknown, expired or answered code is a 404.
- A close is a refusal only while the hub has said nothing. `close_refuses_after(code, closed_after_handshake, code_late, after_authenticated_frame)` is the rule a transport now reads a close by, and `close_refuses` is that rule for a hub that has sent nothing. A hub refuses a client's key before it sends anything, so once any frame from it decrypts under the new session's keys (JSON, WIRE-1 binary, a chunk of a larger message, its encrypted HELLO), a close with 1000, 1005 or 1008 inside the 750 ms window is a drop.
- A hub that closes the link right as an XX handshake ends, before it says anything, pinned a different key for this connection when it first connected. The error is still a refusal (`is_hub_refused()`), and the new `is_client_key_rejected()` is true for it. Its message names the folder this client's key is in and the other likely one, which `client_key_folders()` returns, and says to re-pair or share the key folder. After KK the same close is a plain refusal, since the hub could only complete KK with the key it pinned. `HubSession::run` stops on it at once. `LinkOutcome::ClientKeyRejected` is new: the enum is `#[non_exhaustive]` since 0.13.0, so a match on it already has a wildcard arm, and `LinkSupervisor::after` gives up on it at once with that reason.
- Over HTTPS the same verdict is a request answered 401 or 403 while the client sends the last frames of the XX handshake it completed, or right after, before anything from the hub decrypted: `connect()` returns the rejected-key error, and `RuntimeTransport::closed_refused()` now reports such a refusal for an HTTP link too, where it was always `false`. MQTT cannot see the case: a hub that refuses a key over MQTT just stops answering.
- An identity read from a file keeps its Noise key beside that file. `Client::from_file`, `from_config_file` and `from_config` put `noise_key` and `noise_pins.json` in the file's folder, so every program that reads one identity file presents one key. `~/.config/thalovant/identity.json` keeps its key exactly where it was, since that is the default folder. For a file elsewhere, the first handshake from its folder copies the key and the pins from the default folder when that key has met this hub (a pin filed under the hub's node id), and never moves them. The pins go first and the key last, and a copy that fails writing fails that connect rather than going on with a key of its own, so the next one copies again. A folder the process cannot write keeps using the default, and `set_noise_state_dir` overrides the whole rule, as it always did. `Identity` is unchanged.
- A compressed part of a WIRE-1 frame inflates to at most 32 MiB (`wire::MAX_INFLATED`). One that would inflate further is refused, where a small frame of zeros could make the client allocate gigabytes; a truncated stream was already refused and still is.
- A send can be withdrawn only while it waits for the link. Once it holds the writer (WSS) or the send path (HTTP, MQTT), it is written to the end on a task of its own, bounded by the 20 s send timeout, even if the caller's future is dropped: a reply whose bound landed in the middle of its frames used to leave half a message on the wire and spoil the session. A `disconnect()` still stops such a write, since that link is being torn down: a write stalled on a hub that stopped reading no longer holds the disconnect, or a reconnect, for the whole 20 s. A reply withdrawn while queued is still never sent late.
- An error's display line and its `Debug` print every validation error's `input` as `[omitted]`, in each entry of a `detail` list and of an `errors` list. That field is the request as sent, under any key and in any shape, so no key-name filter could keep an echoed value out. `api_problem()` still returns the body as sent.
- Takes the reference's `d33dc2be8b00` digest. Re-vendors `api-error-vectors.json`, `device-login-vectors.json`, `home-link-vectors.json` and `link-keeping-vectors.json`, and adds `link-carrier-vectors.json` for the new `link-carriers` capability. Those cases run through the real HTTPS polling and MQTT transports against loopback fixtures that pin the client's key and, over HTTPS, answer every request of an aborted session with 401. The handshake cases now connect the way a kept link does, the handshake and then the settle window. The `queued` home-link cases run over a real link whose writer another frame holds. All case and vector digests in `contracts/conformance-results.json` equal the reference's.

## 0.13.0 — 2026-09-27

The Home Assistant link: sign in on a device, create a connection of a named kind, wait for the hub to admit it, keep a link up, and answer the hub's requests. Everything is new API, and no existing call returns a different variant: the finer kinds of failure below are told apart by new methods on the errors the SDK already returned. The one return that changes is the device-poll fix below: a failed poll now keeps the status and body it used to drop.

- Device sign-in one step at a time, for a caller that runs its own loop (a Home Assistant config flow shows the code, then polls on its own schedule). `ControlPlane::begin_device_login(scopes, client_name)` returns the `DeviceAuthorization` to show, refusing a verification URL that is not HTTP(S), has no host or carries credentials; an empty scope list is left out, as the API wants. `poll_device_login(&authorization)` asks once: the approved `ApiToken` (token type, scopes, `expires_at`, `token_id`; its `Debug` never prints the token), kept on the control plane with its id on `token_id()`; or `ThalovantError::DeviceLoginPending { interval }`, `DeviceLoginExpired` or `DeviceLoginDenied`, each with the status and body the API sent. A `slow_down` adds five seconds to that device code's interval for good. `revoke_api_token(None)` revokes the token the control plane signed in with and forgets it; a token may always revoke itself. Revoking the token in use is idempotent: one already revoked or expired cannot authenticate its own revoke, and the API's 401 counts as revoked, so it is forgotten either way, and revoking again sends nothing until the next sign-in. Like every sign-in it takes `&mut self`, so no sign-in can store a new token while a revoke is in flight and have it forgotten in the old one's place. Revoking another token by id still returns the API's 401 or 404. Every sign-in -- password, native, browser and device -- sets `token_id()` from its own answer, so a default revoke never reaches a token signed in with before. `HOME_ASSISTANT_SCOPES` is `hubs:read`, `clients:read`, `clients:write`, all a Free plan can approve. `login_with_browser` is now built on these; it returns what it always returned for a denied or expired code, and now also keeps the token's id.
- Fix: a device poll that failed any other way returned `ThalovantError::Api` with an `HTTP 503 ...` line and dropped the status and the body. It now goes through the one error builder and returns `ThalovantError::ApiResponse`, so `status_code()`, `api_code()`, `api_detail()` and `api_problem()` read it like any other refusal. A caller that matched `Api(_)` for a failed `login_with_browser` poll matches `ApiResponse { .. }` now.
- A control-plane request that never got an answer -- DNS, the connection, TLS, a proxy, the request's timeout -- is still `ThalovantError::Api`, and the new `is_api_unreachable()` tells it from every other `Api` error, so "the API is out of reach, try again later" no longer has to be read out of a message. `is_connection_error()` is true for it. A request that could not even be formed is `Api` and not unreachable.
- `create_client_identity_of_type(hub, connection_type, options)` creates a connection of a named kind, such as `CONNECTION_TYPE_HOME_ASSISTANT`, sent as `spec.connection_type`. The answer must repeat the kind: when it does not, the API made an ordinary connection nobody asked for, so it is deleted (`If-Match:` the answer's etag) before the call fails with the new `ThalovantError::UnsupportedConnectionType`; a 422 that names `connection_type` in its `detail` or `code`, or in a validation error's `loc` or `msg` (under `errors`, or under `detail` when that is a list), fails the same way, keeping the status and the body. Never the rest of the body: a validation error about any other field echoes the request, `spec.connection_type` included, as its `input`. It is a method rather than a field on `BootstrapIdentityOptions` so that no options literal stops compiling. `BootstrapIdentityResult::client_id()`, `connection_type()` and `operation()` read the answer it already holds.
- Refusals are classified, not retyped: `ThalovantError::api_refusal()` returns `ApiRefusal::Auth` (401, 423, 403 `Insufficient scopes`), `ApiRefusal::Plan` (402, 403 `plan_limit`) or `ApiRefusal::AlreadyLinked` (409 `home_assistant_already_linked`), and `linked_client_id()` names the connection that holds a hub's link. A 402 from `get_hub` or any other call is still `ThalovantError::ApiResponse`. `status_code()`, `api_problem()`, `api_code()` and `api_detail()` also answer on the new variants that come from an API answer.
- `get_client(id)` and `delete_client(id, etag)`. Without an etag the delete reads it first; a 412 reads it again and retries once; a 404 on either request counts as deleted.
- `wait_for_admission(operation, timeout, poll_interval)` follows the operation a create answered with (`DEFAULT_ADMISSION_TIMEOUT` 180 s, `DEFAULT_OPERATION_POLL_INTERVAL` 2 s), bounding every read by its deadline. `ready`, no operation, and a 404 return. `failed` and `timed_out` fail with `ThalovantError::AdmissionFailed` carrying the operation's `error_code` and no status. A 401 or 403 is the token, not the connection, and is returned as the `ApiResponse` it is; any other refusal of the wait is `AdmissionFailed` with no `error_code`, keeping its status and problem. A 5xx is ridden out, and so is a 429 (a Free plan allows 60 requests a minute): the next poll waits the problem's `retry_after_seconds` (at its top or inside a `detail` object), else the `Retry-After` header, else `RateLimit-Reset` -- the API's own rate limiter answers in plain text with only those -- or the poll interval when that is longer, and a 429 asking for longer than is left ends the wait as a timeout at once. Running out of time is `ThalovantError::AdmissionTimeout`, which the new `is_timeout()` and `is_connection_error()` both answer `true` for, and whose message ends "it may still admit it later". An API out of reach is returned as it is, the `Api` error `is_api_unreachable()` answers `true` for. A `links.self` on another origin than the API's -- scheme, host and port -- is never fetched.
- The `home` module answers each `thalovant.home.request` at most once with a `thalovant.home.response`, never after the hub's 10 seconds counted from its arrival. `answer_home_requests(&session, handler, timeout)` answers every request a `HubSession` receives, each on its own task, until the returned subscription is stopped or dropped, which also cancels answers still running. `answer_home_request` answers one within the hub's bound, and `answer_home_request_within` within one you give; both return the payload sent, or `None` when there was no time left. The handler runs on a task of its own and gets its timeout (`DEFAULT_HOME_HANDLER_TIMEOUT`, 9 s) or what is left of the bound, whichever is less; when that runs out the `timeout` answer goes out at once and the handler is left to finish. The reply gets what the handler left, is never started after the bound, and is withdrawn when the bound passes. A handler that returns an error or panics is answered `failed_to_handle`, and one outside `home::RESPONSE_TYPES`/`home::ERROR_CODES` `unknown`, with empty speech: the hub speaks its own sentence in the device's language. `plain_speech` removes markup, decodes the portable set of references (`decode_references`: numeric ones, the five XML entities and `&nbsp;`, nothing else) and collapses Unicode White_Space. `HomeRequest`, `HomeAnswer` and `home_response` are public for a caller running its own loop.
- `strip_ssml` removes only real markup: a tag is `<` or `</` followed by an ASCII letter, up to the next `>` outside a quoted attribute value, plus comments and processing instructions. "5 < 6 and 7 > 3" survives whole, where it used to lose everything between the two signs; `Event::display_text` and `Reply::display_text` read it the same way.
- `reply_context(&context)` builds a reply's context (OVOS-MSG-1 §5.2): a deep copy with `destination` set to the old `source`, and `source` to the old `destination`, its first entry when a list; a context with a destination and no source gives a reply with no destination. `Client::reply(event, msg_type, data)` and `HubSession::reply` send one, from the context exactly as the hub sent it, which is what `Event::context` holds. The `Replier` trait lets `answer_home_request` take either.
- A hub that refuses a link now says so at `connect()`, still as the `ThalovantError::Connection` a failed connect has always been. The new `is_hub_refused()` is true for a Noise handshake message that does not authenticate under the key the password derives (a wrong password), a close with 1000, 1005 (no status) or 1008 during the handshake, and a WebSocket upgrade or an HTTP request answered 401 or 403; the new `is_hub_key_changed()` is true for a hub whose Noise key is not the one pinned for it, which is not a refusal. The two kinds ride in the error's message, which begins with a fixed phrase the SDK alone writes, rather than in variants of their own: a new variant would have moved these failures out of every existing `Connection(_)` arm without a compiler error to say so. The SDK never replaces a pin itself. A KK attempt that fails either way is followed at once, inside the same connect, by one XX attempt, whose outcome is the connect's: only XX tells a changed password from a changed hub key, and the pin is still checked when it completes.
- `close_refuses(code, closed_after_handshake, code_late)` is the rule a transport reads a close by: `REFUSAL_CLOSE_CODES` (1000, 1005, 1008), during the handshake or within `REFUSAL_SETTLE` (750 ms) after it, with a code learnt up to `CLOSE_CODE_GRACE` (250 ms) late; everything else is a drop. `WssTransport::closed_refused()` and `RuntimeTransport::closed_refused()` report it for the link's last close, and a transport now wakes whoever waits on it when its link goes down.
- `HubSession` keeps a long-lived link by itself. `on(event_type, handler)` calls a handler for every event of one type on every client the session builds, each on its own task; `off(id)` removes it and cancels what it still has running. `on_state_change(callback)` hears the link go up and down. `connect()` makes one attempt and counts a new link only once it has stayed up for the settle window (`DEFAULT_SETTLE_WINDOW`, 0.75 s): a hub that does not know the connection's key says so only by closing right after the handshake. `run()` stays connected until `close()`, asking a `LinkSupervisor` after every attempt: a link that comes up resets the ladder and the refusal clock, a drop is dialled again at once, a failure waits the retry ladder (10 s doubling to 120 s), refusals are retried the same way until they have lasted `DEFAULT_REFUSAL_GRACE` (600 s), and a changed hub key ends it at once. `LinkSupervisor::after(outcome, now)` is a pure function, so an application can drive its own loop by the same rules. `HubSession::for_identity(identity, policy)` builds the factory. The crate still logs nothing.
- Declares the parity contract's four new capabilities -- `device-login`, `connection-kinds`, `connection-admission` and `home-link` (with `link-keeping-vectors.json`) -- run against the Python reference's vector files, vendored byte for byte under `tests/conformance`. `tests/home_link.rs` serves each HTTP case from a loopback API through the real `ControlPlane` path, with the case's headers, and checks every request; `tests/link_keeping.rs` runs the close rule and the supervisor, and the handshake cases run a real Noise handshake against a loopback hub beside the transport's own fixtures. What each produced is recorded in `contracts/conformance-results.json`: all 128 case digests (device-login 13, connection-kinds 15, connection-admission 18, home-link 30, link-keeping 29, and the api-error, binary and conversation records already there) and all eight vector digests equal the reference's. Every duration in the vectors is whole milliseconds, so the recorder keeps its rule: a whole number within 2^53, or nothing.

## 0.12.0 — 2026-09-26

- `ThalovantError::ApiResponse` carries what the API said, not only the line built from it. Its new `problem` field is the whole error body parsed, when it is a JSON object, as an `ApiProblem`: `code()` is its machine-readable code, `detail()` its sentence whole, exactly as sent, and it derefs to the `serde_json::Map`, so every structured field is reachable. `ThalovantError::api_problem()`, `api_code()` and `api_detail()` read them from any error. The display line was the only place any of this reached a caller, and it is cut at 200 characters: a `platform_image_required` refusal names every image each refused key may be instead, which is longer than that, so the list a caller needed was the part cut off -- and `refused_images`, `allowed_images` and `allowed_repositories` never reached anybody at all. The same held for every structured refusal, `plan_limit`'s `resource`, `limit` and `used` included. The display line itself is unchanged, and still never repeats a value the body echoed back from the request.
- `code()` and `detail()` are also read from inside a `detail` that is itself an object -- FastAPI's own envelope, which the API's Problem+JSON handler normally lifts.
- `ApiProblem`'s `Debug` redacts secret-named keys, so `{:?}` and an `unwrap()` panic never print a password a validation error echoed back. The map itself holds the body as sent. The field is boxed so the enum every `Result` carries stays inside clippy's `result_large_err` limit.
- An error body is decoded as UTF-8 whatever its Content-Type says; the API sends `application/problem+json` with no charset.
- **Breaking:** `ThalovantError::ApiResponse` gained a `problem` field, so constructing it by literal or matching it without `..` no longer compiles; add `problem: None`. Hence 0.12.0 rather than a patch.
- Declares the parity contract's new `api-errors` capability, run against the Python reference's `api-error-vectors.json`: thirteen responses, from the image and plan refusals the API sends to a body that is HTML, empty, or JSON that is not an object, each served by a loopback HTTP peer and read back through `get_hub`, with what it produced recorded in `contracts/conformance-results.json`.

## 0.11.0 — 2026-09-18

- A refusal ends an ask at once instead of letting it run to the deadline. The hub sends `hive.policy.denied` the instant it refuses, with no request id, and the request-id gate dropped it: the ask waited out its whole budget while a caller told somebody their hub "did not answer in time" about a question it had refused and explained. A denial with no request id is taken when it names the type this ask sent and this ask is the only utterance the client has out; a second ask, a query, or a fire-and-forget utterance still inside the shared grace window makes it ambiguous, so neither takes it.
- `ThalovantError::PolicyDenied` carries `quota` -- period, limit, used, reset_after -- for a spent `intent_quota_exceeded`, and its message fits the refusal rather than offering allow-list advice for a spent day or for `backend_unavailable`. The field is boxed so the enum every `Result` carries stays inside clippy's `result_large_err` limit.
- `ThalovantError::Unanswered` is new: `ovos.intent.unmatched` is the hub understanding a question and having nothing for it, which is not a failure. The enum is `#[non_exhaustive]`, so a caller with a wildcard arm keeps compiling.
- `allowed` holds only non-blank, trimmed strings; quota counts are whole, never negative and never past a signed 64-bit integer.
- `ThalovantError::Unanswered { said }` carries what the person said. Both event names put the input in the event's text; the old read of `reason`/`error` left it empty.
- A fire-and-forget utterance is recorded once the connection is up and immediately before the publish, so the grace window is not spent on a handshake; a connect that fails records nothing, and a publish that errors keeps its record, because `emit_bus` over HTTP can fail after the hub already holds the frame. The deque is pruned as entries are added, so a client that only ever sends does not keep them for its lifetime.
- A refusal on a quota the hub sent no numbers for says a quota has run out, rather than claiming "all questions used".
- **Breaking:** `ThalovantError::PolicyDenied` gained a `quota` field, so a match that names every field without `..` no longer compiles. Hence 0.11.0 rather than a patch.
- Declares the parity contract's new `refusal` capability, run against the Python reference's `refusal-vectors.json`.

## 0.10.0 — 2026-09-16

- **Breaking for code that constructs `Client` or `HiveMessage` by literal.** Both gained fields this release -- `Client` the conversation cache and its sequence, `HiveMessage` a `binary` payload. The new fields are `pub(crate)`, so they are invisible outside this crate, but their presence makes an existing `Client { identity, transport }` literal and any exhaustive `HiveMessage` pattern fail to compile. Build them through `Client::new` and the transport constructors. 0.5.2 deliberately preserved public `Client` literals; the 0.9.x-to-0.10.0 boundary is where that changes.
- One conversation, however many session ids reach it. The request id and the id a hub answered with were filed as separate entries, so they aged and were evicted separately: with the cache full, storing the second could evict the first and a caller continuing under the id it sent found no carry. Aliases are now one group with one place in the bound, capped so a hub that re-translates the id every turn cannot grow a group for ever. An empty scalar is no longer carried state, matching the reference.
- `NativeSignIn`'s `Debug` redacts the whole shape of a URL that can carry a secret -- userinfo and fragment as well as the query -- for `redirect_uri` as well as `authorization_url`. Only emptiness is rejected when a redirect is supplied, so it can arrive carrying any of them.

- Speak the rest of the HiveMind protocol. A hub relays more than this client's conversation, and the five hive kinds -- `broadcast`, `propagate`, `escalate`, `intercom`, `rendezvous` -- fell off the end of `dispatch_noise_message` with no arm and no log line. `subscribe_hive` listens to one kind, and `propagate`, `escalate` and `broadcast` send. A refusal is a disconnection rather than an error: a hub's HELLO says nothing about what a client may do, so nothing can check first.
- Receive binary frames. This is how a hub answers `speak:synth`: it renders the utterance and sends the audio back, so a client with no synthesiser of its own can still speak, and it is how a file arrives. `decode_hive_binary_frame` read the WIRE-1 header and then JSON-parsed the payload, so a frame carrying raw audio failed; it now reads the four payload-type bits and hands over the clip untouched, and `subscribe_binary` delivers it. Checked against `binary-frames.json` -- hivemind-bus-client's own encoder output, not frames this SDK built for itself.

## 0.9.0 — 2026-09-13

- Expose advisory reply claim status and first-seen pipeline/skill identifiers, with shared conformance for fallback, mixed stages, legacy hubs and malformed stamps. Existing reply construction remains compatible.

- Add managed hub sessions with persistent subscriptions, bounded background retry backoff, terminal close, and no automatic replay of admitted requests. Preferred-origin selection delegates address binding and failed-attempt cleanup to the transport builder.
- Add presentable skill/intent inventories, regional example selection, tri-state catalogue locale support, and private best-effort inventory caches. Explicit language order survives JSON serialization across SDKs; invalid cache records become misses.
- Match Python question detection, including unnamed-locale patterns and Unicode question marks, with shared executable conformance vectors.
- Require reviewed Python reference and consumer evidence in PR and publishing parity checks, with scheduled fresh-dependency conformance checks.

## 0.8.1 — 2026-09-12

- Refresh bundled listing rules to thalovant-languages 0.2.1, matching Python 0.6.8 across 270 languages. Preserve regional inheritance and the corrected French/Spanish trailing-word behavior.
- Regenerate public-reference cases for every shipped locale, including Spanish questions and French complete phrases.

## 0.8.0 — 2026-09-12

- Add locale-aware sentence listings, canonical slot examples, fuller phrase ranking and OVOS-compatible regional language selection.
- Own custom rule data, retain the selected locale and count unique rendered examples toward limits.
- Bound regex backtracking; expose matching failures and keep sentence output bare when a rule cannot be evaluated safely.

## 0.7.1 — 2026-09-12

- Reject lossy floating-point values in guarded config/personas merges, including stored JSON integer overflow. Preserve native signed and unsigned 64-bit integer values exactly.

## 0.7.0 — 2026-09-12

- Match Python 0.6.3 request hints, location construction, ordered embedded audio replies, strict bounded hex decoding, and speakable intent examples with original phrase priority.
- Default runtime configuration updates to revision-guarded deep merges. Retry only HTTP 412 (three attempts maximum); fail before writing against older servers. Explicit replacement remains available. Merging now requires both hubs:read and hubs:write scopes, plus a paid plan.
- Add regression coverage for conflict preservation, retry limits, unsupported revisions, audio bounds, caller context preservation, and example ranking.
- HTTP response failures use the additive `ApiResponse` error variant with `status_code()`. When constructing `Reply` literals, supply `dropped_media: 0`.

## 0.6.0 — 2026-09-12

- Bound in-flight skill status requests by the wait deadline, preserving the accepted operation ID on timeout.

- Add hub-addressed skill listing, history, install, update and removal.
- Add optional bounded polling and explicit operation resumption without repeating accepted writes.
- Document shared-runtime scope, authorization and cancellation behavior.

## 0.5.2

- Validate saved Noise pin values and pin inputs before loading or changing trust. Preserve malformed files; enforce the documented first-contact rule atomically in `save_noise_pin`, with explicit `forget_noise_pin` for verified rotation.

- Reject duplicate active Ask request IDs and Query query IDs on a shared
  transport, including Client clones, without changing public Client literals.
- Omit unstructured API error bodies that can echo credentials. Preserve
  redacted JSON diagnostics and report peer WSS close status as a connection
  failure instead of a misleading handshake timeout.
- Document historical MQTT field migration and the complete live interop setup.
- Reject blank hub ETags before sending update or delete requests.
- Keep describe timeouts when earlier replies contain no definitions, while
  retaining explicit empty answers and useful partial descriptions.
- Correct idempotency retry and quota documentation; make the recording HTTP
  fixture read complete bodies and compare install JSON independently of key order.

## 0.5.1

- Recover HTTP cleanup after a lost disconnect response by accepting successful JSON responses containing exactly one `error` field with `Already Disconnected` or `Client is not connected`. Additional fields such as `ok` or `status` invalidate these idempotent acknowledgments. Preserve replica affinity and Noise trust across explicit cleanup retry or reconnect; other errors, unsuccessful HTTP responses and `ok: false` remain failures.

## 0.5.0

- Share one absolute timeout across every intent-description window, retain earlier answers, and stop publishing after the budget expires.

- Add `listen`, `wait_for_event`, `ListenOptions`, and `EventStream::recv` with scoped correlation, total deadlines, predicate filters, limits, cancellation-safe subscription ownership, and explicit overflow or disconnect errors.
- Refuse control-plane redirects, including 307/308 login-body replay, require HTTPS for authenticated requests and request bodies outside explicit loopback development endpoints, and remove request URLs from transport errors.
- Validate device verification URLs before prompting or launching a browser; allow only HTTP(S) without embedded credentials and use direct platform commands without a shell.
- Flush complete MQTT broker-fixture packets before waiting for more input, with a buffered-writer regression and native TLS coverage on Windows.

- Raise the minimum Rust version from 1.85 to 1.88 and upgrade `time` to 0.3.55 to fix RUSTSEC-2026-0009 (RFC2822 parser stack exhaustion). HTTP cookie affinity requires this dependency; upgrade the compiler before upgrading the crate.

- Preserve complete Noise identities with atomic publication and OS locks across processes. Interrupted writers cannot publish partial keys, pin transactions cannot lose another process's changes, and malformed or exposed trust files remain errors without automatic reset. Bound OS lock waits and move handshake/store work off Tokio workers so caller deadlines remain responsive.
- Join concurrent connections only after authenticated Noise readiness. A joining caller owns its own deadline; cancellation of the initiating caller retires only that connection generation. Bound transport writes and the caller's cleanup wait, retain cleanup workers after cancellation, abort taken MQTT tasks safely, and retain unacknowledged HTTP admission for a later cleanup attempt.
- Report the first nonempty hub-assigned session from accepted Query events, falling back to the requested session when none is supplied, consistent with Ask.
- Collect Ask/Query replies concurrently with sending; hard failures immediately freeze partial replies. Ask returns received speech at its total deadline even if the write stalls, uses fixed first-event speech windows, reports buffer overflow, and cancels the owned write when collection ends.
- Include connection, send, and response collection in Ask/Query deadlines. Add `AskOptions` and `ask_with_options` for delayed speech and fragment collection, recover soft intent misses when speech arrives, and report empty completion as a timeout. Existing options struct literals remain compatible.
- Fall back to engine manifests when intent listing is denied or silent. Add `intents_with_capabilities`, `HubIntentCapabilities`, `HubFallback`, and `list_fallbacks`; distinguish unknown fallback support from a confirmed empty list and expose conservative `may_answer(language)`. The legacy `denied` field names an unavailable query and is not proof of an ACL denial.
- Exercise stable Rust on Linux, macOS, and Windows, test the declared Rust 1.88 minimum, and audit freshly resolved dependencies in CI. Add failure, cancellation, concurrency, interrupted-write, and fallback-discovery regressions.

## 0.4.8

- Drive WSS, HTTP, and MQTT through one Noise handshake, authenticated framing, and hub trust implementation. Preserve WSS writer ordering and cancellation poisoning, and keep HTTP/MQTT lifecycle ownership unchanged.
- Reuse the protected persisted PSK cache across all runtime transports. A rejected or abandoned unfinished handshake discards the derived cache entry while preserving the authenticated hub pin, so the next connection can derive from the current password.
- Share transport health reporting and validate cached-credential recovery, changed-peer rejection, and a canceled chunked WSS send followed by a fresh authenticated reconnect.

## 0.4.7

- Correct the declared minimum Rust version to 1.85, already required by the
  Noise cryptography dependencies. Rust 1.75 was an inaccurate compatibility
  claim in earlier package metadata.
- Keep HTTP cookie and URL dependencies on releases compatible with Rust 1.85,
  so a fresh dependency resolution does not unexpectedly require Rust 1.88.
- Test all targets and features on Rust 1.85.0 in CI as well as current stable.

## 0.4.6

- Implement the deployed HiveMind v3 Noise handshake for HTTP and MQTT. Offers
  alone no longer mark a transport ready, and all post-handshake HELLO/bus
  traffic is encrypted, including chunked messages and `encrypt=false` calls.
- Reset a previously admitted HTTP peer before reconnecting after failure;
  first connections never evict an unknown peer.
- Preserve HTTP replica cookies, use binary form/poll endpoints, reject redirects
  and JSON error responses, and expose a builder for custom HTTP trust roots.
- Add persistent Noise state directory and remote static key methods to HTTP
  and MQTT; preserve hub pins after authentication failures on every transport.
- Reset WSS cipher, handshake, HELLO and node state before same-object reconnects,
  join old readers, reject unauthenticated bus events, and decode authenticated
  binary bus frames. Interrupted cipher sends invalidate session readiness.
- Serialize MQTT ciphertext delivery, handle full-size Noise frames, and keep
  polling the broker while the ordered protocol worker exchanges messages.
  Broker failures require an explicit reconnect with a new session. Add TLS
  trust configuration and random broker connection IDs.
- Exercise real local TLS HTTP/MQTT and WebSocket responders, XX-to-KK reconnects,
  correlated encrypted replies, concurrent chunked messages, wrong credentials,
  unsupported offers, tampering, plaintext rejection, and changed-key refusal.

## 0.4.0

- **Breaking.** `wss` connections now perform the HiveMind v3 Noise handshake,
  and only that. A HiveMind-core 5.x hub accepts no other key exchange, so this
  release requires one; against an older hub the connection is refused rather
  than downgraded. `Noise_XXpsk2_25519_ChaChaPoly_SHA256` on first contact and
  `Noise_KKpsk0_...` once the hub's static key is pinned, with
  `25519_AESGCM_SHA256` supported where a hub prefers it.
- **Breaking.** `Identity::crypto_key` is gone, along with the whole `crypto`
  module (`encrypt_as_json`, `decrypt_from_json`, `encrypt_as_binary`,
  `decrypt_binary`, `runtime_crypto_key`) and the pre-shared handshake on all
  three transports. Hubs no longer issue a crypto key and v3 derives its
  pre-shared key from the `password`, so the field named a credential that no
  longer exists. `crypto_key` is still accepted and ignored when parsing an
  older identity file, and stays in the bootstrap redaction list so an older
  payload carrying one does not leak it. `https` and `mqtt` now rely on TLS for
  confidentiality, as they already did for everything the crypto key did not
  cover.
- The Noise pre-shared key is derived from `password` with argon2id
  (`t_cost=3`, 64 MiB, `p_cost=1`), salted with SHA-256 of the hub's node id. A
  transport caches it per hub, so only the first connection pays the few
  hundred milliseconds.
- New `noise` and `noise_store` modules: `derive_psk`, `canonical_json`,
  `select_noise_options`, `NoiseHandshake`, `NoiseSession`, and the on-disk
  state helpers `load_or_create_noise_key`, `load_noise_pin`, `save_noise_pin`
  and `forget_noise_pin`.
- Three files persist beside the SDK config file, all `0600`: `noise_key` (this
  client's static X25519 key), `noise_pins.json` (the hub keys it has pinned),
  and `noise_psks.json` (cached derived credentials indexed by hub node ID).
  `WssTransport::set_noise_state_dir` overrides the location; `forget_cached_psk`
  removes one derived credential without removing keys or pins.
- Trust on first use: the first hub key seen for a node id is pinned, and a
  later connection presenting a different key is refused with an error naming
  `forget_noise_pin`, rather than silently re-pinned. A failed `KKpsk0`
  handshake drops the stale pin, because `KK` needs each side to hold the
  other's key and the failure is as likely to mean the hub no longer has this
  client's.
- `WssTransport::remote_static_key` reports the hub's static key for the
  current session.
- A `wss` connection the hub refuses now fails with the close reason instead of
  running out the handshake clock: a wrong password reported as a timeout hid
  what had actually happened.
- The WSS transport takes the writer lock *before* encrypting. `encrypt_message`
  advances the cipher state nonce counter and the hub decrypts strictly in
  counter order, so encrypting outside the lock let two concurrent senders
  consume nonces in one order and reach the wire in the other -- which the hub
  treats as tampering and drops the session for.
- The MQTT handshake no longer requires the legacy `preshared_key` capability
  flag, which a v3 hub does not set. Requiring it rejected the handshake and
  left `connect()` to time out.
- Trust on first use moved into `noise_store::pin_hub_key`, which holds the pin
  lock across the read and the write. Checking for a pin and then writing it as
  separate calls was the race itself.
- The static key is created with `create_new` straight at its final path. The
  pin lock only covers one process, so two processes could each generate a key
  and the rename would leave one of them holding a key the file does not
  contain -- and the hub pins what it was shown, so that client would be
  refused for good. Losing the create now reloads the winner's key. Pin-map
  updates remain process-local; see the note in `noise_store`.
- Noise state files are written to a uniquely named temporary file created
  `0600` with `create_new` and renamed into place. A truncating write left the
  pin file empty on failure, which reads back as "no pins" and would silently
  re-pin whatever key the next connection was offered.
- **Breaking.** `HttpTransport::connect` now refuses a hub endpoint that is not
  `https://`, for the same reason as the MQTT change below: TLS is the only
  confidentiality left on that hop, and the access key travels in the
  `authorization` query.
- `create_client_identity` drops `cryptoKey` and `crypto_key` from a
  caller-supplied `opts.spec` rather than passing them through. The error
  redaction covers only what the SDK mints, so a legacy value left in by a
  caller could otherwise be echoed back inside a `ThalovantError::Api`.
- **Breaking.** The MQTT transport now refuses a broker whose identity does not
  enable TLS. Removing the crypto key took the separate payload cipher with it,
  so TLS is the only confidentiality left on that hop; without it every message
  and the broker password would travel in the clear. Use an `mqtts://`
  endpoint, or set `tls: true` on the identity's `mqtt` block.
- Messages larger than one Noise transport message are chunked at 65000 bytes
  and reassembled by the peer, with reassembly capped at 32 MiB. Any transport
  message that fails to decrypt, and any malformed chunk sequence, drops the
  session rather than the frame.
- `HiveMessage` derives `Default`.

## 0.3.1

- `list_intents` (and so `intents`) returns `ThalovantError::Runtime` carrying the hub's `error` text when the hub answers `ovos.intent.list` with `ok: false`, instead of reading the missing `intents` key as an empty list. A refused listing is not an empty hub, and reporting it as no intents showed a person a device that can do nothing; the engine-manifest fallback still answers a `hive.policy.denied` refusal only, since a failed query is not evidence the connection lacks the type. `describe_intent` keeps returning an empty list for `ok: false`, which is a real answer: the hub does not know that registration, so the intent simply has no sentences. Reported by the Kotlin port's review.
- Documentation: `ovos.intent.list` is always needed; `ovos.intent.describe` only when the client has to ask for the definitions itself -- `describe` is on (the default) *and* the runtime did not attach each row's `definition` to the listing. A runtime that honours `include_definitions` is sent no describe at all. `ThalovantError::PolicyDenied`'s `allowed` already kept string entries only -- the other half of the Kotlin port's report -- and a test now pins it.

## 0.3.0

- Add the intent inventory: `Client::intents(languages, IntentInventoryOptions)` reads the hub runtime's intent manifest (OVOS-INTENT-4 §10) over the client's own session and returns a `HubIntentInventory` — every intent each skill registered, per language, with the sentences a person says to reach it as the skill's locale files wrote them, `{slot}` placeholders included. No control-plane credential is involved. `Client::list_intents(lang, IntentListOptions)` and `Client::describe_intent(skill_id, intent_name, lang, IntentDescribeOptions)` expose the two underlying queries (`ovos.intent.list` / `ovos.intent.describe`).
- New public types `HubIntentInventory`, `HubSkillIntents`, `HubIntent`, `IntentRegistration`, `IntentDefinition`, `IntentInventorySource`, `IntentInventoryOptions`, `IntentListOptions`, `IntentDescribeOptions`, constants `DEFAULT_INTENT_TIMEOUT` and `DESCRIBE_BATCH`, `intents::same_language`, and the event-name constants `EVENT_INTENT_LIST`, `EVENT_INTENT_LIST_RESPONSE`, `EVENT_INTENT_DESCRIBE`, `EVENT_INTENT_DESCRIBE_RESPONSE`, `EVENT_ADAPT_MANIFEST_GET`, `EVENT_ADAPT_MANIFEST`, `EVENT_PADATIOUS_MANIFEST_GET`, and `EVENT_PADATIOUS_MANIFEST`. `HubIntentInventory`, `HubSkillIntents`, and `HubIntent` serialise (and `as_value()`) to the same JSON the other SDKs print.
- Queries are correlated by `context.request_id` like every other request, and a reply delivered more than once is taken once. Describes are sent together and matched by request id, or by the definition's own `skill_id`/`intent_name`/`lang` for a hub that does not echo the id. A describe that never comes leaves that intent without sentences instead of failing the inventory. Language tags compare case-insensitively with `_` and `-` folded: `intents(languages)` trims each tag and asks the hub once per language whatever its spelling (`en-us`, `en-US`, `en_us` are one), keeping the first spelling given in `languages`. `has_phrases()` is true only when at least one intent carries at least one sentence. An intent registered under both engines in one language keeps the template row's sentences whichever order the rows arrive in, and the first row seen names its `engine`; on the names-only fallback the first engine to name an intent decides likewise (adapt is asked before padatious). These are the four points the reference settled in Python SDK 0.4.37, so every SDK reads the same.
- **BREAKING:** `ThalovantError` is now `#[non_exhaustive]`, so a caller matching on it needs a wildcard arm and a later release can add a failure without breaking anyone. This release adds one such variant, which is why it is a minor bump and not a patch: code that matched `ThalovantError` exhaustively must add `_ => ...`.
- Add `ThalovantError::PolicyDenied { denied_type, code, reason, allowed }`, returned at once from the hub's `hive.policy.denied` instead of waiting for a timeout. `IntentInventoryOptions::fallback` (on by default) falls back to the engines' own manifests (`intent.service.adapt.manifest.get` / `intent.service.padatious.manifest.get`) when `ovos.intent.list` is refused; the result then carries names only, `source` of `IntentInventorySource::EngineManifests`, and `denied` naming `ovos.intent.list`.
- A runtime that attaches each row's `definition` to `ovos.intent.list` when asked with `include_definitions` is used as such; one that does not is described row by row, at most `DESCRIBE_BATCH` (32) describes in flight at a time. Each batch is its own subscription window with its own deadline, so a large inventory cannot outrun the transports' 64-slot bus channel and lose replies, the hub is never sent the whole burst at once, and a hub that answers nothing fails after one batch instead of holding every request open. A partial answer is an answer across windows as within one: a window nothing answered contributes nothing rather than discarding the windows that did answer, and the describe fails only when no window produced anything, so a hub silent from the start still fails at the first window.

## Unreleased

- Security: redact secrets from every `Debug`/`{:?}` rendering. `Identity`, `MqttBrokerCredentials`, `control::LoginOptions`, `control::DeviceAuthorization`, `control::BootstrapIdentityResult`, `events::Event`, and `events::Reply` now use hand-written `Debug` implementations that redact credentials — access key, password, crypto key, MQTT username/password, MFA `otp_code`/`recovery_code`, device code, secret-keyed `Identity::metadata` entries, and the end-user bearer token duplicated into `context.auth_token` and `context.auth.token`. `Serialize`/`Deserialize` are **unchanged**, so identity-file persistence and the wire protocol still emit the real values.
- Security: `BootstrapIdentityResult::as_value(false)` now redacts the credential subkeys in the `hub` and `client` resources (for example the `apiKey`, `password`, and `cryptoKey` minted by `POST /v1/clients`, and the `initial_identify.key` access-key alias) as well as secret-keyed `Identity::metadata`, matching how it already redacted the identity block. `as_value(true)` still returns the real values for persistence.
- Security: strip the request URL from stored `reqwest` errors. Data-plane request URLs carry the caller's access key in a `?authorization=` query, which reqwest's `Display` appended as " for url (...)"; that URL is no longer copied into `TransportHealth::last_error` or any rendered `ThalovantError::Http`.
- Security: bound and redact the server response body interpolated into `ThalovantError::Api` messages for `/v1/auth/token`, `/v1/auth/device/token`, and `/v1/clients`. Errors now carry the HTTP status plus a short, single-line, secret-redacted detail instead of the raw body.
- Security: redact `MqttBrokerCredentials::topic_prefix` from `Debug`/`{:?}`. Since the MQTT migration the prefix is `hivemind/<hub-id>/<access-key>`, so it embedded the account access key; its hand-written `Debug` now prints `<redacted>` for the field. `Serialize` still emits the real prefix for persistence and the wire protocol.
- Harden `mqtt_topics_for_identity` topic_prefix validation: trim surrounding whitespace as well as slashes (whitespace-only prefixes like `"/ \t/"` now report the existing "must include topic_prefix" error instead of generating whitespace-only topics), and reject prefixes containing an MQTT wildcard (`#`/`+`) or an ASCII control char (`< 0x20`, incl. NUL) with a new "MQTT topic_prefix contains characters that are not valid in an MQTT topic." error.
- **BREAKING:** remove the admin analytics path. `AnalyticsOverviewOptions::admin` is deleted and `get_analytics_overview` no longer targets `GET /v1/admin/analytics/overview`; it always calls `GET /v1/analytics/overview`. This SDK serves non-admin customers, who never had access to the admin route. Callers that set `admin: true` must drop the field; `owner_id` is still sent and is scoped to the caller's own tenant by the API.

## 0.2.22

- Add the hub-provisioning surface: `create_hub`, `update_hub`, `delete_hub`, `release_hub`, `set_hub_rating`, `clear_hub_rating`, and `get_hub_runtime_capabilities`. `create_hub` sends a generated `Idempotency-Key` unless the caller supplies one. `update_hub` and `delete_hub` take `etag` as a **required** argument, not an option, because the API enforces optimistic locking on both routes and rejects a stale *or missing* `If-Match` with HTTP 412; the etag is read from the hub resource's `etag` body field, as the API emits no `ETag` response header.
- Add runtime-group management: `list_runtime_groups`, `get_runtime_group`, `create_runtime_group`, `update_runtime_group`, `get_runtime_group_config`, `update_runtime_group_config`, `release_runtime_group`, and `delete_runtime_group`. These routes read no `If-Match` and no idempotency header, so concurrent writes are last-write-wins. `update_runtime_group_config` merges rather than replaces, and sends `personas` only when it is `Some(..)`.
- Add skill discovery and installation: `list_marketplace_skills`, `list_runtime_group_marketplace`, `list_runtime_group_inventory`, `install_runtime_group_skill`, and `uninstall_runtime_group_skill`.
- New public types `ReleaseOptions`, `MarketplaceSkillsOptions`, `SkillInstallOptions`, and constant `DEFAULT_SKILL_SOURCE_TYPE`. No existing signature changed and no `ThalovantError` variant was added: HTTP failures on these routes keep the crate's existing mapping to `ThalovantError::Api` carrying the status and response body.
- Scope and plan notes now documented on each method: the provisioning writes require a paid plan and `hubs:write` (HTTP 402 on the free plan, HTTP 403 without the scope); the rating routes require `hubs:write` but are **not** paid-gated; `list_marketplace_skills` needs only `hubs:read` and is not paid-gated, with `owner_id` and `include_inactive` silently ignored for non-admin callers; and `get_hub_runtime_capabilities`, `list_runtime_group_marketplace`, and `list_runtime_group_inventory` require `hubs:inspect`. Only `get_hub_runtime_capabilities` answers HTTP 409 when no client is connected — the two runtime-group reads return an empty `data` list with a pending `source` of `ovos-runtime-operator-pending`.
- Derive both user-agent constants from `CARGO_PKG_VERSION` instead of repeating the version literal, so a release bump can no longer leave them stale.

## 0.2.21

- Document the two HTTP 429 responses the control plane returns for token-authenticated calls: `token_rate_limited` (the plan's per-minute request rate, 60 requests per minute on the free plan) and `token_quota_exceeded` (the plan's daily or monthly call quota, reported in `quota`, `limit`, and `used`). Both carry a `Retry-After` header and a matching `retry_after_seconds`, both surface as `ThalovantError::Api`, `Retry-After` is authoritative, and the SDK does not retry automatically.

## 0.2.20

- Add browser device-flow sign-in: `ControlPlane::login_with_browser(DeviceLoginOptions)` requests a device authorization from `/v1/auth/device/authorize`, shows the verification URI and user code (override with `DeviceLoginOptions::prompt`), best-effort opens the browser at `verification_uri_complete` (`xdg-open`/`open`, never fatal), and polls `/v1/auth/device/token` honoring the server `interval` and `slow_down` back-pressure until approval. On approval the durable scoped API token is stored on `access_token` exactly like `login`.
- New public types `DeviceLoginOptions`, `DeviceAuthorization`, `DevicePrompt`, constant `DEFAULT_DEVICE_POLL_INTERVAL`, and error variants `ThalovantError::DeviceAuthorizationDenied` and `ThalovantError::DeviceAuthorizationExpired` (a wait past `DeviceLoginOptions::timeout` fails with the existing `ThalovantError::Timeout`).
- Document direct API-token auth for CI (`ControlPlane::with_access_token` / `ControlPlane::new` with a pre-provisioned token such as `THALOVANT_API_TOKEN`); no code change, the constructors already accepted a token.

## 0.2.19

- Add MFA login support: `ControlPlane::login_with_options` and `LoginOptions` send optional `otp_code` and `recovery_code` fields to `/v1/auth/token` for accounts that require multi-factor authentication. `ControlPlane::login` is unchanged.
- Realign the control-plane user agent with the crate version (it was stuck at `thalovant-rust-sdk/0.2.17`) and add a regression test that pins both user-agent constants to `CARGO_PKG_VERSION`.

## 0.2.18

- Update the `base64` dependency from 0.22 to 0.23.
- CI and release-process hardening only, no API changes: pin GitHub Actions by full SHA, attest crate releases, add repository security ownership (CODEOWNERS and SECURITY.md), and schedule Dependabot dependency updates limited to minor and patch versions.

## 0.2.17

- Use the native TLS backend for MQTT so the published dependency graph no longer includes vulnerable `rustls-webpki 0.102.8`.
- Add an explicit regression assertion for the MQTT TLS backend and align runtime user-agent versions.
- Give CI and release-guard workflows explicit read-only repository permissions.

## 0.2.16

- Add typed `OperationResource` and `ControlPlane::get_operation` support.
