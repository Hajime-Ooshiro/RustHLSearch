use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

const CREATE_SHIFT_PATHS_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS shift_paths (
        node_id INTEGER PRIMARY KEY,
        parent_node_id INTEGER REFERENCES shift_paths(node_id),
        shift INTEGER NOT NULL,
        depth INTEGER NOT NULL,
        score INTEGER NOT NULL,
        UNIQUE(parent_node_id, shift)
    );
    CREATE UNIQUE INDEX IF NOT EXISTS shift_paths_root_shift
        ON shift_paths(shift) WHERE parent_node_id IS NULL;
";

const CREATE_SHIFT_PATH_LEAVES_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS shift_path_leaves (
        leaf_id INTEGER PRIMARY KEY,
        depth INTEGER NOT NULL,
        node_id INTEGER NOT NULL REFERENCES shift_paths(node_id),
        max_score INTEGER NOT NULL,
        UNIQUE(depth, node_id)
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
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| error.to_string())?;
        migrate_shift_path_schema(&connection)?;
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
            // Leaves identify the full paths belonging to a completed search. Nodes are shared
            // prefixes, so retaining them keeps other depths valid and cannot leave dangling rows.
            connection
                .execute(
                    "DELETE FROM shift_path_leaves WHERE depth = ?1",
                    params![depth],
                )
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
        let mut node_id = 0_i64;

        for (index, &shift) in path.iter().enumerate() {
            let node_depth = index + 1;
            node_id = match transaction.query_row(
                "SELECT node_id FROM shift_paths
                 WHERE parent_node_id IS ?1 AND shift = ?2",
                params![parent_node_id, shift],
                |row| row.get(0),
            ) {
                Ok(id) => id,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    transaction
                        .execute(
                            "INSERT INTO shift_paths (parent_node_id, shift, depth, score)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![parent_node_id, shift, node_depth, max_count],
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
                "UPDATE shift_paths SET score = ?1 WHERE node_id = ?2",
                params![max_count, node_id],
            )
            .map_err(|error| error.to_string())?;
        if path.len() == self.depth {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO shift_path_leaves (depth, node_id, max_score)
                     VALUES (?1, ?2, ?3)",
                    params![self.depth, node_id, max_count],
                )
                .map_err(|error| error.to_string())?;
        }
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite 出力のロック取得に失敗しました".to_string())?;
        let mut statement = connection
            .prepare(
                "WITH RECURSIVE path(leaf_id, node_id, parent_node_id, shift, depth) AS (
                    SELECT leaves.leaf_id, nodes.node_id, nodes.parent_node_id, nodes.shift, nodes.depth
                    FROM shift_path_leaves AS leaves
                    JOIN shift_paths AS nodes ON nodes.node_id = leaves.node_id
                    WHERE leaves.depth = ?1 AND (?2 IS NULL OR leaves.max_score = ?2)
                    UNION ALL
                    SELECT path.leaf_id, parents.node_id, parents.parent_node_id, parents.shift, parents.depth
                    FROM shift_paths AS parents
                    JOIN path ON path.parent_node_id = parents.node_id
                )
                SELECT leaf_id, shift FROM path ORDER BY leaf_id, depth",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![depth, max_count], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, usize>(1)?))
            })
            .map_err(|error| error.to_string())?;
        let mut paths = Vec::new();
        let mut current_leaf = None;
        let mut path = Vec::new();
        for row in rows {
            let (leaf_id, shift) = row.map_err(|error| error.to_string())?;
            if current_leaf != Some(leaf_id) {
                if !path.is_empty() {
                    paths.push(path);
                    path = Vec::new();
                }
                current_leaf = Some(leaf_id);
            }
            path.push(shift);
        }
        if !path.is_empty() {
            paths.push(path);
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

fn migrate_shift_path_schema(connection: &Connection) -> Result<(), String> {
    let paths_are_current = has_expected_schema(
        connection,
        "shift_paths",
        &["node_id", "parent_node_id", "shift", "depth", "score"],
    )
    .map_err(|error| error.to_string())?;
    let leaves_are_current = has_expected_schema(
        connection,
        "shift_path_leaves",
        &["leaf_id", "depth", "node_id", "max_score"],
    )
    .map_err(|error| error.to_string())?;
    if !paths_are_current || !leaves_are_current {
        connection
            .execute_batch(
                "DROP TABLE IF EXISTS shift_path_leaves; DROP TABLE IF EXISTS shift_paths;",
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute_batch(&format!(
            "{CREATE_SHIFT_PATHS_TABLE}{CREATE_SHIFT_PATH_LEAVES_TABLE}"
        ))
        .map_err(|error| error.to_string())
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
        let nodes: Vec<(Option<i64>, usize, usize, usize)> = connection
            .prepare(
                "SELECT parent_node_id, shift, depth, score
                 FROM shift_paths ORDER BY node_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0], (None, 1, 1, 10));
        assert!(nodes[1].0.is_some() && nodes[2].0 == nodes[1].0);
        assert_eq!(nodes[1].1, 2);
        assert_eq!(nodes[2].1, 3);

        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clearing_one_depth_preserves_shared_nodes_and_other_depth_leaves() {
        let path = database_path();
        let depth_two = ShiftPathStore::create(&path, 2, true).unwrap();
        depth_two.append(3, &[1, 2]).unwrap();
        drop(depth_two);

        let depth_three = ShiftPathStore::create(&path, 3, false).unwrap();
        depth_three.append(4, &[1, 2, 3]).unwrap();
        drop(depth_three);

        let depth_two = ShiftPathStore::create(&path, 2, true).unwrap();
        assert!(depth_two.load_full_depth_paths(2).unwrap().is_empty());
        assert_eq!(
            depth_two.load_full_depth_paths(3).unwrap(),
            vec![vec![1, 2, 3]]
        );

        drop(depth_two);
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
}
