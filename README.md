# douane

[![CI](https://github.com/mtsu-dev/douane/actions/workflows/ci.yml/badge.svg)](https://github.com/mtsu-dev/douane/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue)](LICENSE)
![Rust 1.90+](https://img.shields.io/badge/rust-1.90%2B-orange)

One self-hosted endpoint for every AI subscription you pay for.

douane connects your Claude, ChatGPT (Codex), Antigravity, Muse Code, Grok, GitHub Copilot and OpenCode Go subscriptions, plus any API key you hold, then serves them to any coding tool through OpenAI- and Anthropic-compatible APIs. It watches each account's quota, picks the account that should serve the next conversation, and keeps a conversation on the same account so the provider's prompt cache stays warm. A single Rust binary with an embedded dashboard; no Node.js, Docker or external database at runtime.

![Quotas page: accounts grouped by provider, with five-hour and weekly windows, resets and extra credits](docs/images/quotas.png)

## Features

- One base URL and a client key for each of Claude Code, the Grok CLI, Boite and any OpenAI-compatible client.
- Chat Completions, Anthropic Messages and Responses, streaming included, with translation between them; native Codex WebSocket sessions.
- Live quotas per account: five-hour, daily, weekly and monthly windows, reset times, plan tier, extra credits and banked resets, refreshed in the background.
- Four routing strategies, per provider if you like, and failover to another account of the same provider when one runs out.
- Conversation affinity: native session ids and `prompt_cache_key` pin a conversation to its account, so cache reads keep landing.
- Client keys: every call carries a client key, so you see which tool or machine spends what. Limit a client to some accounts, and split each provider's quota windows between clients by percentage.
- Per-account reserve: stop spending an account at 5 % (or any share) of its longest quota window, or of the windows you pick (5 hours, weekly, both), so you keep some for yourself.
- Request history with latency, token and cache counters; never conversation bodies or credentials.
- OAuth and device sign-in from the dashboard, JSON import of existing CLI credentials, and `douanectl` for scripting.
- A dashboard in English and French that follows the browser's language; **Settings** keeps another choice.

## Providers

| Provider | Connect with | Quotas |
| --- | --- | --- |
| Claude (Pro, Max) | OAuth in the dashboard, or credential import | Five-hour, weekly and weekly Opus windows, extra usage, banked resets |
| Codex (ChatGPT plans) | OAuth in the dashboard, or credential import | Five-hour and weekly windows, credits, banked resets |
| Muse Code (Meta subscription) | Meta device code in the dashboard, or credential import | Subscription windows observed in inference responses |
| Antigravity | Google OAuth once a client is configured, or import | Per-model quotas |
| Grok (SuperGrok) | Device code in the dashboard | Weekly or monthly credit windows |
| OpenCode Go | Subscription key in the manual form | Five-hour, weekly and monthly windows |
| Gemini CLI, Kimi | Credential import | Read from the provider |
| GitHub Copilot | Device code in the dashboard, or OpenCode's `auth.json` | Monthly premium requests |
| Qwen | Credential import | Not reported |
| Custom endpoints (DeepSeek, OpenRouter, Groq, NVIDIA NIM...) | An API key in the dashboard or `douanectl endpoint add` | Not reported |

What a provider exposes depends on the account and plan. A quota douane cannot read stays unknown; it is never estimated.

## Quick start

### Executables

Each [release](https://github.com/mtsu-dev/douane/releases/latest) attaches an archive per platform holding `douane`, `douanectl`, the license and this file: `douane-<version>-windows-x64.zip`, `-macos-arm64.tar.gz`, `-macos-x64.tar.gz`, `-linux-x64.tar.gz` and `-linux-arm64.tar.gz`. They need no runtime: Linux builds are static, the Windows build carries its C runtime. `SHA256SUMS` lists every file's checksum and `manifest.json` describes the raw executables for installers.

```sh
tar -xzf douane-<version>-linux-x64.tar.gz
cd douane-<version>-linux-x64 && ./douane --config douane.toml
```

On Windows, unzip the archive and run `douane.exe --config douane.toml` from that folder. The executables are not signed: Windows SmartScreen asks for confirmation on first launch, and macOS needs `xattr -d com.apple.quarantine douane douanectl` after a browser download.

### Douane in Boite

[Boite](https://github.com/mtsu-dev/boite) installs Douane from **Settings > Plugins**: it downloads the executable for its platform from the release named in `boite-plugin.json` at the repository root (written by the first release), checks its SHA-256 and runs it on `127.0.0.1`. Douane's accounts, database and configuration live outside the plugin folder, so uninstalling or updating the plugin keeps the accounts. Each release opens a pull request that moves `boite-plugin.json` to the new executables; Boite offers that version once it is merged.

### Docker

Images for `linux/amd64` and `linux/arm64` are on GitHub Container Registry: `latest` is the last release, `edge` follows `main`.

```sh
docker compose up -d   # with compose.yaml from this repository
```

or without compose:

```sh
docker run -d --name douane --restart unless-stopped \
  -p 127.0.0.1:8787:8787 -v douane:/data ghcr.io/mtsu-dev/douane:latest
```

Accounts, the database and `douane.toml` live in the `/data` volume; put a configuration file there to change settings. The container runs as uid 65532, so a bind-mounted folder must be writable by that uid. `docker exec douane douanectl status` runs the CLI inside it. The port is published on loopback only: read [Security and data](#security-and-data) before exposing it.

### From source

Build with Rust 1.90 or newer. SQLite is bundled and the dashboard is already compiled into the repository.

```sh
cargo build --workspace --release --locked
```

`target/release` now holds `douane` and the management CLI `douanectl`. Both are self-contained and can be copied to another machine with the same OS and architecture.

Start the service. Without a configuration file it listens on `127.0.0.1:8787` and keeps `auth/` and `douane.sqlite` in the working directory. With one, relative paths resolve from the file's folder, and settings changed in the dashboard are saved back to it. [`douane.example.toml`](douane.example.toml) lists every setting.

```sh
douane --config douane.toml --check   # validate, create nothing
douane --config douane.toml
```

Open `http://127.0.0.1:8787/admin`, choose **Connect an account** and sign in to a provider. When douane runs on another machine, paste the callback URL or code shown by the provider back into the dialog: the provider's localhost redirect does not need to reach the server.

Then create a client on the **Clients** page (or `douanectl client add Desktop --key-file desktop.key`) and point a tool at douane with its key:

```sh
curl http://127.0.0.1:8787/v1/models -H 'Authorization: Bearer dk_...'

curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Authorization: Bearer dk_...' \
  -H 'Content-Type: application/json' \
  -d '{"model":"<id from /v1/models>","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Model ids carry their provider prefix, for example `claude/...` or `codex/...`. The [client key](#client-keys) only identifies the caller to douane: authentication upstream uses the connected subscription.

![Accounts page with plan, state and reserve per account](docs/images/accounts.png)

## Clients

The dashboard's **Set up a tool** page writes these settings for you, with your own address.

| Client | Settings |
| --- | --- |
| OpenAI-compatible tools | Base URL `http://127.0.0.1:8787/v1`, API key: a client key `dk_...`, a model id from `/v1/models` |
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:8787` (no `/v1`), `ANTHROPIC_AUTH_TOKEN=dk_...` |
| Grok CLI | `XAI_API_KEY=dk_...`, `GROK_MODELS_BASE_URL` and `GROK_XAI_API_BASE_URL` set to `http://127.0.0.1:8787/v1`, `GROK_MODELS_LIST_URL=http://127.0.0.1:8787/v1/models?provider=xai&ids=upstream` |
| [Boite](https://github.com/mtsu-dev/boite) | Subscription proxy set to the dashboard URL, and its key field to a client key: Boite lists the models and draws each account's quota bars from `/v1/quotas` |

For the Grok CLI, a session saved by `grok login` takes precedence over the key; set `GROK_AUTH_PATH` to a missing file to keep it out.

If clients reach the API at a different address than the dashboard (a reverse proxy, a separate host name), set `server.client_api_base_url` so the generated snippets use it.

## API

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Service health |
| `GET /v1/models` | Available models; `?provider=xai,claude` filters, `&ids=upstream` lists provider-native ids in the provider's order |
| `GET /v1/quotas` | Quotas per provider, per account or averaged, for client status bars |
| `POST /v1/chat/completions` | Chat Completions, streaming included |
| `POST /v1/messages` | Anthropic Messages, streaming included |
| `POST /v1/messages/count_tokens` | Claude token counting, relayed to the provider |
| `POST /v1/responses` | Responses, streaming included |
| `POST /v1/responses/compact` | Native compaction on compatible upstreams |
| `GET /v1/responses` (WebSocket) | Native Codex sessions, and a streaming bridge for the other subscriptions |
| `/admin/api/*` | Accounts, clients, quotas, models, request history, settings and OAuth |
| `/v0/management`, `/v8/management` | A CLIProxyAPI-compatible subset of its management API |

Translation between the three dialects covers text, tools and images; provider-specific extensions do not all have an equivalent. A failed request moves to another account only before its first byte reaches the client: started output is never replayed. A same-dialect request returns the provider's own rate-limit headers and request id, and client errors such as an oversized prompt come back as the provider wrote them.

### Antigravity clients

Use an advertised `antigravity/<model>` ID through Chat Completions, Messages or
Responses. Send a stable conversation ID, such as `x-opencode-session-id`,
`X-Session-Id`, `session_id` or `prompt_cache_key`. OpenCode's custom provider
sends its own session ID. `/v1/models` includes thinking capabilities and
context/output limits; explicit thinking controls follow those capabilities.

Only outgoing Antigravity system text is normalized: Anthropic billing lines
are removed, and the exact Claude Agent SDK and Codex self-identification
phrases receive one zero-width space. Other instructions, user messages, tools
and other providers are preserved. This follows the compatibility approach in
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/tree/bf9341629ef2300ac9d846a2e80fe2756f63a671/internal/translator/antigravity).
Google can still refuse a request; its sanitized reason and retry delay remain
visible.

Native signed model parts are kept in memory and restored after tool results.
Replay is scoped to the account, model, session and normalized root
instructions. Complete visible model output and its recorded lineage must
match; a client may compact its prefix or edit its own results and inline
reminders. Hidden reasoning and opaque signatures never enter the ledger or
disk. Chat Completions exposes returned thought text as `reasoning_content`;
other dialects do not fabricate signed thinking blocks.

The replay cache has a 64 MiB budget and a one-hour idle lifetime by default
(`antigravity_replay_mib`, `antigravity_replay_idle_minutes`), with a 256 KiB
native-turn limit and no independent turn-count limit. Interrupted, invalid or
oversized turns are not recorded. A turn missing from the cache (evicted,
expired, lost at a restart or never recorded) is sent as the client's text, its
first tool call marked with Gemini's `skip_thought_signature_validator`
placeholder, so the conversation continues without that turn's hidden
reasoning. Ambiguous, reordered or cross-scope model history is refused
locally. Provider refusals and empty completions produce explicit errors.
See [signed history](docs/antigravity-replay.md) for retention and compaction
rules. Acceptance by a live Google account remains separate from offline tests.

Each Antigravity account lists its own models (`fetchAvailableModels`), and
plans differ: Claude Opus and Sonnet 5.5 are offered to some Google AI Pro
accounts and not to trial or partner ones. The catalog is the union of every
account's list, and a request goes only to the accounts whose own list names its
model. An account that still answers `NOT_FOUND` for a model is skipped for it
for 24 hours or until a restart, the next account is tried, and the account
stays usable.

### Muse Code subscription

Choose **Muse Code** in **Connect an account**, then authorize the displayed device code with your Meta account. This connects the direct Muse Code subscription. Muse models available through OpenCode Go remain a separate provider.

Model ids use `meta/` followed by an id from the account's live catalog. Chat Completions, Messages and Responses clients use the subscription's Responses endpoint. Quotas stay unknown until Meta reports them in an inference response; the last reported windows survive a restart. douane does not switch an inactive subscription to pay-per-token API billing.

### GitHub Copilot clients

Copilot signs in with GitHub's device flow through the OpenCode GitHub app, the way OpenCode does; an existing OpenCode login imports as is (its `auth.json`). The model list is Copilot's own catalog: models whose policy is enabled for the account, each sent to the path Copilot lists for it (`/v1/messages` for Claude models, `/responses`, or `/chat/completions`). A client's own format is forwarded untouched when the model accepts it.

Copilot bills a premium request when its user starts a turn. douane marks tool results and other follow-ups as the agent's own (`x-initiator: agent`), as OpenCode does. The catalog can list a model the plan does not serve; the first `model_not_supported` refusal skips that account for the model for 24 hours or until a restart, hides the model once no account serves it, and the account stays usable.

## Custom endpoints

Any OpenAI-, Anthropic- or Responses-compatible API becomes a route: its keys serve the models it lists under a prefix of your choice, so `deepseek` serves `deepseek/deepseek-v4-pro`. Keys sharing a route rotate and fail over like accounts of one provider; keys on different routes never serve each other's models.

| Field | Meaning |
| --- | --- |
| `route` | Model prefix: 1 to 32 lowercase letters, digits, `-` or `_`, never a provider's name |
| `base_url` | The API root, usually ending in `/v1` |
| `dialect` | What the API speaks: `chat`, `responses` or `messages`; douane translates the client's format to it. Absent forwards the client's format unchanged |
| `models` | Model ids to serve; empty serves every model the API lists at `/v1/models` |

![Connect dialog adding a DeepSeek key as the deepseek route](docs/images/endpoint.png)

The dashboard's **Connect an account** dialog adds one, and each endpoint's pencil edits it in place. From a shell or an agent, the key comes from a file or stdin, never from the command line:

```sh
douanectl endpoint add deepseek https://api.deepseek.com/v1 --key-file deepseek.key --dialect chat
printf %s "$GROQ_API_KEY" | douanectl endpoint add groq https://api.groq.com/openai/v1 --key-file - --dialect chat
douanectl endpoint set deepseek --models deepseek-v4-pro,deepseek-flash
douanectl endpoint set deepseek --key-file new.key
douanectl endpoint list
douanectl endpoint remove groq
```

The same fields go through `POST /admin/api/accounts/import` with `{"type":"openai_compatible","route":...,"base_url":...,"api_key":...}`, and `PATCH /admin/api/accounts/{id}` changes `route`, `base_url`, `dialect`, `models` or `api_key`. An account without a route keeps its previous behavior under `openai_compatible/`.

The **Models** page lists models under their provider or route, each one folded until opened. A provider rereads its catalog on its own (`POST /admin/api/models/refresh?pool=openrouter`; no `pool` rereads all), and its switch turns every shown model on or off in one write (`PATCH /admin/api/models` with `{"ids":[...],"enabled":false}`). Every catalog is reread each hour, so a model a provider adds or retires shows without a restart. When a live listing fails, the provider keeps its previous models, is marked as such, and is reread every minute.

## Routing and quotas

Pick a strategy in **Settings**, globally or per provider. It applies to new conversations:

| Strategy | Behaviour |
| --- | --- |
| `smart` (default) | Sends 90 % of new conversations to the account whose weekly reset comes within 48 hours, until it is exhausted; otherwise weighs remaining quota against time left, with 20 % plain rotation |
| `reset-first` | Uses the account with the nearest weekly reset, and moves running conversations back to it as soon as it is eligible again |
| `fill-first` | Uses the first eligible account until it becomes unavailable |
| `round-robin` | Rotates through eligible accounts |

A conversation stays on its account while that account is eligible. douane recognises Codex, Claude and OpenCode session ids, `session_id` headers and `prompt_cache_key`, and keeps Claude cache controls untouched. A custom client should send one stable id per conversation. The affinity table holds 10,000 conversations for 24 hours of inactivity, in memory.

When an account is refused, it rests until the provider's own reset (its `retry-after`, Claude's rejected window, Codex's exhausted window), then returns. Accounts whose subscription quota is gone but which still hold usable credits stay available behind accounts with quota left.

A reserve, set per account on the Accounts page or with `PATCH /admin/api/accounts/{id}` and `{"reserve_percent": 5}`, is a hard floor on the account's longest window (monthly over weekly over daily over five-hour): once that window reaches the floor, douane stops using the account, live conversations included, until it resets. To watch other windows, pick them on the Accounts page or send `"reserve_windows": ["five-hour", "weekly"]` (any of `five-hour`, `daily`, `weekly`, `monthly`; `[]` returns to the longest window): the account then stops as soon as one of them reaches the floor. Close to the floor, quotas are reread at most once a minute, so a burst can overshoot by a little.

What is left of a week is lost at its reset, so the last hours before an account's weekly reset lift its limits: the reserve and client shares stop applying and the account serves until it runs out. The default is 6 hours; change it in Settings > End of the week or with `free_before_reset_hours` (0 to 168, 0 keeps the limits to the end). An account whose longest window is a month uses its monthly reset instead; accounts with only shorter windows are never lifted.

Banked resets for Claude and Codex appear on each account card. Using one asks for confirmation, and a retried click reuses the same request id, so a double click cannot spend two.

![Overview page: active accounts, requests, tokens, cache reads and recent activity](docs/images/activity.png)

## Client keys

Every `/v1` call needs a client key, sent as the tool's API key (`Authorization: Bearer dk_...` or `x-api-key`); other keys, and every call while no client exists, get 401. Create clients on the **Clients** page, with `POST /admin/api/clients` or `douanectl client add`. The key is shown once; douane keeps only its hash.

- **Access.** A client reaches every account, accounts connected later included, or only the providers, custom routes and single accounts you pick. `/v1/models` and `/v1/quotas` list only what it may use, and `/v1/quotas` leaves disabled accounts out.
- **Shares.** A share is a percentage of each account-wide window (five hours, day, week, month) of every account of a provider. Shares of a provider add up to 100 % at most. What no share covers is common to every client. A client's unused share stays reserved for it until the last seventh of the window (the last day of a week), then any client may use it. A client with no share uses the common part, then freed shares. A client held back gets 429 until the window resets. In the hours before the weekly reset (`free_before_reset_hours`, see above), shares hold no one back.
- **Usage.** Providers report usage per account, not per caller, so douane attributes it: each rise of a window goes to the clients whose calls ended on that account since the previous reading, weighted by tokens, or to the calls still running. A rise that no call through douane explains counts as outside use. Calls of several clients interleaving on one account make the split an estimate; one client per account keeps it exact.
- **Consumption.** The **Clients** page shows, over 24 hours, 7 or 30 days, each client's tokens, calls, cache reads and errors, its part of the total, its tokens over time and its models, limits or not. `GET /admin/api/clients/stats?hours=168` returns the same totals by client, provider and model, and by client and hour.
- **Quota bars.** With a client key, `/v1/quotas` shows a client holding a share of a provider each window as the part it has used of what it may use, so a full bar means it is held back. A client without a share sees each account's whole quota, other shares included. Percents are whole numbers.

```sh
douanectl client add Family --pools claude --share claude=25 --key-file family.key
douanectl client set <client-id> --share claude=25,codex=40
douanectl client set <client-id> --rotate-key --key-file family.key
douanectl client list
```

In Boite, paste the key in the subscription proxy settings.

## Security and data

- Credentials stay in `auth_dir` as local files. They never appear in the dashboard, logs or errors, and refresh-token rotation is serialised and written to disk. The management API returns them in one place only, a [configuration export](#export-and-restore).
- On Linux and macOS, `auth_dir` is mode 0700 and each credential file 0600. On Windows, douane removes inherited permissions from `auth_dir` and grants full control to the current user alone; keep the configuration under that user's own profile, for example `%LOCALAPPDATA%\douane`.
- SQLite stores request metadata only: time, model, account, client, status, latency and token or cache counts.
- The default listener is loopback. To expose douane, put it behind a reverse proxy that authenticates the dashboard, and set `server.management_token_env` to require a bearer token on `/admin/api`. That token is separate from client keys.
- Google OAuth clients for Gemini and Antigravity are read from `oauth-clients.json` beside `auth_dir`, or from `DOUANE_GEMINI_*` and `DOUANE_ANTIGRAVITY_*` environment variables; douane ships none.

## Export and restore

**Settings > Backup** in the dashboard, or `GET /admin/api/export`, downloads `douane-export-<date>.json`: settings, disabled models, every account with its tokens and keys, and the clients with their key hashes, so their keys keep working after a restore. **Restore** reads that file into another douane. An account whose identity is already there is updated in place, the others are added; settings and disabled models are applied only when asked (`{"bundle": <file>, "settings": true}` on `POST /admin/api/restore`).

The file holds live sign-ins. Refresh tokens rotate on use, so an account must live in one douane at a time: after restoring it elsewhere, remove it from the first one. Keep the file private and delete it once restored.

```sh
douanectl export --output douane-export.json             # owner-only file on Linux and macOS
douanectl --url http://127.0.0.1:8787 restore douane-export.json --settings
```

## Command line

```sh
douanectl health
douanectl status
douanectl accounts
douanectl quotas --refresh
douanectl models
douanectl import account.json --name "Work"
douanectl login codex
douanectl complete <oauth-session-id> --callback-file callback.txt
douanectl disable <account-id>
douanectl enable <account-id>
douanectl remove <account-id>
douanectl endpoint add <route> <base-url> --key-file <file|->
douanectl endpoint set <route|account-id> [--base-url ...] [--dialect ...] [--models ...] [--key-file ...]
douanectl client list
douanectl client add <name> [--pools ...] [--accounts ...] [--share pool=percent,...] [--key-file <file>]
douanectl client set <client-id> [--name ...] [--all|--pools ...] [--share ...] [--rotate-key]
douanectl client remove <client-id>
douanectl export --output <file>
douanectl restore <file> [--settings]
```

`--url` targets another instance and `DOUANE_MANAGEMENT_TOKEN` supplies the management token. Secrets are read from files, never from arguments.

## Development

```sh
cargo test --workspace --all-targets --locked
```

The dashboard is Svelte 5 in [`dash/`](dash). Changing it needs Node.js 24 and pnpm 11:

```sh
cd dash
pnpm install --frozen-lockfile
pnpm check
pnpm build
```

Commit `dash/build` with the source change, then rebuild Rust to embed it. `pnpm dev` runs the dashboard alone and expects the management API at the same origin or behind a proxy for `/admin/api`. `pnpm mock` serves the built dashboard with sample data on `http://127.0.0.1:5199/admin/`, without douane or any account; the README images come from it.

CI tests Rust on the minimum supported version, checks and builds the dashboard, and builds the Docker image on amd64 and arm64, then runs [`docker/smoke.sh`](docker/smoke.sh) against it; a pull request runs only the jobs its files touch. Tests run against local fake upstreams; they do not prove that a real provider accepts a given account or model.

A release is a tag `vX.Y.Z` equal to the version in `Cargo.toml`: CI runs again, the tested image is published as `X.Y.Z`, `X.Y` and `latest`, and the GitHub release is created.

## Disclaimer

douane is an independent project, not affiliated with Anthropic, OpenAI, Google, xAI or any other provider. Use it with accounts you own and within each provider's terms.

## License

[AGPL-3.0-or-later](LICENSE). The model catalogue in `crates/douane/data` is derived from CLIProxyAPI under the MIT license; see [NOTICE](crates/douane/data/NOTICE).
