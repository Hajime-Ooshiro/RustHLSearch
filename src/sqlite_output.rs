use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const MAX_DEPTH: usize = 249;

const CREATE_TARGET_PATHS_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS target_paths (
        depth INTEGER NOT NULL,
        target_count INTEGER NOT NULL,
        target_shifts TEXT NOT NULL,
        PRIMARY KEY (depth, target_shifts)
    );
";

fn shift_path_table(depth: usize) -> String {
    format!("depth_{depth:03}")
}

fn create_shift_path_table_sql(depth: usize) -> String {
    let table = shift_path_table(depth);
    let parent_reference = if depth == 1 {
        String::new()
    } else {
        format!(" REFERENCES {}(node_id)", shift_path_table(depth - 1))
    };
    format!(
        "
    CREATE TABLE IF NOT EXISTS {table} (
        node_id INTEGER PRIMARY KEY,
        parent_depth INTEGER NOT NULL,
        parent_node_id INTEGER{parent_reference},
        shift INTEGER NOT NULL,
        score INTEGER NOT NULL,
        UNIQUE(parent_node_id, shift)
    );
    CREATE UNIQUE INDEX IF NOT EXISTS {table}_root_shift
        ON {table}(shift) WHERE parent_node_id IS NULL;
"
    )
}

#[derive(Clone)]
pub struct ShiftPathStore {
    connection: Arc<Mutex<Connection>>,
    depth: usize,
}

impl ShiftPathStore {
    pub fn create(path: &Path, depth: usize, clear_existing: bool) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| error.to_string())?;
        drop_legacy_shift_path_tables(&connection)?;
        drop_legacy_depth_tables(&connection)?;
        for level in 1..=MAX_DEPTH {
            connection
                .execute_batch(&create_shift_path_table_sql(level))
                .map_err(|error| error.to_string())?;
        }
        connection
            .execute_batch(CREATE_TARGET_PATHS_TABLE)
            .map_err(|error| error.to_string())?;
        if !has_expected_schema(&connection, "target_paths", &["depth", "target_shifts"])
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
                .execute("DELETE FROM target_paths WHERE depth = ?1", params![depth])
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            depth,
        })
    }

    pub fn append(&self, max_count: usize, path: &[usize]) -> Result<(), String> {
        if path.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut parent_node_id = None;

        for (level, &shift) in path.iter().enumerate() {
            let table = shift_path_table(level + 1);
            let node_id = match transaction.query_row(
                &format!("SELECT node_id FROM {table} WHERE parent_node_id IS ?1 AND shift = ?2"),
                params![parent_node_id, shift],
                |row| row.get(0),
            ) {
                Ok(id) => id,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    transaction
                        .execute(
                            &format!(
                                "INSERT INTO {table} (parent_depth, parent_node_id, shift, score) VALUES (?1, ?2, ?3, ?4)"
                            ),
                            params![level, parent_node_id, shift, max_count],
                        )
                        .map_err(|error| error.to_string())?;
                    transaction.last_insert_rowid()
                }
                Err(error) => return Err(error.to_string()),
            };
            parent_node_id = Some(node_id);
        }
        transaction
            .execute(
                &format!(
                    "UPDATE {} SET score = ?1 WHERE node_id = ?2",
                    shift_path_table(path.len())
                ),
                params![max_count, parent_node_id],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
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

    pub fn load_result_paths(&self, max_count: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(self.depth, Some(max_count))
    }

    pub fn load_full_depth_paths(&self, depth: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(depth, None)
    }

    fn load_leaf_paths(
        &self,
        depth: usize,
        max_count: Option<usize>,
    ) -> Result<Vec<Vec<usize>>, String> {
        let table = shift_path_table(depth);
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        if !table_exists(&connection, &table).map_err(|error| error.to_string())? {
            return Ok(Vec::new());
        }
        let leaves: Vec<(Option<i64>, usize)> = {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT parent_node_id, shift FROM {table} WHERE ?1 IS NULL OR score = ?1 ORDER BY node_id"
                ))
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(params![max_count], |row| {
                    Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, usize>(1)?))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<rusqlite::Result<_>>()
                .map_err(|error| error.to_string())?
        };

        let mut paths: Vec<Vec<usize>> = leaves.iter().map(|&(_, shift)| vec![shift]).collect();
        let mut parent_ids: Vec<Option<i64>> = leaves.iter().map(|&(parent, _)| parent).collect();
        for level in (1..depth).rev() {
            let level_table = shift_path_table(level);
            let nodes = load_node_map(&connection, &level_table)?;
            for (path, parent_id) in paths.iter_mut().zip(parent_ids.iter_mut()) {
                let node_id =
                    parent_id.ok_or_else(|| format!("{table} の親ノードが欠けています"))?;
                let &(parent, shift) = nodes
                    .get(&node_id)
                    .ok_or_else(|| format!("{level_table} に node_id {node_id} がありません"))?;
                path.push(shift);
                *parent_id = parent;
            }
        }
        for path in &mut paths {
            path.reverse();
        }
        Ok(paths)
    }

    pub fn load_target_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT target_shifts FROM target_paths WHERE depth = ?1 ORDER BY target_count",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![self.depth], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())
        })
        .collect()
    }
}

fn drop_legacy_shift_path_tables(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("DROP TABLE IF EXISTS shift_path_leaves; DROP TABLE IF EXISTS shift_paths;")
        .map_err(|error| error.to_string())
}

fn drop_legacy_depth_tables(connection: &Connection) -> Result<(), String> {
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name GLOB 'depth_[0-9][0-9][0-9]'",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?
    };
    for table in tables {
        if !has_expected_schema(connection, &table, &["parent_depth"])
            .map_err(|error| error.to_string())?
        {
            connection
                .execute_batch(&format!("DROP TABLE {table};"))
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn load_node_map(
    connection: &Connection,
    table: &str,
) -> Result<HashMap<i64, (Option<i64>, usize)>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT node_id, parent_node_id, shift FROM {table}"
        ))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                (row.get::<_, Option<i64>>(1)?, row.get::<_, usize>(2)?),
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(|error| error.to_string())
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get(0),
    )
}

fn has_expected_schema(
    connection: &Connection,
    table: &str,
    expected_columns: &[&str],
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(expected_columns
        .iter()
        .all(|expected| columns.iter().any(|column| column == expected)))
}

#[cfg(test)]
mod tests {
    use super::ShiftPathStore;

    fn database_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn appends_reusable_parent_nodes_and_reconstructs_leaves_in_order() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(10, &[1]).unwrap();
        store.append(10, &[1, 2]).unwrap();
        store.append(11, &[1, 3]).unwrap();
        store.append(11, &[1, 3]).unwrap();

        assert_eq!(
            store.load_full_depth_paths(2).unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );
        assert_eq!(store.load_result_paths(11).unwrap(), vec![vec![1, 3]]);

        let connection = rusqlite::Connection::open(&path).unwrap();
        let root_id: i64 = connection
            .query_row("SELECT node_id FROM depth_001", [], |row| row.get(0))
            .unwrap();
        let nodes: Vec<(usize, Option<i64>, usize, usize)> = connection
            .prepare(
                "SELECT parent_depth, parent_node_id, shift, score
                 FROM depth_002 ORDER BY node_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            nodes,
            vec![(1, Some(root_id), 2, 10), (1, Some(root_id), 3, 11)]
        );

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prefixes_are_stored_in_their_own_depth_tables() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 3, true).unwrap();
        store.append(4, &[1, 2, 3]).unwrap();

        assert_eq!(store.load_full_depth_paths(1).unwrap(), vec![vec![1]]);
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);
        assert_eq!(store.load_full_depth_paths(3).unwrap(), vec![vec![1, 2, 3]]);
        assert_eq!(store.load_result_paths(4).unwrap(), vec![vec![1, 2, 3]]);

        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clearing_a_depth_keeps_stored_paths_and_removes_its_targets() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(3, &[1, 2]).unwrap();
        store.append_target(3, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);
        assert!(store.load_target_paths().unwrap().is_empty());

        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn migrates_json_path_schema() {
        let path = database_path();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE shift_paths (
                    depth INTEGER NOT NULL,
                    max_count INTEGER NOT NULL,
                    shifts TEXT NOT NULL,
                    PRIMARY KEY (depth, shifts)
                );",
            )
            .unwrap();
        drop(connection);

        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(10, &[1, 2]).unwrap();
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);

        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn replaces_legacy_depth_tables_without_parent_depth() {
        let path = database_path();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE depth_002 (
                    node_id INTEGER PRIMARY KEY,
                    parent_node_id INTEGER REFERENCES depth_002(node_id),
                    shift INTEGER NOT NULL,
                    score INTEGER NOT NULL,
                    is_leaf INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(parent_node_id, shift)
                );
                INSERT INTO depth_002 (parent_node_id, shift, score, is_leaf)
                VALUES (NULL, 9, 9, 1);",
            )
            .unwrap();
        drop(connection);

        let store = ShiftPathStore::create(&path, 2, true).unwrap();
        store.append(10, &[1, 2]).unwrap();
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);

        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
