//! SQLite state store (§3). The daemon holds everything in memory and writes
//! through, so a restart can recover contexts, tasks and pending requests.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub struct Store {
    connection: Mutex<Connection>,
}

pub struct ContextRow {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub mode: String,
    pub on_question: String,
    pub cursor: Option<String>,
    pub owner_pid: Option<i64>,
    pub owner_start: Option<String>,
    pub depth: i64,
    pub closed: bool,
    pub created_at: i64,
}

pub struct TaskRow {
    pub context_id: String,
    pub id: i64,
    pub state: String,
    pub dormant: bool,
    pub prompt: String,
    pub result_text: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

pub struct RequestRow {
    pub context_id: String,
    pub id: String,
    pub task_id: i64,
    pub kind: String,
    pub driver_request_id: String,
    pub payload: String,
    pub answered: bool,
    pub answer: Option<String>,
    pub answered_at: Option<i64>,
    pub created_at: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS contexts (
                id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT,
                reasoning_effort TEXT,
                mode TEXT NOT NULL,
                on_question TEXT NOT NULL DEFAULT 'yield',
                cursor TEXT,
                owner_pid INTEGER,
                owner_start TEXT,
                depth INTEGER NOT NULL DEFAULT 1,
                closed INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                context_id TEXT NOT NULL,
                id INTEGER NOT NULL,
                state TEXT NOT NULL,
                dormant INTEGER NOT NULL DEFAULT 0,
                prompt TEXT NOT NULL,
                result_text TEXT NOT NULL DEFAULT '',
                error TEXT,
                created_at INTEGER NOT NULL,
                finished_at INTEGER,
                PRIMARY KEY (context_id, id)
            );
            CREATE TABLE IF NOT EXISTS requests (
                context_id TEXT NOT NULL,
                id TEXT NOT NULL,
                task_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                driver_request_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                answered INTEGER NOT NULL DEFAULT 0,
                answer TEXT,
                answered_at INTEGER,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (context_id, id)
            );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert_context(&self, context: &ContextRow) {
        let _ = self.with(|db| {
            db.execute(
                "INSERT INTO contexts (id, provider, cwd, model, reasoning_effort, mode, on_question, cursor, owner_pid, owner_start, depth, closed, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    context.id,
                    context.provider,
                    context.cwd,
                    context.model,
                    context.reasoning_effort,
                    context.mode,
                    context.on_question,
                    context.cursor,
                    context.owner_pid,
                    context.owner_start,
                    context.depth,
                    context.closed as i64,
                    context.created_at,
                ],
            )?;
            Ok(())
        });
    }

    pub fn update_cursor(&self, context_id: &str, cursor: Option<&str>) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE contexts SET cursor = ?2 WHERE id = ?1",
                rusqlite::params![context_id, cursor],
            )?;
            Ok(())
        });
    }

    pub fn update_owner(&self, context_id: &str, pid: i64, start: &str) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE contexts SET owner_pid = ?2, owner_start = ?3 WHERE id = ?1",
                rusqlite::params![context_id, pid, start],
            )?;
            Ok(())
        });
    }

    pub fn set_closed(&self, context_id: &str) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE contexts SET closed = 1 WHERE id = ?1",
                rusqlite::params![context_id],
            )?;
            Ok(())
        });
    }

    pub fn insert_task(&self, task: &TaskRow) {
        let _ = self.with(|db| {
            db.execute(
                "INSERT INTO tasks (context_id, id, state, dormant, prompt, result_text, error, created_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    task.context_id,
                    task.id,
                    task.state,
                    task.dormant as i64,
                    task.prompt,
                    task.result_text,
                    task.error,
                    task.created_at,
                    task.finished_at,
                ],
            )?;
            Ok(())
        });
    }

    pub fn update_task_state(&self, context_id: &str, task_id: i64, state: &str) {
        let finished = matches!(state, "completed" | "failed" | "canceled").then(now);
        let _ = self.with(|db| {
            db.execute(
                "UPDATE tasks SET state = ?3, finished_at = COALESCE(?4, finished_at) WHERE context_id = ?1 AND id = ?2",
                rusqlite::params![context_id, task_id, state, finished],
            )?;
            Ok(())
        });
    }

    pub fn update_task_error(&self, context_id: &str, task_id: i64, error: &str) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE tasks SET error = ?3 WHERE context_id = ?1 AND id = ?2",
                rusqlite::params![context_id, task_id, error],
            )?;
            Ok(())
        });
    }

    pub fn update_task_result(&self, context_id: &str, task_id: i64, result_text: &str) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE tasks SET result_text = ?3 WHERE context_id = ?1 AND id = ?2",
                rusqlite::params![context_id, task_id, result_text],
            )?;
            Ok(())
        });
    }

    pub fn update_task_dormant(&self, context_id: &str, task_id: i64, dormant: bool) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE tasks SET dormant = ?3 WHERE context_id = ?1 AND id = ?2",
                rusqlite::params![context_id, task_id, dormant as i64],
            )?;
            Ok(())
        });
    }

    pub fn insert_request(&self, request: &RequestRow) {
        let _ = self.with(|db| {
            db.execute(
                "INSERT INTO requests (context_id, id, task_id, kind, driver_request_id, payload, answered, answer, answered_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    request.context_id,
                    request.id,
                    request.task_id,
                    request.kind,
                    request.driver_request_id,
                    request.payload,
                    request.answered as i64,
                    request.answer,
                    request.answered_at,
                    request.created_at,
                ],
            )?;
            Ok(())
        });
    }

    pub fn update_request_answered(&self, context_id: &str, request_id: &str, answer: &str) {
        let _ = self.with(|db| {
            db.execute(
                "UPDATE requests SET answered = 1, answer = ?3, answered_at = ?4 WHERE context_id = ?1 AND id = ?2",
                rusqlite::params![context_id, request_id, answer, now()],
            )?;
            Ok(())
        });
    }

    pub fn load_contexts(&self) -> Vec<ContextRow> {
        self.with(|db| {
            let mut statement = db.prepare(
                "SELECT id, provider, cwd, model, reasoning_effort, mode, on_question, cursor, owner_pid, owner_start, depth, closed, created_at FROM contexts",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ContextRow {
                        id: row.get(0)?,
                        provider: row.get(1)?,
                        cwd: row.get(2)?,
                        model: row.get(3)?,
                        reasoning_effort: row.get(4)?,
                        mode: row.get(5)?,
                        on_question: row.get(6)?,
                        cursor: row.get(7)?,
                        owner_pid: row.get(8)?,
                        owner_start: row.get(9)?,
                        depth: row.get(10)?,
                        closed: row.get::<_, i64>(11)? != 0,
                        created_at: row.get(12)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn load_tasks(&self) -> Vec<TaskRow> {
        self.with(|db| {
            let mut statement = db.prepare(
                "SELECT context_id, id, state, dormant, prompt, result_text, error, created_at, finished_at FROM tasks ORDER BY context_id, id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(TaskRow {
                        context_id: row.get(0)?,
                        id: row.get(1)?,
                        state: row.get(2)?,
                        dormant: row.get::<_, i64>(3)? != 0,
                        prompt: row.get(4)?,
                        result_text: row.get(5)?,
                        error: row.get(6)?,
                        created_at: row.get(7)?,
                        finished_at: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn load_task_result(&self, context_id: &str, task_id: i64) -> Option<String> {
        self.with(|db| {
            db.query_row(
                "SELECT result_text FROM tasks WHERE context_id = ? AND id = ?",
                [context_id, &task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or(Ok(None))
        })
        .unwrap_or(None)
    }

    pub fn load_requests(&self) -> Vec<RequestRow> {
        self.with(|db| {
            let mut statement = db.prepare(
                "SELECT context_id, id, task_id, kind, driver_request_id, payload, answered, answer, answered_at, created_at FROM requests ORDER BY context_id, created_at",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(RequestRow {
                        context_id: row.get(0)?,
                        id: row.get(1)?,
                        task_id: row.get(2)?,
                        kind: row.get(3)?,
                        driver_request_id: row.get(4)?,
                        payload: row.get(5)?,
                        answered: row.get::<_, i64>(6)? != 0,
                        answer: row.get(7)?,
                        answered_at: row.get(8)?,
                        created_at: row.get(9)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        f(&connection)
            .map_err(|error| anyhow::anyhow!("state.db write failed: {error}"))
    }
}
