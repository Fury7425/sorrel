# Native Multi-Agent Chat App — Architecture

As of 2026-10-04. Live version: https://claude.ai/code/artifact/8bcd12d9-7bdb-4170-86e1-4b76c6bd2639

## Goal and constraints

Build a native desktop app that does what the Claude and ChatGPT apps do (chat, projects, files, co-work tasks) but drives the official `claude` and `codex` CLIs, so people use the subscriptions they already pay for.

- **General purpose, not code-first.** Chat threads, projects with attached files, artifacts and documents, long-running co-work tasks. Code is one mode among several.
- **Subscriptions, not API keys.** The user signs in once through each official CLI. The app never sees or stores their tokens. API-key mode is a fallback.
- **Cross-platform.** Windows, macOS and Linux from one codebase. This rules out WinUI 3 as the main UI.
- **Light on resources.** No bundled browser engine. Target 150 MB or less idle for the whole app, startup under 300 ms, and smooth scrolling through long transcripts. The Electron wrappers below measure 700 MB to over 20 GB.
- **Multi-provider.** Claude Code and Codex at launch. Gemini CLI, OpenCode and others later, through one adapter layer.

## Prior art

Zeron already ships the stack you want: Rust + GPUI, a headless engine with a separate UI, and native drivers for each CLI. Every other wrapper here is Electron. Copy Zeron's architecture and build the general-purpose (chat and co-work) layer it lacks.

| App | UI stack | Process model | Claude driver | Codex driver | Storage | Reported RAM |
| --- | --- | --- | --- | --- | --- | --- |
| [Zeron](https://github.com/zeronsh/zeron) | Rust, GPUI fork (`zui`), 17 crates | Headless engine daemon + GPUI UI over localhost WebSocket | `claude` stream-json, `can_use_tool` control requests | `codex app-server` JSON-RPC | SQLite run journal, Loro CRDT for sync | Engine about 160 MB, UI 170 to 200 MB ([baseline](https://raw.githubusercontent.com/zeronsh/zeron/main/docs/performance/resource-baseline.json)) |
| [T3 Code](https://github.com/pingdotgg/t3code) | Electron 44, React, Effect (TypeScript) | Node WebSocket server owns providers; desktop, web and mobile are thin clients | Claude Agent SDK pointed at user's `claude` binary | `codex app-server` | SQLite, git refs for checkpoints | About 700 MB server ([issue](https://github.com/pingdotgg/t3code/issues/11601)) |
| [Nimbalyst](https://github.com/Nimbalyst/nimbalyst) | Electron, React, Jotai | Electron main owns DB, files, MCP; renderer is UI only | Claude Agent SDK | `codex app-server` (ACP in alpha) | PGLite, plain markdown in git | Not published |
| [OpenCode](https://github.com/anomalyco/opencode) | TUI on OpenTUI; desktop moved from Tauri to Electron | HTTP server (OpenAPI) + TUI/desktop/web clients | None: calls model APIs directly | None: own ChatGPT OAuth | SQLite (drizzle) | Leaks to 20 GB+ ([issue](https://github.com/anomalyco/opencode/issues/16697)) |
| [Cursor](https://cursor.com/docs/cli/acp) | VS Code fork on Electron | Stock VS Code processes; `agent` CLI separate | n/a (own agent) | n/a | Local | Up to 22 GB reported ([forum](https://forum.cursor.com/t/cursor-consuming-22-gb-ram-across-dozens-of-helper-processes-ide-becomes-extremely-slow/158844)) |
| [Zed](https://zed.dev/docs/ai/external-agents) | Rust, GPUI | Each agent a subprocess over ACP | `claude-agent-acp` adapter | `codex-acp` adapter | Local | About 140 to 220 MB (unverified) |

**Steal**

- **Engine and UI as two processes** (Zeron, T3, OpenCode). Agents keep running when the window closes or crashes, and a phone or web client can attach later.
- **Native driver per CLI plus one normalized event model** (Zeron's `normalize.rs`, T3's adapter boundary). ACP is only the fallback for long-tail agents.
- **Never handle credentials.** Run the vendor's own `claude` and `codex login`, then read their state. Add a pre-flight auth check before each session (Nimbalyst).
- **Plain files as the source of truth** for projects and documents: markdown in a folder, optionally under git (Nimbalyst). Per-turn checkpoints as hidden git refs (T3).
- **Cursor's agent UX:** queue vs steer-now, plan, ask and agent modes, live todo cards, structured question and plan-approval cards, and checkpoint undo on every turn.

**Avoid**

- **Unbounded in-memory session state.** It is the root cause of OpenCode's and Cursor's multi-GB leaks. Stream tool output to disk and keep only what is on screen in RAM.
- **Custom OAuth that impersonates a vendor CLI.** This got OpenCode blocked by Anthropic in January 2026.
- **CRDT sync in v1.** Zeron's 1 MB transcripts jammed its sync relay.
- **Trusting headless CLIs to exit.** Cursor's `agent -p` has open hang reports. Every spawned process needs a watchdog and kill timer.

## Recommended architecture

Two Rust layers, a GPUI front end and a headless engine, joined by one typed message API. Each CLI runs as a child process the engine supervises. In the MVP the engine runs inside the app process, on a tokio thread. Later the same API moves onto a local socket, so the engine can run as a background daemon. That gives Zeron's resilience later without paying for two processes on day one.

```
+------------------- App process (MVP) -------------------+          Child processes
|                                                         |         +------------------------------+
|  +-------------------+  proto  +----------------------+ |    +--->| claude                       |
|  | UI (GPUI)         |<------->| Engine (Rust, tokio) | |    |    | one process per active       |
|  | sidebar, chats,   |         | drivers + normalize  |-+----+    | session, stream-json / stdio |
|  | projects          |         | process supervisor   | |    |    +------------------------------+
|  | virtual transcript|         | auth check, MCP cfg  | |    +--->| codex app-server             |
|  | composer, approval|         | scheduler, storage   | |    |    | one shared process,          |
|  | cards, file pane  |         +----------+-----------+ |    |    | JSON-RPC 2.0 / stdio         |
|  +-------------------+                    |             |    |    +------------------------------+
+-------------------------------------------+-------------+    +--->| ACP agents                   |
                                            | read, write           | Cursor, Gemini, OpenCode     |
+------------------------- On disk ---------v-------------+         +------------------------------+
|  SQLite index          Blob files         Projects      |
|  threads, events,      tool output,       folders +     |
|  session IDs           attachments        instructions  |
+---------------------------------------------------------+

Later: the engine moves into a background daemon behind the same API,
so agents survive a closed window.
```

The UI sends commands and renders `AgentEvent`s. Only the engine spawns CLIs or touches disk, and every CLI runs with a project folder as its working directory.

### Crates

Six crates, not Zeron's seventeen. Split one off only when it hurts.

| Crate | Job | Key dependencies |
| --- | --- | --- |
| `proto` | Normalized events and commands shared by UI and engine | `serde` |
| `drivers` | One driver per CLI: spawn it, speak its wire format, translate into `proto` events | `tokio`, `serde_json`, `agent-client-protocol` |
| `engine` | Sessions, process supervisor, auth checks, storage, MCP config, scheduled tasks | `rusqlite`, `tokio` |
| `markdown` | Incremental markdown to GPUI elements, lazy syntax highlighting | `pulldown-cmark`, `tree-sitter` |
| `ui` | Windows, sidebar, thread view, composer, approval cards, file and artifact pane | `gpui`, `gpui-component` |
| `app` | Binary: wires engine to UI, CLI flags, `--daemon` mode later | all of the above |

### Agent drivers

Use each vendor's native protocol. Fall back to ACP for everything else, because native protocols expose more (steering, interrupts, richer permission hooks).

| Provider | How it runs | Wire format | Permissions | Auth |
| --- | --- | --- | --- | --- |
| Claude Code | One `claude` process per active session | `--input-format stream-json --output-format stream-json`, resume by session ID | `can_use_tool` control requests (undocumented SDK internals, so feature-detect from the init event) or the documented `--permission-prompt-tool` | User's own `claude` login |
| Codex | One shared `codex app-server` process, many threads | JSON-RPC 2.0 over stdio: `thread/start`, `turn/start`, `turn/steer`, `turn/interrupt` | Approval requests on the same channel | `account/login/start` in `chatgpt` mode: Codex runs the OAuth and the app never sees tokens. API key also works. |
| Cursor, Gemini, OpenCode, others | `agent acp`, `gemini --acp`, `opencode acp` | ACP: `session/new`, `session/prompt`, `session/update` | `session/request_permission` | Each agent's own login |

Every driver emits the same event stream, so the UI never knows which vendor it is talking to:

```rust
enum AgentEvent {
    SessionStarted { session_id: String, model: String },
    TextDelta { msg_id: MsgId, text: String },
    ThinkingDelta { msg_id: MsgId, text: String },
    ToolCall { call_id: String, kind: ToolKind, title: String },
    ToolUpdate { call_id: String, status: ToolStatus, output: BlobRef },
    PermissionRequest { req_id: String, call_id: String, options: Vec<PermOption> },
    Plan { items: Vec<TodoItem> },
    Usage { input: u64, output: u64 },
    TurnEnded { reason: StopReason },
    Error { message: String },
}
```

### General-purpose layer

This is what turns a coding wrapper into a Claude- or ChatGPT-style app. Each feature maps onto things both CLIs already understand, so there are no custom model calls.

- **Chats.** Each thread without a project gets its own scratch folder as its working directory. The CLI's file tools become "attachments".
- **Projects.** A project is a plain folder holding instructions, files and outputs. Instructions are written to `CLAUDE.md` and `AGENTS.md` there, so both CLIs pick them up natively.
- **Memory.** One `memory.md` in app data, imported from both instruction files. Users can read and edit it directly.
- **Artifacts and documents.** Whatever the agent writes into the folder. Markdown, code and images render natively. HTML opens in the system browser, so no webview gets bundled.
- **Connectors.** One MCP server list in the app, translated into each CLI's config at spawn time.
- **Co-work tasks.** The engine queues and schedules prompts against a project, runs them unattended with a stricter permission profile, and posts results to the thread.
- **Approvals.** All three permission mechanisms render as one card: allow once, allow always, or deny.

### Storage

- **SQLite (WAL mode, incremental auto-vacuum)** holds only the index: threads, projects, turns, normalized events, CLI session IDs.
- **Large payloads live in files on disk,** keyed by content hash: tool outputs, attachments, images. Rows store a `BlobRef`, never the bytes.
- **The CLIs keep their own transcripts.** The app stores only enough to render and resume, then passes the session ID back to the CLI.

## Performance budget

The CLIs, not the UI, will dominate memory. Claude Code runs on a JavaScript runtime, so each live `claude` process likely costs more than the whole native UI (measure it on your machine). Process lifecycle is therefore the biggest lever. Rendering discipline comes second.

| Metric | Target | Reference point |
| --- | --- | --- |
| App idle RSS (UI + in-process engine) | 150 MB or less | Zeron UI 170 to 200 MB plus engine about 160 MB |
| Engine alone, daemon mode | 40 MB or less | Zeron gates its engine at 40 MB in CI |
| Idle CPU | About 0% | Zeron cut idle CPU from 27% to 7.5% by throttling animation clocks |
| Cold start to first frame | Under 300 ms | Electron apps typically take 1 to 3 s |
| Live `claude` processes | Only sessions with a turn running or used in the last 10 min | One per session, never one per thread ever opened |
| Live `codex` processes | 1 | One `app-server` serves every thread |

**Tactics, by impact**

1. **Reap idle CLI processes.** Kill a `claude` process after 10 minutes without a turn. Respawn it on the next message using the stored session ID. Run Codex threads through one shared `app-server`.
2. **Supervise every child.** Use a watchdog timer per turn, kill on hang, cap concurrent sessions, and parse stderr for auth errors. Headless CLIs do hang (see Cursor's open reports).
3. **Keep tool output out of RAM.** Stream it to a blob file. Show the first 20 lines and load the rest only when expanded.
4. **Virtualize the transcript.** Use GPUI's `list` with measured row heights. Only visible messages get layout. An LRU evicts parsed markdown for rows far off screen.
5. **Parse markdown incrementally.** On each delta, re-parse only the last open block. Coalesce deltas to the frame rate instead of re-rendering per token.
6. **No idle animation.** Spinners tick at 15 to 30 Hz, only while visible and the window is focused. Nothing redraws when nothing changes.
7. **Load lazily.** Load tree-sitter grammars on first use per language. Decode images at display size and cache thumbnails.
8. **Page from SQLite.** Load threads 50 messages at a time from the end. Use WAL mode and prepared statements.
9. **Measure in CI.** Use a release profile with LTO and `strip`. Add a scripted test that streams a long scripted transcript and fails the build if RSS or frame time regresses.

## Alternative stacks

GPUI stays the pick. It is the only lightweight stack with a shipping app of exactly this kind (Zeron). Slint is the fallback if the Phase 0 spike fails. Because the engine is UI-agnostic, switching costs only the `ui` crate.

RAM figures are blog-grade, not one shared benchmark, so treat them as rough.

| Stack | Idle RAM (reported) | Text, IME, accessibility | Verdict |
| --- | --- | --- | --- |
| **GPUI** (Rust) | Zeron UI 170 to 200 MB; Zed about 140 to 220 MB | No built-in markdown (`gpui-component` adds it). Open IME bugs on Linux fcitx5/X11. No Windows screen reader yet. | **Pick.** Proven for this app shape. Pre-1.0, and Zed has slowed standalone work ([HN](https://news.ycombinator.com/item?id=47003569)), so pin a git revision. |
| **Slint** (Rust + `.slint` DSL) | About 30 MB with software renderer (one Linux data point, [blog](https://trystan-sarrade.com/article/rust-gui-135mb-to-30mb-egui-to-slint/)) | Markdown support is only "a first step" ([1.16](https://slint.dev/blog/slint-1.16-released)) | **Fallback.** Lightest pure-Rust option and stable at 1.x. Rich text is thin. Licence is GPL, royalty-free or commercial. |
| **Iced 0.14** (Rust) | Not measured | Markdown and `rich_text` widgets, IME fixes; accessibility still in progress ([release](https://github.com/iced-rs/iced/releases/tag/0.14.0)) | **Second fallback.** Solid Elm-style architecture with an accessibility gap. |
| **Freya** (Rust, Skia) | Not measured | `MarkdownViewer`, AccessKit ([repo](https://github.com/marc2332/freya)) | **Watch.** Still a release candidate with a small community. |
| **egui** (Rust) | About 135 MB in the same blog | IME candidate window ignores the caret | **Prototype only.** Immediate mode redraws too much for an idle-quiet app. |
| Makepad, Xilem, Dioxus Blitz (Rust) | Not measured | Unverified | **Skip for now.** Alpha or beta. |
| **Qt/QML** via `cxx-qt` | About 50 MB (anecdote, [deska](https://deska.dev/blog/qt-vs-electron-2026)) | Best IME and accessibility of any option | **Boring and strong** if accessibility is a hard requirement. QML learning curve, LGPL duties, `cxx-qt` pre-1.0. |
| **Avalonia 12** (C#) | Not measured | Markdown viewer and rich text editor added in 12.0 ([what's new](https://avaloniaui.net/whats-new/12-0)); a Linux IME bug is open | **Best non-Rust pick** if C# is fine. The Rust engine runs as a sidecar over local IPC. |
| **Flutter** (Dart) | About 90 MB (blog) | Good; Canonical now maintains desktop support ([OMG Ubuntu](https://www.omgubuntu.co.uk/2026/05/flutter-desktop-canonical-maintained)) | **Viable.** Dart UI plus FFI to the Rust engine. |
| **Tauri 2** | 30 to 80 MB headline; WebView2 helper processes push real usage higher | Browser-grade markdown, IME and accessibility. WebKitGTK is slow on Linux ([wry#1315](https://github.com/tauri-apps/wry/issues/1315)). | **Excluded by the brief.** Its real advantage is text handling, not RAM. |
| **Electron** | 150 to 400 MB; the wrappers above leak into GBs | Browser-grade | **Excluded.** What every competitor uses. |
| **WinUI 3** | Low | Excellent on Windows | **Out.** Windows only. |
| **Compose Multiplatform** (Kotlin) | 108 to 146 MB for hello world ([issue](https://github.com/JetBrains/compose-multiplatform/issues/1632)) | Good | **No.** JVM overhead. |
| **Native per OS** (SwiftUI + WinUI + GTK) | Lowest | Best on each platform | **No.** Three UIs to build and maintain. |

## Risks

The biggest risk is policy, not technology. Anthropic allows a user signing in to the unmodified `claude` binary, but not third-party apps routing subscription credentials through the Agent SDK. So spawn the real CLI and never touch its tokens.

Anthropic's [Agent SDK page](https://code.claude.com/docs/en/agent-sdk/overview) goes further: without prior approval, third-party developers may not offer claude.ai login or rate limits. The paused billing change also covered `claude -p`, so expect headless use to be metered eventually. Show limit and credit errors clearly in the UI, keep "Claude" out of the product name, and ask Anthropic for approval before selling the app.

| Risk | What could happen | Mitigation |
| --- | --- | --- |
| Anthropic subscription policy | Third-party subscription use gets metered or blocked. A move to separate credit was planned for June 15, 2026, then paused ([support article](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)). | Spawn only the unmodified `claude` CLI. No Agent SDK on subscriptions, no reading or copying credential files, no account-slot swapping. Ship API-key mode from day one. Rules: [legal and compliance](https://code.claude.com/docs/en/legal-and-compliance). |
| OpenAI policy | Low. ChatGPT sign-in for `app-server` clients is used openly by T3 Code, Zeron and Nimbalyst. | Let `codex login` own the flow. Never run your own OAuth. |
| CLI protocol churn | A CLI update renames a stream-json event or an app-server method, and sessions break silently. | Check the CLI version at spawn and warn below a tested minimum. Record real sessions as replay fixtures and run them in CI (T3 Code does this). Treat unknown events as no-ops, not crashes. |
| GPUI maturity | `gpui` on crates.io is stuck at 0.2.2. The input example has an IME composition bug. There is no screen reader support on Windows. | Pin a git revision (or the `gpui-ce` fork). Build on `gpui-component`. Test Korean and Japanese IME plus a screen reader on Windows in week one, not month six. |
| CLI memory cost | Several open Claude sessions outweigh the whole native UI. | Idle reaping and session caps (see Performance budget). |
| Features the CLIs lack | No native voice mode, no image generation, no claude.ai Projects or memory sync, no connector directory. | Local equivalents: projects as folders, `memory.md`, MCP servers for tools. Voice and image generation can come later through MCP or direct APIs. |
| HTML artifacts | GPUI cannot render HTML, so interactive artifacts need a browser. | Open them in the system browser. Add an on-demand webview window later only if users ask, and never in the main process. |

## Roadmap

Phase 0 exists to kill the riskiest assumption cheaply: that GPUI handles text input and streaming well enough on Windows and Linux. If it fails, switch stacks before writing the engine. The engine and drivers carry over unchanged to any UI.

| Phase | Scope | Exit gate |
| --- | --- | --- |
| 0. Spike | GPUI window with a virtualized message list. Claude driver: spawn `claude`, parse stream-json. Stream one reply with incremental markdown. | Korean IME and smooth streaming on Windows, macOS and Linux |
| 1. Chat MVP | Threads, composer, SQLite index, blob files. Codex app-server driver, one event model. Approval cards, auth check, API-key mode. Idle reaping and watchdogs. | 150 MB idle; replay fixtures pass in CI |
| 2. Co-work | Projects as folders with `CLAUDE.md` and `AGENTS.md`. `memory.md`, MCP connectors, file pane. Scheduled and queued tasks. Plan, todo and question cards. | A week of daily use without a restart |
| 3. v1 | ACP providers: Cursor, Gemini, OpenCode. Engine daemon mode over a local socket. Per-turn checkpoints, queue vs steer. Signed installers and auto-update. | Signed builds ship on Windows, macOS and Linux |

Later: phone and web clients over WebSocket, cross-device sync, voice.

No phase starts until the previous gate passes. Durations are left out on purpose: set them after the Phase 0 spike shows how fast GPUI work actually goes.

## Sources

- [Zeron repository](https://github.com/zeronsh/zeron) and its [resource baseline](https://raw.githubusercontent.com/zeronsh/zeron/main/docs/performance/resource-baseline.json)
- [T3 Code repository](https://github.com/pingdotgg/t3code) and [internals overview](https://github.com/pingdotgg/t3code/blob/main/docs/internals/overview.md)
- [Nimbalyst repository](https://github.com/Nimbalyst/nimbalyst)
- [OpenCode repository](https://github.com/anomalyco/opencode) and [memory leak umbrella issue](https://github.com/anomalyco/opencode/issues/16697)
- [Cursor CLI ACP docs](https://cursor.com/docs/cli/acp) and [stream-json output format](https://cursor.com/docs/cli/reference/output-format)
- [Zed external agents](https://zed.dev/docs/ai/external-agents) and [Agent Client Protocol](https://agentclientprotocol.com/protocol/overview)
- [Claude Code headless mode](https://code.claude.com/docs/en/headless) and [CLI reference](https://code.claude.com/docs/en/cli-reference)
- [Claude Code legal and compliance](https://code.claude.com/docs/en/legal-and-compliance)
- [Codex app-server docs](https://learn.chatgpt.com/docs/app-server)
- [gpui-component](https://github.com/longbridge/gpui-component)

## Decision log

**2026-10-04 — GPUI is pinned through gpui-component, as an exact `gpui-pre` snapshot.** The app depends on `gpui-kit` (gpui-component) at git revision `3a39e9d`. gpui-component no longer takes GPUI from a Zed git revision: it pins exact `gpui-pre =0.3.7` snapshot crates from crates.io and checks the pin in its CI. We take GPUI from that pin instead of patching in a raw Zed revision, because a different GPUI revision is not one gpui-component compiles or tests against. The rule's intent still holds: the stale `gpui` 0.2.2 crate is not used, and an exact version plus `Cargo.lock` is as immutable as a git revision. Bump both together, by moving the `gpui-kit` revision.

**2026-10-04 — No `markdown` crate in Phase 0.** gpui-component's `TextViewState::push_str` already parses on a background thread, re-parses only the last block on each append, and coalesces appends that arrive faster than a parse. Rows off screen use keyed `TextView` state, which GPUI drops when a row is not drawn, so parsed markdown stays bounded by the viewport. That covers tactics 4 and 5 without our own crate. Create `markdown` when measurement shows this falls short, or when lazy tree-sitter grammars need a home.

**2026-10-04 — All roadmap phases were built before any exit gate ran.** The user asked for the whole app at once, compiled on GitHub, with testing later. Phases 1 to 3 therefore exist without a measured Phase 0 gate. Treat every gate as open until `docs/PERF.md` has numbers and the IME check is done on each OS.

**2026-10-04 — ACP uses a small JSON-RPC peer, not the agent-client-protocol SDK.** One `rpc` module (JSON-RPC 2.0 over stdio) serves both Codex and the ACP agents, the way Zeron does it. The SDK's 2.x builder API could not be checked against a compiler on the build machine, and a hand-written translator matches the other drivers: read only the fields we need, drop anything unknown. Wire names were checked against `agent-client-protocol-schema` 1.9.1, the official schema crate. Revisit if ACP grows features the peer would have to re-implement.

**2026-10-04 — Checkpoints live in a shadow git directory, not in the user's repository.** Each working folder gets a git dir and index under the app's data dir, with refs under `refs/sorrel/`. This works for folders that are not repositories (every chat), and it never touches the user's `.git`, index or HEAD. The sequence (`add`, `write-tree`, `commit-tree`, `update-ref`, and `read-tree` plus `checkout-index` to restore) was run against real git, including non-ASCII paths.

**2026-10-04 — Codex sign-in is checked with `codex login status`.** That keeps the shared `codex app-server` at zero processes until a Codex thread or a ChatGPT sign-in needs it. The output strings were taken from the Codex CLI source at 0.160.0.

**2026-10-04 — Modes per CLI.** Claude: Plan uses the CLI's own plan permission mode (switched with the `set_permission_mode` control request, which exists in 2.1.286), and its `ExitPlanMode` request becomes the plan-approval card; Ask denies edit tools without asking. Codex: Plan and Ask run with a read-only sandbox plus an instruction line. ACP agents get the instruction line only.

**2026-10-04 — API keys are stored in `settings.json` in the user's data folder** (mode 0600 on Unix), never sent to clients, and handed to a CLI only in API-key mode. The OS keychain would add a dependency per platform; revisit before selling the app.

**2026-10-04 — Updates are checked, not installed.** Release builds compare themselves with the repository's latest GitHub release and offer the download page. Installing in place waits for signing certificates, since replacing the app with an unsigned download is unsafe. The release workflow signs when the secrets in `scripts/package.sh` are set.

**2026-10-04 — Daemon mode is opt-in.** `sorrel --daemon` runs the engine alone on a local socket (a named pipe on Windows); `sorrel` attaches to it when it is running and otherwise runs the engine in-process. Clients must first send a token the daemon writes to the user's data folder, because the default named-pipe ACL lets other local users open the pipe.

**2026-10-04 — The UI base follows T3 Code.** Threads remember their composer settings (model, reasoning effort, Build or Plan, and an access mode of Supervised, Auto-accept edits or Full access); approvals and questions appear inside the composer; tool activity folds into one row per stretch. A thread keeps its CLI once it has messages, as in T3 Code; an empty thread can switch CLI from the model picker. Visual polish, inspired by Zeron, comes later as separate work.
