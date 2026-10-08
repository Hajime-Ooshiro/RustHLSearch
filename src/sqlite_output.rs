use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

const CREATE_SHIFT_PATHS_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS shift_paths (
        depth INTEGER NOT NULL,
        max_count INTEGER NOT NULL,
        shifts TEXT NOT NULL,
        PRIMARY KEY (depth, shifts)
    );
";

const CREATE_TARGET_PATHS_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS target_paths (
        depth INTEGER NOT NULL,
        target_count INTEGER NOT NULL,
        target_shifts TEXT NOT NULL,
        PRIMARY KEY (depth, target_shifts)
    );
";

#[derive(Clone)]
pub struct ShiftPathStore {
    connection: Arc<Mutex<Connection>>,
    depth: usize,
}

impl ShiftPathStore {
    pub fn create(path: &Path, depth: usize, clear_existing: bool) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(&format!(
                "{CREATE_SHIFT_PATHS_TABLE}{CREATE_TARGET_PATHS_TABLE}"
            ))
            .map_err(|error| error.to_string())?;
        if !has_expected_schema(&connection, "shift_paths", "shifts")
            .map_err(|error| error.to_string())?
        {
            connection
                .execute_batch(&format!(
                    "DROP TABLE shift_paths; {CREATE_SHIFT_PATHS_TABLE}"
                ))
                .map_err(|error| error.to_string())?;
        }
        if !has_expected_schema(&connection, "target_paths", "target_shifts")
            .map_err(|error| error.to_string())?
        {
            connection
                .execute_batch(&format!(
                    "DROP TABLE target_paths; {CREATE_TARGET_PATHS_TABLE}"
                ))
                .map_err(|error| error.to_string())?;
        }
        if clear_existing {
            connection
                .execute("DELETE FROM shift_paths WHERE depth = ?1", params![depth])
                .map_err(|error| error.to_string())?;
            connection
                .execute("DELETE FROM target_paths WHERE depth = ?1", params![depth])
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            depth,
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
            .execute(
                "DELETE FROM shift_paths WHERE depth = ?1",
                params![self.depth],
            )
            .map_err(|error| error.to_string())?;
        for path in paths {
            let shifts = serde_json::to_string(path).map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT INTO shift_paths (depth, max_count, shifts) VALUES (?1, ?2, ?3)",
                    params![self.depth, max_count, shifts],
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
                "INSERT INTO shift_paths (depth, max_count, shifts) VALUES (?1, ?2, ?3)",
                params![self.depth, max_count, shifts],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn append_target(&self, target_count: usize, path: &[usize]) -> Result<(), String> {
        let target_shifts = serde_json::to_string(path).map_err(|error| error.to_string())?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        connection
            .execute(
                "INSERT INTO target_paths (depth, target_count, target_shifts) VALUES (?1, ?2, ?3)",
                params![self.depth, target_count, target_shifts],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn load_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        self.load_path_column("shift_paths", "shifts", "rowid")
    }

    pub fn load_target_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        self.load_path_column("target_paths", "target_shifts", "target_count")
    }

    fn load_path_column(
        &self,
        table: &str,
        paths_column: &str,
        order_column: &str,
    ) -> Result<Vec<Vec<usize>>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {paths_column} FROM {table} WHERE depth = ?1 ORDER BY {order_column}"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![self.depth], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        let mut paths = Vec::new();
        for row in rows {
            let path = row.map_err(|error| error.to_string())?;
            paths.push(serde_json::from_str(&path).map_err(|error| error.to_string())?);
        }
        Ok(paths)
    }
}

fn has_expected_schema(
    connection: &Connection,
    table: &str,
    paths_column: &str,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, usize>(5)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(columns
        .iter()
        .any(|(name, primary_key_position)| name == "depth" && *primary_key_position == 1)
        && columns
            .iter()
            .any(|(name, primary_key_position)| name == paths_column && *primary_key_position == 2))
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
        let store = ShiftPathStore::create(&path, 2, true).unwrap();

        store.append(10, &[1, 2]).unwrap();
        store.replace_all(11, &[vec![3, 4]]).unwrap();
        store.append(11, &[5, 6]).unwrap();
        store.append_target(1, &[3, 4]).unwrap();
        store.append_target(2, &[5, 6]).unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let rows: Vec<(usize, usize, String)> = connection
            .prepare("SELECT depth, max_count, shifts FROM shift_paths ORDER BY shifts")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![(2, 11, "[3,4]".to_string()), (2, 11, "[5,6]".to_string())]
        );
        let target_rows: Vec<(usize, usize, String)> = connection
            .prepare(
                "SELECT depth, target_count, target_shifts FROM target_paths ORDER BY target_count",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            target_rows,
            vec![(2, 1, "[3,4]".to_string()), (2, 2, "[5,6]".to_string())]
        );

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reuses_existing_shift_paths_table() {
        let path = std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(10, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 3, false).unwrap();
        store.append(10, &[1, 2]).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let count: usize = connection
            .query_row(
                "SELECT COUNT(*) FROM shift_paths WHERE depth = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn migrates_legacy_shift_paths_table() {
        let path = std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let connection = rusqlite::Connection::open(&path).unwrap();
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
            .unwrap();
        drop(connection);

        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(10, &[1, 2]).unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let primary_key_columns: Vec<String> = connection
            .prepare("PRAGMA table_info(shift_paths)")
            .unwrap()
            .query_map([], |row| {
                let primary_key_position: usize = row.get(5)?;
                Ok((row.get::<_, String>(1)?, primary_key_position))
            })
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|(name, primary_key_position)| {
                (primary_key_position > 0).then_some((primary_key_position, name))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(primary_key_columns, vec!["depth", "shifts"]);

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
