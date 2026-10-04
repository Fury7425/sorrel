//! The SQLite index: projects, threads, tasks, checkpoints and each thread's
//! event log. Large payloads live in blob files; rows hold refs. The CLIs keep
//! their own transcripts; this keeps only what the app renders and resumes.

use std::path::{Path, PathBuf};

use proto::{ProjectId, Provider, Seq, TaskId, TaskInfo, ThreadEvent, ThreadId};
use rusqlite::{Connection, OptionalExtension, Result, Row, params};

const SCHEMA: &str = "
PRAGMA auto_vacuum = INCREMENTAL;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS projects(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    folder TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS threads(
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    provider TEXT NOT NULL,
    folder TEXT NOT NULL,
    session TEXT,
    turns INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events(
    id INTEGER PRIMARY KEY,
    thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    item INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_by_thread ON events(thread_id, id);
CREATE TABLE IF NOT EXISTS checkpoints(
    thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    turn INTEGER NOT NULL,
    before_commit TEXT,
    after_commit TEXT,
    PRIMARY KEY(thread_id, turn)
);
CREATE TABLE IF NOT EXISTS tasks(
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    prompt TEXT NOT NULL,
    every_minutes INTEGER,
    next_run INTEGER NOT NULL,
    thread_id INTEGER REFERENCES threads(id) ON DELETE SET NULL,
    last_status TEXT NOT NULL DEFAULT ''
);
";

pub struct Store {
    db: Connection,
}

pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub folder: PathBuf,
}

pub struct ThreadRow {
    pub id: ThreadId,
    pub project: Option<ProjectId>,
    pub title: String,
    pub provider: Provider,
    pub folder: PathBuf,
    pub session: Option<String>,
    pub turns: u32,
    pub updated_at: i64,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn path(text: String) -> PathBuf {
    PathBuf::from(text)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl Store {
    pub fn open(file: &Path) -> Result<Store> {
        Self::init(Connection::open(file)?)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Store> {
        db.execute_batch(SCHEMA)?;
        Ok(Store { db })
    }

    // Projects.

    pub fn create_project(&self, name: &str, folder: &Path) -> Result<ProjectId> {
        self.db.execute(
            "INSERT INTO projects(name, folder) VALUES (?1, ?2)",
            params![name, text(folder)],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn rename_project(&self, id: ProjectId, name: &str) -> Result<()> {
        self.db.execute(
            "UPDATE projects SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_project(&self, id: ProjectId) -> Result<()> {
        self.db
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<ProjectRow>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, name, folder FROM projects ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                folder: path(r.get(2)?),
            })
        })?;
        rows.collect()
    }

    pub fn project(&self, id: ProjectId) -> Result<Option<ProjectRow>> {
        self.db
            .query_row(
                "SELECT id, name, folder FROM projects WHERE id = ?1",
                params![id],
                |r| {
                    Ok(ProjectRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        folder: path(r.get(2)?),
                    })
                },
            )
            .optional()
    }

    // Threads.

    pub fn create_thread(
        &self,
        project: Option<ProjectId>,
        title: &str,
        provider: Provider,
        folder: &Path,
    ) -> Result<ThreadId> {
        self.db.execute(
            "INSERT INTO threads(project_id, title, provider, folder, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project, title, provider.key(), text(folder), now()],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn set_thread_folder(&self, id: ThreadId, folder: &Path) -> Result<()> {
        self.db.execute(
            "UPDATE threads SET folder = ?2 WHERE id = ?1",
            params![id, text(folder)],
        )?;
        Ok(())
    }

    pub fn set_title(&self, id: ThreadId, title: &str) -> Result<()> {
        self.db.execute(
            "UPDATE threads SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn set_session(&self, id: ThreadId, session: &str) -> Result<()> {
        self.db.execute(
            "UPDATE threads SET session = ?2 WHERE id = ?1",
            params![id, session],
        )?;
        Ok(())
    }

    pub fn delete_thread(&self, id: ThreadId) -> Result<()> {
        self.db
            .execute("DELETE FROM threads WHERE id = ?1", params![id])?;
        Ok(())
    }

    fn thread_row(r: &Row<'_>) -> Result<ThreadRow> {
        let provider: String = r.get(3)?;
        Ok(ThreadRow {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            provider: Provider::from_key(&provider).unwrap_or(Provider::Claude),
            folder: path(r.get(4)?),
            session: r.get(5)?,
            turns: r.get(6)?,
            updated_at: r.get(7)?,
        })
    }

    pub fn threads(&self) -> Result<Vec<ThreadRow>> {
        let mut stmt = self.db.prepare(
            "SELECT id, project_id, title, provider, folder, session, turns, updated_at
             FROM threads ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], Self::thread_row)?;
        rows.collect()
    }

    pub fn thread(&self, id: ThreadId) -> Result<Option<ThreadRow>> {
        self.db
            .query_row(
                "SELECT id, project_id, title, provider, folder, session, turns, updated_at
                 FROM threads WHERE id = ?1",
                params![id],
                Self::thread_row,
            )
            .optional()
    }

    // The event log.

    /// Appends one event. A user message that starts a turn bumps the
    /// thread's turn count; each row records the count before it.
    pub fn append(&self, thread: ThreadId, event: &ThreadEvent) -> Result<Seq> {
        let body = serde_json::to_string(event).expect("events serialize");
        let turn: u32 = self.db.query_row(
            "SELECT turns FROM threads WHERE id = ?1",
            params![thread],
            |r| r.get(0),
        )?;
        self.db.execute(
            "INSERT INTO events(thread_id, item, turn, body) VALUES (?1, ?2, ?3, ?4)",
            params![thread, event.starts_item(), turn, body],
        )?;
        let seq = self.db.last_insert_rowid();
        let starts_turn = matches!(event, ThreadEvent::User { steer: false, .. });
        self.db.execute(
            "UPDATE threads SET updated_at = ?2, turns = turns + ?3 WHERE id = ?1",
            params![thread, now(), starts_turn as i64],
        )?;
        Ok(seq)
    }

    /// The last `items` rows' worth of events before `before`, oldest first,
    /// with the turn count before the first one and where the next older page
    /// would end.
    pub fn page(
        &self,
        thread: ThreadId,
        before: Option<Seq>,
        items: usize,
    ) -> Result<(Vec<ThreadEvent>, u32, Option<Seq>)> {
        let before = before.unwrap_or(i64::MAX);
        let start: Seq = self
            .db
            .query_row(
                "SELECT id FROM events WHERE thread_id = ?1 AND item = 1 AND id < ?2
                 ORDER BY id DESC LIMIT 1 OFFSET ?3",
                params![thread, before, items.saturating_sub(1) as i64],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut stmt = self.db.prepare(
            "SELECT id, turn, body FROM events WHERE thread_id = ?1 AND id >= ?2 AND id < ?3 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![thread, start, before], |r| {
                Ok((
                    r.get::<_, Seq>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?;
        let first = rows.first().map(|(seq, turn, _)| (*seq, *turn));
        let events = rows
            .iter()
            // ponytail: rows from an older, incompatible build are skipped, not migrated.
            .filter_map(|(_, _, body)| serde_json::from_str(body).ok())
            .collect();
        let older = match first {
            Some((seq, _)) => self
                .db
                .query_row(
                    "SELECT 1 FROM events WHERE thread_id = ?1 AND id < ?2 LIMIT 1",
                    params![thread, seq],
                    |_| Ok(()),
                )
                .optional()?
                .map(|_| seq),
            None => None,
        };
        Ok((events, first.map(|(_, turn)| turn).unwrap_or(0), older))
    }

    // Checkpoints.

    pub fn set_checkpoint(
        &self,
        thread: ThreadId,
        turn: u32,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO checkpoints(thread_id, turn, before_commit, after_commit) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(thread_id, turn) DO UPDATE SET
                 before_commit = COALESCE(excluded.before_commit, before_commit),
                 after_commit = COALESCE(excluded.after_commit, after_commit)",
            params![thread, turn, before, after],
        )?;
        Ok(())
    }

    pub fn checkpoint(
        &self,
        thread: ThreadId,
        turn: u32,
    ) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .db
            .query_row(
                "SELECT before_commit, after_commit FROM checkpoints WHERE thread_id = ?1 AND turn = ?2",
                params![thread, turn],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    // Tasks.

    pub fn create_task(
        &self,
        project: Option<ProjectId>,
        provider: Provider,
        prompt: &str,
        every_minutes: Option<u32>,
    ) -> Result<TaskId> {
        self.db.execute(
            "INSERT INTO tasks(project_id, provider, prompt, every_minutes, next_run) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project, provider.key(), prompt, every_minutes, now()],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn delete_task(&self, id: TaskId) -> Result<()> {
        self.db
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// `next_run` below zero means the task is done.
    pub fn set_task_run(
        &self,
        id: TaskId,
        next_run: i64,
        thread: Option<ThreadId>,
        status: &str,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE tasks SET next_run = ?2, thread_id = COALESCE(?3, thread_id), last_status = ?4 WHERE id = ?1",
            params![id, next_run, thread, status],
        )?;
        Ok(())
    }

    pub fn set_task_status(&self, id: TaskId, status: &str) -> Result<()> {
        self.db.execute(
            "UPDATE tasks SET last_status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    fn task_query(&self, filter: &str, at: i64) -> Result<Vec<TaskInfo>> {
        let sql = format!(
            "SELECT id, project_id, provider, prompt, every_minutes, next_run, thread_id, last_status
             FROM tasks {filter} ORDER BY id"
        );
        let mut stmt = self.db.prepare(&sql)?;
        let map = |r: &Row<'_>| {
            let provider: String = r.get(2)?;
            Ok(TaskInfo {
                id: r.get(0)?,
                project: r.get(1)?,
                provider: Provider::from_key(&provider).unwrap_or(Provider::Claude),
                prompt: r.get(3)?,
                every_minutes: r.get(4)?,
                next_run: r.get(5)?,
                thread: r.get(6)?,
                last_status: r.get(7)?,
            })
        };
        if filter.contains("?1") {
            stmt.query_map(params![at], map)?.collect()
        } else {
            stmt.query_map([], map)?.collect()
        }
    }

    pub fn tasks(&self) -> Result<Vec<TaskInfo>> {
        self.task_query("", 0)
    }

    pub fn due_tasks(&self, at: i64) -> Result<Vec<TaskInfo>> {
        self.task_query("WHERE next_run >= 0 AND next_run <= ?1", at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{AgentEvent, StopReason};

    #[test]
    fn pages_start_on_a_row_and_carry_the_turn_count() {
        let store = Store::in_memory().unwrap();
        let thread = store
            .create_thread(None, "t", Provider::Claude, Path::new("/tmp"))
            .unwrap();
        for i in 0..5 {
            store
                .append(
                    thread,
                    &ThreadEvent::User {
                        text: format!("q{i}"),
                        steer: false,
                    },
                )
                .unwrap();
            store
                .append(
                    thread,
                    &ThreadEvent::Agent(AgentEvent::Usage {
                        input: 1,
                        output: 1,
                    }),
                )
                .unwrap();
            store
                .append(
                    thread,
                    &ThreadEvent::Agent(AgentEvent::TurnEnded {
                        reason: StopReason::EndTurn,
                    }),
                )
                .unwrap();
        }
        assert_eq!(store.thread(thread).unwrap().unwrap().turns, 5);

        // 4 rows back from the end: q3's user row, its turn end, q4, its turn end.
        let (events, turn, older) = store.page(thread, None, 4).unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(
            events[0],
            ThreadEvent::User {
                text: "q3".into(),
                steer: false
            }
        );
        assert_eq!(turn, 3);
        let older = older.expect("there is more");

        let (events, turn, rest) = store.page(thread, Some(older), 100).unwrap();
        assert_eq!(events.len(), 9);
        assert_eq!(turn, 0);
        assert_eq!(rest, None);
    }

    #[test]
    fn checkpoints_merge_before_and_after() {
        let store = Store::in_memory().unwrap();
        let thread = store
            .create_thread(None, "t", Provider::Codex, Path::new("/tmp"))
            .unwrap();
        store.set_checkpoint(thread, 1, Some("a"), None).unwrap();
        store.set_checkpoint(thread, 1, None, Some("b")).unwrap();
        assert_eq!(
            store.checkpoint(thread, 1).unwrap(),
            (Some("a".into()), Some("b".into()))
        );
    }
}
