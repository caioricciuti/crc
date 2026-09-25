//! What you accepted from completion, so the next list puts it first.
//!
//! One SQLite file, `~/Library/Application Support/crc/completion.db`, one
//! row per project, language, context and text, counted. Nothing leaves the
//! machine; Edit > Forget Completion History deletes a project's rows. The
//! file is plain SQLite on purpose: you can read it with `sqlite3`.

use std::path::{Path, PathBuf};

use super::Boost;
use crate::index::db::{Db, Result, Value};

pub struct History {
    db: Db,
}

impl History {
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/crc/completion.db"))
    }

    pub fn open(path: &Path) -> Result<History> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Db::open(path)?;
        db.execute(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS accepted (
                 project  TEXT NOT NULL,
                 language TEXT NOT NULL,
                 context  TEXT NOT NULL,
                 text     TEXT NOT NULL,
                 count    INTEGER NOT NULL,
                 last     INTEGER NOT NULL,
                 PRIMARY KEY (project, language, context, text)
             ) STRICT;",
        )?;
        Ok(History { db })
    }

    /// Counts one acceptance of `text` after `context`.
    pub fn record(
        &self,
        project: &str,
        language: &str,
        context: &str,
        text: &str,
        now: i64,
    ) -> Result<()> {
        let mut statement = self.db.prepare(
            "INSERT INTO accepted VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT (project, language, context, text)
             DO UPDATE SET count = count + 1, last = excluded.last",
        )?;
        statement.run(&[
            Value::Text(project),
            Value::Text(language),
            Value::Text(context),
            Value::Text(text),
            Value::Int(now),
        ])
    }

    /// What was accepted before that starts like `prefix`: per text, the
    /// count after this same `context` if there is one, otherwise the count
    /// everywhere in the project.
    pub fn boosts(
        &self,
        project: &str,
        language: &str,
        context: &str,
        prefix: &str,
        now: i64,
    ) -> Result<Vec<Boost>> {
        // The first letter narrows it in SQL; the ranking does the rest,
        // scattered-letter matches included.
        let first: String = prefix.chars().take(1).collect();
        let like = format!("{}%", escape_like(&first));
        let mut statement = self.db.prepare(
            "SELECT text, context, count, last FROM accepted
             WHERE project = ?1 AND language = ?2 AND text LIKE ?3 ESCAPE '\\'
             ORDER BY count DESC LIMIT 400",
        )?;
        statement.bind(&[
            Value::Text(project),
            Value::Text(language),
            Value::Text(&like),
        ])?;
        let mut out: Vec<Boost> = Vec::new();
        while statement.step()? {
            let (text, row_context, count, last) = (
                statement.text(0),
                statement.text(1),
                statement.int(2),
                statement.int(3),
            );
            let age_days = (now - last).max(0) as f32 / 86_400.0;
            let same = !context.is_empty() && row_context == context;
            match out.iter_mut().find(|b| b.text == text) {
                Some(boost) if same => {
                    boost.count = count as u32;
                    boost.same_context = true;
                    boost.context = row_context;
                    boost.age_days = age_days;
                }
                Some(boost) if !boost.same_context => {
                    boost.count += count as u32;
                    boost.age_days = boost.age_days.min(age_days);
                }
                Some(_) => {}
                None => out.push(Boost {
                    text,
                    count: count as u32,
                    age_days,
                    same_context: same,
                    context: row_context,
                }),
            }
        }
        Ok(out)
    }

    /// Deletes everything recorded for `project`. Returns how many rows.
    pub fn forget(&self, project: &str) -> Result<i32> {
        let mut statement = self.db.prepare("DELETE FROM accepted WHERE project = ?1")?;
        statement.run(&[Value::Text(project)])?;
        Ok(self.db.changes())
    }
}

/// `prefix` with LIKE's wildcards made literal.
pub fn escape_like(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_context_and_forgets_per_project() {
        let dir = std::env::temp_dir().join(format!("crc-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let history = History::open(&dir.join("completion.db")).unwrap();
        for _ in 0..3 {
            history.record("/p", "rs", "tree.", "rows", 1_000).unwrap();
        }
        history.record("/p", "rs", "", "rows", 1_000).unwrap();
        history.record("/p", "rs", "", "root", 1_000).unwrap();
        history.record("/q", "rs", "", "rows", 1_000).unwrap();

        let here = history
            .boosts("/p", "rs", "tree.", "r", 1_000 + 86_400)
            .unwrap();
        let rows = here.iter().find(|b| b.text == "rows").unwrap();
        assert_eq!(
            (rows.count, rows.same_context),
            (3, true),
            "the same receiver's count wins"
        );
        assert!((rows.age_days - 1.0).abs() < 0.01);
        let elsewhere = history.boosts("/p", "rs", "", "r", 1_000).unwrap();
        assert_eq!(
            elsewhere.iter().find(|b| b.text == "rows").unwrap().count,
            4,
            "summed without a context"
        );
        assert!(
            history
                .boosts("/p", "py", "", "r", 1_000)
                .unwrap()
                .is_empty(),
            "per language"
        );
        assert!(
            history
                .boosts("/p", "rs", "", "x", 1_000)
                .unwrap()
                .is_empty()
        );

        assert_eq!(history.forget("/p").unwrap(), 3);
        assert!(
            history
                .boosts("/p", "rs", "", "r", 1_000)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            history.boosts("/q", "rs", "", "r", 1_000).unwrap().len(),
            1,
            "other projects stay"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn like_wildcards_are_literal() {
        assert_eq!(escape_like("a_b%c\\"), "a\\_b\\%c\\\\");
    }
}
