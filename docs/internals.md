# Internals

How Sorrel works today. Keep this short and current.

## Crates

| Crate | Job |
| --- | --- |
| `proto` | The typed API: `Request` in, `Update` out, the normalized `AgentEvent`, the `ThreadEvent` log entry, and `Transcript`, the projection that turns a log into rows. |
| `drivers` | One driver per CLI (`claude`, `codex`, `acp`), the shared JSON-RPC peer (`rpc`) and the blob store. Every driver has the same shape: `run(config, commands, events)`. |
| `engine` | The actor that owns sessions, supervision, the SQLite index (`store`), checkpoints, settings and instruction files (`files`), tasks, auth checks, and daemon mode (`daemon`). |
| `ui` | The GPUI window, laid out like T3 Code with Zeron's surfaces. `Workspace` (`lib.rs`): custom title bar, wallpaper and glass, the Chat/Code switch, the add-project dialog and toasts. `sidebar.rs`: sessions or chats with search, project filter, pins and a right-click menu. `settings.rs`: General, Appearance, Providers (per-CLI on/off, sign-in, API key, binary, arguments, environment), Connectors, Scheduled tasks, Archived, About, plus the project and usage pages. `ThreadView` (`thread.rs`): the timeline (tool runs fold into one row in Code; Chat shows lookups as one line and files as cards) and the composer: model chip with an effort slider up to Ultrathink and Ultracode, fast mode and context window, Build/Plan, access, slash commands, steer/queue/stop, and the approval or question waiting on the user. A draft (id 0) is the new-thread screen. `style.rs`: shared cards, rows and segmented controls, the wallpaper layer, and the native border beam and thinking orb. |
| `app` | The `sorrel` binary: in-process engine or daemon client, `--daemon`, `--replay` and `--bench` for perf runs, the update check. |

There is no `markdown` crate; see the decision log in `ARCHITECTURE.md`.

## A message, end to end

1. The composer sends `Request::Send { thread, text, mode, delivery }`.
2. The engine makes sure the thread has a driver task. On first use it writes the folder's `CLAUDE.md` and `AGENTS.md` (project instructions plus memory, inside a `<!-- sorrel:begin -->` block that leaves the user's own text alone) and spawns the driver with the thread's folder, resume id, MCP servers and, in API-key mode only, the key.
3. If no turn is running and a session slot is free, the engine snapshots the folder (checkpoint "before"), logs `ThreadEvent::User` and sends `DriverCommand::Prompt`. A busy thread queues the message, or steers it into the running turn when the user picked "Steer now". With every slot busy, the turn waits for one.
4. The driver spawns its CLI on demand and translates its output into `AgentEvent`s.
5. The engine forwards every event live as `Update::Event` and logs it. Text deltas are coalesced so the log holds one row per stretch of text. On `TurnEnded` it snapshots the folder again (checkpoint "after"), then starts the next queued message.
6. The UI applies each event to its `Transcript` and re-renders only the touched row.

Opening a thread loads the last 50 rows (`Update::Page`); "Load older" pages further back. Pages start on an event that begins a row and carry the turn count before it, so the projection numbers turns correctly.

## Drivers

| Driver | Process | Notes |
| --- | --- | --- |
| Claude | One `claude --print` per active thread, stream-json both ways | Permission, question and plan cards come from `can_use_tool` control requests (`--permission-prompt-tool stdio`). Steer is a user line with `"priority": "now"`. Interrupt and mode switches are control requests. A `claude auth status` pre-flight runs before every spawn unless an API key is in use. |
| Codex | One shared `codex app-server` for all threads | JSON-RPC: `thread/start` or `thread/resume`, `turn/start`, `turn/steer`, `turn/interrupt`; approvals and `requestUserInput` are server requests. Sign-in is `account/login/start` in ChatGPT mode; Codex runs the OAuth. |
| ACP (Cursor, Gemini, OpenCode) | One process per active thread | `initialize`, `session/new` or `session/load`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`. There is no mid-turn input, so "Steer now" cancels and goes next. |

Every driver drops unknown messages, kills a turn that is silent for 10 minutes (the timer pauses while a card waits on the user), kills an idle process after 10 minutes and resumes it by id on the next prompt, and puts the last 20 stderr lines into the error a crash reports. `SORREL_<NAME>_BIN` overrides where a CLI is found; `SORREL_<NAME>_ARGS` overrides an ACP agent's arguments.

## Storage

- `sorrel.db` (SQLite, WAL, incremental auto-vacuum): projects, threads (with each CLI's session id), the append-only event log, checkpoints, tasks.
- `blobs/`: tool output beyond the 20-line preview, keyed by SHA-256.
- `chats/<id>/`: each chat's scratch folder. Projects are folders anywhere; new ones go under `projects/`.
- `checkpoints/<hash>.git`: shadow git dirs, one per working folder.
- `settings.json` (API keys, connector list, session cap) and `memory.md`.

All of it lives in the per-user data folder (`%APPDATA%\Sorrel`, `~/Library/Application Support/Sorrel`, `~/.local/share/sorrel`), or `SORREL_DATA_DIR`.

## Memory bounds

Channels are bounded. Parsed markdown is held only for rows on screen plus the streaming row. Tool output stays on disk past its preview. Idle driver tasks beyond 32 are dropped, which kills their CLI. Not bounded yet: the rows of one open thread grow as the user pages back, and a single stdout line is held whole.

## Tests and fixtures

`cargo test --workspace` runs the projection tests (`proto`), the Claude fixtures and supervision tests with a fake CLI, the Codex and ACP translators (`drivers`), store paging, checkpoints against real git, and instruction files (`engine`).

`fixtures/claude/*.jsonl` are stdout captures with `*.events.json` snapshots; `UPDATE_SNAPSHOTS=1 cargo test -p drivers` rewrites them. `auth_failed.jsonl` is a real claude 2.1.286 session. `synthetic_stream.jsonl` and `tools_and_permission.jsonl` are synthetic, built in the envelope found in claude 2.1.286; replace them with real recordings when a signed-in CLI is available.

## Perf run

`cargo build --release -p sorrel --features bench`, then `python scripts/perf.py target/release/sorrel`. The app replays the synthetic stream over 10,000 seeded rows, scrolls the whole list, idles for 15 s and writes GPUI's frame timings; the script adds RSS and CPU and checks the budget. Results go in `docs/PERF.md`.

## Releases

Push a `v*` tag. `.github/workflows/release.yml` builds on all three OSes, runs `scripts/package.sh` (signing when the secrets exist) and publishes a GitHub release.
