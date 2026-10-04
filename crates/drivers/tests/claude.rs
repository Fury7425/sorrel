use std::{fs, path::PathBuf, time::Duration};

use drivers::{
    DriverCommand, SessionConfig,
    claude::{Translator, run},
};
use proto::{AgentEvent, StopReason, TurnSettings};
use tokio::sync::mpsc;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude")
}

fn blob_dir() -> PathBuf {
    std::env::temp_dir().join(format!("sorrel-test-blobs-{}", std::process::id()))
}

fn translate(fixture: &str) -> Vec<AgentEvent> {
    let mut translator = Translator::new(blob_dir());
    let mut events = Vec::new();
    for line in fixture.lines() {
        translator.translate(line, &mut events);
    }
    events
}

/// Every `fixtures/claude/*.jsonl` must translate to its `*.events.json`
/// snapshot. `UPDATE_SNAPSHOTS=1` rewrites the snapshots instead.
#[test]
fn fixtures_match_snapshots() {
    let mut checked = 0;
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let events = translate(&fs::read_to_string(&path).unwrap());
        let snapshot = path.with_extension("events.json");
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            let lines: Vec<String> = events
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .collect();
            fs::write(&snapshot, format!("[\n{}\n]\n", lines.join(",\n"))).unwrap();
        }
        let expected: Vec<AgentEvent> = serde_json::from_str(
            &fs::read_to_string(&snapshot)
                .unwrap_or_else(|_| panic!("missing snapshot {}", snapshot.display())),
        )
        .unwrap();
        assert_eq!(events, expected, "{}", path.display());
        checked += 1;
    }
    assert!(checked > 0, "no fixtures found");
}

#[test]
fn unknown_and_malformed_lines_are_ignored() {
    let lines = [
        "",
        "not json",
        r#"{"type":"brand_new_event","x":1}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"citations_delta"}}}"#,
        r#"{"type":"stream_event","parent_tool_use_id":"toolu_1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"from a subagent"}}}"#,
    ];
    assert!(translate(&lines.join("\n")).is_empty());
}

#[test]
fn permission_requests_come_back_to_the_caller() {
    let fixture = fs::read_to_string(fixtures().join("tools_and_permission.jsonl")).unwrap();
    let mut translator = Translator::new(blob_dir());
    let mut events = Vec::new();
    let requests: Vec<_> = fixture
        .lines()
        .filter_map(|line| translator.translate(line, &mut events))
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].request_id, "req_1");
    assert_eq!(requests[0].tool_name, "Bash");
    assert_eq!(requests[0].input["command"], "ls");
}

fn fake(
    cwd: PathBuf,
    turn_timeout: Duration,
) -> (mpsc::Sender<DriverCommand>, mpsc::Receiver<AgentEvent>) {
    let mut cfg = SessionConfig::new(env!("CARGO_BIN_EXE_fake_claude").into(), cwd, blob_dir());
    cfg.turn_timeout = turn_timeout;
    // API-key mode skips the `claude auth status` pre-flight.
    cfg.api_key = Some("test".into());
    let (command_tx, command_rx) = mpsc::channel(1);
    let (event_tx, event_rx) = mpsc::channel(64);
    tokio::spawn(run(cfg, command_rx, event_tx));
    (command_tx, event_rx)
}

fn send(text: &str) -> DriverCommand {
    DriverCommand::Prompt {
        text: text.into(),
        settings: TurnSettings::default(),
        steer: false,
    }
}

#[tokio::test]
async fn live_turn_streams_through_the_translator() {
    let cwd = std::env::temp_dir().join(format!("sorrel-fake-live-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    let fixture = fs::read_to_string(fixtures().join("synthetic_stream.jsonl")).unwrap();
    fs::write(cwd.join("fake_reply.jsonl"), &fixture).unwrap();

    let (commands, mut events) = fake(cwd, Duration::from_secs(10));
    commands.send(send("hi")).await.unwrap();
    let mut got = Vec::new();
    while let Some(event) = events.recv().await {
        let done = matches!(event, AgentEvent::TurnEnded { .. });
        got.push(event);
        if done {
            break;
        }
    }
    assert_eq!(got, translate(&fixture));
}

#[tokio::test]
async fn silent_turn_is_killed_by_the_watchdog() {
    let cwd = std::env::temp_dir().join(format!("sorrel-fake-silent-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    let (commands, mut events) = fake(cwd, Duration::from_millis(300));
    commands.send(send("hi")).await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(AgentEvent::Error { .. })
    ));
    assert_eq!(
        events.recv().await,
        Some(AgentEvent::TurnEnded {
            reason: StopReason::Timeout
        })
    );
}
