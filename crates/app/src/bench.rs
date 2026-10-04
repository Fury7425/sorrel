//! `--bench`: lets the replay stream, scrolls the whole transcript, then idles,
//! recording every frame GPUI draws. Writes JSON that `scripts/perf.py` reads.

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::{
    profiler::{FrameEvent, FrameTimingCollector, set_trace_enabled},
    *,
};
use ui::{Screen, Workspace};

/// perf.py samples RSS and CPU inside this window.
const IDLE: Duration = Duration::from_secs(15);
const SCROLL_ROWS_PER_STEP: usize = 20;
const SCROLL_STEP: Duration = Duration::from_millis(8);
const SETTLE: Duration = Duration::from_secs(3);
const SCREEN_DWELL: Duration = Duration::from_millis(500);
const SCREENS: [(&str, Screen); 5] = [
    ("settings", Screen::Settings),
    ("tasks", Screen::Tasks),
    ("project", Screen::Project),
    ("home", Screen::Home),
    ("thread", Screen::Thread),
];

pub fn run(
    window: AnyWindowHandle,
    view: Entity<Workspace>,
    out: PathBuf,
    first_frame: Rc<Cell<Option<f64>>>,
    cx: &mut App,
) {
    set_trace_enabled(true);
    let mut collector = FrameTimingCollector::new();
    cx.spawn(async move |cx| {
        // Startup (opening the window and loading the seeded page) is not
        // streaming; wait for the thread to show, then start counting.
        loop {
            if view.read_with(cx, |workspace, _| workspace.thread_view().is_some()) {
                break;
            }
            cx.background_executor().timer(Duration::from_millis(10)).await;
        }
        cx.background_executor().timer(Duration::from_millis(200)).await;
        collector.collect_unseen();

        // Stream: wait for the replayed turn to end.
        let thread = loop {
            let done = view.read_with(cx, |workspace, cx| {
                workspace
                    .thread_view()
                    .filter(|thread| thread.read(cx).turns_ended() > 0)
            });
            if let Some(thread) = done {
                break thread;
            }
            cx.background_executor().timer(Duration::from_millis(50)).await;
        };
        let stream = Phase::new(collector.collect_unseen());

        // Scroll from the newest row to the oldest. Each step jumps past rows
        // that were never on screen, so every frame lays out fresh rows.
        let rows = thread.read_with(cx, |thread, _| thread.row_count());
        for ix in (0..rows).rev().step_by(SCROLL_ROWS_PER_STEP) {
            thread.update(cx, |thread, cx| thread.scroll_to_row(ix, cx));
            cx.background_executor().timer(SCROLL_STEP).await;
        }
        let scroll = Phase::new(collector.collect_unseen());

        // Every screen: switch to it, then let it settle. The first frame
        // after the switch is the cost of opening that screen.
        let mut screens = Vec::new();
        for (name, screen) in SCREENS {
            collector.collect_unseen();
            let shown = view.clone();
            let _ = cx.update_window(window, |_, window, cx| {
                shown.update(cx, |workspace, cx| workspace.show(screen, window, cx))
            });
            cx.background_executor().timer(SCREEN_DWELL).await;
            screens.push(format!(
                r#""{name}":{}"#,
                Phase::new(collector.collect_unseen()).json()
            ));
        }

        // Let the last transitions (streamed-text fade, scrollbar, jump
        // button) finish; idle starts once nothing is moving.
        cx.background_executor().timer(SETTLE).await;
        collector.collect_unseen();

        // Idle: nothing changes, so nothing should draw.
        write(&out, r#"{"phase":"idle"}"#);
        cx.background_executor().timer(IDLE).await;
        let idle = Phase::new(collector.collect_unseen());

        let first_frame_ms = first_frame.get().unwrap_or(-1.);
        let report = format!(
            r#"{{"phase":"done","first_frame_ms":{first_frame_ms:.1},"rows":{rows},"stream":{},"scroll":{},"screens":{{{}}},"idle":{}}}"#,
            stream.json(),
            scroll.json(),
            screens.join(","),
            idle.json(),
        );
        write(&out, &report);
        cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Phase {
    /// Time spent in `Window::draw` per frame.
    draw_ms: Vec<f64>,
    /// Time between consecutive presented frames.
    present_gap_ms: Vec<f64>,
}

impl Phase {
    fn new(events: Vec<FrameEvent>) -> Self {
        let mut draw_ms = Vec::new();
        let mut presents = Vec::new();
        for event in events {
            match event {
                FrameEvent::Draw(timing) => {
                    draw_ms.push(timing.draw_duration().as_secs_f64() * 1000.)
                }
                FrameEvent::Present(timing) => presents.push(timing.present_end),
            }
        }
        let present_gap_ms = presents
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs_f64() * 1000.)
            .collect();
        Self {
            draw_ms,
            present_gap_ms,
        }
    }

    fn json(mut self) -> String {
        format!(
            r#"{{"frames":{},"draw_ms":{},"present_gap_ms":{}}}"#,
            self.draw_ms.len(),
            percentiles(&mut self.draw_ms),
            percentiles(&mut self.present_gap_ms),
        )
    }
}

fn percentiles(values: &mut [f64]) -> String {
    values.sort_by(f64::total_cmp);
    let at = |p: f64| {
        let ix = (values.len().saturating_sub(1) as f64 * p).round() as usize;
        values.get(ix).copied().unwrap_or(0.)
    };
    format!(
        r#"{{"p50":{:.2},"p95":{:.2},"p99":{:.2},"max":{:.2}}}"#,
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0)
    )
}

fn write(path: &Path, json: &str) {
    if let Err(e) = std::fs::write(path, json) {
        eprintln!("sorrel: {}: {e}", path.display());
    }
}
