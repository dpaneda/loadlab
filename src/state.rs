use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::{
    error::LoadlabError,
    setting::{Dimension, Setting},
};

/// Configuration for the shared memory and copy-on-write controller.
///
/// Every field defaults to generating nothing, for the same reason as [`FileBackedConfig`]: a
/// default that spawned processes would make any partial `PATCH` that never mentions `shared`
/// fork a process tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedConfig {
    /// Size of the segment every actor maps `MAP_SHARED`, as a number or expression in bytes.
    ///
    /// This is one copy of physical memory regardless of `actors`. Every actor mapping it adds a
    /// full copy to that actor's RSS and only a `1/actors` share to its PSS, which is the
    /// distinction this controller exists to expose.
    pub shared_bytes: Option<Setting>,
    /// Size of the segment each actor maps `MAP_PRIVATE` and writes to, a number or expression in
    /// bytes.
    ///
    /// Unlike `shared_bytes` this is per actor, so the physical cost is
    /// `private_bytes * actors * cow_write_ratio`.
    pub private_bytes: Option<Setting>,
    /// Kernel object backing the shared segment: `memfd` or `sysv`.
    ///
    /// Both are accounted as `RssShmem`. They differ in the kernel mechanism involved, and `sysv`
    /// is additionally visible in `/proc/sysvipc/shm`.
    pub backing: String,
    /// How much of each private segment to write, from 0 to 1, as a number or expression.
    ///
    /// This is the copy-on-write knob. A write is what makes the kernel allocate a private page
    /// for that actor, so `0` produces a mapping that costs a page table and nothing else, and
    /// `1` produces a full private copy of the segment.
    pub cow_write_ratio: Setting,
    /// Number of actors, including the loadlab process itself.
    ///
    /// One actor is a shared segment nothing else maps, which is a valid workload and shows no
    /// double counting. Two or more is where `sum(Rss)` and `sum(Pss)` part company. Each actor
    /// beyond the first is a real process.
    pub actors: u32,
}

impl Default for SharedConfig {
    fn default() -> Self {
        Self {
            shared_bytes: None,
            private_bytes: None,
            backing: crate::shared::SharedBacking::default().as_str().to_owned(),
            cow_write_ratio: Setting::constant(0.0),
            actors: 1,
        }
    }
}

impl SharedConfig {
    /// Validate the configuration.
    pub fn validate(&self, max_memory_bytes: u64) -> Result<(), LoadlabError> {
        if crate::shared::SharedBacking::parse(&self.backing).is_none() {
            return Err(LoadlabError::InvalidConfig(format!(
                "shared.backing {:?} is not one of {}",
                self.backing,
                crate::shared::SharedBacking::ALL
                    .map(crate::shared::SharedBacking::as_str)
                    .join(", ")
            )));
        }
        let cow_write_ratio = self
            .cow_write_ratio
            .validate(Dimension::Unitless, "shared.cow_write_ratio")?;
        if cow_write_ratio > 1.0 {
            return Err(LoadlabError::InvalidConfig(format!(
                "shared.cow_write_ratio {cow_write_ratio} must be between 0 and 1 inclusive"
            )));
        }
        // `NaN` fails the range check above, so it never reaches here.
        if self.actors < 1 {
            return Err(LoadlabError::InvalidConfig(format!(
                "shared.actors {} must be at least 1, since the loadlab process is itself an \
                 actor",
                self.actors
            )));
        }
        if self.actors > crate::shared::MAX_ACTORS {
            return Err(LoadlabError::InvalidConfig(format!(
                "shared.actors {} exceeds the maximum of {}",
                self.actors,
                crate::shared::MAX_ACTORS
            )));
        }
        // Both segments are checked against the same ceiling as anonymous memory. The controller
        // clamps the *combined* footprint separately, because `private_bytes * actors` can exceed
        // any single field's limit, but a single field above the limit is refused here rather than
        // silently clamped: it is a request that can never be satisfied.
        for (field, setting) in [
            ("shared.shared_bytes", &self.shared_bytes),
            ("shared.private_bytes", &self.private_bytes),
        ] {
            let Some(setting) = setting else {
                continue;
            };
            let value = setting.validate(Dimension::Bytes, field)?;
            if value > max_memory_bytes as f64 {
                return Err(LoadlabError::InvalidConfig(format!(
                    "{field} at t=0 exceeds max_memory_bytes ({max_memory_bytes})"
                )));
            }
        }
        Ok(())
    }

    /// The backing to build the segment on.
    ///
    /// Falls back to the default rather than erroring, because `validate` has already rejected an
    /// unknown name and this is only reached on a state that has not been validated.
    #[must_use]
    pub fn shared_backing(&self) -> crate::shared::SharedBacking {
        crate::shared::SharedBacking::parse(&self.backing).unwrap_or_default()
    }
}

/// One actor's share of the memory, as the kernel accounts it.
///
/// Per-actor rather than a single aggregate, because an aggregate over several processes cannot
/// answer "who is holding this", and the per-actor figures are what make a shared/private
/// difference legible.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ActorRuntime {
    /// Process id, or 0 for the loadlab process itself.
    pub pid: u32,
    /// Whether this entry is the loadlab process rather than a forked actor.
    pub is_controller: bool,
    /// Resident set size: every page this actor has mapped, shared pages included.
    pub rss_bytes: u64,
    /// Proportional set size: shared pages divided among the actors mapping them.
    ///
    /// Summing this across actors gives the physical memory the set holds, which is the whole
    /// reason it is reported next to `rss_bytes`.
    pub pss_bytes: u64,
    /// The `RssShmem` component: this actor's view of the shared segment.
    pub rss_shmem_bytes: u64,
    /// The `RssAnon` component, which grows as this actor's copy-on-write pages are written.
    pub rss_anon_bytes: u64,
    /// Minor faults in this actor. Copy-on-write writes arrive as minor faults.
    pub minor_faults: u64,
}

/// What the shared memory and copy-on-write controller is currently doing.
///
/// `accounted_bytes` and `physical_bytes` are sums over the actors and they are *expected* to
/// differ. A shared segment mapped by four actors is one copy of physical memory that `sum(Rss)`
/// counts four times, and `shared_overcount_bytes` is that difference reported rather than left
/// for the reader to infer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SharedRuntime {
    /// The kernel object backing the segment, or empty when none exists.
    pub backing: String,
    /// Size of the shared segment, fixed at creation.
    pub shared_bytes: u64,
    /// Size of each actor's private segment.
    pub private_bytes_per_actor: u64,
    /// Configured fraction of each private segment to write.
    pub cow_write_ratio: f64,
    /// Live actors, including the loadlab process.
    pub actors: u32,
    /// Live forked actors.
    pub child_actors: u32,
    /// Physical pages of the shared segment, counted once.
    pub shared_physical_bytes: u64,
    /// `sum(RssShmem)` over every actor: the shared segment counted once per mapper.
    ///
    /// Reported next to `shared_physical_bytes` because the two are individually meaningful, and
    /// their difference is what `shared_overcount_bytes` makes explicit.
    pub shared_rss_bytes: u64,
    /// Physical pages of copy-on-write across all actors.
    pub cow_physical_bytes: u64,
    /// `sum(Rss)` over every actor. What naive accounting reports.
    pub accounted_bytes: u64,
    /// `sum(Pss)` over every actor. What the memory actually costs.
    pub physical_bytes: u64,
    /// Shared pages counted once per mapper rather than once in total.
    ///
    /// Narrower than `accounted_bytes - physical_bytes`, which also contains every other shared
    /// page the actors have in common, such as the executable's text.
    pub shared_overcount_bytes: u64,
    /// Copy-on-write bytes requested this tick, before clamping to the operator ceiling.
    pub requested_cow_bytes: u64,
    /// Copy-on-write bytes the controller was held to this tick.
    pub target_cow_bytes: u64,
    /// Whether the observed copy-on-write bytes are within tolerance of the target.
    pub converged: bool,
    /// Whether the request exceeded the operator ceiling.
    pub saturated: bool,
    /// Per-actor accounting. Empty when no actor exists.
    ///
    /// Named for what it is rather than `actors`, because [`Self::actors`] is the count and a
    /// caller that reaches for the wrong one gets a type error instead of the wrong number.
    pub actor_accounting: Vec<ActorRuntime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkloadState {
    pub running: bool,
    pub controller: ControllerConfig,
    pub memory: MemoryConfig,
    pub cpu: CpuConfig,
    pub memory_access: MemoryAccessConfig,
    pub file_backed: FileBackedConfig,
    pub shared: SharedConfig,
    pub io: IoConfig,
    pub network: NetworkConfig,
}

impl Default for WorkloadState {
    fn default() -> Self {
        Self {
            running: false,
            controller: ControllerConfig::default(),
            memory: MemoryConfig::default(),
            cpu: CpuConfig::default(),
            memory_access: MemoryAccessConfig::default(),
            file_backed: FileBackedConfig::default(),
            shared: SharedConfig::default(),
            io: IoConfig::default(),
            network: NetworkConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerConfig {
    pub tick_ms: u64,
    pub tolerance_bytes: u64,
    pub max_memory_bytes: u64,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            tick_ms: 100,
            tolerance_bytes: 2 * 1024 * 1024,
            max_memory_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub anonymous_rss: Option<Setting>,
    /// Transparent-huge-page policy for the anonymous arena: `default`, `always`, `madvise`, or
    /// `never`.
    ///
    /// Kept as a string for the same reason as `file_backed.madvise` and `shared.backing`: the
    /// wire name and the parsed enum come from one list, so they cannot drift. The policy is a
    /// request, not a guarantee -- a process cannot enable THP on a host whose sysfs setting is
    /// `never` -- and the effective result is reported as `AnonHugePages`.
    pub thp: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            anonymous_rss: None,
            thp: crate::memory::ThpPolicy::default().as_str().to_owned(),
        }
    }
}

impl MemoryConfig {
    /// The transparent-huge-page policy, or an error naming the valid values.
    pub fn thp_policy(&self) -> Result<crate::memory::ThpPolicy, LoadlabError> {
        crate::memory::ThpPolicy::parse(&self.thp).ok_or_else(|| {
            LoadlabError::InvalidConfig(format!(
                "memory.thp {:?} is not one of {}",
                self.thp,
                crate::memory::ThpPolicy::ALL
                    .map(crate::memory::ThpPolicy::as_str)
                    .join(", ")
            ))
        })
    }
}

/// CPU capacity configuration.
///
/// `utilization` is a plain number or an expression in `t`, e.g. `150` or
/// `"100 + 50*sin(t/10)"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CpuConfig {
    /// Utilization target as an aggregate percentage of total logical CPUs.
    /// 100% = one full CPU, 200% = two, and so on.
    pub utilization: Setting,
    /// Number of dedicated worker threads. If zero, use one per selected logical CPU.
    pub workers: usize,
    /// Logical CPU indices the pool may run on. Empty means every logical CPU.
    pub cpus: Vec<usize>,
    /// Where workers run: "auto" leaves placement to the scheduler, "pin" binds worker
    /// `i` to `cpus[i % cpus.len()]`. Pinning makes utilization reproducible, which is
    /// what a replay needs; leaving it on "auto" is what you want when the point
    /// is to consume capacity without caring where it is consumed.
    pub affinity: String,
}

/// Page-access patterns
///
/// The wire names are the snake_case ones the docs publish, and they come from [`Self::as_str`]
/// rather than from a derived serde representation. That makes `as_str`/`parse` the single source
/// of truth: a pattern cannot be spelled one way in the API and another in the documentation,
/// because there is only one list of names and both directions read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessPattern {
    #[default]
    Sequential,
    Random,
    Stride,
    WorkingSet,
    HotCold,
    ColdScan,
}

impl AccessPattern {
    /// The configuration name of this pattern.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Random => "random",
            Self::Stride => "stride",
            Self::WorkingSet => "working_set",
            Self::HotCold => "hot_cold",
            Self::ColdScan => "cold_scan",
        }
    }

    /// Every pattern, in documentation order. Used where the set has to be enumerated rather
    /// than a single one chosen.
    pub const ALL: [Self; 6] = [
        Self::Sequential,
        Self::Random,
        Self::Stride,
        Self::WorkingSet,
        Self::HotCold,
        Self::ColdScan,
    ];

    /// Parse a pattern name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|pattern| pattern.as_str() == value)
    }
}

impl Serialize for AccessPattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AccessPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown access pattern {raw:?}; expected one of {}",
                Self::ALL.map(Self::as_str).join(", ")
            ))
        })
    }
}

/// Configuration for the memory-access controller.
///
/// Every field defaults rather than being required, like every other controller's configuration.
/// A `PATCH` that names one field of a nested object must not have to spell out the rest of it:
/// requiring `pattern` and `random_seed` to go along with `pages_per_second` would make the
/// fault rate's canonical spelling unusable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryAccessConfig {
    /// The access pattern to use
    pub pattern: AccessPattern,
    /// Target page fault rate (minor faults per second), as a number or expression.
    pub pages_per_second: Setting,
    /// For working_set and hot_cold patterns: size of the working set
    pub working_set_pages: Option<usize>,
    /// For stride pattern: stride distance in pages
    pub stride_pages: Option<usize>,
    /// For random pattern: seed for reproducible randomness
    pub random_seed: Option<u64>,
}

impl Default for MemoryAccessConfig {
    fn default() -> Self {
        Self {
            pattern: AccessPattern::Sequential,
            pages_per_second: Setting::constant(0.0),
            working_set_pages: None,
            stride_pages: None,
            random_seed: None,
        }
    }
}

impl MemoryAccessConfig {
    /// Validate the configuration
    pub fn validate(&self) -> Result<(), LoadlabError> {
        self.pages_per_second
            .validate(Dimension::Unitless, "memory_access.pages_per_second")?;

        match self.pattern {
            AccessPattern::WorkingSet => {
                if self.working_set_pages.is_none() {
                    return Err(LoadlabError::InvalidConfig(
                        "memory_access.working_set_pages must be set for WorkingSet pattern"
                            .to_owned(),
                    ));
                }
            }
            AccessPattern::Stride => {
                if self.stride_pages.is_none() {
                    return Err(LoadlabError::InvalidConfig(
                        "memory_access.stride_pages must be set for Stride pattern".to_owned(),
                    ));
                }
            }
            AccessPattern::Random => {
                if self.random_seed.is_none() {
                    return Err(LoadlabError::InvalidConfig(
                        "memory_access.random_seed must be set for Random pattern".to_owned(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Configuration for the file-backed memory controller.
///
/// Every field defaults to generating nothing, because `WorkloadState` is `#[serde(default)]` and
/// a default that allocated a file would make any partial `PATCH` that never mentions
/// `file_backed` start writing to disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileBackedConfig {
    /// Target file-backed resident memory, as a number or expression in bytes. None means none.
    ///
    /// This is the *resident* size of loadlab's own mapping, not the file length. The file is
    /// always at least as large as the mapping and is never truncated below it, because
    /// truncating a mapped file out from under the mapping raises `SIGBUS` on the next access.
    pub rss: Option<Setting>,
    /// Target page-fault rate, in faults per second, as a number or expression. Zero means none.
    pub pages_per_second: Setting,
    /// Which pages the access pattern walks.
    pub access_pattern: AccessPattern,
    /// Readahead policy applied to the whole mapping: `random`, `sequential`, or `normal`.
    ///
    /// `random` is the default because it is the only setting under which a page access is
    /// guaranteed to read that page and nothing else. Under the other two the kernel reads ahead,
    /// so most of the faults a pattern would have produced arrive as one large fault instead, and
    /// the reported rate is no longer comparable between runs.
    pub madvise: String,
    /// For working_set and hot_cold: size of the working set, in pages.
    pub working_set_pages: Option<usize>,
    /// For stride: distance between accesses, in pages.
    pub stride_pages: Option<usize>,
    /// For random: seed, so a replay walks the same pages.
    pub random_seed: Option<u64>,
}

impl Default for FileBackedConfig {
    fn default() -> Self {
        Self {
            rss: None,
            pages_per_second: Setting::constant(0.0),
            access_pattern: AccessPattern::Sequential,
            madvise: String::from("random"),
            working_set_pages: None,
            stride_pages: None,
            random_seed: None,
        }
    }
}

impl FileBackedConfig {
    /// Validate the configuration.
    pub fn validate(&self, max_memory_bytes: u64) -> Result<(), LoadlabError> {
        self.pages_per_second
            .validate(Dimension::Unitless, "file_backed.pages_per_second")?;
        if crate::file_backed::MadvisePolicy::parse(&self.madvise).is_none() {
            return Err(LoadlabError::InvalidConfig(format!(
                "file_backed.madvise {:?} is not one of {}",
                self.madvise,
                crate::file_backed::MadvisePolicy::ALL
                    .map(crate::file_backed::MadvisePolicy::as_str)
                    .join(", ")
            )));
        }
        if self.working_set_pages == Some(0) {
            return Err(LoadlabError::InvalidConfig(
                "file_backed.working_set_pages must be greater than zero".to_owned(),
            ));
        }
        if self.stride_pages == Some(0) {
            return Err(LoadlabError::InvalidConfig(
                "file_backed.stride_pages must be greater than zero".to_owned(),
            ));
        }
        if let Some(setting) = &self.rss {
            let value = setting.validate(Dimension::Bytes, "file_backed.rss")?;
            // Checked against the same ceiling as anonymous memory rather than a separate one:
            // the two mappings compete for the page cache and for RSS, so a file-backed target
            // allowed past the operator's ceiling would defeat the ceiling.
            if value > max_memory_bytes as f64 {
                return Err(LoadlabError::InvalidConfig(format!(
                    "file_backed.rss at t=0 exceeds max_memory_bytes ({max_memory_bytes})"
                )));
            }
        }
        Ok(())
    }

    /// The readahead policy, or an error naming the valid values.
    pub fn madvise_policy(&self) -> Result<crate::file_backed::MadvisePolicy, LoadlabError> {
        crate::file_backed::MadvisePolicy::parse(&self.madvise).ok_or_else(|| {
            LoadlabError::InvalidConfig(format!(
                "file_backed.madvise {:?} is not one of {}",
                self.madvise,
                crate::file_backed::MadvisePolicy::ALL
                    .map(crate::file_backed::MadvisePolicy::as_str)
                    .join(", ")
            ))
        })
    }
}

/// Disk I/O generation: read and write throughput, operation rate, and durability, over a
/// temporary file loadlab owns.
///
/// Every field defaults to generating nothing, because `WorkloadState` is `#[serde(default)]` and
/// a default that created a file or moved bytes would make any partial `PATCH` that never mentions
/// `io` start a disk workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IoConfig {
    /// Target read throughput, in bytes per second, as a number or expression. Zero means generate
    /// no reads.
    pub read_bps: Setting,
    /// Target write throughput, in bytes per second, as a number or expression. Zero means generate
    /// no writes.
    pub write_bps: Setting,
    /// Cap on total read plus write operations per second, as a number or expression. Zero means
    /// uncapped.
    ///
    /// When both byte targets are zero and this is non-zero, the operation budget is split evenly
    /// between reads and writes. Otherwise it is a ceiling on top of the byte targets.
    pub iops: Setting,
    /// How many `fsync`/`fdatasync` calls to issue per second, as a number or expression. Zero
    /// means none.
    ///
    /// Independent of read and write: durability is a separate cost, and a low rate is honoured
    /// by a credit accumulator rather than rounding to nothing.
    pub fsync_rate: Setting,
    /// Block size for every operation, in bytes. Also the granularity of the region.
    pub block_size: usize,
    /// Maximum in-flight operations per tick. One means strictly serial.
    pub queue_depth: usize,
    /// Which blocks the walk visits: `sequential`, `random`, or `stride`.
    pub pattern: String,
    /// For stride: distance between blocks.
    pub stride_blocks: Option<u64>,
    /// For random: seed, so a replay walks the same blocks.
    pub random_seed: Option<u64>,
    /// Open the file with `O_DIRECT`, so the I/O bypasses the page cache and reaches the device.
    ///
    /// Off by default, because the buffered default is what a page-cache-oriented workload wants
    /// and turning this on makes throughput dramatically lower and more variable. It requires
    /// `block_size` to be a multiple of 4096, and a filesystem that supports `O_DIRECT`; if either
    /// is missing, the controller reports the error and generates nothing rather than quietly
    /// issuing buffered I/O under a `direct: true` label.
    #[serde(default)]
    pub direct: bool,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            read_bps: Setting::constant(0.0),
            write_bps: Setting::constant(0.0),
            iops: Setting::constant(0.0),
            fsync_rate: Setting::constant(0.0),
            block_size: 4096,
            queue_depth: 1,
            pattern: crate::io::IoPattern::default().as_str().to_owned(),
            stride_blocks: None,
            random_seed: None,
            direct: false,
        }
    }
}

impl IoConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), LoadlabError> {
        for (field, setting) in [
            ("io.read_bps", &self.read_bps),
            ("io.write_bps", &self.write_bps),
        ] {
            setting.validate(Dimension::Bytes, field)?;
        }
        for (field, setting) in [("io.iops", &self.iops), ("io.fsync_rate", &self.fsync_rate)] {
            setting.validate(Dimension::Unitless, field)?;
        }
        if self.block_size == 0 || self.block_size > 16 * 1024 * 1024 {
            return Err(LoadlabError::InvalidConfig(
                "io.block_size must be between 1 and 16777216 bytes".to_owned(),
            ));
        }
        if self.queue_depth == 0 || self.queue_depth > 64 {
            return Err(LoadlabError::InvalidConfig(
                "io.queue_depth must be between 1 and 64".to_owned(),
            ));
        }
        // Rejected here rather than at the syscall, because the alternative is a controller that
        // reports success while every operation returns EINVAL. The default is already 4096, so
        // this only fires on a configuration that deliberately asked for something else.
        if self.direct && self.block_size % crate::io::DIRECT_ALIGNMENT != 0 {
            return Err(LoadlabError::InvalidConfig(format!(
                "io.block_size of {} bytes must be a multiple of {} when io.direct is true",
                self.block_size,
                crate::io::DIRECT_ALIGNMENT
            )));
        }
        if crate::io::IoPattern::parse(&self.pattern).is_none() {
            return Err(LoadlabError::InvalidConfig(format!(
                "io.pattern {:?} is not one of {}",
                self.pattern,
                crate::io::IoPattern::ALL
                    .map(crate::io::IoPattern::as_str)
                    .join(", ")
            )));
        }
        if self.stride_blocks == Some(0) {
            return Err(LoadlabError::InvalidConfig(
                "io.stride_blocks must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }

    /// The walk pattern, or an error naming the valid values.
    pub fn pattern(&self) -> Result<crate::io::IoPattern, LoadlabError> {
        crate::io::IoPattern::parse(&self.pattern).ok_or_else(|| {
            LoadlabError::InvalidConfig(format!(
                "io.pattern {:?} is not one of {}",
                self.pattern,
                crate::io::IoPattern::ALL
                    .map(crate::io::IoPattern::as_str)
                    .join(", ")
            ))
        })
    }

    /// The effective per-tick settings, with every target zeroed while the workload is stopped.
    ///
    /// The four rates are passed in already evaluated rather than read from the config, because any
    /// of them may be an expression in `t`: the controller is the only place that knows `t` and
    /// holds the per-expression cache, so it is the one that can turn a target into a number here.
    #[must_use]
    pub fn settings(
        &self,
        running: bool,
        read_bps: f64,
        write_bps: f64,
        iops: f64,
        fsync_rate: f64,
    ) -> crate::io::IoSettings {
        let active = |value: f64| if running { value } else { 0.0 };
        crate::io::IoSettings {
            read_bps: active(read_bps),
            write_bps: active(write_bps),
            iops: active(iops),
            fsync_rate: active(fsync_rate),
            block_size: self.block_size,
            queue_depth: self.queue_depth,
            pattern: self.pattern().unwrap_or_default(),
            stride_blocks: self.stride_blocks.unwrap_or(1),
            random_seed: self.random_seed,
            direct: self.direct,
        }
    }
}

/// Configuration for the TCP/UDP traffic generator.
///
/// Every field defaults to generating nothing, for the same reason as [`IoConfig`]: a default with
/// a live target would make any partial `PATCH` that never mentions `network` start sending traffic
/// somewhere. The target is a `host:port` string; empty (the default) means the controller does not
/// generate at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Transport to generate: `tcp` or `udp`.
    pub transport: String,
    /// Destination as `host:port`. Empty means generate nothing.
    ///
    /// The controller resolves this once per change and never opens a socket while it is empty, so
    /// the safe default is an absence rather than a destination.
    pub target: String,
    /// Worker slots, and so concurrent connections (TCP) or sockets (UDP).
    pub concurrency: usize,
    /// Fresh TCP connections to open per second. Ignored for UDP.
    ///
    /// A finite, non-negative number or an expression in `t`, so the rate can follow a moving
    /// setpoint. Connection churn is a separate rate from message throughput because it exercises a
    /// different part of the stack: the handshake and accept path rather than the data path.
    pub connections_per_second: Setting,
    /// Messages to send per second, each `message_size` bytes.
    ///
    /// A finite, non-negative number or an expression in `t`. On TCP a message is one write over a
    /// pool of `concurrency` long-lived connections; on UDP it is one datagram. Fresh connections
    /// opened for `connections_per_second` contribute their bytes to the byte totals but are not
    /// counted as messages.
    pub messages_per_second: Setting,
    /// Bytes per message. Also the size of every fresh connection's single write.
    pub message_size: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            transport: crate::network::Transport::default().as_str().to_owned(),
            target: String::new(),
            concurrency: 1,
            connections_per_second: Setting::constant(0.0),
            messages_per_second: Setting::constant(0.0),
            message_size: 1024,
        }
    }
}

impl NetworkConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), LoadlabError> {
        self.transport()?;
        self.connections_per_second
            .validate(Dimension::Unitless, "network.connections_per_second")?;
        self.messages_per_second
            .validate(Dimension::Unitless, "network.messages_per_second")?;
        if self.concurrency == 0 || self.concurrency > crate::network::MAX_CONCURRENCY {
            return Err(LoadlabError::InvalidConfig(format!(
                "network.concurrency must be between 1 and {}",
                crate::network::MAX_CONCURRENCY
            )));
        }
        if self.message_size == 0 || self.message_size > crate::network::MAX_MESSAGE_SIZE {
            return Err(LoadlabError::InvalidConfig(format!(
                "network.message_size must be between 1 and {} bytes",
                crate::network::MAX_MESSAGE_SIZE
            )));
        }
        // A non-empty target has to be a `host:port`, not just a host: the controller resolves it
        // and silently doing nothing because a port was omitted would look like a converged
        // workload. The check is syntactic (a trailing `:port`); name resolution happens on the
        // controller thread so a slow resolver is not held under the state lock.
        let target = self.target.trim();
        let has_port = target
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
        if !target.is_empty() && !has_port {
            return Err(LoadlabError::InvalidConfig(format!(
                "network.target {target:?} must be a host:port pair"
            )));
        }
        Ok(())
    }

    /// The transport, or an error naming the valid values.
    pub fn transport(&self) -> Result<crate::network::Transport, LoadlabError> {
        crate::network::Transport::parse(&self.transport).ok_or_else(|| {
            LoadlabError::InvalidConfig(format!(
                "network.transport {:?} is not one of {}",
                self.transport,
                crate::network::Transport::ALL
                    .map(crate::network::Transport::as_str)
                    .join(", ")
            ))
        })
    }

    /// The effective per-tick settings, with every target zeroed while the workload is stopped.
    ///
    /// The two rates are passed in already evaluated rather than read from the config, because
    /// either may be an expression in `t`: the controller is the only place that knows `t` and
    /// holds the per-expression cache, so it is the one that can turn a target into a number here.
    #[must_use]
    pub fn settings(
        &self,
        running: bool,
        connections_per_second: f64,
        messages_per_second: f64,
    ) -> crate::network::NetworkSettings {
        let active = |value: f64| if running { value } else { 0.0 };
        let target = self.target.trim();
        crate::network::NetworkSettings {
            transport: self.transport().unwrap_or_default(),
            target: if running && !target.is_empty() {
                Some(target.to_owned())
            } else {
                None
            },
            concurrency: self.concurrency,
            connections_per_second: active(connections_per_second),
            messages_per_second: active(messages_per_second),
            message_size: self.message_size,
        }
    }
}

impl Default for CpuConfig {
    /// A quiet workload: the CPU target is zero, not something busy.
    ///
    /// `WorkloadState` is `#[serde(default)]`, so this is also what a `PATCH` that never
    /// mentions CPU restores. A default that burned a core would mean any partial update
    /// silently started generating CPU load.
    fn default() -> Self {
        Self {
            utilization: Setting::constant(0.0),
            workers: 0,
            cpus: Vec::new(),
            affinity: "auto".to_string(),
        }
    }
}

impl WorkloadState {
    pub fn validate(&self) -> Result<(), LoadlabError> {
        if !(1..=10_000).contains(&self.controller.tick_ms) {
            return Err(LoadlabError::InvalidConfig(
                "controller.tick_ms must be between 1 and 10000".to_owned(),
            ));
        }
        if self.controller.tolerance_bytes == 0 {
            return Err(LoadlabError::InvalidConfig(
                "controller.tolerance_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.controller.max_memory_bytes == 0 {
            return Err(LoadlabError::InvalidConfig(
                "controller.max_memory_bytes must be greater than zero".to_owned(),
            ));
        }
        // The controller narrows the arena target to `usize` before handing it to the mmap arena.
        // On a 32-bit target that would silently truncate, so a ceiling the platform cannot
        // represent is rejected at configuration time instead of reported as a target that no
        // allocation ever reaches. This is a no-op on 64-bit, where `usize::MAX == u64::MAX`.
        if self.controller.max_memory_bytes > usize::MAX as u64 {
            return Err(LoadlabError::InvalidConfig(format!(
                "controller.max_memory_bytes {} exceeds the addressable limit of this platform ({})",
                self.controller.max_memory_bytes,
                usize::MAX
            )));
        }

        if let Some(setting) = &self.memory.anonymous_rss {
            let value = setting.validate(Dimension::Bytes, "memory.anonymous_rss")?;
            if value > self.controller.max_memory_bytes as f64 {
                return Err(LoadlabError::InvalidConfig(format!(
                    "memory.anonymous_rss at t=0 exceeds max_memory_bytes ({})",
                    self.controller.max_memory_bytes
                )));
            }
        }

        self.memory.thp_policy()?;

        self.cpu
            .utilization
            .validate(Dimension::Unitless, "cpu.utilization")?;
        // A target above the machine's whole capacity is not an error -- an expression that
        // peaks there is a legitimate way to ask for "as much as you can do" -- but it can
        // never be reached, so the controller clamps to capacity and reports it saturated.
        // Rejecting it would make a sinusoidal workload that briefly overshoots unusable.

        crate::cpu::validate(self.cpu.workers, &self.cpu.cpus, &self.cpu.affinity)?;
        self.memory_access.validate()?;
        self.file_backed
            .validate(self.controller.max_memory_bytes)?;
        self.shared.validate(self.controller.max_memory_bytes)?;
        self.io.validate()?;
        self.network.validate()?;

        Ok(())
    }
}

#[derive(Debug)]
pub struct RuntimeStatus {
    /// When the *process* started, which is what `elapsed_seconds` reports.
    ///
    /// Deliberately not the same thing as [`Self::run_clock`]: uptime is a property of the process,
    /// and the run clock is a property of the current run. They are different instants, and the run
    /// clock restarts while uptime keeps counting.
    pub started_at: Instant,
    /// The origin of `t` for the current run.
    pub run_clock: crate::clock::RunClock,
    pub last_tick_at: Option<Instant>,
    pub tick_count: u64,
    pub arena_resident_bytes: u64,
    pub baseline_rss_anon_bytes: u64,
    pub last_target_arena_bytes: u64,
    /// Signed gap between the last target and what the arena actually holds, in bytes.
    ///
    /// Positive means the arena is below target. Exposed because the interesting monitoring
    /// question is not "what memory is this process using" but "is it tracking what was asked".
    pub convergence_error_bytes: i64,
    /// Whether the last tick finished within `tolerance_bytes` of its target.
    pub converged: bool,
    /// Cumulative failed reconciliations.
    ///
    /// `last_error` alone is not enough to alert on: it is cleared again on the next successful
    /// tick, so a workload that flaps between succeeding and failing is invisible to a scraper
    /// that happens to poll during a good tick. This counter keeps the history.
    pub reconcile_errors_total: u64,
    pub last_error: Option<String>,
    /// CPU controller state.
    pub cpu: CpuRuntime,
    /// Memory access controller state.
    pub memory_access: MemoryAccessRuntime,
    /// The disk ceiling the operator set on the file-backed file, as actually applied.
    ///
    /// A process-level limit rather than desired state, so it does not live in `WorkloadState`
    /// and a client cannot change it. Published here so an alert can compare the file against the
    /// limit the operator chose instead of a constant written into a rule.
    pub file_backed_ceiling_bytes: u64,
    /// File-backed memory controller state.
    pub file_backed: FileBackedRuntime,
    /// The ceiling the operator set on shared plus copy-on-write memory, as applied.
    ///
    /// A process-level limit, like [`Self::file_backed_ceiling_bytes`]: it bounds the combined
    /// footprint of the shared segment and every actor's copy-on-write pages, which is the number
    /// that can grow multiplicatively and so cannot be inferred from any one requested field.
    pub shared_ceiling_bytes: u64,
    /// Shared memory and copy-on-write controller state.
    pub shared: SharedRuntime,
    /// The ceiling the operator set on the I/O region, as applied.
    ///
    /// A process-level limit, like [`Self::file_backed_ceiling_bytes`]: it bounds how large the
    /// temporary file may grow, which is a disk cost the client cannot be trusted to cap.
    pub io_ceiling_bytes: u64,
    /// Disk I/O controller state.
    pub io: IoRuntime,
    /// The operations-per-second ceiling the operator set on network traffic, as applied.
    ///
    /// A process-level limit, like [`Self::io_ceiling_bytes`]: generating traffic against an
    /// arbitrary destination is the one thing here that can affect systems outside this host, so
    /// the operator caps it and a client cannot lift it.
    pub network_ceiling_ops_per_second: u64,
    /// TCP/UDP generator state.
    pub network: NetworkRuntime,
    /// Transparent-huge-page policy and the host setting it was requested against.
    pub thp: ThpRuntime,
}

/// What the transparent-huge-page policy is doing.
///
/// `requested` and `host` are separate because they answer different questions: what the client
/// asked for, and what the machine will allow. A request of `always` on a host set to `never`
/// produces no huge pages, and without both values that misconfiguration is invisible. The
/// effective outcome is `process.memory.anon_huge_pages_bytes`, which is the only honest answer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ThpRuntime {
    /// Policy requested by `memory.thp`.
    pub requested: String,
    /// The host's `/sys/kernel/mm/transparent_hugepage/enabled` value, or `unavailable`.
    pub host: String,
    /// Whether the host permits huge pages at all (`always` or `madvise`).
    pub host_enabled: bool,
}

/// What the CPU controller is currently doing.
///
/// The requested/target pair exists because they are not the same number: a request above
/// what the pool can deliver is clamped, and without both values a target of 400% on a
/// 4-worker pool would be indistinguishable from a request that genuinely asked for 400%
/// and got it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuRuntime {
    /// Worker threads currently running.
    pub worker_count: usize,
    /// Aggregate percent the pool can deliver with every worker saturated.
    pub capacity_pct: f64,
    /// What the expression asked for this tick, before clamping.
    pub requested_utilization_pct: f64,
    /// What the pool was actually told to deliver.
    pub target_utilization_pct: f64,
    /// Smoothed aggregate utilization of the whole process, as a percent of one logical
    /// CPU: 100 is one saturated core, and 100 times the core count is the whole machine.
    pub actual_utilization_pct: f64,
    /// Fraction of one worker's core the workers were last told to keep busy.
    pub duty_cycle: f64,
    /// Whether actual is within tolerance of target and the target was reachable.
    pub converged: bool,
    /// Whether the request was above what the pool can deliver.
    pub saturated: bool,
    /// Whether the kernel exposes per-thread scheduler statistics.
    ///
    /// When false, the run-queue figures below are zero because they could not be read, not
    /// because nothing waited. A dashboard needs to be able to tell those apart, or it will chart a
    /// flat line on a kernel that simply does not publish the counters.
    pub scheduling_available: bool,
    /// Nanoseconds the workers spent runnable but not running, summed.
    ///
    /// This is the missing input to `actual_utilization_pct`: a worker that waited for a CPU
    /// consumed none, so an under-target reading with a large wait here is a scheduler story and not
    /// a duty-cycle one.
    pub worker_runqueue_wait_nanoseconds: u64,
    /// The largest run-queue wait of any single worker, for the same reason.
    pub worker_worst_runqueue_wait_nanoseconds: u64,
}

/// What the memory-access controller is currently doing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MemoryAccessRuntime {
    pub pattern: AccessPattern,
    pub pages_per_second: f64,
    pub actual_pages_per_second: f64,
    pub minor_faults_total: u64,
    pub major_faults_total: u64,
    pub minor_faults_rate: f64,
    pub major_faults_rate: f64,
    /// Whether the page faults could be split between the controller thread and the rest.
    ///
    /// False when the controller thread could not be read, in which case the generated and
    /// incidental figures below are zero rather than a split that was never made.
    pub attribution_available: bool,
    /// Minor faults taken by the controller thread, which touches the pages on purpose.
    ///
    /// `minor_faults_total` is process-wide, so it also counts faults taken incidentally by the
    /// runtime, the allocator and the HTTP server. This is the part of it loadlab generated.
    pub generated_minor_faults: u64,
    /// Major faults taken by the controller thread.
    pub generated_major_faults: u64,
    /// Minor faults taken by every other thread.
    pub incidental_minor_faults: u64,
    /// Major faults taken by every other thread.
    pub incidental_major_faults: u64,
    pub converged: bool,
    pub saturated: bool,
}

/// What the file-backed memory controller is currently doing.
///
/// `rss_bytes` and `pages` describe loadlab's own mapping, measured with `mincore`. The
/// process-wide `RssFile` from procfs is reported separately as `process_rss_file_bytes`,
/// because it is a different and larger number: it includes the executable's own text and any
/// other file the process has mapped, none of which the controller is steering.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileBackedRuntime {
    /// Path of the temporary file loadlab created and owns. Empty until a file exists.
    pub path: String,
    /// File length in bytes. Always at least the mapping length, never truncated below it.
    pub file_bytes: u64,
    /// Resident pages of loadlab's mapping, from `mincore`.
    pub pages: u64,
    /// Those pages in bytes.
    pub rss_bytes: u64,
    /// What the `file_backed.rss` expression asked for this tick, before clamping.
    ///
    /// Reported separately from the effective target because a request above the operator's
    /// ceiling is a misconfiguration an operator needs to see, and clamping it silently would
    /// hide exactly the case worth noticing.
    pub requested_rss_bytes: u64,
    /// The target the controller was actually held to, after the ceiling.
    pub target_rss_bytes: u64,
    /// Process-wide file-backed RSS from procfs, for comparison with `rss_bytes`.
    pub process_rss_file_bytes: u64,
    pub access_pattern: AccessPattern,
    pub madvise: String,
    pub pages_per_second: f64,
    pub actual_pages_per_second: f64,
    pub minor_faults_rate: f64,
    pub major_faults_rate: f64,
    /// Whether the mapping is within `controller.tolerance_bytes` of its target.
    pub converged: bool,
    /// Whether the target is above what the mapping can hold at this tick rate.
    pub saturated: bool,
}

/// What the disk I/O controller is currently doing.
///
/// Every rate is reported twice: the requested value and what the filesystem actually delivered.
/// They are separate because the whole point of the controller is to expose the gap between them,
/// and a model that echoed the target would hide exactly that.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IoRuntime {
    /// Path of the temporary file loadlab created and owns. Empty until a file exists.
    pub path: String,
    /// The block-aligned size of the region being walked.
    pub region_bytes: u64,
    pub block_size_bytes: u64,
    /// Whether the file was opened with `O_DIRECT`, so the numbers above are device-bound rather
    /// than page-cache-bound.
    pub direct: bool,
    pub queue_depth: usize,
    /// Which blocks the walk visits.
    pub pattern: String,
    /// Requested read throughput, in bytes per second.
    pub read_bps: f64,
    /// Measured read throughput from the bytes the reads actually returned.
    pub read_bps_actual: f64,
    /// Requested write throughput, in bytes per second.
    pub write_bps: f64,
    /// Measured write throughput from the bytes the writes actually stored.
    pub write_bps_actual: f64,
    /// Requested total-operations cap. Zero means uncapped.
    pub iops: f64,
    /// Measured operations per second.
    pub iops_actual: f64,
    /// Requested `fsync` calls per second.
    pub fsync_rate: f64,
    /// Measured `fsync` calls per second.
    pub fsyncs_per_second: f64,
    /// Cumulative bytes returned by reads.
    pub read_bytes_total: u64,
    /// Cumulative bytes stored by writes.
    pub write_bytes_total: u64,
    /// Cumulative read plus write operations.
    pub ops_total: u64,
    /// Cumulative `fsync` calls.
    pub fsyncs_total: u64,
    /// Read latencies, since the controller started.
    pub read_latency: crate::io::LatencySummary,
    /// Write latencies, since the controller started.
    pub write_latency: crate::io::LatencySummary,
    /// `fsync` latencies, since the controller started.
    ///
    /// Separate from read and write because durability is a separate cost: on most devices an
    /// `fsync` is orders of magnitude slower than the `pwrite` it follows, and averaging the two
    /// would produce a number describing neither.
    pub fsync_latency: crate::io::LatencySummary,
    /// Whether every requested rate is within tolerance of what was measured.
    pub converged: bool,
    /// Whether an operation cap or the safety bound stopped the byte target being reached.
    pub saturated: bool,
}

/// What the TCP/UDP generator is currently doing.
///
/// Every rate is reported twice: the requested value and what the sockets actually delivered. They
/// are separate for the same reason as the I/O controller's: the whole point is to expose the gap
/// between an asked-for rate and the stack's real limit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkRuntime {
    /// `tcp` or `udp`.
    pub transport: String,
    /// The target currently applied, or empty when there is none.
    pub target: String,
    pub concurrency: usize,
    pub message_size_bytes: u64,
    /// Requested fresh TCP connections per second. Zero on UDP.
    pub connections_per_second: f64,
    /// Measured fresh connections per second.
    pub connections_per_second_actual: f64,
    /// Requested messages per second.
    pub messages_per_second: f64,
    /// Measured messages per second.
    pub messages_per_second_actual: f64,
    /// Measured bytes per second, including fresh connections' writes.
    pub bytes_per_second_actual: f64,
    /// Cumulative messages sent over persistent connections (TCP) or datagrams (UDP).
    pub messages_total: u64,
    /// Cumulative bytes the sockets accepted for sending.
    pub bytes_total: u64,
    /// Cumulative fresh TCP connections established.
    pub connections_total: u64,
    /// Cumulative failed socket operations: connects, writes, and sends.
    pub errors_total: u64,
    /// Whether every requested rate is within tolerance of what was measured.
    pub converged: bool,
    /// Whether the operator's operations-per-second ceiling stopped the request being reached.
    pub saturated: bool,
}

impl RuntimeStatus {
    #[must_use]
    pub fn new(baseline_rss_anon_bytes: u64) -> Self {
        Self {
            started_at: Instant::now(),
            run_clock: crate::clock::RunClock::new(),
            last_tick_at: None,
            tick_count: 0,
            arena_resident_bytes: 0,
            baseline_rss_anon_bytes,
            last_target_arena_bytes: 0,
            convergence_error_bytes: 0,
            converged: true,
            reconcile_errors_total: 0,
            last_error: None,
            cpu: CpuRuntime::default(),
            memory_access: MemoryAccessRuntime::default(),
            file_backed: FileBackedRuntime::default(),
            file_backed_ceiling_bytes: 0,
            shared: SharedRuntime::default(),
            shared_ceiling_bytes: 0,
            io: IoRuntime::default(),
            io_ceiling_bytes: 0,
            network: NetworkRuntime::default(),
            network_ceiling_ops_per_second: 0,
            thp: ThpRuntime::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessPattern, FileBackedConfig, WorkloadState};
    use crate::setting::Setting;

    fn state_with_ceiling(max_memory_bytes: u64) -> WorkloadState {
        let mut state = WorkloadState::default();
        state.controller.max_memory_bytes = max_memory_bytes;
        state
    }

    #[test]
    fn accepts_representable_ceilings() {
        assert!(state_with_ceiling(1).validate().is_ok());
        assert!(state_with_ceiling(usize::MAX as u64).validate().is_ok());

        // The built-in 8GiB default is only representable where `usize` is 64 bits wide. On a
        // 32-bit host the startup clamp narrows the process ceiling to `usize::MAX`.
        let eight_gib = 8u64 * 1024 * 1024 * 1024;
        assert_eq!(
            state_with_ceiling(eight_gib).validate().is_ok(),
            eight_gib <= usize::MAX as u64
        );
    }

    #[test]
    fn rejects_zero_ceiling() {
        assert!(state_with_ceiling(0).validate().is_err());
    }

    #[test]
    fn the_default_thp_policy_is_default_and_every_policy_validates() {
        assert_eq!(WorkloadState::default().memory.thp, "default");
        for policy in crate::memory::ThpPolicy::ALL {
            let mut state = WorkloadState::default();
            state.memory.thp = policy.as_str().to_owned();
            assert!(state.validate().is_ok(), "{policy:?} should be accepted");
        }
    }

    #[test]
    fn an_unknown_thp_policy_is_rejected_and_names_the_valid_ones() {
        let mut state = WorkloadState::default();
        state.memory.thp = "huge".to_owned();
        let error = state
            .validate()
            .expect_err("an unknown THP policy must be rejected");
        let message = error.to_string();
        for name in crate::memory::ThpPolicy::ALL.map(crate::memory::ThpPolicy::as_str) {
            assert!(message.contains(name), "{message} should name {name}");
        }
    }

    #[test]
    fn rejects_out_of_range_tick() {
        let mut state = state_with_ceiling(1024);
        state.controller.tick_ms = 0;
        assert!(state.validate().is_err());
        state.controller.tick_ms = 10_001;
        assert!(state.validate().is_err());
    }

    // The controller narrows byte targets to `usize`, so a ceiling the platform cannot represent
    // must be refused rather than silently truncated. This only bites on a 32-bit target, so the
    // test is gated to run where it is meaningful.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn rejects_ceiling_wider_than_the_address_space() {
        let error = state_with_ceiling(u64::from(u32::MAX) + 1)
            .validate()
            .expect_err("ceiling above 4GiB should be rejected on 32-bit");
        assert!(error.to_string().contains("addressable limit"), "{error}");
    }

    /// A default `WorkloadState` has to be a workload that does nothing, because
    /// `#[serde(default)]` means it is also what every partial update falls back to.
    #[test]
    fn the_default_workload_generates_no_cpu_load() {
        let state = WorkloadState::default();
        assert!(!state.running);
        assert!(state.validate().is_ok());
        assert_eq!(
            state.cpu.utilization,
            Setting::constant(0.0),
            "a default that burned a core would make any partial PATCH start CPU work"
        );
    }

    /// A CPU target is dimensionless. Reading `1MiB` as a utilization percent would be a
    /// ten-million-fold error that still parses, so it has to be refused instead.
    #[test]
    fn rejects_a_cpu_target_given_in_bytes() {
        let mut state = WorkloadState::default();
        state.cpu.utilization = Setting::Expression("1MiB".to_owned());
        let error = state
            .validate()
            .expect_err("byte units are not a percentage");
        assert!(error.to_string().contains("1MiB"), "{error}");

        // The identical text is a valid memory target, so this is a real distinction
        // between the two controllers rather than a broken expression.
        let mut state = WorkloadState::default();
        state.memory.anonymous_rss = Some(Setting::Expression("1MiB".to_owned()));
        assert!(state.validate().is_ok());
    }

    #[test]
    fn rejects_a_negative_cpu_target() {
        let mut state = WorkloadState::default();
        state.cpu.utilization = Setting::Expression("50 - 100".to_owned());
        let error = state.validate().expect_err("negative utilization");
        assert!(error.to_string().contains("non-negative"), "{error}");
    }

    /// A target above the machine's whole capacity is clamped by the controller and
    /// reported, not rejected: an expression that peaks there is a legitimate way to ask
    /// for "as much as this box can do".
    #[test]
    fn accepts_a_cpu_target_above_machine_capacity() {
        let mut state = WorkloadState::default();
        state.cpu.utilization = Setting::Expression("100000".to_owned());
        assert!(state.validate().is_ok());
    }

    #[test]
    fn rejects_an_unknown_affinity_mode() {
        let mut state = WorkloadState::default();
        state.cpu.affinity = "none".to_owned();
        let error = state.validate().expect_err("none is not a mode");
        assert!(error.to_string().contains("cpu.affinity"), "{error}");
    }

    #[test]
    fn rejects_a_logical_cpu_that_does_not_exist() {
        let mut state = WorkloadState::default();
        state.cpu.cpus = vec![num_cpus::get()];
        let error = state.validate().expect_err("no such CPU on this host");
        assert!(error.to_string().contains("logical CPU"), "{error}");
    }

    /// `/workload` is remotely controllable, so a client must not be able to ask for an
    /// unbounded number of kernel threads.
    #[test]
    fn rejects_an_unbounded_worker_count() {
        let mut state = WorkloadState::default();
        state.cpu.workers = crate::cpu::MAX_WORKERS + 1;
        let error = state.validate().expect_err("unbounded thread count");
        assert!(error.to_string().contains("cpu.workers"), "{error}");

        state.cpu.workers = crate::cpu::MAX_WORKERS;
        assert!(state.validate().is_ok());
    }

    /// The full documented shape has to round-trip through the API's serde, since a
    /// client that cannot express a field cannot use it.
    #[test]
    fn a_cpu_config_round_trips_through_json() {
        let mut state = WorkloadState::default();
        state.cpu.utilization = Setting::Expression("100 + 50*sin(t/10)".to_owned());
        state.cpu.workers = 2;
        state.cpu.cpus = vec![0, 1];
        state.cpu.affinity = "pin".to_owned();

        let encoded = serde_json::to_string(&state).expect("encode");
        assert!(
            encoded.contains(r#""utilization":"100 + 50*sin(t/10)""#),
            "{encoded}"
        );
        assert!(encoded.contains(r#""cpus":[0,1]"#), "{encoded}");

        let decoded: WorkloadState = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.cpu.utilization, state.cpu.utilization);
        assert_eq!(decoded.cpu.cpus, state.cpu.cpus);
        assert_eq!(decoded.cpu.workers, state.cpu.workers);
        assert_eq!(decoded.cpu.affinity, "pin");
    }

    /// A state written before CPU existed must still load, and must come back quiet.
    #[test]
    fn a_payload_without_a_cpu_section_defaults_to_no_cpu_load() {
        let decoded: WorkloadState = serde_json::from_str(
            r#"{"running":true,"controller":{"tick_ms":100,"tolerance_bytes":2097152,"max_memory_bytes":1073741824},"memory":{"anonymous_rss":"256MiB"}}"#,
        )
        .expect("decode");
        assert_eq!(decoded.cpu, WorkloadState::default().cpu);
        assert!(decoded.validate().is_ok());
    }

    /// The documented pattern names are a wire contract, and they are also what
    /// `AccessPattern::as_str` produces. If serde ever derived its own names from the variant
    /// names instead, a client following the documentation would start getting parse errors
    /// with no change on this side to explain them.
    #[test]
    fn access_patterns_serde_as_the_documented_names() {
        for (name, pattern) in [
            ("sequential", AccessPattern::Sequential),
            ("random", AccessPattern::Random),
            ("stride", AccessPattern::Stride),
            ("working_set", AccessPattern::WorkingSet),
            ("hot_cold", AccessPattern::HotCold),
            ("cold_scan", AccessPattern::ColdScan),
        ] {
            assert_eq!(pattern.as_str(), name, "as_str drifted from the docs");
            assert_eq!(AccessPattern::parse(name), Some(pattern));
            // Round trip through the real wire format, not just the string.
            let encoded = serde_json::to_string(&pattern).expect("encode");
            assert_eq!(encoded, format!("\"{name}\""), "unexpected wire name");
            let decoded: AccessPattern = serde_json::from_str(&encoded).expect("decode");
            assert_eq!(decoded, pattern);
        }
        assert_eq!(
            AccessPattern::parse("Sequential"),
            None,
            "Rust names are not wire names"
        );
    }

    /// An unknown pattern is a 400 that names the valid ones, rather than a bare "unknown
    /// variant" that gives the client nothing to correct.
    #[test]
    fn an_unknown_access_pattern_lists_the_valid_names() {
        let error =
            serde_json::from_str::<AccessPattern>("\"sideways\"").expect_err("not a pattern");
        let message = error.to_string();
        for name in ["sequential", "hot_cold", "cold_scan"] {
            assert!(message.contains(name), "{message} should mention {name}");
        }
    }

    /// The default file-backed configuration must write nothing to disk, for the same reason
    /// the default CPU configuration must not spawn a thread: `WorkloadState` is
    /// `#[serde(default)]`, so this is what every partial `PATCH` that never mentions
    /// `file_backed` restores.
    #[test]
    fn the_default_file_backed_configuration_generates_nothing() {
        let state = WorkloadState::default();
        assert!(state.file_backed.rss.is_none());
        assert_eq!(state.file_backed.pages_per_second, Setting::constant(0.0));
        assert!(state.validate().is_ok());
    }

    /// A `file_backed` section that never existed still decodes, because an older client must
    /// not break on a newer server.
    #[test]
    fn a_payload_without_a_file_backed_section_defaults_to_quiet() {
        let decoded: WorkloadState =
            serde_json::from_str(r#"{"running":true,"memory":{"anonymous_rss":"256MiB"}}"#)
                .expect("decode");
        assert_eq!(decoded.file_backed, FileBackedConfig::default());
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn accepts_a_file_backed_target_within_the_ceiling() {
        let mut state = WorkloadState::default();
        state.file_backed.rss = Some(Setting::Expression("128MiB".to_owned()));
        state.file_backed.pages_per_second = Setting::constant(500.0);
        state.file_backed.access_pattern = AccessPattern::HotCold;
        state.file_backed.madvise = String::from("sequential");
        assert!(state.validate().is_ok());
    }

    /// A file-backed target above the memory ceiling is refused at write time, while the
    /// separate disk ceiling is applied at runtime and reported as saturated. That split is
    /// deliberate: the memory ceiling is part of desired state and so can be checked, while the
    /// disk ceiling belongs to the process and only the evaluated target can be clamped.
    #[test]
    fn rejects_a_file_backed_target_above_the_memory_ceiling() {
        let mut state = WorkloadState::default();
        state.controller.max_memory_bytes = 64 * 1024 * 1024;
        state.file_backed.rss = Some(Setting::Expression("128MiB".to_owned()));
        let error = state.validate().expect_err("above max_memory_bytes");
        assert!(error.to_string().contains("file_backed.rss"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_madvise_policy_naming_the_valid_ones() {
        let mut state = WorkloadState::default();
        state.file_backed.madvise = String::from("sideways");
        let error = state.validate().expect_err("not a policy");
        assert!(error.to_string().contains("file_backed.madvise"), "{error}");
        assert!(error.to_string().contains("random"), "{error}");
    }

    #[test]
    fn rejects_a_zero_sized_file_backed_working_set() {
        let mut state = WorkloadState::default();
        state.file_backed.working_set_pages = Some(0);
        let error = state.validate().expect_err("zero pages");
        assert!(error.to_string().contains("working_set_pages"), "{error}");
    }

    #[test]
    fn rejects_a_negative_file_backed_target() {
        let mut state = WorkloadState::default();
        state.file_backed.pages_per_second = Setting::constant(-1.0);
        assert!(state.validate().is_err());
        state.file_backed.rss = Some(Setting::Expression("64MiB - 128MiB".to_owned()));
        let error = state.validate().expect_err("negative at t=0");
        assert!(error.to_string().contains("non-negative"), "{error}");
    }

    /// A full round trip through the documented shape, so a client can express the whole
    /// file-backed section rather than only the parts that happen to have tests.
    #[test]
    fn a_file_backed_config_round_trips_through_json() {
        // Exactly the body a client PATCHes: the `file_backed` section and nothing else. The
        // rest of the state comes from `#[serde(default)]`, which is the path the API's
        // merge-then-deserialize handler takes.
        let body = r#"{"file_backed":{
            "rss":"64MiB + 16MiB*sin(t/10)",
            "pages_per_second":2000.0,
            "access_pattern":"working_set",
            "madvise":"normal",
            "working_set_pages":512,
            "stride_pages":4,
            "random_seed":99
        }}"#;
        let state: WorkloadState = serde_json::from_str(body).expect("decode");
        assert!(state.validate().is_ok());

        let encoded = serde_json::to_string(&state).expect("encode");
        assert!(
            encoded.contains(r#""access_pattern":"working_set""#),
            "{encoded}"
        );

        let decoded: WorkloadState = serde_json::from_str(&encoded).expect("re-decode");
        assert_eq!(decoded.file_backed.rss, state.file_backed.rss);
        assert_eq!(
            decoded.file_backed.pages_per_second,
            Setting::constant(2000.0)
        );
        assert_eq!(
            decoded.file_backed.access_pattern,
            AccessPattern::WorkingSet
        );
        assert_eq!(decoded.file_backed.madvise, "normal");
        assert_eq!(decoded.file_backed.working_set_pages, Some(512));
        assert_eq!(decoded.file_backed.stride_pages, Some(4));
        assert_eq!(decoded.file_backed.random_seed, Some(99));
    }

    /// A nested object must accept a partial body, whichever controller it belongs to.
    ///
    /// `memory_access` was the one configuration missing `#[serde(default)]`, so a `PATCH` naming
    /// a single one of its fields was rejected for a missing "pattern" and the fault rate's
    /// canonical spelling could not be used at all, so a partial nested body has to merge rather
    /// than fail.
    #[allow(clippy::float_cmp)]
    #[test]
    fn a_partial_nested_controller_body_is_accepted() {
        let state: WorkloadState =
            serde_json::from_str(r#"{"memory_access":{"pages_per_second":2000}}"#)
                .expect("a partial memory_access body");
        assert_eq!(
            state.memory_access.pages_per_second,
            Setting::constant(2000.0)
        );
        assert_eq!(state.memory_access.pattern, AccessPattern::Sequential);
        assert!(state.validate().is_ok());
    }
}

#[cfg(test)]
mod composite_tests {
    use super::WorkloadState;
    use crate::setting::Setting;

    /// A body that names several controllers at once is accepted. The composite spelling used to
    /// carry the `anonymous` and `faults` aliases; with those gone the canonical fields spell the
    /// same request.
    // Exact comparison is the assertion here, not an approximation: these values came from
    // integer literals in the body, which are exactly representable, and "the field received
    // what the body asked for" is a statement about equality rather than about magnitude.
    #[allow(clippy::float_cmp)]
    #[test]
    fn a_multi_controller_body_is_accepted() {
        let body = r#"{
            "controller": { "max_memory_bytes": 536870912 },
            "memory": { "anonymous_rss": "500MiB + 200MiB*sin(t/30)" },
            "file_backed": { "rss": "200MiB" },
            "cpu": { "utilization": "100 + 50*sin(t/10)" },
            "memory_access": { "pages_per_second": 10000 }
        }"#;
        let state: WorkloadState = serde_json::from_str(body).expect("decode");
        assert_eq!(
            state.memory.anonymous_rss,
            Some(Setting::Expression("500MiB + 200MiB*sin(t/30)".to_owned()))
        );
        assert_eq!(
            state.file_backed.rss,
            Some(Setting::Expression("200MiB".to_owned()))
        );
        assert_eq!(
            state.cpu.utilization,
            Setting::Expression("100 + 50*sin(t/10)".to_owned())
        );
        assert_eq!(
            state.memory_access.pages_per_second,
            Setting::constant(10000.0)
        );
        assert!(state.validate().is_ok());
    }

    /// A network rate keeps accepting the plain JSON number every existing client sends, while
    /// also accepting an expression in `t` like the memory and CPU targets do.
    #[test]
    fn a_network_rate_accepts_a_number_or_an_expression() {
        let state: WorkloadState = serde_json::from_str(
            r#"{"network":{"target":"127.0.0.1:9000",
                           "connections_per_second":10,
                           "messages_per_second":"500 + 250*sin(t/286.5)"}}"#,
        )
        .expect("decode");
        assert_eq!(
            state.network.connections_per_second,
            Setting::constant(10.0)
        );
        assert_eq!(
            state.network.messages_per_second,
            Setting::Expression("500 + 250*sin(t/286.5)".to_owned())
        );
    }

    /// `GET /state` echoes the configuration, so a rate must round-trip in the spelling it was
    /// given: a number stays a number and an expression stays a string. Turning every rate into a
    /// string would break every client that reads the field as a number today.
    #[test]
    fn a_network_rate_round_trips_its_spelling() {
        let state: WorkloadState = serde_json::from_str(
            r#"{"network":{"connections_per_second":10,
                           "messages_per_second":"500 + 250*sin(t/286.5)"}}"#,
        )
        .expect("decode");
        let encoded = serde_json::to_value(&state.network).expect("encode");
        // An integer stays an integer rather than growing a `.0`, so the value the client wrote is
        // the value it reads back.
        assert_eq!(encoded["connections_per_second"], serde_json::json!(10));
        assert_eq!(
            encoded["messages_per_second"],
            serde_json::json!("500 + 250*sin(t/286.5)")
        );
    }

    /// A malformed or negative rate is refused at configuration time, not left for the
    /// controller's first tick. A value that dips negative later is a different matter and is
    /// clamped at runtime, so only the `t = 0` value is checked here.
    #[test]
    fn a_network_rate_is_validated_at_configuration_time() {
        let mut state = WorkloadState::default();
        state.network.connections_per_second = Setting::constant(-1.0);
        let error = state.validate().expect_err("negative constant");
        assert!(
            error.to_string().contains("network.connections_per_second"),
            "{error}"
        );

        let mut state = WorkloadState::default();
        state.network.messages_per_second = Setting::Expression("50 - 100".to_owned());
        let error = state.validate().expect_err("negative at t=0");
        assert!(error.to_string().contains("non-negative"), "{error}");

        let mut state = WorkloadState::default();
        state.network.messages_per_second = Setting::Expression("100MiB".to_owned());
        assert!(state.validate().is_err(), "byte units are not a rate");
    }

    /// The controller evaluates the expression, not `settings`: stopping the workload has to zero
    /// the rate whatever the caller passes, so a stopped workload reports no traffic.
    #[test]
    fn a_stopped_workload_zeroes_the_network_rate() {
        let state = WorkloadState::default();
        let settings = state.network.settings(false, 750.0, 250.0);
        assert_eq!(settings.connections_per_second, 0.0);
        assert_eq!(settings.messages_per_second, 0.0);
        assert!(settings.target.is_none());
    }
}
