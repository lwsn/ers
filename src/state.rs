//! Persistent reading state, library and bookmarks in sqlite

use std::path::PathBuf;

use anyhow::Result;
use chrono::NaiveDateTime;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

use crate::models::{LibraryItem, ReadingState};

/// Directory for config and state: `~/.config/ers` if `~/.config` exists,
/// otherwise `~/.ers`. `None` if there is no home directory.
pub fn app_prefix() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    let userdir = std::env::var_os("USERPROFILE").filter(|h| !h.is_empty());
    let prefix = if let Some(home) = home {
        let home = PathBuf::from(home);
        if home.join(".config").is_dir() {
            home.join(".config").join("ers")
        } else {
            home.join(".ers")
        }
    } else {
        PathBuf::from(userdir?).join(".ers")
    };
    std::fs::create_dir_all(&prefix).ok()?;
    Some(prefix)
}

pub struct State {
    conn: Connection,
}

impl State {
    pub fn new() -> Result<Self> {
        let (conn, is_new) = match app_prefix().map(|p| p.join("states.db")) {
            Some(path) => {
                let is_new = !path.is_file();
                (Connection::open(path)?, is_new)
            }
            None => (Connection::open_in_memory()?, true),
        };
        let state = Self { conn };
        if is_new {
            state.init_db()?;
        }
        Ok(state)
    }

    pub fn get_from_history(&self) -> Result<Vec<LibraryItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT last_read, filepath, title, author, reading_progress
             FROM library ORDER BY last_read DESC",
        )?;
        let items = stmt
            .query_map([], |row| {
                let last_read: String = row.get(0)?;
                Ok(LibraryItem {
                    last_read: NaiveDateTime::parse_from_str(&last_read, "%Y-%m-%d %H:%M:%S")
                        .or_else(|_| NaiveDateTime::parse_from_str(&last_read, "%Y-%m-%dT%H:%M:%S%.f"))
                        .unwrap_or_default(),
                    filepath: row.get(1)?,
                    title: row.get(2)?,
                    author: row.get(3)?,
                    reading_progress: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn delete_from_library(&self, filepath: &str) -> Result<()> {
        self.conn.execute("PRAGMA foreign_keys = ON", [])?;
        self.conn.execute("DELETE FROM reading_states WHERE filepath=?", [filepath])?;
        self.conn.execute("PRAGMA foreign_keys = OFF", [])?;
        Ok(())
    }

    pub fn get_last_read(&self) -> Result<Option<String>> {
        Ok(self.get_from_history()?.into_iter().next().map(|i| i.filepath))
    }

    pub fn update_library(
        &self,
        filepath: &str,
        title: Option<&str>,
        author: Option<&str>,
        reading_progress: Option<f64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO library (filepath, title, author, reading_progress)
             VALUES (?, ?, ?, ?)",
            params![filepath, title, author, reading_progress],
        )?;
        Ok(())
    }

    pub fn get_last_reading_state(&self, filepath: &str) -> Result<ReadingState> {
        let state = self
            .conn
            .query_row(
                "SELECT content_index, textwidth, row, rel_pctg FROM reading_states WHERE filepath=?",
                [filepath],
                |row| {
                    Ok(ReadingState {
                        content_index: row.get::<_, i64>(0)?.max(0) as usize,
                        textwidth: row.get(1)?,
                        row: row.get(2)?,
                        rel_pctg: row.get(3)?,
                        section: None,
                    })
                },
            )
            .optional()?;
        Ok(state.unwrap_or_else(|| ReadingState::new(0, 80, 0)))
    }

    pub fn set_last_reading_state(&self, filepath: &str, rs: &ReadingState) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO reading_states VALUES (?, ?, ?, ?, ?)",
            params![filepath, rs.content_index as i64, rs.textwidth, rs.row, rs.rel_pctg],
        )?;
        Ok(())
    }

    /// Returns `false` if a bookmark with that name already exists.
    pub fn insert_bookmark(&self, filepath: &str, name: &str, rs: &ReadingState) -> Result<bool> {
        let id = sha1_smol::Sha1::from(format!("{filepath}{name}")).digest().to_string()[..10].to_string();
        let res = self.conn.execute(
            "INSERT INTO bookmarks VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![id, filepath, name, rs.content_index as i64, rs.textwidth, rs.row, rs.rel_pctg],
        );
        match res {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::ConstraintViolation => {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_bookmark(&self, filepath: &str, name: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM bookmarks WHERE filepath=? AND name=?", [filepath, name])?;
        Ok(())
    }

    pub fn get_bookmarks(&self, filepath: &str) -> Result<Vec<(String, ReadingState)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, content_index, textwidth, row, rel_pctg FROM bookmarks WHERE filepath=?",
        )?;
        let items = stmt
            .query_map([filepath], |row| {
                Ok((
                    row.get(0)?,
                    ReadingState {
                        content_index: row.get::<_, i64>(1)?.max(0) as usize,
                        textwidth: row.get(2)?,
                        row: row.get(3)?,
                        rel_pctg: row.get(4)?,
                        section: None,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    fn init_db(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS reading_states (
                filepath TEXT PRIMARY KEY,
                content_index INTEGER,
                textwidth INTEGER,
                row INTEGER,
                rel_pctg REAL
            );

            CREATE TABLE IF NOT EXISTS library (
                last_read DATETIME DEFAULT (datetime('now','localtime')),
                filepath TEXT PRIMARY KEY,
                title TEXT,
                author TEXT,
                reading_progress REAL,
                FOREIGN KEY (filepath) REFERENCES reading_states(filepath)
                ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS bookmarks (
                id TEXT PRIMARY KEY,
                filepath TEXT,
                name TEXT,
                content_index INTEGER,
                textwidth INTEGER,
                row INTEGER,
                rel_pctg REAL,
                FOREIGN KEY (filepath) REFERENCES reading_states(filepath)
                ON DELETE CASCADE
            );",
        )?;
        Ok(())
    }
}
