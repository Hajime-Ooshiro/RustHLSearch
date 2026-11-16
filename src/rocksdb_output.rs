//! シフトパス木の RocksDB 永続化。
//!
//! - `nodes` CF:  key = node_id (u64 big-endian)
//!                value = `{"parent_depth":..,"parent_node_id":..,"shift":..,"score":..}` (JSON)
//! - `node_index` CF: (parent_node_id, shift) -> node_id (重複ノード検出用の補助インデックス)
//! - `target_paths` CF: (depth, target_shifts JSON) -> target_count
//! - `meta` CF: 設定ハッシュ
//!
//! node_id は全深さで一意で、上位 16 bit に深さ、下位 48 bit に連番を持つ。
//! そのためビッグエンディアンのキー順 = (深さ, 連番) 順となり、
//! 特定の深さのノードを範囲スキャンで列挙できる。

use rocksdb::{ColumnFamilyDescriptor, Direction, IteratorMode, Options, WriteBatch, DB};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_DEPTH: usize = 249;
const SHARD_COUNT: usize = 210;
const SHARD_CONFIG_FILENAME: &str = "shards.config";
const DEPTH_SHIFT: u32 = 48;
const SEQ_MASK: u64 = (1u64 << DEPTH_SHIFT) - 1;

const CF_NODES: &str = "nodes";
const CF_INDEX: &str = "node_index";
const CF_TARGETS: &str = "target_paths";
const CF_META: &str = "meta";

type PendingAppends = Vec<(usize, Vec<usize>)>;

/// `nodes` CF に JSON で保存されるノード情報
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub parent_depth: usize,
    pub parent_node_id: Option<u64>,
    pub shift: usize,
    pub score: usize,
}

fn err<E: ToString>(error: E) -> String {
    error.to_string()
}

fn make_node_id(depth: usize, seq: u64) -> u64 {
    ((depth as u64) << DEPTH_SHIFT) | (seq & SEQ_MASK)
}

fn node_depth(node_id: u64) -> usize {
    (node_id >> DEPTH_SHIFT) as usize
}

fn decode_u64(bytes: &[u8]) -> Result<u64, String> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("不正な u64 バイト列です (len={})", bytes.len()))?;
    Ok(u64::from_be_bytes(array))
}

/// 親 (ルートは 0) と shift から補助インデックスのキーを作る
fn index_key(parent_node_id: Option<u64>, shift: usize) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&parent_node_id.unwrap_or(0).to_be_bytes());
    key[8..].copy_from_slice(&(shift as u64).to_be_bytes());
    key
}

fn target_prefix(depth: usize) -> [u8; 2] {
    (depth as u16).to_be_bytes()
}

fn target_key(depth: usize, target_shifts: &str) -> Vec<u8> {
    let mut key = target_prefix(depth).to_vec();
    key.extend_from_slice(target_shifts.as_bytes());
    key
}

fn config_key(depth: usize) -> String {
    format!("config_hash:{depth}")
}

fn cf<'a>(db: &'a DB, name: &str) -> Result<&'a rocksdb::ColumnFamily, String> {
    db.cf_handle(name)
        .ok_or_else(|| format!("カラムファミリー {name} が見つかりません"))
}

#[derive(Clone)]
struct ShardStore {
    db: Arc<DB>,
    depth: usize,
    pending_appends: Arc<Mutex<PendingAppends>>,
    /// 深さごとの次に払い出す連番 (index = depth)
    next_seq: Arc<Mutex<Vec<u64>>>,
}

impl ShardStore {
    pub fn create(
        path: &Path,
        depth: usize,
        clear_existing: bool,
        config_hash: u64,
    ) -> Result<Self, String> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = [CF_NODES, CF_INDEX, CF_TARGETS, CF_META]
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
        let db = DB::open_cf_descriptors(&options, path, descriptors).map_err(err)?;

        check_config_hash(&db, depth, config_hash)?;
        if clear_existing {
            clear_targets(&db, depth)?;
        }
        store_config_hash(&db, depth, config_hash)?;
        let next_seq = load_next_seq(&db)?;

        Ok(Self {
            db: Arc::new(db),
            depth,
            pending_appends: Arc::new(Mutex::new(Vec::new())),
            next_seq: Arc::new(Mutex::new(next_seq)),
        })
    }

    pub fn append(&self, max_count: usize, path: &[usize]) -> Result<(), String> {
        if path.is_empty() {
            return Ok(());
        }
        {
            let mut buffer = self
                .pending_appends
                .lock()
                .map_err(|_| "バッファのロック取得に失敗しました".to_string())?;
            buffer.push((max_count, path.to_vec()));
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<(), String> {
        let mut buffer = self
            .pending_appends
            .lock()
            .map_err(|_| "バッファのロック取得に失敗しました".to_string())?;
        if buffer.is_empty() {
            return Ok(());
        }
        let mut counters = self
            .next_seq
            .lock()
            .map_err(|_| "RocksDB 出力のロック取得に失敗しました".to_string())?;

        let nodes_cf = cf(&self.db, CF_NODES)?;
        let index_cf = cf(&self.db, CF_INDEX)?;

        // バッチ内の書き込みは未コミットなので、オーバーレイで管理する
        let mut next_seq = counters.clone();
        let mut touched: HashMap<u64, NodeRecord> = HashMap::new();
        let mut new_index: HashMap<[u8; 16], u64> = HashMap::new();

        for (max_count, path) in buffer.iter() {
            if path.len() > MAX_DEPTH {
                return Err(format!(
                    "パス長 {} が上限 {MAX_DEPTH} を超えています",
                    path.len()
                ));
            }
            let mut parent_node_id: Option<u64> = None;
            for (level, &shift) in path.iter().enumerate() {
                let depth = level + 1;
                let key = index_key(parent_node_id, shift);
                let node_id = if let Some(&id) = new_index.get(&key) {
                    id
                } else if let Some(value) = self.db.get_cf(index_cf, key).map_err(err)? {
                    decode_u64(&value)?
                } else {
                    let id = make_node_id(depth, next_seq[depth]);
                    next_seq[depth] += 1;
                    touched.insert(
                        id,
                        NodeRecord {
                            parent_depth: level,
                            parent_node_id,
                            shift,
                            score: *max_count,
                        },
                    );
                    new_index.insert(key, id);
                    id
                };
                parent_node_id = Some(node_id);
            }

            // 葉ノードのスコアを更新する
            let leaf_id = parent_node_id.expect("path is non-empty");
            if let Some(record) = touched.get_mut(&leaf_id) {
                record.score = *max_count;
            } else {
                let mut record = read_node(&self.db, leaf_id)?
                    .ok_or_else(|| format!("node_id {leaf_id} がありません"))?;
                record.score = *max_count;
                touched.insert(leaf_id, record);
            }
        }

        let mut batch = WriteBatch::default();
        for (node_id, record) in &touched {
            batch.put_cf(
                nodes_cf,
                node_id.to_be_bytes(),
                serde_json::to_vec(record).map_err(err)?,
            );
        }
        for (key, node_id) in &new_index {
            batch.put_cf(index_cf, key, node_id.to_be_bytes());
        }
        self.db.write(batch).map_err(err)?;

        // コミット成功後にだけ状態を進める
        *counters = next_seq;
        buffer.clear();
        Ok(())
    }

    pub fn append_target(&self, target_count: usize, path: &[usize]) -> Result<(), String> {
        let target_shifts = serde_json::to_string(path).map_err(err)?;
        let targets_cf = cf(&self.db, CF_TARGETS)?;
        let key = target_key(self.depth, &target_shifts);
        // INSERT OR IGNORE 相当: 既にあれば何もしない
        if self.db.get_cf(targets_cf, &key).map_err(err)?.is_none() {
            self.db
                .put_cf(targets_cf, &key, (target_count as u64).to_be_bytes())
                .map_err(err)?;
        }
        Ok(())
    }

    pub fn clear_all_paths(&self) -> Result<(), String> {
        self.pending_appends
            .lock()
            .map_err(|_| "バッファのロック取得に失敗しました".to_string())?
            .clear();
        for name in [CF_NODES, CF_INDEX, CF_TARGETS] {
            clear_column_family(&self.db, cf(&self.db, name)?)?;
        }
        let mut next_seq = self
            .next_seq
            .lock()
            .map_err(|_| "RocksDB 出力のロック取得に失敗しました".to_string())?;
        *next_seq = vec![1; MAX_DEPTH + 1];
        Ok(())
    }

    pub fn load_result_paths(&self, max_count: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(self.depth, None, Some(max_count))
    }

    /*
    pub fn load_full_depth_paths(&self, depth: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(depth, None, None)
    }
    */

    pub fn load_viable_depth_paths(
        &self,
        depth: usize,
        min_score: usize,
    ) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(depth, Some(min_score), None)
    }

    fn load_leaf_paths(
        &self,
        depth: usize,
        min_score: Option<usize>,
        max_count: Option<usize>,
    ) -> Result<Vec<Vec<usize>>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH {
            return Ok(Vec::new());
        }
        let leaves: Vec<NodeRecord> = self
            .load_nodes(depth)?
            .into_iter()
            .map(|(_, record)| record)
            .filter(|record| {
                min_score.map(|score| record.score >= score).unwrap_or(true)
                    && max_count.map(|score| record.score == score).unwrap_or(true)
            })
            .collect();

        let mut paths: Vec<Vec<usize>> = leaves.iter().map(|leaf| vec![leaf.shift]).collect();
        let mut parent_ids: Vec<Option<u64>> =
            leaves.iter().map(|leaf| leaf.parent_node_id).collect();
        let mut cache: HashMap<u64, NodeRecord> = HashMap::new();

        for _ in 1..depth {
            for (path, parent_id) in paths.iter_mut().zip(parent_ids.iter_mut()) {
                let node_id =
                    parent_id.ok_or_else(|| format!("depth {depth} の親ノードが欠けています"))?;
                if !cache.contains_key(&node_id) {
                    let record = read_node(&self.db, node_id)?
                        .ok_or_else(|| format!("node_id {node_id} がありません"))?;
                    cache.insert(node_id, record);
                }
                let record = &cache[&node_id];
                path.push(record.shift);
                *parent_id = record.parent_node_id;
            }
        }
        for path in &mut paths {
            path.reverse();
        }
        Ok(paths)
    }

    /// 指定した深さのノードを node_id 昇順で返す
    pub(crate) fn load_nodes(&self, depth: usize) -> Result<Vec<(u64, NodeRecord)>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH {
            return Ok(Vec::new());
        }
        let nodes_cf = cf(&self.db, CF_NODES)?;
        let start = make_node_id(depth, 0).to_be_bytes();
        let mut nodes = Vec::new();
        for item in self
            .db
            .iterator_cf(nodes_cf, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) != depth {
                break;
            }
            nodes.push((node_id, serde_json::from_slice(&value).map_err(err)?));
        }
        Ok(nodes)
    }

    pub fn load_target_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        self.flush()?;
        let targets_cf = cf(&self.db, CF_TARGETS)?;
        let prefix = target_prefix(self.depth);
        let mut targets: Vec<(u64, Vec<usize>)> = Vec::new();
        for item in self
            .db
            .iterator_cf(targets_cf, IteratorMode::From(&prefix, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            if !key.starts_with(&prefix) {
                break;
            }
            let shifts: Vec<usize> = serde_json::from_slice(&key[prefix.len()..]).map_err(err)?;
            targets.push((decode_u64(&value)?, shifts));
        }
        targets.sort_by_key(|(count, _)| *count); // 安定ソート
        Ok(targets.into_iter().map(|(_, shifts)| shifts).collect())
    }
}

impl Drop for ShardStore {
    fn drop(&mut self) {
        if Arc::strong_count(&self.db) == 1 {
            let _ = self.flush();
        }
    }
}

/// 210 個の RocksDB パーティションへパスを安定して分散するストア。
///
/// 接頭辞を含むパス全体でパーティションを選ぶため、各パーティションは独立した
/// 木として読み書きできる。接頭辞は複数のパーティションに複製され得る。
#[derive(Clone)]
pub struct ShiftPathStore {
    path: PathBuf,
    depth: usize,
    clear_existing: bool,
    config_hash: u64,
    shards: Arc<Mutex<HashMap<usize, ShardStore>>>,
}

impl ShiftPathStore {
    pub fn create(
        path: &Path,
        depth: usize,
        clear_existing: bool,
        config_hash: u64,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(path).map_err(err)?;
        check_shard_config(path, depth, config_hash)?;
        let store = Self {
            path: path.to_path_buf(),
            depth,
            clear_existing,
            config_hash,
            shards: Arc::new(Mutex::new(HashMap::new())),
        };
        for index in store.shard_indexes()? {
            store.shard(index)?;
        }
        Ok(store)
    }

    pub fn append(&self, max_count: usize, path: &[usize]) -> Result<(), String> {
        if path.is_empty() {
            return Ok(());
        }
        self.shard(shard_index(path))?.append(max_count, path)
    }

    pub fn flush(&self) -> Result<(), String> {
        for shard in self.loaded_shards()? {
            shard.flush()?;
        }
        Ok(())
    }

    pub fn append_target(&self, target_count: usize, path: &[usize]) -> Result<(), String> {
        self.shard(shard_index(path))?
            .append_target(target_count, path)
    }

    pub fn clear_all_paths(&self) -> Result<(), String> {
        for index in self.shard_indexes()? {
            self.shard(index)?.clear_all_paths()?;
        }
        Ok(())
    }

    pub fn load_result_paths(&self, max_count: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_paths(|shard| shard.load_result_paths(max_count))
    }

    pub fn load_viable_depth_paths(
        &self,
        depth: usize,
        min_score: usize,
    ) -> Result<Vec<Vec<usize>>, String> {
        self.load_paths(|shard| shard.load_viable_depth_paths(depth, min_score))
    }

    #[cfg(test)]
    fn load_leaf_paths(
        &self,
        depth: usize,
        min_score: Option<usize>,
        max_count: Option<usize>,
    ) -> Result<Vec<Vec<usize>>, String> {
        self.load_paths(|shard| shard.load_leaf_paths(depth, min_score, max_count))
    }

    #[cfg(test)]
    pub(crate) fn load_nodes(&self, depth: usize) -> Result<Vec<(u64, NodeRecord)>, String> {
        let mut nodes = Vec::new();
        for shard in self.all_shards()? {
            nodes.extend(shard.load_nodes(depth)?);
        }
        nodes.sort_unstable_by_key(|(_, record)| (record.parent_node_id, record.shift));
        Ok(nodes)
    }

    pub fn load_target_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        self.load_paths(|shard| shard.load_target_paths())
    }

    fn load_paths<F>(&self, load: F) -> Result<Vec<Vec<usize>>, String>
    where
        F: Fn(&ShardStore) -> Result<Vec<Vec<usize>>, String>,
    {
        let mut paths = Vec::new();
        for shard in self.all_shards()? {
            paths.extend(load(&shard)?);
        }
        paths.sort_unstable();
        paths.dedup();
        Ok(paths)
    }

    #[cfg(test)]
    fn pending_append_count(&self) -> usize {
        self.loaded_shards()
            .unwrap()
            .iter()
            .map(|shard| shard.pending_appends.lock().unwrap().len())
            .sum()
    }

    #[cfg(test)]
    fn node_is_buffered(&self, path: &[usize]) -> Result<bool, String> {
        let shard = self.shard(shard_index(path))?;
        let nodes_cf = cf(&shard.db, CF_NODES)?;
        Ok(shard
            .db
            .get_cf(nodes_cf, make_node_id(path.len(), 1).to_be_bytes())
            .map_err(err)?
            .is_some())
    }

    fn shard(&self, index: usize) -> Result<ShardStore, String> {
        let mut shards = self
            .shards
            .lock()
            .map_err(|_| "RocksDB パーティションのロック取得に失敗しました".to_string())?;
        if let Some(shard) = shards.get(&index) {
            return Ok(shard.clone());
        }
        let shard_path = self.path.join(format!("part-{index:03}.rocksdb"));
        let shard = ShardStore::create(
            &shard_path,
            self.depth,
            self.clear_existing,
            self.config_hash,
        )?;
        shards.insert(index, shard.clone());
        Ok(shard)
    }

    fn loaded_shards(&self) -> Result<Vec<ShardStore>, String> {
        Ok(self
            .shards
            .lock()
            .map_err(|_| "RocksDB パーティションのロック取得に失敗しました".to_string())?
            .values()
            .cloned()
            .collect())
    }

    fn all_shards(&self) -> Result<Vec<ShardStore>, String> {
        self.shard_indexes()?
            .into_iter()
            .map(|index| self.shard(index))
            .collect()
    }

    fn shard_indexes(&self) -> Result<Vec<usize>, String> {
        let mut indexes: Vec<usize> = std::fs::read_dir(&self.path)
            .map_err(err)?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix("part-"))
                    .and_then(|name| name.strip_suffix(".rocksdb"))
                    .and_then(|index| index.parse::<usize>().ok())
            })
            .filter(|&index| index < SHARD_COUNT)
            .collect();
        {
            let shards = self
                .shards
                .lock()
                .map_err(|_| "RocksDB パーティションのロック取得に失敗しました".to_string())?;
            indexes.extend(shards.keys().copied());
        }
        indexes.sort_unstable();
        indexes.dedup();
        Ok(indexes)
    }
}

fn shard_index(path: &[usize]) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &shift in path {
        for byte in (shift as u64).to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    (hash as usize) % SHARD_COUNT
}

fn check_shard_config(path: &Path, depth: usize, config_hash: u64) -> Result<(), String> {
    let config_path = path.join(SHARD_CONFIG_FILENAME);
    let current = format!("{depth}:{config_hash}");
    if config_path.exists() {
        let stored = std::fs::read_to_string(&config_path).map_err(err)?;
        if let Some(previous) = stored
            .lines()
            .find(|line| line.starts_with(&format!("{depth}:")))
        {
            if previous != current {
                return Err(format!(
                    "設定ハッシュが異なります。別の --output-dir を使用してください (stored={}, current={})",
                    previous, current
                ));
            }
        } else {
            std::fs::write(config_path, format!("{}{current}\n", stored)).map_err(err)?;
        }
    } else {
        std::fs::write(config_path, format!("{current}\n")).map_err(err)?;
    }
    Ok(())
}

fn read_node(db: &DB, node_id: u64) -> Result<Option<NodeRecord>, String> {
    let nodes_cf = cf(db, CF_NODES)?;
    match db.get_cf(nodes_cf, node_id.to_be_bytes()).map_err(err)? {
        Some(value) => Ok(Some(serde_json::from_slice(&value).map_err(err)?)),
        None => Ok(None),
    }
}

/// 各深さの最後の node_id を逆方向シークで調べ、次の連番を求める
fn load_next_seq(db: &DB) -> Result<Vec<u64>, String> {
    let nodes_cf = cf(db, CF_NODES)?;
    let mut next_seq = vec![1u64; MAX_DEPTH + 1];
    for (depth, slot) in next_seq.iter_mut().enumerate().skip(1) {
        let upper = make_node_id(depth, SEQ_MASK).to_be_bytes();
        let mut iter = db.iterator_cf(nodes_cf, IteratorMode::From(&upper, Direction::Reverse));
        if let Some(item) = iter.next() {
            let (key, _) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) == depth {
                *slot = (node_id & SEQ_MASK) + 1;
            }
        }
    }
    Ok(next_seq)
}

fn clear_targets(db: &DB, depth: usize) -> Result<(), String> {
    let targets_cf = cf(db, CF_TARGETS)?;
    let prefix = target_prefix(depth);
    let mut batch = WriteBatch::default();
    for item in db.iterator_cf(targets_cf, IteratorMode::From(&prefix, Direction::Forward)) {
        let (key, _) = item.map_err(err)?;
        if !key.starts_with(&prefix) {
            break;
        }
        batch.delete_cf(targets_cf, key);
    }
    db.write(batch).map_err(err)
}

fn check_config_hash(db: &DB, depth: usize, config_hash: u64) -> Result<(), String> {
    let meta_cf = cf(db, CF_META)?;
    if let Some(value) = db.get_cf(meta_cf, config_key(depth)).map_err(err)? {
        let stored_hash = decode_u64(&value)?;
        if stored_hash != config_hash {
            return Err(format!(
                "設定ハッシュが異なります。別の --output-dir を使用してください (stored_hash={}, current_hash={})",
                stored_hash as i64, config_hash as i64
            ));
        }
    }
    Ok(())
}

fn store_config_hash(db: &DB, depth: usize, config_hash: u64) -> Result<(), String> {
    let meta_cf = cf(db, CF_META)?;
    db.put_cf(meta_cf, config_key(depth), config_hash.to_be_bytes())
        .map_err(err)
}

fn clear_column_family(db: &DB, column_family: &rocksdb::ColumnFamily) -> Result<(), String> {
    const BATCH_SIZE: usize = 1024;
    let mut batch = WriteBatch::default();
    let mut pending = 0;
    for item in db.iterator_cf(column_family, IteratorMode::Start) {
        let (key, _) = item.map_err(err)?;
        batch.delete_cf(column_family, key);
        pending += 1;
        if pending == BATCH_SIZE {
            db.write(batch).map_err(err)?;
            batch = WriteBatch::default();
            pending = 0;
        }
    }
    if pending > 0 {
        db.write(batch).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NodeRecord, ShiftPathStore, SHARD_COUNT};

    fn database_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.rocksdb",
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
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(10, &[1]).unwrap();
        store.append(10, &[1, 2]).unwrap();
        store.append(11, &[1, 3]).unwrap();
        store.append(11, &[1, 3]).unwrap();

        assert_eq!(
            store.load_leaf_paths(2, None, None).unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );
        assert_eq!(store.load_result_paths(11).unwrap(), vec![vec![1, 3]]);

        let roots = store.load_nodes(1).unwrap();
        assert_eq!(roots.len(), 3);
        let root_id = roots[0].0;
        let nodes: Vec<NodeRecord> = store
            .load_nodes(2)
            .unwrap()
            .into_iter()
            .map(|(_, record)| record)
            .collect();
        let record = |shift, score| NodeRecord {
            parent_depth: 1,
            parent_node_id: Some(root_id),
            shift,
            score,
        };
        assert_eq!(nodes, vec![record(2, 10), record(3, 11)]);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn prefixes_are_stored_in_their_own_depth_ranges() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 3, true, 12345).unwrap();
        store.append(4, &[1, 2, 3]).unwrap();

        assert_eq!(store.load_leaf_paths(1, None, None).unwrap(), vec![vec![1]]);
        assert_eq!(
            store.load_leaf_paths(2, None, None).unwrap(),
            vec![vec![1, 2]]
        );
        assert_eq!(
            store.load_leaf_paths(3, None, None).unwrap(),
            vec![vec![1, 2, 3]]
        );
        assert_eq!(store.load_result_paths(4).unwrap(), vec![vec![1, 2, 3]]);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn append_buffers_paths_until_flush() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 1, true, 12345).unwrap();

        store.append(4, &[1]).unwrap();

        assert_eq!(store.pending_append_count(), 1);
        assert!(!store.node_is_buffered(&[1]).unwrap());

        store.flush().unwrap();

        assert_eq!(store.pending_append_count(), 0);
        assert!(store.node_is_buffered(&[1]).unwrap());

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn clearing_a_depth_keeps_stored_paths_and_removes_its_targets() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(3, &[1, 2]).unwrap();
        store.append_target(3, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        assert_eq!(
            store.load_leaf_paths(2, None, None).unwrap(),
            vec![vec![1, 2]]
        );
        assert!(store.load_target_paths().unwrap().is_empty());

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn clearing_all_paths_removes_data_and_resets_node_ids() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(5, &[1, 2]).unwrap();
        store.append_target(1, &[1, 2]).unwrap();

        store.clear_all_paths().unwrap();

        assert!(store.load_nodes(1).unwrap().is_empty());
        assert!(store.load_nodes(2).unwrap().is_empty());
        assert!(store.load_target_paths().unwrap().is_empty());
        store.append(7, &[3, 4]).unwrap();
        assert_eq!(store.load_nodes(1).unwrap()[0].0, super::make_node_id(1, 1));

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn node_ids_continue_after_reopen_without_duplicating_nodes() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(5, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 2, false, 12345).unwrap();
        store.append(6, &[1, 3]).unwrap();
        store.append(5, &[1, 2]).unwrap();
        assert_eq!(
            store.load_leaf_paths(2, None, None).unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );
        assert_eq!(store.load_nodes(1).unwrap().len(), 2);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn targets_are_ordered_by_count_and_deduplicated() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append_target(2, &[1, 3]).unwrap();
        store.append_target(1, &[1, 2]).unwrap();
        store.append_target(9, &[1, 2]).unwrap();
        assert_eq!(
            store.load_target_paths().unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rejects_mismatched_config_hash() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 1).unwrap();
        drop(store);
        assert!(ShiftPathStore::create(&path, 2, false, 2).is_err());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn distributes_paths_across_all_shards() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 1, true, 12345).unwrap();
        assert_eq!(SHARD_COUNT, 210);

        for shift in 0..SHARD_COUNT {
            store.append(1, &[shift]).unwrap();
        }
        store.flush().unwrap();
        assert_eq!(store.load_nodes(1).unwrap().len(), SHARD_COUNT);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
