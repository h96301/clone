//! Template snapshot and CoW fork system.
//!
//! This module implements the core fast-boot mechanism:
//! 1. Boot a "template" VM to idle state for a given runtime
//! 2. Snapshot its full memory + register state to disk
//! 3. Fork new VMs by mmap-ing the snapshot with MAP_PRIVATE (CoW)
//! 4. Inject per-VM identity, then start — pages fault in on demand
//!
//! This is the primary source of <20ms cold starts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Serialized vCPU register state.
///
/// Complete vCPU state for snapshot/restore.
/// Stored as raw bytes so this module compiles on all platforms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    pub regs: Vec<u8>,
    pub sregs: Vec<u8>,
    #[serde(default)]
    pub lapic: Vec<u8>,
    #[serde(default)]
    pub fpu: Vec<u8>,
    #[serde(default)]
    pub xsave: Vec<u8>,
    #[serde(default)]
    pub xcrs: Vec<u8>,
    #[serde(default)]
    pub mp_state: Vec<u8>,
    #[serde(default)]
    pub vcpu_events: Vec<u8>,
    #[serde(default)]
    pub debug_regs: Vec<u8>,
    /// MSRs as JSON array of {index, data} pairs.
    #[serde(default)]
    pub msrs: Vec<u8>,
    #[serde(default)]
    pub tsc_khz: u32,
    /// CPUID entries (serialized as bytes for cross-platform compat).
    #[serde(default)]
    pub cpuid: Vec<u8>,
}

impl VcpuState {
    pub fn empty() -> Self {
        Self {
            regs: Vec::new(), sregs: Vec::new(), lapic: Vec::new(),
            fpu: Vec::new(), xsave: Vec::new(), xcrs: Vec::new(),
            mp_state: Vec::new(), vcpu_events: Vec::new(),
            debug_regs: Vec::new(), msrs: Vec::new(), tsc_khz: 0,
            cpuid: Vec::new(),
        }
    }
}

/// Serialized device state for template restoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStates {
    /// Serial port state (if any).
    pub serial: Option<Vec<u8>>,
    /// Virtio device configs, keyed by device name.
    pub virtio_configs: HashMap<String, Vec<u8>>,
    /// Serialized MmioTransportState per device.
    #[serde(default)]
    pub transports: Vec<Vec<u8>>,
    /// In-kernel irqchip state: [PIC master, PIC slave, IOAPIC].
    #[serde(default)]
    pub irqchip: Vec<Vec<u8>>,
    /// In-kernel PIT state.
    #[serde(default)]
    pub pit: Vec<u8>,
}

impl Default for DeviceStates {
    fn default() -> Self {
        Self {
            serial: None,
            virtio_configs: HashMap::new(),
            transports: Vec::new(),
            irqchip: Vec::new(),
            pit: Vec::new(),
        }
    }
}

/// A template snapshot capturing a VM's full state for CoW forking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSnapshot {
    /// Path to the raw memory dump file on disk.
    pub memory_file: PathBuf,
    /// vCPU register states (one per vCPU).
    pub vcpu_states: Vec<VcpuState>,
    /// Device configuration states.
    pub device_states: DeviceStates,
    /// Original guest memory size in bytes.
    pub memory_size: u64,
    /// Runtime type this template was created for (e.g., "node20", "python312", "bare").
    pub runtime_type: String,
    /// SHA-256 hash of the memory file for integrity verification.
    pub memory_hash: String,
    /// Block device path used when the template was created (e.g., rootfs image).
    /// Fork uses this to register the same block device so device indices match.
    #[serde(default)]
    pub block_device: Option<String>,
    /// KVM clock value (nanoseconds) at snapshot time.
    /// Used to restore kvmclock on fork so the guest's clocksource works.
    #[serde(default)]
    pub clock_ns: u64,
    /// Overlay block device path for save/restore (persistent writable layer).
    #[serde(default)]
    pub overlay_path: Option<String>,
    /// Guest IP address at snapshot time (used to restore same IP).
    #[serde(default)]
    pub guest_ip: Option<String>,
    /// Optional tag identifying this snapshot inside a diff chain.
    /// If None, this snapshot is not part of any tracked chain.
    #[serde(default)]
    pub tag: Option<String>,
    /// Tag of the parent snapshot this one was derived from.
    /// None for a root snapshot; Some for the head of a chain.
    #[serde(default)]
    pub parent_tag: Option<String>,
    /// Depth from the chain root (root = 0).
    #[serde(default)]
    pub chain_depth: u32,
    /// Discriminator: "full" for full snapshots, "incremental" otherwise.
    #[serde(default = "default_snapshot_type_full")]
    pub snapshot_type: String,
}

fn default_snapshot_type_full() -> String {
    "full".to_string()
}

/// Metadata file name stored alongside the memory dump.
const TEMPLATE_METADATA_FILE: &str = "template.json";

impl TemplateSnapshot {
    /// Load a template snapshot from a directory.
    ///
    /// Expects `template.json` (metadata) and a memory dump file in the directory.
    pub fn load(template_dir: &str, verify: bool) -> Result<Self> {
        let meta_path = Path::new(template_dir).join(TEMPLATE_METADATA_FILE);
        let meta_data = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read template metadata: {}", meta_path.display()))?;
        let snapshot: TemplateSnapshot =
            serde_json::from_str(&meta_data).context("Failed to parse template metadata")?;

        // Verify the memory file exists
        if !snapshot.memory_file.exists() {
            anyhow::bail!(
                "Template memory file not found: {}",
                snapshot.memory_file.display()
            );
        }

        // Verify memory file integrity
        if verify {
            let mem_data = std::fs::read(&snapshot.memory_file)
                .with_context(|| format!("Failed to read template memory for verification: {}", snapshot.memory_file.display()))?;
            let actual_hash = crate::boot::measured::compute_sha256(&mem_data);
            let actual_hex: String = actual_hash.iter().map(|b| format!("{b:02x}")).collect();
            if actual_hex != snapshot.memory_hash {
                anyhow::bail!(
                    "Template integrity check failed: expected {}, got {}",
                    snapshot.memory_hash, actual_hex
                );
            }
            tracing::info!("Template integrity verified (SHA-256 matches)");
        }

        tracing::info!(
            "Loaded template: runtime={}, memory_size={}MB, vcpus={}",
            snapshot.runtime_type,
            snapshot.memory_size >> 20,
            snapshot.vcpu_states.len(),
        );

        Ok(snapshot)
    }

    /// Save this template's metadata to a directory.
    pub fn save_metadata(&self, template_dir: &str) -> Result<()> {
        std::fs::create_dir_all(template_dir)
            .with_context(|| format!("Failed to create template dir: {template_dir}"))?;

        let meta_path = Path::new(template_dir).join(TEMPLATE_METADATA_FILE);
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize template metadata")?;
        std::fs::write(&meta_path, json)
            .with_context(|| format!("Failed to write template metadata: {}", meta_path.display()))?;

        tracing::info!("Saved template metadata to {}", meta_path.display());
        Ok(())
    }
}

/// Save a VM's state as a template snapshot.
///
/// This dumps the full guest memory to a file and saves register + device state
/// as JSON metadata alongside it.
///
/// On Linux, this reads directly from the guest memory mapping.
/// On other platforms, this is a stub.
#[cfg(target_os = "linux")]
pub fn save_template(
    guest_mem: &crate::memory::GuestMem,
    vcpu_states: Vec<VcpuState>,
    device_states: DeviceStates,
    runtime_type: &str,
    output_dir: &str,
    block_device: Option<String>,
    clock_ns: u64,
    overlay_path: Option<String>,
    guest_ip: Option<String>,
) -> Result<TemplateSnapshot> {
    use crate::boot::measured::compute_sha256;

    let memory_size = guest_mem.size();
    let mem_file_path = Path::new(output_dir).join("memory.raw");

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output dir: {output_dir}"))?;

    // Dump raw guest memory to file
    let mem_data = guest_mem.read_at(0, memory_size as usize)?;
    std::fs::write(&mem_file_path, &mem_data)
        .with_context(|| format!("Failed to write memory dump: {}", mem_file_path.display()))?;

    // Compute integrity hash
    let hash = compute_sha256(&mem_data);
    let hash_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();

    let snapshot = TemplateSnapshot {
        memory_file: mem_file_path,
        vcpu_states,
        device_states,
        memory_size,
        runtime_type: runtime_type.to_string(),
        memory_hash: hash_hex,
        block_device,
        clock_ns,
        overlay_path,
        guest_ip,
        tag: None,
        parent_tag: None,
        chain_depth: 0,
        snapshot_type: "full".to_string(),
    };

    snapshot.save_metadata(output_dir)?;

    tracing::info!(
        "Template saved: runtime={runtime_type}, memory={}MB, file={}",
        memory_size >> 20,
        output_dir,
    );

    Ok(snapshot)
}

/// Fork a new VM from a template snapshot using CoW memory mapping.
///
/// The template memory file is mmap-ed with MAP_PRIVATE (no MAP_POPULATE),
/// so pages are copy-on-write references that only fault in on demand.
/// This is the core mechanism for <20ms cold starts.
///
/// After this call, the caller must:
/// 1. Call `inject_identity()` to write per-VM state
/// 2. Restore vCPU registers from `template.vcpu_states`
/// 3. Start vCPU execution
#[cfg(target_os = "linux")]
pub fn fork_from_template(template: &TemplateSnapshot) -> Result<crate::memory::GuestMem> {
    use std::os::unix::io::AsRawFd;

    let mem_file = std::fs::File::open(&template.memory_file).with_context(|| {
        format!(
            "Failed to open template memory: {}",
            template.memory_file.display()
        )
    })?;

    let fd = mem_file.as_raw_fd();
    let size = template.memory_size as usize;

    // mmap with MAP_PRIVATE — CoW semantics, pages shared until written.
    // NO MAP_POPULATE — pages fault in on demand for minimal startup latency.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE, // CoW: shared read, private write
            fd,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        anyhow::bail!(
            "mmap failed for template memory ({size} bytes): {}",
            std::io::Error::last_os_error()
        );
    }

    // Enable KSM on the forked mapping — identical pages across VMs get merged
    unsafe {
        libc::madvise(ptr, size, libc::MADV_MERGEABLE);
    }

    tracing::info!(
        "Forked VM from template: {}MB CoW-mapped at {ptr:?} (runtime={})",
        size >> 20,
        template.runtime_type,
    );

    // Wrap in GuestMem. Note: GuestMem::drop calls munmap, which is correct here.
    // For VMs > 3GB, the MMIO hole splits GPA space. The memory file is contiguous
    // (hole_start bytes + above-hole bytes), so we must tell GuestMem about the hole
    // so GPA-to-offset translations work correctly.
    let mmio_hole_start: u64 = 0xC000_0000; // 3 GB
    let mmio_hole_end: u64 = 0x1_0000_0000; // 4 GB
    if template.memory_size > mmio_hole_start {
        Ok(crate::memory::GuestMem::from_raw_with_hole(
            ptr as *mut u8, template.memory_size, mmio_hole_start, mmio_hole_end,
        ))
    } else {
        Ok(crate::memory::GuestMem::from_raw(ptr as *mut u8, template.memory_size))
    }
}

/// An incremental snapshot capturing only modified pages since the base.
///
/// Combined with a base template, this provides fast warm snapshots:
/// only dirty pages are dumped (typically 10-100x smaller than full).
///
/// Supports chain structure: `parent_tag` may point at another
/// incremental snapshot, forming a diff chain leaf → ... → root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalSnapshot {
    /// Path to the base template directory (legacy field, kept for
    /// backward compatibility; superseded by `parent_tag`).
    pub base_template: String,
    /// Dirty page bitmap (one bit per 4KiB page).
    pub dirty_bitmap: Vec<u8>,
    /// Only the modified page data (concatenated dirty pages).
    pub dirty_pages_file: PathBuf,
    /// vCPU register states at snapshot time.
    pub vcpu_states: Vec<VcpuState>,
    /// Device states at snapshot time.
    pub device_states: DeviceStates,
    /// Total guest memory size in bytes.
    pub memory_size: u64,
    /// Optional tag identifying this snapshot inside a diff chain.
    #[serde(default)]
    pub tag: Option<String>,
    /// Tag of the parent snapshot (full or incremental). When None,
    /// falls back to `base_template` path.
    #[serde(default)]
    pub parent_tag: Option<String>,
    /// Depth from the chain root (root = 0).
    #[serde(default)]
    pub chain_depth: u32,
    /// Discriminator: always "incremental" for this struct.
    #[serde(default = "default_snapshot_type_inc")]
    pub snapshot_type: String,
}

fn default_snapshot_type_inc() -> String {
    "incremental".to_string()
}

const INCREMENTAL_METADATA_FILE: &str = "incremental.json";

impl IncrementalSnapshot {
    /// Save incremental snapshot metadata.
    pub fn save_metadata(&self, output_dir: &str) -> Result<()> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("Failed to create output dir: {output_dir}"))?;

        let meta_path = Path::new(output_dir).join(INCREMENTAL_METADATA_FILE);
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize incremental snapshot metadata")?;
        std::fs::write(&meta_path, json)
            .with_context(|| format!("Failed to write metadata: {}", meta_path.display()))?;

        tracing::info!("Saved incremental snapshot metadata to {}", meta_path.display());
        Ok(())
    }

    /// Load incremental snapshot metadata.
    pub fn load(snapshot_dir: &str) -> Result<Self> {
        let meta_path = Path::new(snapshot_dir).join(INCREMENTAL_METADATA_FILE);
        let meta_data = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read incremental metadata: {}", meta_path.display()))?;
        let snapshot: IncrementalSnapshot = serde_json::from_str(&meta_data)
            .context("Failed to parse incremental snapshot metadata")?;
        Ok(snapshot)
    }
}

/// Save an incremental snapshot (only dirty pages since base).
///
/// `kvm_slot_size` is the actual KVM memory slot size (may include guard region)
/// and must be used for `get_dirty_log` to match the registered slot size.
/// Only pages within `guest_mem.size()` are actually collected.
#[cfg(target_os = "linux")]
pub fn save_incremental(
    guest_mem: &crate::memory::GuestMem,
    vm_fd: &kvm_ioctls::VmFd,
    vcpu_states: Vec<VcpuState>,
    device_states: DeviceStates,
    base_template: &str,
    output_dir: &str,
    kvm_slot_size: u64,
) -> Result<IncrementalSnapshot> {
    let mem_size = guest_mem.size();
    // Use kvm_slot_size for get_dirty_log (must match registered KVM slot),
    // but only collect pages within mem_size (actual guest memory).
    let tracker = crate::memory::overcommit::DirtyPageTracker::new(kvm_slot_size);

    let (bitmap, dirty_data) = tracker.collect_dirty_pages(vm_fd, guest_mem.as_ptr() as *const u8, mem_size)?;

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output dir: {output_dir}"))?;

    let dirty_file = Path::new(output_dir).join("dirty_pages.raw");
    std::fs::write(&dirty_file, &dirty_data)
        .with_context(|| format!("Failed to write dirty pages: {}", dirty_file.display()))?;

    let snapshot = IncrementalSnapshot {
        base_template: base_template.to_string(),
        dirty_bitmap: bitmap,
        dirty_pages_file: dirty_file,
        vcpu_states,
        device_states,
        memory_size: mem_size,
        tag: None,
        parent_tag: None,
        chain_depth: 0,
        snapshot_type: "incremental".to_string(),
    };

    snapshot.save_metadata(output_dir)?;

    tracing::info!(
        "Incremental snapshot saved: dirty_data={}KB, base={}",
        dirty_data.len() / 1024,
        base_template,
    );

    Ok(snapshot)
}

/// Manages a pool of pre-created template snapshots, one per runtime type.
///
/// Templates are created lazily on first request and cached. The pool can be
/// refreshed when base images are updated.
pub struct TemplatePool {
    /// Base directory where templates are stored on disk.
    base_dir: PathBuf,
    /// Cached template snapshots, keyed by runtime type.
    templates: HashMap<String, TemplateSnapshot>,
}

impl TemplatePool {
    /// Create a new template pool rooted at the given directory.
    pub fn new(base_dir: &str) -> Self {
        Self {
            base_dir: PathBuf::from(base_dir),
            templates: HashMap::new(),
        }
    }

    /// Get an existing template for a runtime type, or return None if not cached.
    pub fn get(&self, runtime_type: &str) -> Option<&TemplateSnapshot> {
        self.templates.get(runtime_type)
    }

    /// Get a template, loading from disk if not already cached.
    pub fn get_or_load(&mut self, runtime_type: &str) -> Result<&TemplateSnapshot> {
        if !self.templates.contains_key(runtime_type) {
            let template_dir = self.base_dir.join(runtime_type);
            let template = TemplateSnapshot::load(
                template_dir
                    .to_str()
                    .context("Invalid template directory path")?,
                true,
            )?;
            self.templates.insert(runtime_type.to_string(), template);
        }
        Ok(self.templates.get(runtime_type).unwrap())
    }

    /// Register a freshly-created template in the pool.
    pub fn register(&mut self, runtime_type: &str, template: TemplateSnapshot) {
        tracing::info!("Registered template in pool: {runtime_type}");
        self.templates.insert(runtime_type.to_string(), template);
    }

    /// Refresh a template by removing the cached version.
    ///
    /// The next call to `get_or_load` will reload from disk, picking up
    /// any updates to the template files.
    pub fn refresh(&mut self, runtime_type: &str) {
        if self.templates.remove(runtime_type).is_some() {
            tracing::info!("Refreshed template: {runtime_type} (removed from cache)");
        } else {
            tracing::info!("Template not cached, nothing to refresh: {runtime_type}");
        }
    }

    /// List all cached runtime types.
    pub fn cached_runtime_types(&self) -> Vec<&str> {
        self.templates.keys().map(|s| s.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Diff chain operations
// ---------------------------------------------------------------------------

/// Metadata about a single node inside a diff chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainNode {
    pub tag: Option<String>,
    pub snapshot_type: String,
    pub chain_depth: u32,
    pub dir: String,
}

/// Resolved diff chain information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    pub nodes: Vec<ChainNode>,
    pub total_depth: u32,
}

const PAGE_SIZE: u64 = 4096;

/// Walk the diff chain from `leaf_dir` back to the root, collecting
/// all intermediate incremental snapshots and the base template.
///
/// Returns the chain in root→leaf order.
pub fn walk_chain(leaf_dir: &str) -> Result<Vec<ChainNode>> {
    let mut chain = Vec::new();
    let mut current_dir = leaf_dir.to_string();

    loop {
        // Try loading as incremental first, then as full template.
        if let Ok(inc) = IncrementalSnapshot::load(&current_dir) {
            chain.push(ChainNode {
                tag: inc.tag.clone(),
                snapshot_type: inc.snapshot_type.clone(),
                chain_depth: inc.chain_depth,
                dir: current_dir.clone(),
            });
            // Walk to parent.
            let parent = if let Some(ref pt) = inc.parent_tag {
                // parent_tag is a tag name; resolve to directory.
                // For now assume parent_tag is a directory path.
                pt.clone()
            } else {
                inc.base_template.clone()
            };
            current_dir = parent;
        } else if let Ok(full) = TemplateSnapshot::load(&current_dir, false) {
            chain.push(ChainNode {
                tag: full.tag.clone(),
                snapshot_type: full.snapshot_type.clone(),
                chain_depth: full.chain_depth,
                dir: current_dir.clone(),
            });
            if let Some(ref pt) = full.parent_tag {
                current_dir = pt.clone();
            } else {
                break; // root reached
            }
        } else {
            anyhow::bail!("Cannot load snapshot at {current_dir}: not a valid template or incremental");
        }

        // Safety limit to prevent infinite loops.
        if chain.len() > 256 {
            anyhow::bail!("Chain too deep (>256), possible cycle at {current_dir}");
        }
    }

    // Reverse to get root→leaf order.
    chain.reverse();
    Ok(chain)
}

/// Return chain info for a given leaf snapshot.
pub fn chain_info(leaf_dir: &str) -> Result<ChainInfo> {
    let nodes = walk_chain(leaf_dir)?;
    let total_depth = nodes.last().map(|n| n.chain_depth).unwrap_or(0);
    Ok(ChainInfo { nodes, total_depth })
}

/// Apply an incremental diff on top of a base memory buffer.
///
/// `base_mem` is modified in place: dirty pages from `diff` are written
/// over the corresponding offsets in `base_mem`.
pub fn apply_diff(base_mem: &mut [u8], diff: &IncrementalSnapshot) -> Result<()> {
    let dirty_data = std::fs::read(&diff.dirty_pages_file)
        .with_context(|| format!("Failed to read dirty pages: {}", diff.dirty_pages_file.display()))?;

    let total_pages = diff.memory_size / PAGE_SIZE;
    let mut dirty_offset = 0usize;

    for page_idx in 0..total_pages {
        let byte_idx = (page_idx / 8) as usize;
        let bit_idx = page_idx % 8;
        if byte_idx >= diff.dirty_bitmap.len() {
            break;
        }
        if diff.dirty_bitmap[byte_idx] & (1 << bit_idx) != 0 {
            let mem_offset = (page_idx * PAGE_SIZE) as usize;
            let end = mem_offset + PAGE_SIZE as usize;
            if end > base_mem.len() || dirty_offset + PAGE_SIZE as usize > dirty_data.len() {
                break;
            }
            base_mem[mem_offset..end].copy_from_slice(&dirty_data[dirty_offset..dirty_offset + PAGE_SIZE as usize]);
            dirty_offset += PAGE_SIZE as usize;
        }
    }

    tracing::info!(
        pages_applied = dirty_offset / PAGE_SIZE as usize,
        chain_depth = diff.chain_depth,
        "Applied incremental diff"
    );
    Ok(())
}

/// Resolve a diff chain into a fully reconstructed base memory.
///
/// Walks from root to leaf, applying each incremental diff in order.
/// Returns the reconstructed memory as a Vec<u8>, plus the vCPU and
/// device states from the chain leaf.
pub fn resolve_chain(leaf_dir: &str) -> Result<(Vec<u8>, Vec<VcpuState>, DeviceStates)> {
    let chain = walk_chain(leaf_dir)?;

    if chain.is_empty() {
        anyhow::bail!("Empty chain at {leaf_dir}");
    }

    // Load root (first element) as a full template.
    let root = TemplateSnapshot::load(&chain[0].dir, false)?;
    let mut mem = std::fs::read(&root.memory_file)
        .with_context(|| format!("Failed to read root memory: {}", root.memory_file.display()))?;

    // Apply each incremental diff in root→leaf order.
    for node in &chain[1..] {
        let inc = IncrementalSnapshot::load(&node.dir)?;
        apply_diff(&mut mem, &inc)?;
    }

    // Return vCPU/device state from the leaf (most recent).
    let leaf = chain.last().unwrap();
    if let Ok(inc) = IncrementalSnapshot::load(&leaf.dir) {
        Ok((mem, inc.vcpu_states, inc.device_states))
    } else {
        let full = TemplateSnapshot::load(&leaf.dir, false)?;
        Ok((mem, full.vcpu_states, full.device_states))
    }
}

/// Compact a diff chain into a single, self-contained full snapshot.
///
/// Resolves the chain and writes the result as a new template in
/// `output_dir`. The output has no parent (chain_depth = 0).
pub fn compact_chain(leaf_dir: &str, output_dir: &str) -> Result<TemplateSnapshot> {
    let (mem, vcpu_states, device_states) = resolve_chain(leaf_dir)?;
    let memory_size = mem.len() as u64;

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output dir: {output_dir}"))?;

    let mem_file_path = Path::new(output_dir).join("memory.raw");
    std::fs::write(&mem_file_path, &mem)
        .with_context(|| format!("Failed to write compacted memory: {}", mem_file_path.display()))?;

    use crate::boot::measured::compute_sha256;
    let hash = compute_sha256(&mem);
    let hash_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();

    // Read original chain info for runtime_type, block_device, etc.
    let root_meta = if let Ok(t) = TemplateSnapshot::load(
        &walk_chain(leaf_dir)?[0].dir, false
    ) {
        (t.runtime_type.clone(), t.block_device.clone(), t.clock_ns, t.overlay_path.clone(), t.guest_ip.clone())
    } else {
        ("compacted".to_string(), None, 0, None, None)
    };

    let snapshot = TemplateSnapshot {
        memory_file: mem_file_path,
        vcpu_states,
        device_states,
        memory_size,
        runtime_type: root_meta.0,
        memory_hash: hash_hex,
        block_device: root_meta.1,
        clock_ns: root_meta.2,
        overlay_path: root_meta.3,
        guest_ip: root_meta.4,
        tag: None,
        parent_tag: None,
        chain_depth: 0,
        snapshot_type: "full".to_string(),
    };

    snapshot.save_metadata(output_dir)?;

    tracing::info!(
        output = output_dir,
        memory_mb = memory_size >> 20,
        "Compacted diff chain into full snapshot"
    );

    Ok(snapshot)
}

/// Garbage-collect unreachable snapshots under `base_dir`.
///
/// A snapshot is "reachable" if it is in `active_tags` or is an ancestor
/// of any snapshot whose tag is in `active_tags`.
///
/// Returns the list of removed snapshot directories.
pub fn gc_snapshots(base_dir: &str, active_tags: &[String]) -> Result<Vec<String>> {
    let mut reachable = std::collections::HashSet::new();
    for tag in active_tags {
        let tag_dir = Path::new(base_dir).join(tag);
        if !tag_dir.exists() {
            continue;
        }
        reachable.insert(tag.clone());

        // Walk ancestors.
        if let Ok(chain) = walk_chain(tag_dir.to_str().unwrap_or("")) {
            for node in &chain {
                if let Some(ref t) = node.tag {
                    reachable.insert(t.clone());
                }
            }
        }
    }

    let mut removed = Vec::new();
    let entries = std::fs::read_dir(base_dir)
        .with_context(|| format!("Failed to read snapshot dir: {base_dir}"))?;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        // Check if it has a template.json or incremental.json (is a snapshot).
        let has_meta = entry.path().join("template.json").exists()
            || entry.path().join("incremental.json").exists();
        if !has_meta {
            continue;
        }
        if reachable.contains(&name) {
            continue;
        }
        // Remove unreachable snapshot.
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            tracing::info!(snapshot = %name, "GC removed unreachable snapshot");
            removed.push(name);
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an IncrementalSnapshot pointing to a dirty_pages.raw with
    /// specific pages marked dirty. Returns (snapshot, dirty_data_written).
    fn make_incremental(
        dir: &std::path::Path,
        memory_size: u64,
        dirty_page_indices: &[u64],
    ) -> (IncrementalSnapshot, Vec<u8>) {
        std::fs::create_dir_all(dir).unwrap();
        let total_pages = memory_size / PAGE_SIZE;
        let mut bitmap = vec![0u8; ((total_pages + 7) / 8) as usize];
        for &idx in dirty_page_indices {
            bitmap[(idx / 8) as usize] |= 1 << (idx % 8);
        }
        // Each dirty page contains its own index mod 256 for verification.
        let mut dirty_data = Vec::new();
        for &idx in dirty_page_indices {
            let fill = (idx % 256) as u8;
            dirty_data.extend(std::iter::repeat(fill).take(PAGE_SIZE as usize));
        }
        let dp_path = dir.join("dirty_pages.raw");
        std::fs::write(&dp_path, &dirty_data).unwrap();
        let snap = IncrementalSnapshot {
            base_template: String::new(),
            dirty_bitmap: bitmap,
            dirty_pages_file: dp_path,
            vcpu_states: Vec::new(),
            device_states: DeviceStates::default(),
            memory_size,
            tag: None,
            parent_tag: None,
            chain_depth: 0,
            snapshot_type: "incremental".to_string(),
        };
        (snap, dirty_data)
    }

    #[test]
    fn apply_diff_writes_only_dirty_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_size = PAGE_SIZE * 16; // 16 pages
        let dirty_indices = vec![0u64, 3, 7, 15];
        let (inc, _) = make_incremental(tmp.path(), memory_size, &dirty_indices);

        // Base memory: all zeros.
        let mut base = vec![0u8; memory_size as usize];
        apply_diff(&mut base, &inc).unwrap();

        // Dirty pages should now contain their fill pattern.
        for &idx in &dirty_indices {
            let off = (idx * PAGE_SIZE) as usize;
            let fill = (idx % 256) as u8;
            assert_eq!(base[off], fill, "page {idx} byte 0 mismatch");
            assert_eq!(base[off + PAGE_SIZE as usize - 1], fill);
        }

        // Non-dirty pages should be untouched.
        for idx in 0u64..16 {
            if dirty_indices.contains(&idx) {
                continue;
            }
            let off = (idx * PAGE_SIZE) as usize;
            assert_eq!(base[off], 0, "non-dirty page {idx} unexpectedly modified");
        }
    }

    #[test]
    fn walk_chain_root_only_returns_single_node() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a root full template with no parent.
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        let memory_size = PAGE_SIZE * 4;
        let memory: Vec<u8> = vec![0xAA; memory_size as usize];
        let mem_file = root_dir.join("memory.raw");
        std::fs::write(&mem_file, &memory).unwrap();

        use crate::boot::measured::compute_sha256;
        let hash: String = compute_sha256(&memory).iter().map(|b| format!("{b:02x}")).collect();
        let snap = TemplateSnapshot {
            memory_file: mem_file,
            vcpu_states: Vec::new(),
            device_states: DeviceStates::default(),
            memory_size,
            runtime_type: "bare".to_string(),
            memory_hash: hash,
            block_device: None,
            clock_ns: 0,
            overlay_path: None,
            guest_ip: None,
            tag: None,
            parent_tag: None,
            chain_depth: 0,
            snapshot_type: "full".to_string(),
        };
        snap.save_metadata(root_dir.to_str().unwrap()).unwrap();

        let chain = walk_chain(root_dir.to_str().unwrap()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].snapshot_type, "full");
    }

    #[test]
    fn resolve_chain_applies_diffs_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_size = PAGE_SIZE * 8;

        // 1. Create root full snapshot.
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        let root_mem: Vec<u8> = vec![0u8; memory_size as usize];
        let mem_file = root_dir.join("memory.raw");
        std::fs::write(&mem_file, &root_mem).unwrap();

        use crate::boot::measured::compute_sha256;
        let hash: String = compute_sha256(&root_mem).iter().map(|b| format!("{b:02x}")).collect();
        let root_snap = TemplateSnapshot {
            memory_file: mem_file,
            vcpu_states: Vec::new(),
            device_states: DeviceStates::default(),
            memory_size,
            runtime_type: "bare".to_string(),
            memory_hash: hash,
            block_device: None,
            clock_ns: 0,
            overlay_path: None,
            guest_ip: None,
            tag: Some("root".to_string()),
            parent_tag: None,
            chain_depth: 0,
            snapshot_type: "full".to_string(),
        };
        root_snap.save_metadata(root_dir.to_str().unwrap()).unwrap();

        // 2. Create an incremental on top.
        let inc_dir = tmp.path().join("inc1");
        let (mut inc, _) = make_incremental(&inc_dir, memory_size, &[1, 4]);
        // parent_tag points at the parent directory (resolved by walk_chain).
        inc.parent_tag = Some(root_dir.to_string_lossy().to_string());
        inc.base_template = root_dir.to_string_lossy().to_string();
        inc.tag = Some("inc1".to_string());
        inc.chain_depth = 1;
        inc.save_metadata(inc_dir.to_str().unwrap()).unwrap();

        // 3. Resolve the chain → should return memory with pages 1 and 4 patched.
        let (resolved_mem, _, _) = resolve_chain(inc_dir.to_str().unwrap()).unwrap();
        // Page 1 should contain fill pattern 1.
        assert_eq!(resolved_mem[PAGE_SIZE as usize], 1);
        // Page 4 should contain fill pattern 4.
        assert_eq!(resolved_mem[(4 * PAGE_SIZE) as usize], 4);
        // Page 0 should be untouched (0).
        assert_eq!(resolved_mem[0], 0);
    }
}
