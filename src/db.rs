//! Interactions with the sqlite candidate database
use rusqlite::{Connection, Result};
use std::path::PathBuf;

/// Connect to the database, and create the injection table if it doesn't already exist
pub fn connect_and_create(db_path: PathBuf) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS injection (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mjd REAL NOT NULL,
        filename TEXT NOT NULL,
        sample INTEGER NOT NULL
    ) STRICT;",
        (),
    )?;
    Ok(conn)
}

#[derive(Debug)]
pub struct InjectionRecord {
    pub mjd: f64,
    pub filename: String,
    pub sample: u64,
}

impl InjectionRecord {
    /// Insert an injection record into the connected database
    pub fn db_insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO injection (mjd, filename, sample)",
            (&self.mjd, &self.filename, &self.sample),
        )?;
        Ok(())
    }
}
