# Antigravity signed history

Translated clients return visible assistant text and tool calls. They do not
retain every native thought, signature or part boundary. Douane records those
native parts in memory and restores them before forwarding the next request.

Client-only tool IDs never enter the stored native parts. Replay keeps a
mapping from each client ID to the provider's original ID, including its
absence, and applies that identity to the corresponding tool result. Native
IDs supplied by the provider stay unchanged. Context fingerprints use the
client's chronological projection before native restoration.

Some SSE responses wrap tool calls with boundary deltas consisting only of
`{"text":""}`. Those deltas are omitted from replay history. Empty text with a
signature, thought flag or any other field is never discarded, nor is whitespace.

A final empty-text signature delta closes the preceding unsigned text block
when all other metadata match exactly. The assembled block keeps every text
and signature byte, including the signature field's spelling. It never merges
across a signed block, a thought boundary or mismatched auxiliary metadata.
Nonempty signed text and signatures without a text field remain separate parts.

Replay requires the same account, model, session and normalized root
instructions. A strict preceding-context match is preferred. When compaction
changes that context, replay requires one unique match for the complete visible
model output in that scope: text and ordered call IDs, names and arguments.
Provider auxiliary metadata that translation never exposes does not enter this
identity. Original native parts retain that metadata unchanged.

Every recorded generation has an immutable identity and one validated parent.
Retained model turns must follow one ordered lineage; reordered turns, sibling
branches and ambiguous matches are rejected; an altered model output never
inherits native parts. Repeated generations never overwrite nodes that existing
descendants reference. Parent links and indexes use linear memory within the
existing cache budget.

The authenticated client may remove a prefix or edit its own user text, tool
results and inline system/developer reminders. Their new values are forwarded
exactly, with no old context silently reinserted. Root instructions remain
immutable. This replaces the earlier policy that rejected edits to preceding
inline reminders. No summary marker, tool ID alone or parsed summary text grants
replay access.

## Account ownership and rate limits

A recorded conversation stays on its original account, even when the scheduler
would prefer another account. Owner lookup validates the full history without
renewing its idle TTL. Ambiguous ownership is rejected explicitly. Normal
account access, disabled-model, reserve and share checks still apply.

If that account returns HTTP 429, Douane preserves HTTP 429 and sends a
`Retry-After` header. Further requests during its cooldown return the remaining
retry delay without contacting any account. Once the cooldown ends, the same
conversation resumes on its original account. Other upstream refusals also
stay on that account. A disabled, removed or forbidden owner remains refused;
history never migrates to an unauthorized account.

New conversations retain the configured scheduling and failover policies.
Ownership is ephemeral with the replay cache and is not a new persistent pin.

## Retention

The cache retains whole scopes rather than independently expiring individual
turns. Each scope indexes immutable generations by preceding context and
complete visible output.
The memory budget is `antigravity_replay_mib` (64 MiB by default), including
conservative allocation/index overhead, and each native turn remains limited
to 256 KiB of serialized parts. There is no independent turn-count limit. A
heavy coding-agent step (4 KiB thought summary, 8 KiB signature, 1 KiB tool
call) costs about 18 KB in that accounting, so a 150-step conversation takes
about 2.7 MB and ten of them about 27 MB: the default holds ten parallel long
conversations at half its budget. The former 8 MiB fit three, and swarms of six
to eight Gemini agents lost conversations to eviction (2026-10-29). Changing the
setting applies at once; a smaller budget evicts idle scopes immediately.

A fully validated restore or successfully recorded native completion renews the
scope's idle TTL (`antigravity_replay_idle_minutes`, one hour by default). A generation that outlasts the TTL while pinned
therefore remains replayable after completion. A failed restore, changed session
or refused recording does not renew an existing scope. Capacity pressure evicts
whole idle scopes in least-recently-used order.
Active upstream calls pin their scope, including concurrent calls in the same
scope, until completion or cancellation. An extension that cannot fit alongside
its own prefix, or beside pinned scopes, is delivered unrecorded without
deleting that prefix.

## Missing turns

A model turn the cache no longer holds (evicted, expired, lost at a restart, or
a completion it could not keep) is forwarded as the client's projection. Its
first tool call without a signature gets Gemini's documented
`skip_thought_signature_validator` placeholder, the way
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/signature/gemini_validation.go)
handles history Gemini never signed; later parallel calls and text stay
unsigned, as Gemini emits them. Recorded turns in the same request keep their
native parts and must still follow one ordered lineage, so unrecorded turns may
sit before or between them. The next completion records with the last recorded
turn as its parent. Before 2026-10-29 such a request was refused with HTTP 400
(`cannot mix recorded and unrecorded native lineages`, or `unavailable ... start
a new conversation`) and every retry failed again.

An output recorded in another account's scope of the same session, or a call
ID recorded anywhere, is still refused rather than sent unsigned. A history
without a stable session identifier is still refused.

Owner lookup takes the first model turn the cache still holds, so a
conversation whose oldest turns were evicted stays on its account.

JSON responses report a native recording failure (transport, truncation,
refusal, empty output, oversized turn) as HTTP 502. Translated SSE responses
report an error event and withhold the successful completion event until
recording succeeds.

Native recording distinguishes transport/interruption, missing completion,
empty output, truncation without output, provider refusal and turn-size errors.
A SAFETY or other refused completion is an error even when it contains partial
text; it never produces a successful SSE completion. Token counters alone do
not determine whether output exists. Unsupported native media is rejected by
both translated JSON and SSE responses.

The cache is process-local. After a restart, idle expiry or whole-scope
eviction the conversation continues without the hidden reasoning of the lost
turns. No private reasoning is persisted, added to client
payloads or written to diagnostics. This change does not provide durable replay
or change client compaction settings.

## Offline checks

```sh
cargo test -p douane --lib antigravity_replay::tests --locked --jobs 2
cargo test -p douane --test antigravity_harness --locked --jobs 2
```

The cache tests cover two interleaved 160-turn conversations, ten parallel
150-step conversations within the default budget, atomic eviction, evicted and
partly recorded conversations continuing, sliding expiry, rejected restores,
concurrent pins and reconfiguration. The
public HTTP harness covers tool cycles and text correction with dynamic system
reminders across Messages, Chat and Responses over JSON and SSE, including
reasoning deltas whose signature arrives in a final empty-text frame,
recording failures before successful completion, account-bound rate-limit recovery,
access revocation and upstream refusals without cross-account failover. It also
covers compacted suffixes, edited tool results, native auxiliary metadata and
empty/refused/truncated terminations across all three client dialects.
