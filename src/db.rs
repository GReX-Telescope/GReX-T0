//! Interactions with the sqlite candidate database
use rusqlite::{Connection, Result};
use std::path::PathBuf;

fn create_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS injection (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mjd REAL NOT NULL,
        filename TEXT NOT NULL,
        sample INTEGER NOT NULL
    ) STRICT",
        (),
    )?;
    Ok(())
}

/// Connect to the database, and create the injection table if it doesn't already exist
pub fn connect_and_create(db_path: PathBuf) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    create_table(&conn)?;
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
            "INSERT INTO injection (mjd, filename, sample) VALUES (?1, ?2, ?3)",
            (&self.mjd, &self.filename, &self.sample),
        )?;
        Ok(())
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    fn test_db() {
        let conn = Connection::open_in_memory().unwrap();
        create_table(&conn).unwrap();
        let ir = InjectionRecord {
            mjd: 123.456,
            filename: "foo".to_owned(),
            sample: 12345,
        };
        ir.db_insert(&conn).unwrap()
    }
}
