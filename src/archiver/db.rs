use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    url         TEXT UNIQUE NOT NULL,
    platform    TEXT NOT NULL,
    archive_dir TEXT NOT NULL,
    last_scan   TIMESTAMP
);

CREATE TABLE IF NOT EXISTS videos (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id  INTEGER REFERENCES channels(id) ON DELETE CASCADE,
    video_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    upload_date TEXT,
    duration    REAL,
    playlist    TEXT,
    downloaded  BOOLEAN DEFAULT FALSE,
    UNIQUE(channel_id, video_id)
);

CREATE INDEX IF NOT EXISTS idx_videos_channel ON videos(channel_id);
CREATE INDEX IF NOT EXISTS idx_videos_downloaded ON videos(channel_id, downloaded);
"#;

pub fn open_and_init(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn upsert_channel(
    conn: &Connection,
    name: &str,
    url: &str,
    platform: &str,
    archive_dir: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO channels (name, url, platform, archive_dir) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(url) DO UPDATE SET name = ?1, archive_dir = ?4",
        rusqlite::params![name, url, platform, archive_dir],
    )?;
    let id = conn.query_row("SELECT id FROM channels WHERE url = ?1", [url], |r| {
        r.get(0)
    })?;
    Ok(id)
}

pub fn insert_videos(
    conn: &Connection,
    channel_id: i64,
    videos: &[(String, String, String, Option<f64>, Option<String>)],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO videos (channel_id, video_id, title, upload_date, duration, playlist) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (vid, title, date, dur, playlist) in videos {
        stmt.execute(rusqlite::params![
            channel_id, vid, title, date, dur, playlist
        ])?;
    }
    Ok(())
}

pub fn mark_downloaded(conn: &Connection, channel_id: i64, video_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE videos SET downloaded = TRUE WHERE channel_id = ?1 AND video_id = ?2",
        rusqlite::params![channel_id, video_id],
    )?;
    Ok(())
}

pub fn get_pending_videos(
    conn: &Connection,
    channel_id: i64,
) -> Result<Vec<(String, String, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT video_id, title, upload_date, playlist FROM videos WHERE channel_id = ?1 AND downloaded = FALSE ORDER BY upload_date DESC",
    )?;
    let results = stmt
        .query_map([channel_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(results)
}
