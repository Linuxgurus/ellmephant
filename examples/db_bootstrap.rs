//! Open a throwaway store, report what the migration produced, and tear it down.
//!
//! `cargo run --example db_bootstrap`

use llmphant::config::Paths;
use llmphant::db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("llmphant-example");
    let paths = Paths::from_root(&dir);
    let conn = db::open(&paths)?;

    println!("root           {}", paths.root().display());
    println!("schema version {}", db::migrate::version(&conn)?);

    let mut stmt = conn.prepare(
        "SELECT type, name FROM sqlite_master
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (kind, name) = row?;
        println!("  {kind:<7} {name}");
    }

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
