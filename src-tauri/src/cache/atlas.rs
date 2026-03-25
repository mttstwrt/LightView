use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

/// A single entry in the atlas index, describing where a thumbnail lives in the atlas file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasEntry {
    /// Byte offset into the atlas .bin file.
    pub offset: u64,
    /// BC7 data size in bytes.
    pub byte_size: u32,
    /// Thumbnail width in pixels (must be multiple of 4 for BC7).
    pub width: u32,
    /// Thumbnail height in pixels (must be multiple of 4 for BC7).
    pub height: u32,
    /// Source file mtime for cache invalidation.
    pub mtime: u64,
}

/// Serializable atlas index: maps media path → atlas entry.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AtlasIndex {
    pub entries: HashMap<String, AtlasEntry>,
}

/// Errors from atlas operations.
#[derive(Debug, thiserror::Error)]
pub enum AtlasError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Bincode error: {0}")]
    Bincode(String),
    #[error("BC7 encode error: {0}")]
    Bc7Encode(String),
    #[error("Atlas not open")]
    NotOpen,
    #[error("Thumbnail not found in atlas: {0}")]
    NotFound(String),
}

impl From<Box<bincode::ErrorKind>> for AtlasError {
    fn from(e: Box<bincode::ErrorKind>) -> Self {
        AtlasError::Bincode(e.to_string())
    }
}

/// Handle to an open BC7 thumbnail atlas.
///
/// The atlas consists of two files in `.lightview/`:
/// - `thumb_atlas.bin` — packed BC7 texture data, append-only
/// - `thumb_atlas.idx` — bincode-serialized `AtlasIndex`
///
/// Reads use mmap for zero-copy access (NVMe → page cache → GPU upload).
/// Writes append to the .bin file and flush the index periodically.
pub struct ThumbAtlas {
    /// Directory containing the atlas files.
    dir: PathBuf,
    /// In-memory index.
    index: AtlasIndex,
    /// Memory-mapped atlas data file (re-mapped after writes).
    mmap: Option<Mmap>,
    /// File handle for appending new thumbnails.
    write_file: File,
    /// Current write offset (end of the atlas data).
    write_offset: u64,
    /// Number of upserts since last index flush.
    dirty_count: u32,
}

impl ThumbAtlas {
    /// Open or create an atlas in the given `.lightview/` directory.
    pub fn open(lightview_dir: &Path) -> Result<Self, AtlasError> {
        let bin_path = lightview_dir.join("thumb_atlas.bin");
        let idx_path = lightview_dir.join("thumb_atlas.idx");

        // Open or create the data file.
        let write_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&bin_path)?;

        let write_offset = write_file.metadata()?.len();

        // Load or create the index.
        let index = if idx_path.exists() {
            let file = File::open(&idx_path)?;
            let reader = BufReader::new(file);
            bincode::deserialize_from(reader).unwrap_or_default()
        } else {
            AtlasIndex::default()
        };

        // Memory-map the data file if it has content.
        let mmap = if write_offset > 0 {
            let read_file = File::open(&bin_path)?;
            let m = unsafe { Mmap::map(&read_file)? };
            Some(m)
        } else {
            None
        };

        Ok(Self {
            dir: lightview_dir.to_path_buf(),
            index,
            mmap,
            write_file,
            write_offset,
            dirty_count: 0,
        })
    }

    /// Check if a thumbnail exists and is valid (mtime matches).
    pub fn is_valid(&self, path: &str, current_mtime: u64) -> bool {
        self.index
            .entries
            .get(path)
            .map(|e| e.mtime == current_mtime)
            .unwrap_or(false)
    }

    /// Get a thumbnail entry's metadata (without the data).
    pub fn get_entry(&self, path: &str) -> Option<&AtlasEntry> {
        self.index.entries.get(path)
    }

    /// Read raw BC7 bytes for a thumbnail via mmap (zero-copy).
    /// Returns None if the thumbnail isn't in the atlas or mmap isn't available.
    pub fn read_bc7(&self, path: &str) -> Option<&[u8]> {
        let entry = self.index.entries.get(path)?;
        let mmap = self.mmap.as_ref()?;
        let start = entry.offset as usize;
        let end = start + entry.byte_size as usize;
        if end <= mmap.len() {
            Some(&mmap[start..end])
        } else {
            None
        }
    }

    /// Append a BC7-encoded thumbnail to the atlas.
    /// `rgba_data` is the resized thumbnail in RGBA8 format.
    /// Width and height must be multiples of 4 for BC7.
    pub fn upsert(
        &mut self,
        media_path: &str,
        rgba_data: &[u8],
        width: u32,
        height: u32,
        mtime: u64,
    ) -> Result<(), AtlasError> {
        let bc7_data = encode_bc7(rgba_data, width, height)?;

        // Append to the data file.
        self.write_file.seek(SeekFrom::End(0))?;
        self.write_file.write_all(&bc7_data)?;

        let entry = AtlasEntry {
            offset: self.write_offset,
            byte_size: bc7_data.len() as u32,
            width,
            height,
            mtime,
        };

        self.write_offset += bc7_data.len() as u64;

        self.index
            .entries
            .insert(media_path.to_string(), entry);
        self.dirty_count += 1;

        // Flush index periodically (every 100 upserts).
        if self.dirty_count >= 100 {
            self.flush_index()?;
        }

        Ok(())
    }

    /// Batch-upsert multiple thumbnails. Flushes index once at the end.
    pub fn upsert_batch(
        &mut self,
        items: &[(String, Vec<u8>, u32, u32, u64)], // (path, rgba, w, h, mtime)
    ) -> Result<(), AtlasError> {
        for (media_path, rgba_data, width, height, mtime) in items {
            let bc7_data = encode_bc7(rgba_data, *width, *height)?;

            self.write_file.seek(SeekFrom::End(0))?;
            self.write_file.write_all(&bc7_data)?;

            let entry = AtlasEntry {
                offset: self.write_offset,
                byte_size: bc7_data.len() as u32,
                width: *width,
                height: *height,
                mtime: *mtime,
            };

            self.write_offset += bc7_data.len() as u64;
            self.index.entries.insert(media_path.clone(), entry);
        }

        self.flush_index()?;
        self.remap()?;
        Ok(())
    }

    /// Append pre-encoded BC7 data directly (skips CPU BC7 encoding).
    /// Used by the GPU BC7 encode pipeline.
    pub fn upsert_bc7_raw(
        &mut self,
        media_path: &str,
        bc7_data: &[u8],
        width: u32,
        height: u32,
        mtime: u64,
    ) -> Result<(), AtlasError> {
        self.write_file.seek(SeekFrom::End(0))?;
        self.write_file.write_all(bc7_data)?;

        let entry = AtlasEntry {
            offset: self.write_offset,
            byte_size: bc7_data.len() as u32,
            width,
            height,
            mtime,
        };

        self.write_offset += bc7_data.len() as u64;
        self.index
            .entries
            .insert(media_path.to_string(), entry);
        self.dirty_count += 1;

        if self.dirty_count >= 100 {
            self.flush_index()?;
        }

        Ok(())
    }

    /// Batch-append pre-encoded BC7 data. Flushes index once at the end.
    pub fn upsert_bc7_raw_batch(
        &mut self,
        items: &[(String, Vec<u8>, u32, u32, u64)], // (path, bc7_data, w, h, mtime)
    ) -> Result<(), AtlasError> {
        for (media_path, bc7_data, width, height, mtime) in items {
            self.write_file.seek(SeekFrom::End(0))?;
            self.write_file.write_all(bc7_data)?;

            let entry = AtlasEntry {
                offset: self.write_offset,
                byte_size: bc7_data.len() as u32,
                width: *width,
                height: *height,
                mtime: *mtime,
            };

            self.write_offset += bc7_data.len() as u64;
            self.index.entries.insert(media_path.clone(), entry);
        }

        self.flush_index()?;
        self.remap()?;
        Ok(())
    }

    /// Write the index to disk.
    pub fn flush_index(&mut self) -> Result<(), AtlasError> {
        let idx_path = self.dir.join("thumb_atlas.idx");
        let file = File::create(&idx_path)?;
        let writer = BufWriter::new(file);
        bincode::serialize_into(writer, &self.index)?;
        self.dirty_count = 0;
        Ok(())
    }

    /// Re-map the data file after writes so reads see the new data.
    pub fn remap(&mut self) -> Result<(), AtlasError> {
        if self.write_offset > 0 {
            let bin_path = self.dir.join("thumb_atlas.bin");
            let read_file = File::open(&bin_path)?;
            let m = unsafe { Mmap::map(&read_file)? };
            self.mmap = Some(m);
        }
        Ok(())
    }

    /// Get all indexed paths with their mtimes (for cache validation).
    pub fn all_paths(&self) -> Vec<(&str, u64)> {
        self.index
            .entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.mtime))
            .collect()
    }

    /// Number of thumbnails in the atlas.
    pub fn len(&self) -> usize {
        self.index.entries.len()
    }

    /// Clear the atlas (delete files and reset state).
    pub fn clear(&mut self) -> Result<(), AtlasError> {
        self.index.entries.clear();
        self.write_offset = 0;
        self.dirty_count = 0;
        self.mmap = None;

        let idx_path = self.dir.join("thumb_atlas.idx");

        // Truncate the data file.
        self.write_file.set_len(0)?;
        self.write_file.seek(SeekFrom::Start(0))?;

        // Remove the index file.
        if idx_path.exists() {
            std::fs::remove_file(&idx_path)?;
        }

        Ok(())
    }

    /// Ensure all pending data is flushed and the mmap is current.
    pub fn sync(&mut self) -> Result<(), AtlasError> {
        self.write_file.sync_all()?;
        if self.dirty_count > 0 {
            self.flush_index()?;
        }
        self.remap()?;
        Ok(())
    }
}

impl Drop for ThumbAtlas {
    fn drop(&mut self) {
        let _ = self.write_file.sync_all();
        if self.dirty_count > 0 {
            let _ = self.flush_index();
        }
    }
}

/// Encode RGBA8 pixel data to BC7 compressed texture data.
///
/// Width and height must be multiples of 4. If they aren't, the image is
/// padded to the next multiple of 4 with transparent black pixels.
fn encode_bc7(rgba_data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AtlasError> {
    // BC7 requires dimensions that are multiples of 4.
    let padded_w = (width + 3) & !3;
    let padded_h = (height + 3) & !3;

    let padded_data = if padded_w != width || padded_h != height {
        let mut padded = vec![0u8; (padded_w * padded_h * 4) as usize];
        for y in 0..height {
            let src_start = (y * width * 4) as usize;
            let src_end = src_start + (width * 4) as usize;
            let dst_start = (y * padded_w * 4) as usize;
            let dst_end = dst_start + (width * 4) as usize;
            padded[dst_start..dst_end].copy_from_slice(&rgba_data[src_start..src_end]);
        }
        padded
    } else {
        rgba_data.to_vec()
    };

    let surface = intel_tex_2::RgbaSurface {
        width: padded_w,
        height: padded_h,
        stride: padded_w * 4,
        data: &padded_data,
    };

    // Use fast BC7 encoding (mode 6 only — good quality, fastest encoding).
    let settings = intel_tex_2::bc7::opaque_ultra_fast_settings();
    let bc7_data = intel_tex_2::bc7::compress_blocks(
        &settings,
        &surface,
    );

    Ok(bc7_data)
}

/// Compute the BC7 compressed size in bytes for a given width/height.
/// BC7 uses 16 bytes per 4x4 block.
pub fn bc7_byte_size(width: u32, height: u32) -> u32 {
    let bw = (width + 3) / 4;
    let bh = (height + 3) / 4;
    bw * bh * 16
}

/// Round dimensions up to the nearest multiple of 4 (BC7 block alignment).
pub fn align_to_bc7(dim: u32) -> u32 {
    (dim + 3) & !3
}
