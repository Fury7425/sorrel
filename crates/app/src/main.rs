// Release builds on Windows are GUI apps: no console window opens with them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Sorrel: wires the engine to the window.
//!
//! sorrel                  open the window; attach to a running daemon, else run the engine in-process
//! sorrel --in-process     always run the engine in-process
//! sorrel --daemon         run only the engine, on a local socket, so agents outlive the window
//! sorrel --replay FIXTURE.jsonl [--pace-ms N] [--seed N] [--bench OUT.json]
//!                         replay a recorded claude session over N seeded rows (perf runs)

#[cfg(feature = "bench")]
mod bench;
mod update;

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use drivers::claude;
use gpui_kit::*;
use proto::{
    AgentEvent, AuthState, AuthStatus, McpServer, ProjectInfo, Provider, Request, SettingsView,
    TaskInfo, ThreadEvent, ThreadInfo, Update,
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast::error::RecvError, mpsc},
};
use ui::{Connection, Workspace};

/// Lucide icons the UI uses beyond gpui-kit's default set (ISC license, see
/// `assets/icons/LICENSE-LUCIDE`) and Sorrel's own provider marks, served in
/// front of the default assets.
const EXTRA_ICONS: [(&str, &[u8]); 18] = [
    (
        "icons/monitor.svg",
        include_bytes!("../assets/icons/monitor.svg"),
    ),
    (
        "icons/provider-claude.svg",
        include_bytes!("../assets/icons/provider-claude.svg"),
    ),
    (
        "icons/provider-codex.svg",
        include_bytes!("../assets/icons/provider-codex.svg"),
    ),
    (
        "icons/provider-cursor.svg",
        include_bytes!("../assets/icons/provider-cursor.svg"),
    ),
    (
        "icons/provider-gemini.svg",
        include_bytes!("../assets/icons/provider-gemini.svg"),
    ),
    (
        "icons/provider-opencode.svg",
        include_bytes!("../assets/icons/provider-opencode.svg"),
    ),
    (
        "icons/square-pen.svg",
        include_bytes!("../assets/icons/square-pen.svg"),
    ),
    (
        "icons/archive.svg",
        include_bytes!("../assets/icons/archive.svg"),
    ),
    (
        "icons/chart-column.svg",
        include_bytes!("../assets/icons/chart-column.svg"),
    ),
    (
        "icons/clock.svg",
        include_bytes!("../assets/icons/clock.svg"),
    ),
    (
        "icons/folder-plus.svg",
        include_bytes!("../assets/icons/folder-plus.svg"),
    ),
    (
        "icons/layout-grid.svg",
        include_bytes!("../assets/icons/layout-grid.svg"),
    ),
    ("icons/lock.svg", include_bytes!("../assets/icons/lock.svg")),
    (
        "icons/paperclip.svg",
        include_bytes!("../assets/icons/paperclip.svg"),
    ),
    ("icons/plug.svg", include_bytes!("../assets/icons/plug.svg")),
    (
        "icons/sliders-horizontal.svg",
        include_bytes!("../assets/icons/sliders-horizontal.svg"),
    ),
    (
        "icons/text-align-start.svg",
        include_bytes!("../assets/icons/text-align-start.svg"),
    ),
    ("icons/zap.svg", include_bytes!("../assets/icons/zap.svg")),
];

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = EXTRA_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(std::borrow::Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

const USAGE: &str = "usage: sorrel [--in-process | --daemon] [--replay FIXTURE.jsonl [--pace-ms N] [--seed N] [--bench OUT.json]]";

#[derive(Default)]
struct Args {
    daemon: bool,
    in_process: bool,
    replay: Option<PathBuf>,
    pace: Option<Duration>,
    seed: usize,
    bench: Option<PathBuf>,
}

fn main() {
    let start = Instant::now();
    let args = parse_args();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let data_dir = engine::data_dir();

    if args.daemon {
        let served = runtime.block_on(async {
            let handle = engine::start(data_dir.clone())?;
            eprintln!("sorrel: engine running; windows that open now attach to it");
            engine::daemon::serve(handle, &data_dir)
                .await
                .map_err(|e| e.to_string())
        });
        if let Err(e) = served {
            exit(&e);
        }
        return;
    }

    let (update_tx, update_rx) = mpsc::channel(4096);
    let requests = match &args.replay {
        Some(fixture) => replay(&runtime, fixture, &args, &data_dir, update_tx),
        None => {
            update::check(update_tx.clone());
            connect(&runtime, &data_dir, args.in_process, update_tx)
        }
    };
    let connection = Connection {
        requests,
        updates: update_rx,
    };
    let bench = args.bench;

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // The window draws its own title bar, so the sidebar and wallpaper reach the top edge.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1280.), px(840.)), cx)),
                ..gpui_kit::component::TitleBar::window_options()
            };
            let first_frame = Rc::new(Cell::new(None::<f64>));
            let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
                let first_frame = first_frame.clone();
                window.on_next_frame(move |_, _| {
                    let ms = startup_ms(start);
                    eprintln!("sorrel: first frame after {ms:.0} ms");
                    first_frame.set(Some(ms));
                });
                cx.new(|cx| Workspace::new(connection, window, cx))
            })
            .expect("open window");

            #[cfg(feature = "bench")]
            if let Some(out) = bench {
                bench::run(window, view, out, first_frame, cx);
            }
            #[cfg(not(feature = "bench"))]
            let _ = (window, view, bench, first_frame);
        });
    drop(runtime);
}

/// Attaches to a running daemon, or starts the engine in this process.
fn connect(
    runtime: &Runtime,
    data_dir: &Path,
    in_process: bool,
    ui: mpsc::Sender<Update>,
) -> mpsc::Sender<Request> {
    if !in_process
        && let Ok((requests, mut updates)) = runtime.block_on(engine::daemon::connect(data_dir))
    {
        runtime.spawn(async move {
            while let Some(update) = updates.recv().await {
                if ui.send(update).await.is_err() {
                    break;
                }
            }
        });
        return requests;
    }

    let _guard = runtime.enter();
    let handle = engine::start(data_dir.to_owned()).unwrap_or_else(|e| exit(&e));
    let requests = handle.requests();
    let resync = requests.clone();
    let mut updates = handle.subscribe();
    runtime.spawn(async move {
        loop {
            match updates.recv().await {
                Ok(update) => {
                    if ui.send(update).await.is_err() {
                        break;
                    }
                }
                // The window fell behind: ask for everything again.
                Err(RecvError::Lagged(_)) => {
                    let _ = resync.send(Request::Hello).await;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    requests
}

/// A stand-in engine that shows one thread of `seed` rows, then streams a
/// recorded claude session into it.
fn replay(
    runtime: &Runtime,
    fixture: &Path,
    args: &Args,
    data_dir: &Path,
    ui: mpsc::Sender<Update>,
) -> mpsc::Sender<Request> {
    let fixture = std::fs::read_to_string(fixture)
        .unwrap_or_else(|e| exit(&format!("{}: {e}", fixture.display())));
    let pace = args.pace.unwrap_or(Duration::from_millis(16));
    let seed = args.seed;
    let blob_dir = data_dir.join("replay-blobs");
    let (requests, mut ignored) = mpsc::channel(256);
    runtime.spawn(async move { while ignored.recv().await.is_some() {} });
    runtime.spawn(async move {
        let mut thread = ThreadInfo {
            id: 1,
            project: None,
            title: "Replay".into(),
            provider: Provider::Claude,
            folder: PathBuf::new(),
            running: true,
            needs_input: false,
            queue: Vec::new(),
            updated_at: 0,
            pinned: false,
            archived: false,
            failed: false,
            settings: Default::default(),
            chat: false,
        };
        let opening = [
            Update::Snapshot {
                projects: vec![ProjectInfo {
                    id: 1,
                    name: "Replay project".into(),
                    folder: PathBuf::from("replay"),
                    instructions: "Keep answers short.

Prefer small diffs."
                        .into(),
                }],
                threads: vec![thread.clone()],
                tasks: vec![TaskInfo {
                    id: 1,
                    project: Some(1),
                    provider: Provider::Claude,
                    prompt: "Summarize yesterday's changes".into(),
                    every_minutes: Some(60),
                    next_run: 0,
                    thread: Some(1),
                    last_status: "finished".into(),
                }],
                auth: Provider::ALL
                    .iter()
                    .map(|&provider| AuthStatus {
                        provider,
                        state: AuthState::Subscription,
                        detail: "replay".into(),
                        bin: String::new(),
                        version: String::new(),
                        account: String::new(),
                    })
                    .collect(),
                settings: SettingsView {
                    mcp_servers: vec![McpServer {
                        name: "files".into(),
                        command: "npx".into(),
                        ..Default::default()
                    }],
                    max_sessions: 4,
                    data_dir: PathBuf::from("replay"),
                    ..Default::default()
                },
                memory: "I prefer Rust and short answers.".into(),
            },
            Update::Page {
                thread: 1,
                events: seed_history(seed),
                turn: 0,
                prepend: false,
                older: None,
            },
        ];
        for update in opening {
            if ui.send(update).await.is_err() {
                return;
            }
        }
        let (event_tx, mut event_rx) = mpsc::channel(1024);
        tokio::spawn(claude::replay(fixture, pace, blob_dir, event_tx));
        while let Some(event) = event_rx.recv().await {
            let update = Update::Event {
                thread: 1,
                event: ThreadEvent::Agent(event),
            };
            if ui.send(update).await.is_err() {
                return;
            }
        }
        // The recorded turn is over, as the engine would report it.
        thread.running = false;
        let _ = ui.send(Update::Threads(vec![thread])).await;
    });
    requests
}

/// A synthetic transcript for scroll tests: questions alternating with
/// markdown answers.
fn seed_history(count: usize) -> Vec<ThreadEvent> {
    const ANSWERS: [&str; 4] = [
        "Short answer with **bold** text and `inline code`.",
        "A list:\n\n- one\n- two\n- three\n\nAnd a paragraph after it, long enough to wrap onto a second line in a normal window.",
        "```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| a | b |\n| --- | --- |\n| 1 | 2 |",
        "한국어 답변입니다. 긴 문장이 창 폭에 맞게 줄바꿈되는지, 글자 폭이 고르게 보이는지 확인합니다.",
    ];
    (0..count)
        .map(|i| {
            if i % 2 == 0 {
                ThreadEvent::User {
                    text: format!("Question {i}"),
                    steer: false,
                }
            } else {
                ThreadEvent::Agent(AgentEvent::TextDelta {
                    msg_id: format!("seed-{i}"),
                    text: format!("### Answer {i}\n\n{}", ANSWERS[i / 2 % ANSWERS.len()]),
                })
            }
        })
        .collect()
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| exit(&format!("{flag} needs a value")))
        };
        let number = |v: String| {
            v.parse::<u64>()
                .unwrap_or_else(|_| exit(&format!("{flag} takes a number")))
        };
        match flag.as_str() {
            "--daemon" => args.daemon = true,
            "--in-process" => args.in_process = true,
            "--replay" => args.replay = Some(value().into()),
            "--pace-ms" => args.pace = Some(Duration::from_millis(number(value()))),
            "--seed" => args.seed = number(value()) as usize,
            "--bench" if cfg!(feature = "bench") => args.bench = Some(value().into()),
            "--bench" => exit("--bench needs a build with `--features bench`"),
            _ => exit(&format!("unknown argument {flag}")),
        }
    }
    if args.bench.is_some() && args.replay.is_none() {
        exit("--bench needs --replay");
    }
    args
}

fn exit(message: &str) -> ! {
    eprintln!("sorrel: {message}\n{USAGE}");
    std::process::exit(2)
}

/// Milliseconds since `SORREL_T0_MS` (Unix epoch ms, set by whoever launched
/// the process, so process creation is counted), else since `main` began.
fn startup_ms(start: Instant) -> f64 {
    let t0 = std::env::var("SORREL_T0_MS")
        .ok()
        .and_then(|v| v.parse::<u128>().ok());
    match t0 {
        Some(t0) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            now.as_millis().saturating_sub(t0) as f64
        }
        None => start.elapsed().as_secs_f64() * 1000.,
    }
}
