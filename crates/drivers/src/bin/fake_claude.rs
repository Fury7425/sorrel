//! Stand-in for `claude` in driver tests. Answers every stdin line with the
//! contents of `fake_reply.jsonl` in its working directory, or never answers
//! when that file is missing.

use std::io::{BufRead, Write};

fn main() {
    let reply = std::fs::read_to_string("fake_reply.jsonl").ok();
    for line in std::io::stdin().lock().lines() {
        if line.is_err() {
            return;
        }
        if let Some(reply) = &reply {
            let mut out = std::io::stdout().lock();
            out.write_all(reply.as_bytes()).unwrap();
            out.flush().unwrap();
        }
    }
}
