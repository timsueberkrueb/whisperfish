#![recursion_limit = "512"]

#[macro_use]
extern crate cpp;

#[macro_use]
extern crate diesel;

#[macro_use]
extern crate diesel_migrations;

pub mod config;
pub mod qmlapp;

pub mod actor;
pub mod model;

pub mod worker;

pub mod schema;

#[cfg(feature = "sailfish")]
pub mod gui;
#[cfg(not(feature = "sailfish"))]
pub mod gui_ng;
#[cfg(not(feature = "sailfish"))]
pub use gui_ng as gui;

pub mod store;

pub fn user_agent() -> String {
    format!("Whisperfish/{}", env!("CARGO_PKG_VERSION"))
}

pub fn millis_to_naive_chrono(ts: u64) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::from_timestamp((ts / 1000) as i64, ((ts % 1000) * 1_000_000) as u32)
}

pub fn conf_dir() -> std::path::PathBuf {
    let conf_dir = dirs::config_dir()
        .expect("config directory")
        .join("harbour-whisperfish");

    if !conf_dir.exists() {
        std::fs::create_dir(&conf_dir).unwrap();
    }

    conf_dir
}

/// Checks if the db contains foreign key violations.
pub fn check_foreign_keys(db: &diesel::SqliteConnection) -> Result<(), anyhow::Error> {
    use diesel::prelude::*;
    use diesel::sql_types::*;

    #[derive(Queryable, QueryableByName, Debug)]
    pub struct ForeignKeyViolation {
        #[sql_type = "Text"]
        table: String,
        #[sql_type = "Integer"]
        rowid: i32,
        #[sql_type = "Text"]
        parent: String,
        #[sql_type = "Integer"]
        fkid: i32,
    }

    db.execute("PRAGMA foreign_keys = ON;").unwrap();
    let violations: Vec<ForeignKeyViolation> = diesel::sql_query("PRAGMA main.foreign_key_check;")
        .load(db)
        .unwrap();

    if !violations.is_empty() {
        anyhow::bail!(
            "There are foreign key violations. Here the are: {:?}",
            violations
        );
    } else {
        Ok(())
    }
}
