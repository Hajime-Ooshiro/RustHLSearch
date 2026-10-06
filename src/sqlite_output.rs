use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ShiftPathStore {
    connection: Arc<Mutex<Connection>>,
}

impl ShiftPathStore {
    pub fn create(path: &Path) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "
                CREATE TABLE shift_paths (
                    id INTEGER PRIMARY KEY,
                    max_count INTEGER NOT NULL,
                    shifts TEXT NOT NULL UNIQUE
                );
                ",
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute("DELETE FROM shift_paths", [])
            .map_err(|error| error.to_string())?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn replace_all(&self, max_count: usize, paths: &[Vec<usize>]) -> Result<(), String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM shift_paths", [])
            .map_err(|error| error.to_string())?;
        for path in paths {
            let shifts = serde_json::to_string(path).map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT INTO shift_paths (max_count, shifts) VALUES (?1, ?2)",
                    params![max_count, shifts],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn append(&self, max_count: usize, path: &[usize]) -> Result<(), String> {
        let shifts = serde_json::to_string(path).map_err(|error| error.to_string())?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        connection
            .execute(
                "INSERT INTO shift_paths (max_count, shifts) VALUES (?1, ?2)",
                params![max_count, shifts],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ShiftPathStore;

    #[test]
    fn replaces_old_paths_and_appends_equal_best_paths() {
        let path = std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ShiftPathStore::create(&path).unwrap();

        store.append(10, &[1, 2]).unwrap();
        store.replace_all(11, &[vec![3, 4]]).unwrap();
        store.append(11, &[5, 6]).unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let rows: Vec<(usize, String)> = connection
            .prepare("SELECT max_count, shifts FROM shift_paths ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![(11, "[3,4]".to_string()), (11, "[5,6]".to_string())]
        );

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
