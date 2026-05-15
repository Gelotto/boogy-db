use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use memmap2::MmapMut;

use crate::crypto::Cipher;
use crate::error::{BoogyError, Result};
use crate::vector::types::DistanceMetric;

// ── Constants ────────────────────────────────────────────────────────────────

const MAGIC: &[u8; 4] = b"BVEC";
const VERSION: u32 = 1;
const HEADER_SIZE: u64 = 4096;

/// Sentinel: no entry point / no record.
const NONE_U32: u32 = 0xFFFF_FFFF;
/// Sentinel: no graph record.
const NONE_U64: u64 = u64::MAX;

// ── Header field offsets (byte positions within the 4096-byte header) ────────

const OFF_MAGIC: usize = 0; // [u8; 4]
const OFF_VERSION: usize = 4; // u32
const OFF_DIMENSIONS: usize = 8; // u32
const OFF_METRIC: usize = 12; // u8
const OFF_M: usize = 13; // u32
const OFF_EF_CONSTRUCTION: usize = 17; // u32
const OFF_ENTRY_POINT: usize = 21; // u32
const OFF_NODE_COUNT: usize = 25; // u32
const OFF_MAX_LAYER: usize = 29; // u32
const OFF_NODE_CAPACITY: usize = 33; // u32
const OFF_FREE_LIST_HEAD: usize = 37; // u32
const OFF_FREE_LIST_LEN: usize = 41; // u32
const OFF_GRAPH_DATA_LEN: usize = 45; // u64

// ── In-memory header ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct VecFileHeader {
    pub dimensions: u32,
    pub metric: DistanceMetric,
    pub m: u32,
    pub ef_construction: u32,
    /// `None` = no entry point.
    pub entry_point: Option<u32>,
    pub node_count: u32,
    pub max_layer: u32,
    pub node_capacity: u32,
    /// `None` = free list empty.
    pub free_list_head: Option<u32>,
    pub free_list_len: u32,
    /// Bytes used in the graph data area.
    pub graph_data_len: u64,
}

// ── File-region offset helpers ────────────────────────────────────────────────

/// Byte offset of the vector data region within the file.
#[inline]
fn vector_region_offset() -> u64 {
    HEADER_SIZE
}

/// Byte size of the vector data region for a given capacity/dims.
#[inline]
fn vector_region_size(node_capacity: u32, dims: u32) -> u64 {
    node_capacity as u64 * dims as u64 * 4
}

/// Byte offset of the node index region.
#[inline]
fn node_index_offset(node_capacity: u32, dims: u32) -> u64 {
    vector_region_offset() + vector_region_size(node_capacity, dims)
}

/// Byte size of the node index region.
#[inline]
fn node_index_size(node_capacity: u32) -> u64 {
    node_capacity as u64 * 8
}

/// Byte offset of the graph data region.
#[inline]
fn graph_data_offset(node_capacity: u32, dims: u32) -> u64 {
    node_index_offset(node_capacity, dims) + node_index_size(node_capacity)
}

/// Pre-allocated size for the graph data area.
/// Each node needs at most: 4 (max_layer) + (2*M+1)*4 for layer 0 + M*(M+1)*4 for higher layers.
/// We use a generous per-node estimate: (1 + 2*m + max_layers * m) * 4 bytes, max_layers ~ 16.
#[inline]
fn graph_data_preallocated(node_capacity: u32, m: u32) -> u64 {
    // Layer 0: count(4) + 2*m neighbors(u32 each)
    // Layer i>0 (up to 16 layers estimated): count(4) + m neighbors(u32 each)
    // max_layer field: 4
    let per_node: u64 = 4 + (4 + 2 * m as u64 * 4) + 16 * (4 + m as u64 * 4);
    per_node * node_capacity as u64
}

/// Byte offset of the deleted-flags region.
#[inline]
fn deleted_flags_offset(node_capacity: u32, dims: u32, m: u32) -> u64 {
    graph_data_offset(node_capacity, dims) + graph_data_preallocated(node_capacity, m)
}

/// Total file size.
#[inline]
fn total_file_size(node_capacity: u32, dims: u32, m: u32) -> u64 {
    deleted_flags_offset(node_capacity, dims, m) + node_capacity as u64
}

// ── Graph record layout helpers ───────────────────────────────────────────────

/// Byte size of a single layer entry: 4 (count) + capacity * 4 (neighbor u32s).
#[inline]
fn layer_entry_size(layer: u32, m: u32) -> u64 {
    let cap = if layer == 0 { 2 * m } else { m };
    4 + cap as u64 * 4
}

/// Total byte size of a graph record for a node with `max_layer` layers.
#[inline]
fn graph_record_size(max_layer: u32, m: u32) -> u64 {
    // 4 bytes for max_layer field + sum of all layer entries 0..=max_layer
    let mut size: u64 = 4;
    for l in 0..=max_layer {
        size += layer_entry_size(l, m);
    }
    size
}

/// Byte offset of a layer entry within a graph record (relative to record start).
#[inline]
fn layer_offset_in_record(layer: u32, m: u32) -> u64 {
    // Skip max_layer field (4) + preceding layer entries
    let mut off: u64 = 4;
    for l in 0..layer {
        off += layer_entry_size(l, m);
    }
    off
}

// ── VecFile ───────────────────────────────────────────────────────────────────

pub struct VecFile {
    path: PathBuf,
    _file: File,
    mmap: MmapMut,
    header: VecFileHeader,
    cipher: Option<Cipher>,
}

impl VecFile {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Create a new vector file.
    pub fn create(
        path: impl AsRef<Path>,
        dimensions: u32,
        metric: DistanceMetric,
        m: u32,
        ef_construction: u32,
        initial_capacity: u32,
        key: Option<&[u8; 32]>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let cipher = key.map(Cipher::new);

        let size = total_file_size(initial_capacity, dimensions, m);

        // When encrypted, the mmap is backed by an anonymous tempfile.
        // The on-disk path only receives encrypted content on flush.
        let file = if cipher.is_some() {
            // Touch the path to claim it (error if it already exists).
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            // Create a tempfile for the mmap backing.
            let tmp_path = path.with_extension("bvec.tmp");
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            let _ = fs::remove_file(&tmp_path);
            f
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?
        };

        file.set_len(size)?;

        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Zero the entire file.
        mmap.fill(0);

        // Write magic + version.
        mmap[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(MAGIC);
        write_u32(&mut mmap, OFF_VERSION, VERSION);
        write_u32(&mut mmap, OFF_DIMENSIONS, dimensions);
        mmap[OFF_METRIC] = metric.to_tag();
        write_u32(&mut mmap, OFF_M, m);
        write_u32(&mut mmap, OFF_EF_CONSTRUCTION, ef_construction);
        write_u32(&mut mmap, OFF_ENTRY_POINT, NONE_U32);
        write_u32(&mut mmap, OFF_NODE_COUNT, 0);
        write_u32(&mut mmap, OFF_MAX_LAYER, 0);
        write_u32(&mut mmap, OFF_NODE_CAPACITY, initial_capacity);
        write_u32(&mut mmap, OFF_FREE_LIST_HEAD, NONE_U32);
        write_u32(&mut mmap, OFF_FREE_LIST_LEN, 0);
        write_u64(&mut mmap, OFF_GRAPH_DATA_LEN, 0);

        // Initialize node index to NONE_U64.
        let ni_off = node_index_offset(initial_capacity, dimensions) as usize;
        for i in 0..initial_capacity as usize {
            write_u64_at(&mut mmap, ni_off + i * 8, NONE_U64);
        }

        mmap.flush()?;

        let header = VecFileHeader {
            dimensions,
            metric,
            m,
            ef_construction,
            entry_point: None,
            node_count: 0,
            max_layer: 0,
            node_capacity: initial_capacity,
            free_list_head: None,
            free_list_len: 0,
            graph_data_len: 0,
        };

        Ok(VecFile { path, _file: file, mmap, header, cipher })
    }

    /// Open an existing vector file.
    ///
    /// If the file is encrypted (does not start with BVEC magic), a key must be
    /// provided. The encrypted content is decrypted into a temporary file which
    /// is memory-mapped for zero-copy reads. On flush, the plaintext mmap is
    /// encrypted back to the original path.
    pub fn open(path: impl AsRef<Path>, key: Option<&[u8; 32]>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let cipher = key.map(Cipher::new);

        // Read the first 4 bytes to check if the file is encrypted.
        let raw = fs::read(&path)?;
        let is_encrypted = raw.len() < 4 || &raw[..4] != MAGIC;

        let (file, mmap) = if is_encrypted {
            // File is encrypted — decrypt and mmap a tempfile.
            let c = cipher.as_ref().ok_or_else(|| {
                BoogyError::DecryptionFailed("vec file is encrypted but no key provided".into())
            })?;
            let plaintext = c.decrypt_bytes(&raw)?;

            // Write plaintext to a temp file in the same directory (same filesystem
            // so that rename on flush is atomic).
            let tmp_path = path.with_extension("bvec.tmp");
            {
                let mut tmp = File::create(&tmp_path)?;
                tmp.write_all(&plaintext)?;
                tmp.sync_all()?;
            }
            let file = OpenOptions::new().read(true).write(true).open(&tmp_path)?;
            // Remove the temp file from the directory entry; the fd keeps it alive.
            let _ = fs::remove_file(&tmp_path);
            let mmap = unsafe { MmapMut::map_mut(&file)? };
            (file, mmap)
        } else {
            // Unencrypted — mmap directly.
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mmap = unsafe { MmapMut::map_mut(&file)? };
            (file, mmap)
        };

        // Validate magic (now guaranteed to be plaintext).
        if &mmap[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
            return Err(BoogyError::Corruption(
                "vec file: bad magic".into(),
            ));
        }

        let version = read_u32(&mmap, OFF_VERSION);
        if version != VERSION {
            return Err(BoogyError::Corruption(format!(
                "vec file: unsupported version {version}"
            )));
        }

        let dimensions = read_u32(&mmap, OFF_DIMENSIONS);
        let metric_tag = mmap[OFF_METRIC];
        let metric = DistanceMetric::from_tag(metric_tag).ok_or_else(|| {
            BoogyError::Corruption(format!("vec file: unknown metric tag {metric_tag}"))
        })?;
        let m = read_u32(&mmap, OFF_M);
        let ef_construction = read_u32(&mmap, OFF_EF_CONSTRUCTION);
        let ep_raw = read_u32(&mmap, OFF_ENTRY_POINT);
        let entry_point = if ep_raw == NONE_U32 { None } else { Some(ep_raw) };
        let node_count = read_u32(&mmap, OFF_NODE_COUNT);
        let max_layer = read_u32(&mmap, OFF_MAX_LAYER);
        let node_capacity = read_u32(&mmap, OFF_NODE_CAPACITY);
        let fl_raw = read_u32(&mmap, OFF_FREE_LIST_HEAD);
        let free_list_head = if fl_raw == NONE_U32 { None } else { Some(fl_raw) };
        let free_list_len = read_u32(&mmap, OFF_FREE_LIST_LEN);
        let graph_data_len = read_u64(&mmap, OFF_GRAPH_DATA_LEN);

        let header = VecFileHeader {
            dimensions,
            metric,
            m,
            ef_construction,
            entry_point,
            node_count,
            max_layer,
            node_capacity,
            free_list_head,
            free_list_len,
            graph_data_len,
        };

        Ok(VecFile { path, _file: file, mmap, header, cipher })
    }

    // ── Header accessors ──────────────────────────────────────────────────────

    pub fn header(&self) -> &VecFileHeader {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut VecFileHeader {
        &mut self.header
    }

    pub fn dimensions(&self) -> u32 {
        self.header.dimensions
    }

    // ── Vector read/write ─────────────────────────────────────────────────────

    /// Zero-copy read of a node's vector via pointer cast into the mmap.
    pub fn read_vector(&self, node_id: u32) -> &[f32] {
        let dims = self.header.dimensions as usize;
        let off = vector_region_offset() as usize + node_id as usize * dims * 4;
        // SAFETY: the region is aligned to 4 bytes (vector_region_offset = 4096,
        // each slot is dims*4 bytes). f32 has align 4. mmap is live for 'self.
        unsafe {
            let ptr = self.mmap[off..off + dims * 4].as_ptr() as *const f32;
            std::slice::from_raw_parts(ptr, dims)
        }
    }

    /// Write a vector into the mmap.
    pub fn write_vector(&mut self, node_id: u32, vector: &[f32]) {
        let dims = self.header.dimensions as usize;
        let off = vector_region_offset() as usize + node_id as usize * dims * 4;
        // SAFETY: same alignment reasoning as read_vector.
        let dst = unsafe {
            let ptr = self.mmap[off..off + dims * 4].as_mut_ptr() as *mut f32;
            std::slice::from_raw_parts_mut(ptr, dims)
        };
        dst.copy_from_slice(vector);
    }

    // ── Deleted flags ─────────────────────────────────────────────────────────

    pub fn is_deleted(&self, node_id: u32) -> bool {
        let off = self.deleted_flag_offset(node_id);
        self.mmap[off] != 0
    }

    pub fn set_deleted(&mut self, node_id: u32, deleted: bool) {
        let off = self.deleted_flag_offset(node_id);
        self.mmap[off] = if deleted { 1 } else { 0 };
    }

    #[inline]
    fn deleted_flag_offset(&self, node_id: u32) -> usize {
        let h = &self.header;
        deleted_flags_offset(h.node_capacity, h.dimensions, h.m) as usize + node_id as usize
    }

    // ── Node index ────────────────────────────────────────────────────────────

    /// Offset of a node's u64 entry in the node index.
    #[inline]
    fn node_index_entry_offset(&self, node_id: u32) -> usize {
        let h = &self.header;
        node_index_offset(h.node_capacity, h.dimensions) as usize + node_id as usize * 8
    }

    /// Read the graph record offset for a node. Returns `None` if no record.
    pub fn graph_record_offset(&self, node_id: u32) -> Option<u64> {
        let off = self.node_index_entry_offset(node_id);
        let val = read_u64_at(&self.mmap, off);
        if val == NONE_U64 { None } else { Some(val) }
    }

    /// Write a graph record offset into the node index.
    fn set_graph_record_offset(&mut self, node_id: u32, record_off: u64) {
        let off = self.node_index_entry_offset(node_id);
        write_u64_at(&mut self.mmap, off, record_off);
    }

    // ── Graph record accessors ────────────────────────────────────────────────

    /// Absolute mmap offset of a graph record.
    #[inline]
    fn graph_record_abs_offset(&self, record_off: u64) -> usize {
        let h = &self.header;
        graph_data_offset(h.node_capacity, h.dimensions) as usize + record_off as usize
    }

    /// Read the max_layer stored in a graph record.
    pub fn read_node_max_layer(&self, node_id: u32) -> Option<u32> {
        let record_off = self.graph_record_offset(node_id)?;
        let abs = self.graph_record_abs_offset(record_off);
        Some(read_u32_at(&self.mmap, abs))
    }

    /// Read neighbor list for a node at a given layer.
    pub fn read_neighbors(&self, node_id: u32, layer: u32) -> Vec<u32> {
        let record_off = match self.graph_record_offset(node_id) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let m = self.header.m;
        let abs_record = self.graph_record_abs_offset(record_off);
        let layer_rel = layer_offset_in_record(layer, m) as usize;
        let abs_layer = abs_record + layer_rel;

        let count = read_u32_at(&self.mmap, abs_layer) as usize;
        let cap = if layer == 0 { 2 * m as usize } else { m as usize };
        let actual = count.min(cap);

        let mut neighbors = Vec::with_capacity(actual);
        for i in 0..actual {
            let nb = read_u32_at(&self.mmap, abs_layer + 4 + i * 4);
            if nb != NONE_U32 {
                neighbors.push(nb);
            }
        }
        neighbors
    }

    /// Write neighbor list for a node at a given layer. Clears unused slots.
    pub fn write_neighbors(&mut self, node_id: u32, layer: u32, neighbors: &[u32]) {
        let record_off = match self.graph_record_offset(node_id) {
            Some(r) => r,
            None => return,
        };
        let m = self.header.m;
        let abs_record = self.graph_record_abs_offset(record_off);
        let layer_rel = layer_offset_in_record(layer, m) as usize;
        let abs_layer = abs_record + layer_rel;
        let cap = if layer == 0 { 2 * m as usize } else { m as usize };

        // Write count.
        write_u32_at(&mut self.mmap, abs_layer, neighbors.len() as u32);

        // Write neighbors, clear remaining slots.
        for i in 0..cap {
            let val = if i < neighbors.len() { neighbors[i] } else { NONE_U32 };
            write_u32_at(&mut self.mmap, abs_layer + 4 + i * 4, val);
        }
    }

    /// Allocate a graph record for node_id with the given max_layer.
    /// Appends at graph_data_len.
    pub fn allocate_graph_record(&mut self, node_id: u32, max_layer: u32) {
        let m = self.header.m;
        let record_off = self.header.graph_data_len;
        let record_size = graph_record_size(max_layer, m);

        let abs = self.graph_record_abs_offset(record_off);

        // Write max_layer.
        write_u32_at(&mut self.mmap, abs, max_layer);

        // Initialize each layer: count=0, all slots = NONE_U32.
        let mut layer_abs = abs + 4;
        for l in 0..=max_layer {
            let cap = if l == 0 { 2 * m as usize } else { m as usize };
            write_u32_at(&mut self.mmap, layer_abs, 0); // count
            for i in 0..cap {
                write_u32_at(&mut self.mmap, layer_abs + 4 + i * 4, NONE_U32);
            }
            layer_abs += layer_entry_size(l, m) as usize;
        }

        // Update node index and graph_data_len.
        self.set_graph_record_offset(node_id, record_off);
        self.header.graph_data_len = record_off + record_size;
    }

    // ── Node allocation / free list ───────────────────────────────────────────

    /// Allocate a node. Reuses a free-list entry if available, else bumps node_count.
    /// Calls `grow()` if at capacity.
    pub fn allocate_node(&mut self) -> Result<u32> {
        if let Some(head) = self.header.free_list_head {
            // Reuse: read next pointer from first 4 bytes of the vector slot.
            let dims = self.header.dimensions as usize;
            let off = vector_region_offset() as usize + head as usize * dims * 4;
            let next_raw = read_u32_at(&self.mmap, off);
            self.header.free_list_head = if next_raw == NONE_U32 { None } else { Some(next_raw) };
            self.header.free_list_len -= 1;

            // Clear deleted flag.
            self.set_deleted(head, false);

            // Clear the node index entry.
            self.set_graph_record_offset(head, NONE_U64);

            Ok(head)
        } else {
            if self.header.node_count >= self.header.node_capacity {
                self.grow()?;
            }
            let id = self.header.node_count;
            self.header.node_count += 1;
            Ok(id)
        }
    }

    /// Free a node: mark deleted, push onto free list (store next in vector slot[0..4]).
    pub fn free_node(&mut self, node_id: u32) {
        // Store old free_list_head as u32 in first 4 bytes of vector slot.
        let dims = self.header.dimensions as usize;
        let off = vector_region_offset() as usize + node_id as usize * dims * 4;
        let old_head = self.header.free_list_head.unwrap_or(NONE_U32);
        write_u32_at(&mut self.mmap, off, old_head);

        self.set_deleted(node_id, true);
        self.header.free_list_head = Some(node_id);
        self.header.free_list_len += 1;
    }

    // ── Grow ──────────────────────────────────────────────────────────────────

    /// Double capacity. Copies existing regions to new positions since the
    /// vector region grows and shifts everything after it.
    pub fn grow(&mut self) -> Result<()> {
        let old_cap = self.header.node_capacity;
        let dims = self.header.dimensions;
        let m = self.header.m;
        let new_cap = old_cap * 2;

        // Snapshot existing data before resize.
        let old_ni_off = node_index_offset(old_cap, dims) as usize;
        let old_ni_size = node_index_size(old_cap) as usize;
        let old_gd_off = graph_data_offset(old_cap, dims) as usize;
        let old_gd_len = self.header.graph_data_len as usize;
        let old_del_off = deleted_flags_offset(old_cap, dims, m) as usize;

        let node_index_data: Vec<u8> = self.mmap[old_ni_off..old_ni_off + old_ni_size].to_vec();
        let graph_data: Vec<u8> = self.mmap[old_gd_off..old_gd_off + old_gd_len].to_vec();
        let deleted_data: Vec<u8> =
            self.mmap[old_del_off..old_del_off + old_cap as usize].to_vec();

        // Resize the file.
        let new_size = total_file_size(new_cap, dims, m);
        self._file.set_len(new_size)?;
        self.mmap = unsafe { MmapMut::map_mut(&self._file)? };

        // Update capacity in header (needed for offset recalculation).
        self.header.node_capacity = new_cap;

        // Write data to new positions.
        let new_ni_off = node_index_offset(new_cap, dims) as usize;
        let new_gd_off = graph_data_offset(new_cap, dims) as usize;
        let new_del_off = deleted_flags_offset(new_cap, dims, m) as usize;

        // Copy node index (existing entries).
        self.mmap[new_ni_off..new_ni_off + old_ni_size].copy_from_slice(&node_index_data);

        // Initialize new node index entries to NONE_U64.
        for i in old_cap as usize..new_cap as usize {
            write_u64_at(&mut self.mmap, new_ni_off + i * 8, NONE_U64);
        }

        // Copy graph data.
        self.mmap[new_gd_off..new_gd_off + old_gd_len].copy_from_slice(&graph_data);

        // Copy deleted flags.
        self.mmap[new_del_off..new_del_off + old_cap as usize].copy_from_slice(&deleted_data);
        // Zero new deleted flags.
        for i in old_cap as usize..new_cap as usize {
            self.mmap[new_del_off + i] = 0;
        }

        // Flush and persist the updated capacity.
        self.flush()?;

        Ok(())
    }

    // ── Flush ─────────────────────────────────────────────────────────────────

    /// Write in-memory header fields back to the mmap and msync.
    /// When a cipher is set, the plaintext mmap content is encrypted and written
    /// to the original on-disk path.
    pub fn flush(&mut self) -> Result<()> {
        let h = self.header.clone();

        mmap_write_u32(&mut self.mmap, OFF_DIMENSIONS, h.dimensions);
        self.mmap[OFF_METRIC] = h.metric.to_tag();
        mmap_write_u32(&mut self.mmap, OFF_M, h.m);
        mmap_write_u32(&mut self.mmap, OFF_EF_CONSTRUCTION, h.ef_construction);
        mmap_write_u32(
            &mut self.mmap,
            OFF_ENTRY_POINT,
            h.entry_point.unwrap_or(NONE_U32),
        );
        mmap_write_u32(&mut self.mmap, OFF_NODE_COUNT, h.node_count);
        mmap_write_u32(&mut self.mmap, OFF_MAX_LAYER, h.max_layer);
        mmap_write_u32(&mut self.mmap, OFF_NODE_CAPACITY, h.node_capacity);
        mmap_write_u32(
            &mut self.mmap,
            OFF_FREE_LIST_HEAD,
            h.free_list_head.unwrap_or(NONE_U32),
        );
        mmap_write_u32(&mut self.mmap, OFF_FREE_LIST_LEN, h.free_list_len);
        mmap_write_u64(&mut self.mmap, OFF_GRAPH_DATA_LEN, h.graph_data_len);

        self.mmap.flush()?;

        // If encrypted, write the encrypted content to the on-disk path.
        if let Some(ref cipher) = self.cipher {
            let plaintext = &self.mmap[..];
            let encrypted = cipher.encrypt_bytes(plaintext)?;
            fs::write(&self.path, &encrypted)?;
        }

        Ok(())
    }

    /// Path of the underlying file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this file is encrypted at rest.
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }
}

// ── Primitive read/write helpers (mmap slice) ─────────────────────────────────

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

#[inline]
fn read_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

#[inline]
fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

#[inline]
fn read_u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

#[inline]
fn write_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn write_u32_at(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn write_u64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn write_u64_at(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

/// These wrappers take &mut MmapMut directly (for flush path).
#[inline]
fn mmap_write_u32(mmap: &mut MmapMut, off: usize, val: u32) {
    mmap[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn mmap_write_u64(mmap: &mut MmapMut, off: usize, val: u64) {
    mmap[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_file(dir: &TempDir, dims: u32, m: u32, cap: u32) -> VecFile {
        let path = dir.path().join("test.bvec");
        VecFile::create(&path, dims, DistanceMetric::Cosine, m, 200, cap, None).unwrap()
    }

    #[test]
    fn test_create_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bvec");

        {
            let mut vf =
                VecFile::create(&path, 4, DistanceMetric::Euclidean, 8, 100, 16, None).unwrap();
            assert_eq!(vf.header().dimensions, 4);
            assert_eq!(vf.header().metric, DistanceMetric::Euclidean);
            assert_eq!(vf.header().m, 8);
            assert_eq!(vf.header().ef_construction, 100);
            assert_eq!(vf.header().node_capacity, 16);
            assert_eq!(vf.header().node_count, 0);
            assert_eq!(vf.header().entry_point, None);
            assert_eq!(vf.header().free_list_head, None);
            assert_eq!(vf.header().graph_data_len, 0);
            vf.flush().unwrap();
        }

        {
            let vf = VecFile::open(&path, None).unwrap();
            assert_eq!(vf.header().dimensions, 4);
            assert_eq!(vf.header().metric, DistanceMetric::Euclidean);
            assert_eq!(vf.header().m, 8);
            assert_eq!(vf.header().ef_construction, 100);
            assert_eq!(vf.header().node_capacity, 16);
            assert_eq!(vf.header().node_count, 0);
            assert_eq!(vf.header().entry_point, None);
            assert_eq!(vf.header().free_list_head, None);
            assert_eq!(vf.header().graph_data_len, 0);
        }
    }

    #[test]
    fn test_vector_read_write() {
        let dir = TempDir::new().unwrap();
        let mut vf = make_file(&dir, 4, 4, 16);

        let node = vf.allocate_node().unwrap();
        let vec_in = [1.0f32, 2.0, 3.0, 4.0];
        vf.write_vector(node, &vec_in);
        let vec_out = vf.read_vector(node);
        assert_eq!(vec_out, &vec_in);
    }

    #[test]
    fn test_free_list_reuse() {
        let dir = TempDir::new().unwrap();
        let mut vf = make_file(&dir, 4, 4, 16);

        let n0 = vf.allocate_node().unwrap();
        let n1 = vf.allocate_node().unwrap();
        assert_eq!(n0, 0);
        assert_eq!(n1, 1);
        assert_eq!(vf.header().node_count, 2);

        vf.free_node(n0);
        assert_eq!(vf.header().free_list_head, Some(0));
        assert_eq!(vf.header().free_list_len, 1);
        assert!(vf.is_deleted(n0));

        let n2 = vf.allocate_node().unwrap();
        assert_eq!(n2, 0, "should reuse node 0 from free list");
        assert_eq!(vf.header().free_list_head, None);
        assert_eq!(vf.header().free_list_len, 0);
        assert!(!vf.is_deleted(n2));
    }

    #[test]
    fn test_graph_record_neighbors() {
        let dir = TempDir::new().unwrap();
        let mut vf = make_file(&dir, 4, 4, 16);

        let node = vf.allocate_node().unwrap();

        // Allocate a graph record with max_layer = 1.
        vf.allocate_graph_record(node, 1);

        assert_eq!(vf.read_node_max_layer(node), Some(1));

        // Layer 0: capacity 2*M = 8.
        let l0_neighbors = vec![1u32, 2, 3];
        vf.write_neighbors(node, 0, &l0_neighbors);
        let got0 = vf.read_neighbors(node, 0);
        assert_eq!(got0, l0_neighbors);

        // Layer 1: capacity M = 4.
        let l1_neighbors = vec![5u32, 6];
        vf.write_neighbors(node, 1, &l1_neighbors);
        let got1 = vf.read_neighbors(node, 1);
        assert_eq!(got1, l1_neighbors);

        // Verify layer 0 still intact after writing layer 1.
        assert_eq!(vf.read_neighbors(node, 0), l0_neighbors);
    }

    #[test]
    fn test_grow_preserves_data() {
        let dir = TempDir::new().unwrap();
        let cap = 4u32;
        let dims = 3u32;
        let m = 2u32;
        let path = dir.path().join("grow.bvec");
        let mut vf =
            VecFile::create(&path, dims, DistanceMetric::DotProduct, m, 50, cap, None).unwrap();

        // Fill capacity: allocate all 4 nodes, write vectors + graph records.
        for i in 0..cap {
            let node = vf.allocate_node().unwrap();
            assert_eq!(node, i);
            let v = [i as f32, i as f32 + 1.0, i as f32 + 2.0];
            vf.write_vector(node, &v);
            vf.allocate_graph_record(node, 0);
            vf.write_neighbors(node, 0, &[i.wrapping_sub(1).min(cap)]);
        }

        // Allocate one more — triggers grow().
        let extra = vf.allocate_node().unwrap();
        assert_eq!(extra, cap); // should be node #4 (new count = 5)
        assert_eq!(vf.header().node_capacity, cap * 2);

        // Verify old vectors survive.
        for i in 0..cap {
            let got = vf.read_vector(i);
            let expected = [i as f32, i as f32 + 1.0, i as f32 + 2.0];
            assert_eq!(got, &expected, "vector {i} corrupted after grow");
        }

        // Verify old graph records survive.
        for i in 0..cap {
            assert_eq!(vf.read_node_max_layer(i), Some(0), "node {i} max_layer lost");
        }

        // Flush + reopen to verify persistence.
        vf.flush().unwrap();
        let vf2 = VecFile::open(&path, None).unwrap();
        assert_eq!(vf2.header().node_capacity, cap * 2);
        assert_eq!(vf2.header().node_count, cap + 1);
        for i in 0..cap {
            let got = vf2.read_vector(i);
            let expected = [i as f32, i as f32 + 1.0, i as f32 + 2.0];
            assert_eq!(got, &expected, "vector {i} corrupted after reopen");
        }
    }
}
