//! One-shot hardware detection: storage class and CPU count.
//!
//! Read once at startup and never updated. Its output sizes the bounded
//! thumbnail thread pool, so it runs before any of that exists — which is also
//! why it is best-effort throughout: every probe degrades to a conservative
//! default rather than failing startup.
//!
//! **`storage_type` is load-bearing and is not decoration.** It is the sole
//! input to [`HardwareProfile::thumbnail_threads`], which sizes the one rayon
//! pool every thumbnail in the system runs on. Deleting it as "a value that is
//! logged and drives nothing" — which is true of the filesystem and reflink
//! probes that *were* deleted — would silently replace an I/O-class-aware 2–12
//! threads with whatever number someone invented, on the N100 server the idle
//! backfill exists for and on the NAS mount where "Network → 4" is the whole
//! point.
//!
//! There is no GPU probe. Its only consumer was the fused crop+resize on wgpu,
//! which was reachable only from the square grid this design deletes.
//!
//! **There is no RAM probe either, for the same reason.** It fed two
//! recommendations — how many full-resolution images to hold and how many to
//! prefetch — that nothing ever asked for; the viewer sizes its own caches. The
//! frontend carried matching settings fields that nothing read. Both ends are
//! gone rather than one, so nothing is left looking like a feature that is
//! merely unfinished.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HardwareProfile {
    pub storage_type: StorageType,
    pub cpu_cores: usize,
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

        Self {
            storage_type: detect_storage_type(),
            cpu_cores,
        }
    }

    /// Recommended thumbnail thread count.
    pub fn thumbnail_threads(&self) -> usize {
        match self.storage_type {
            StorageType::NVMe => self.cpu_cores.min(12),
            StorageType::SSD => (self.cpu_cores / 2).clamp(2, 8),
            StorageType::HDD => 2,
            StorageType::Network => (self.cpu_cores / 4).clamp(1, 4),
            StorageType::Unknown => (self.cpu_cores / 2).max(2),
        }
    }


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
