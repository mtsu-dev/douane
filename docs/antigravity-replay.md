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

Replay requires the same account, model, session, root instructions, preceding
history and visible output. Inline system and developer reminders remain at
their historical positions in a private validation projection. The provider
still receives their complete text in `systemInstruction`. Appending a reminder
does not invalidate earlier turns; editing a reminder preceding a recorded turn
does invalidate that turn.

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
turns. Each scope indexes turns by preceding context and visible output.
The memory budget remains 8 MiB, including conservative allocation/index
overhead, and each native turn remains limited to 256 KiB of serialized parts.
There is no independent turn-count limit.

A fully validated restore or successfully recorded native completion renews the
scope's one-hour idle TTL. A generation that outlasts the TTL while pinned
therefore remains replayable after completion. A failed restore, changed session
or refused recording does not renew an existing scope. Capacity pressure evicts
whole idle scopes in least-recently-used order.
Active upstream calls pin their scope, including concurrent calls in the same
scope, until completion or cancellation. An extension that cannot fit alongside
its own prefix fails without deleting that prefix. If pinned scopes occupy the
available budget, new scopes or responses fail explicitly.

JSON responses report a replay recording failure as HTTP 502. Translated SSE
responses report an error event and withhold the successful completion event
until recording succeeds. Missing unsigned text history is rejected locally,
just like missing unsigned tool history, before contacting the provider.

The cache is process-local. A restart, idle expiry or whole-scope eviction can
require a new conversation. No private reasoning is persisted, added to client
payloads or written to diagnostics. This change does not provide durable replay
or change client compaction settings.

## Offline checks

```sh
cargo test -p douane --lib antigravity_replay::tests --locked --jobs 2
cargo test -p douane --test antigravity_harness --locked --jobs 2
```

The cache tests cover two interleaved 160-turn conversations, atomic eviction,
sliding expiry, rejected restores, concurrent pins and capacity refusal. The
public HTTP harness covers tool cycles and text correction with dynamic system
reminders across Messages, Chat and Responses over JSON and SSE, including
reasoning deltas whose signature arrives in a final empty-text frame,
recording failures before successful completion, account-bound rate-limit recovery,
access revocation and upstream refusals without cross-account failover.
