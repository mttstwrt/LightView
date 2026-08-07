//! One-shot hardware detection: storage class, CPU, RAM, GPU.
//!
//! Read once at startup and never updated. Its output sizes the bounded
//! thumbnail thread pool and decides whether initializing the GPU pipeline is
//! worth the cost, so it runs before any of that exists — which is also why it
//! is best-effort throughout: every probe degrades to a conservative default
//! rather than failing startup.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HardwareProfile {
    pub storage_type: StorageType,
    pub filesystem: String,
    pub supports_reflink: bool,
    pub cpu_cores: usize,
    pub total_ram_mb: u64,
}

/// Snapshot of current memory status for pressure-aware cache management.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryStatus {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
}

impl MemoryStatus {
    /// Sample current memory status from the OS.
    pub fn sample() -> Self {
        use sysinfo::System;
        let mut sys = System::new();
        sys.refresh_memory();
        Self {
            total_ram_mb: sys.total_memory() / (1024 * 1024),
            available_ram_mb: sys.available_memory() / (1024 * 1024),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageType {
    NVMe,
    SSD,
    HDD,
    Network,
    Unknown,
}

impl HardwareProfile {
    /// Detect hardware capabilities at startup.
    pub fn detect() -> Self {
        let cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let total_ram_mb = detect_ram_mb();
        let storage_type = detect_storage_type();
        let filesystem = detect_filesystem();
        let supports_reflink = filesystem == "btrfs" || filesystem == "zfs";

        Self {
            storage_type,
            filesystem,
            supports_reflink,
            cpu_cores,
            total_ram_mb,
        }
    }

    /// Recommended thumbnail thread count.
    pub fn thumbnail_threads(&self) -> usize {
        match self.storage_type {
            StorageType::NVMe => self.cpu_cores.min(12),
            StorageType::SSD => (self.cpu_cores / 2).max(2).min(8),
            StorageType::HDD => 2,
            StorageType::Network => (self.cpu_cores / 4).max(1).min(4),
            StorageType::Unknown => (self.cpu_cores / 2).max(2),
        }
    }

    /// Recommended number of images to prefetch.
    pub fn prefetch_count(&self) -> usize {
        match self.storage_type {
            StorageType::NVMe => 5,
            StorageType::SSD => 3,
            StorageType::HDD => 1,
            StorageType::Network => 2,
            StorageType::Unknown => 3,
        }
    }

    /// Recommended LRU cache size (number of full-res images).
    pub fn lru_cache_size(&self) -> usize {
        if self.total_ram_mb > 32_000 {
            10
        } else if self.total_ram_mb > 16_000 {
            5
        } else {
            3
        }
    }
}

/// Detect total system RAM in MB.
fn detect_ram_mb() -> u64 {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    sys.total_memory() / (1024 * 1024)
}

/// Detect whether the primary storage is NVMe, SSD, or HDD.
/// On Linux, checks /sys/block/*/queue/rotational.
fn detect_storage_type() -> StorageType {
    #[cfg(target_os = "linux")]
    {
        // Try to find the root filesystem's block device
        if let Ok(entries) = std::fs::read_dir("/sys/block") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Skip loop, ram, and other virtual devices
                if name.starts_with("loop")
                    || name.starts_with("ram")
                    || name.starts_with("dm-")
                {
                    continue;
                }

                let rotational_path = format!("/sys/block/{}/queue/rotational", name);
                if let Ok(val) = std::fs::read_to_string(&rotational_path) {
                    let rotational: u8 = val.trim().parse().unwrap_or(1);
                    if rotational == 0 {
                        // SSD — check if NVMe
                        if name.starts_with("nvme") {
                            return StorageType::NVMe;
                        }
                        return StorageType::SSD;
                    } else {
                        return StorageType::HDD;
                    }
                }
            }
        }
    }
    StorageType::Unknown
}

/// Detect the filesystem type of the current working directory.
fn detect_filesystem() -> String {
    #[cfg(target_os = "linux")]
    {
        // Read /proc/mounts to find filesystem type for root or home
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            // Find the mount for /home or /
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            let mut best_match = ("", "unknown");
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let mount_point = parts[1];
                    let fs_type = parts[2];
                    if home.starts_with(mount_point)
                        && mount_point.len() > best_match.0.len()
                    {
                        best_match = (mount_point, fs_type);
                    }
                }
            }
            return best_match.1.to_string();
        }
    }
    "unknown".to_string()
}
