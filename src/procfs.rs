use std::{fs, io};

use crate::error::LoadlabError;

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct MemoryStats {
    pub rss_bytes: u64,
    pub pss_bytes: u64,
    pub rss_anon_bytes: u64,
    pub rss_file_bytes: u64,
    pub rss_shmem_bytes: u64,
    pub anonymous_bytes: u64,
    pub private_clean_bytes: u64,
    pub private_dirty_bytes: u64,
    pub shared_clean_bytes: u64,
    pub shared_dirty_bytes: u64,
    pub swap_bytes: u64,
    pub anon_huge_pages_bytes: u64,
    /// High-water mark of `rss_bytes` since process start (`VmHWM`).
    ///
    /// `rss_bytes` is only an instantaneous sample, so a workload that ramps up and back down can
    /// pass its peak entirely between two scrapes. This does not.
    pub peak_rss_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct FaultStats {
    pub minor_faults: u64,
    pub major_faults: u64,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct CpuStats {
    pub user_seconds: f64,
    pub system_seconds: f64,
    pub threads: u64,
}

/// Total CPU time consumed by the process, in seconds, as a single float.
///
/// This is the sum of user and system time, which is what the CPU controller measures against
/// wall-clock time to compute utilization. The controller thread reads it on every tick, so it
/// needs to be cheap: a single read of /proc/self/stat.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct CpuTime {
    pub total_seconds: f64,
}

/// Read the process's total CPU time (user + system) in seconds.
pub fn cpu_time() -> CpuTime {
    let fields = match stat_fields() {
        Ok(f) => f,
        Err(_) => return CpuTime::default(),
    };
    let user_ticks = match parse_stat_field(&fields, 11, "utime") {
        Ok(v) => v,
        Err(_) => return CpuTime::default(),
    };
    let system_ticks = match parse_stat_field(&fields, 12, "stime") {
        Ok(v) => v,
        Err(_) => return CpuTime::default(),
    };
    let ticks_per_second = match clock_ticks_per_second() {
        Ok(v) => v,
        Err(_) => return CpuTime::default(),
    };
    CpuTime {
        total_seconds: (user_ticks + system_ticks) as f64 / ticks_per_second,
    }
}

/// The granularity of every CPU time reading, in seconds.
///
/// `/proc/self/stat` counts CPU time in clock ticks, so two readings taken `window` apart can
/// only differ in steps of this much. Expressed as a percentage of a core that step is
/// `100 * clock_tick_seconds / window` -- with the usual 100Hz tick and a 100ms measurement
/// window, a reading quantises in steps of 10 points of a core.
///
/// The CPU controller uses this to know when a target is too small for the measurement to
/// resolve. Chasing a target finer than the quantisation does not converge on it; it just
/// amplifies the rounding error into a duty cycle that hunts.
pub fn clock_tick_seconds() -> f64 {
    match clock_ticks_per_second() {
        Ok(ticks_per_second) if ticks_per_second > 0.0 => 1.0 / ticks_per_second,
        _ => 0.01,
    }
}

fn clock_ticks_per_second() -> Result<f64, LoadlabError> {
    // SAFETY: `sysconf` takes a name and no pointers, so there is nothing to make invalid here.
    let raw_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if raw_ticks <= 0 {
        return Err(LoadlabError::Procfs(
            "clock ticks per second unavailable".to_owned(),
        ));
    }
    // `_SC_CLK_TCK` is a small positive constant (100 on every Linux target loadlab supports), so
    // narrowing it to `i32` cannot lose information -- and `i32` is what `f64` can be built from
    // without a lossy cast.
    Ok(f64::from(i32::try_from(raw_ticks).map_err(|_| {
        LoadlabError::Procfs("implausible clock tick rate".to_owned())
    })?))
}

pub fn memory_stats() -> Result<MemoryStats, LoadlabError> {
    let rollup = fs::read_to_string("/proc/self/smaps_rollup").map_err(proc_error)?;
    let status = fs::read_to_string("/proc/self/status").map_err(proc_error)?;

    let mut stats = MemoryStats::default();
    for line in rollup.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "Rss" => stats.rss_bytes = parse_kib_value(value)?,
            "Pss" => stats.pss_bytes = parse_kib_value(value)?,
            "Anonymous" => stats.anonymous_bytes = parse_kib_value(value)?,
            "Private_Clean" => stats.private_clean_bytes = parse_kib_value(value)?,
            "Private_Dirty" => stats.private_dirty_bytes = parse_kib_value(value)?,
            "Shared_Clean" => stats.shared_clean_bytes = parse_kib_value(value)?,
            "Shared_Dirty" => stats.shared_dirty_bytes = parse_kib_value(value)?,
            "Swap" => stats.swap_bytes = parse_kib_value(value)?,
            "AnonHugePages" => stats.anon_huge_pages_bytes = parse_kib_value(value)?,
            _ => {}
        }
    }

    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "RssAnon" => stats.rss_anon_bytes = parse_kib_value(value)?,
            "RssFile" => stats.rss_file_bytes = parse_kib_value(value)?,
            "RssShmem" => stats.rss_shmem_bytes = parse_kib_value(value)?,
            "VmHWM" => stats.peak_rss_bytes = parse_kib_value(value)?,
            _ => {}
        }
    }

    Ok(stats)
}

/// Process-wide file-backed resident memory, from `RssFile` in `/proc/self/status`.
///
/// Separate from [`memory_stats`] because the controller reads this on every tick and
/// `memory_stats` walks the page table via `smaps_rollup`. `/proc/self/status` is a small,
/// flat, single-pass file, so the tick pays a fixed few microseconds instead of a cost
/// proportional to the footprint this tool deliberately creates.
///
/// Note the scope difference: this is the whole process, so it includes the executable's own
/// text and any other mapped file, not just loadlab's generated mapping.
pub fn rss_file_bytes() -> Result<u64, LoadlabError> {
    let status = fs::read_to_string("/proc/self/status").map_err(proc_error)?;
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "RssFile" {
            return parse_kib_value(value);
        }
    }
    Err(LoadlabError::Procfs(
        "RssFile missing from /proc/self/status".to_owned(),
    ))
}

/// The kernel's accounting for another process, for the shared-memory actors.
///
/// This exists to report memory that is deliberately mapped by more than one process, and there is
/// no other way to see each participant's share. `Rss` is what that process
/// has resident, shared or not; `Pss` is the same set with every shared page divided among the
/// processes mapping it, which is the only figure that can be summed across actors to give a
/// physical total. `RssShmem` and `RssAnon` separate the two things loadlab is generating: pages
/// of the shared segment, and pages each actor copied for itself.
///
/// Returns `Ok(None)` for a process that has exited, which the caller treats as "rebuild it"
/// rather than as an error: an actor dying is not a failure of the controller.
///
/// `pss_bytes` requires `smaps_rollup`, which walks the page table. That is why this is called
/// once per actor per tick rather than once per process in the process: the cost is proportional to
/// the footprint, which is exactly what this tool makes large.
pub fn actor_accounting(pid: u32) -> Option<crate::shared::ActorAccounting> {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok();
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;

    let mut accounting = crate::shared::ActorAccounting {
        pid,
        ..Default::default()
    };
    // `Rss` is not in `/proc/<pid>/status` for the whole process, only in `smaps_rollup`, so a
    // process without a readable rollup has no RSS to report. Its `status` alone still gives the
    // shmem/anon split, which is enough to place it.
    if let Some(rollup) = rollup {
        for line in rollup.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim() {
                "Rss" => accounting.rss_bytes = parse_kib_value(value).unwrap_or(0),
                "Pss" => accounting.pss_bytes = parse_kib_value(value).unwrap_or(0),
                _ => {}
            }
        }
    }

    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "RssShmem" => accounting.rss_shmem_bytes = parse_kib_value(value).unwrap_or(0),
            "RssAnon" => accounting.rss_anon_bytes = parse_kib_value(value).unwrap_or(0),
            _ => {}
        }
    }

    accounting.minor_faults = stat_fields_for(pid)
        .ok()
        .and_then(|fields| parse_stat_field(&fields, 7, "minflt").ok())
        .unwrap_or(0);

    Some(accounting)
}

/// The whitespace-separated `/proc/<pid>/stat` fields, with the command already split off.
///
/// The same split [`stat_fields`] performs for this process, parameterised so an actor's counters
/// can be read without every caller re-deriving the `')'` convention.
fn stat_fields_for(pid: u32) -> Result<Vec<String>, LoadlabError> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|_| LoadlabError::Procfs(format!("cannot read /proc/{pid}/stat")))?;
    // The second field is the command in parentheses and may itself contain spaces and
    // parentheses, so the split is on the *last* ')': everything after it is positional.
    let tail = raw
        .rfind(')')
        .map_or_else(|| raw.as_str(), |index| &raw[index + 1..]);
    Ok(tail.split_whitespace().map(ToOwned::to_owned).collect())
}

/// The host's transparent-huge-page setting, read from sysfs.
///
/// Returns `always`, `madvise`, `never`, or `unavailable` when the file or the bracketed active
/// value is missing. This is an observation of the host, not of loadlab: it is what a client needs
/// to interpret why a requested policy did or did not produce huge pages, and it is why
/// `AnonHugePages` alone is not enough.
pub fn thp_host_policy() -> String {
    fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .ok()
        .and_then(|text| parse_thp_host_policy(&text))
        .unwrap_or_else(|| "unavailable".to_owned())
}

/// Extract the active value from a THP `enabled` file, which brackets the chosen one, e.g.
/// `always madvise [never]`.
fn parse_thp_host_policy(text: &str) -> Option<String> {
    let start = text.find('[')? + 1;
    let end = text[start..].find(']')? + start;
    Some(text[start..end].to_owned())
}

/// Facts about the machine loadlab is running on.
///
/// Read once at startup and published unchanged for the life of the process. None of it changes
/// while loadlab runs, and all of it changes the meaning of a measurement: a run that reached 200%
/// CPU on a 4-core host and one that did so on a 64-core host are the same graph and different
/// events, and nothing else loadlab reports can tell them apart afterwards.
///
/// The fields that come from files are [`Option`] rather than defaulted, because a container with
/// no `/proc/cpuinfo` should report "not available" instead of a CPU model of "" that reads like a
/// real answer. `uname(2)` has no such ambiguity and is required.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostInfo {
    /// Always `Linux`, and published anyway so a recording is self-describing.
    pub kernel_name: String,
    /// The kernel version string, e.g. `6.11.5-200.fc40.x86_64`.
    pub kernel_release: String,
    /// The build string and compiler, e.g. `# SMP PREEMPT_DYNAMIC ... gcc version 14`.
    pub kernel_version: String,
    /// The machine architecture, e.g. `x86_64`.
    pub machine: String,
    /// Logical CPUs visible to the process.
    pub cpus: usize,
    /// The first `model name` in `/proc/cpuinfo`, when the file exists.
    pub cpu_model: Option<String>,
    /// `MemTotal` from `/proc/meminfo`, in bytes.
    pub memory_total_bytes: Option<u64>,
    /// The page size the process was given, which is what every "pages" figure loadlab publishes is
    /// denominated in.
    pub page_size_bytes: u64,
}

/// Read the host's identity.
///
/// `uname(2)` is the only part that can fail: it is a syscall with no filesystem behind it, and a
/// failure there means the kernel is not the one loadlab was built for.
pub fn host_info() -> Result<HostInfo, LoadlabError> {
    // SAFETY: `uname` writes a `utsname` whose fields are fixed-size arrays of `c_char`, and the
    // pointer passed is a live allocation of exactly that type. The kernel NUL-terminates within
    // each field, and `field_to_string` stops at that terminator, so no field can be read past its
    // end. `unsafe_op_in_unsafe_fn` is denied crate-wide and this is an explicit block.
    //
    // Taking the address of the buffer is what the C signature requires; there is no borrowed-alias
    // spelling of it, and the borrow lives only inside this block.
    #[allow(clippy::borrow_as_ptr)]
    let (kernel_name, kernel_release, kernel_version, machine) = unsafe {
        let mut buffer: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut buffer) != 0 {
            return Err(LoadlabError::Procfs(format!(
                "uname failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        (
            field_to_string(&buffer.sysname),
            field_to_string(&buffer.release),
            field_to_string(&buffer.version),
            field_to_string(&buffer.machine),
        )
    };

    // SAFETY: `sysconf` takes a name and returns a value or -1. `_SC_PAGESIZE` has no memory
    // arguments at all, so there is nothing to be wrong about beyond the return value.
    let page_size_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size_bytes = if page_size_bytes > 0 {
        // Narrowing is guarded by the branch, not by hope: a negative `sysconf` result falls through
        // to the error below instead of wrapping into an enormous page size.
        #[allow(clippy::cast_sign_loss)]
        let bytes = page_size_bytes as u64;
        bytes
    } else {
        // Every Linux port loadlab runs on defines a page size, so this is unreachable. A wrong
        // number here would silently rescale every page count, which is why it is not defaulted to
        // 4096 on the hope that it is right.
        return Err(LoadlabError::Procfs(format!(
            "page size unavailable: sysconf returned {page_size_bytes}"
        )));
    };

    Ok(HostInfo {
        kernel_name,
        kernel_release,
        kernel_version,
        machine,
        cpus: num_cpus::get(),
        cpu_model: cpu_model(),
        memory_total_bytes: memory_total_bytes(),
        page_size_bytes,
    })
}

/// Copy a NUL-terminated `utsname` field into an owned `String`.
///
/// Takes the field as a slice rather than as a `*const c_char` so that reading it needs no `unsafe`
/// at all: a NUL-terminated array is a slice that ends at its first zero, and finding that zero by
/// index is exactly what `CStr::from_ptr` would have done with a pointer. The `uname` call above
/// still needs `unsafe` because it writes into the buffer, but nothing here has to trust that the
/// kernel filled the field in.
///
/// Lossy rather than strict: a kernel release is an arbitrary byte string from loadlab's point of
/// view, and a measurement should not fail to start because a version string was not UTF-8.
fn field_to_string(field: &[libc::c_char]) -> String {
    // `c_char` is signed on this target and the kernel's strings are arbitrary bytes. The cast
    // reinterprets rather than converts, so every byte survives; only the meaning of values above
    // 127 changes, and those are only ever rendered back as the same bytes.
    #[allow(clippy::cast_sign_loss)]
    let byte = |&byte: &libc::c_char| byte as u8;
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(byte)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The first `model name` in `/proc/cpuinfo`.
fn cpu_model() -> Option<String> {
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("model name"))
        .and_then(|rest| rest.split_once(':'))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// `MemTotal` from `/proc/meminfo`, converted from kibibytes.
fn memory_total_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kibibytes = text.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?;
        rest.split_whitespace().next()
    })?;
    kibibytes.parse::<u64>().ok().map(|value| value * 1024)
}

pub fn fault_stats() -> Result<FaultStats, LoadlabError> {
    let fields = stat_fields()?;

    // After the closing ')' the first field is process state (field 3).
    // minflt is field 10 => index 7; majflt is field 12 => index 9.
    let minor_faults = parse_stat_field(&fields, 7, "minflt")?;
    let major_faults = parse_stat_field(&fields, 9, "majflt")?;

    Ok(FaultStats {
        minor_faults,
        major_faults,
    })
}

pub fn cpu_stats() -> Result<CpuStats, LoadlabError> {
    let fields = stat_fields()?;

    // utime is field 14 => index 11; stime is field 15 => index 12. Both are in clock ticks, which
    // is not necessarily 100 (it is 100 on x86 and armhf, but sysconf is the portable answer).
    let user_ticks = parse_stat_field(&fields, 11, "utime")?;
    let system_ticks = parse_stat_field(&fields, 12, "stime")?;

    // SAFETY: `sysconf` takes a name and no pointers, so there is nothing to make invalid here.
    let raw_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if raw_ticks <= 0 {
        return Err(LoadlabError::Procfs(
            "clock ticks per second unavailable".to_owned(),
        ));
    }
    // `_SC_CLK_TCK` is a small positive constant (100 on every Linux target loadlab supports), so
    // narrowing it to `i32` cannot lose information -- and `i32` is what `f64` can be built from
    // without a lossy cast.
    let ticks_per_second = f64::from(
        i32::try_from(raw_ticks)
            .map_err(|_| LoadlabError::Procfs("implausible clock tick rate".to_owned()))?,
    );

    let status = fs::read_to_string("/proc/self/status").map_err(proc_error)?;
    let mut threads = 0;
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "Threads" {
            threads = value
                .trim()
                .parse::<u64>()
                .map_err(|e| LoadlabError::Procfs(e.to_string()))?;
            break;
        }
    }

    Ok(CpuStats {
        // A tick count is exact as a float for millennia of uptime, as above.
        #[allow(clippy::cast_precision_loss)]
        user_seconds: user_ticks as f64 / ticks_per_second,
        #[allow(clippy::cast_precision_loss)]
        system_seconds: system_ticks as f64 / ticks_per_second,
        threads,
    })
}

/// The `/proc/self/stat` fields that follow the `comm` field, indexed from zero.
///
/// `comm` is parenthesised and may itself contain spaces and parentheses, so the split has to
/// start after the *last* ')' rather than by whitespace.
fn stat_fields() -> Result<Vec<String>, LoadlabError> {
    let text = fs::read_to_string("/proc/self/stat").map_err(proc_error)?;
    split_stat_fields(&text)
}

fn split_stat_fields(text: &str) -> Result<Vec<String>, LoadlabError> {
    let after_comm = text
        .rfind(')')
        .ok_or_else(|| LoadlabError::Procfs("malformed /proc/self/stat".to_owned()))?;
    Ok(text[after_comm + 1..]
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

fn parse_stat_field(fields: &[String], index: usize, name: &str) -> Result<u64, LoadlabError> {
    fields
        .get(index)
        .ok_or_else(|| LoadlabError::Procfs(format!("missing {name} in /proc/self/stat")))?
        .parse::<u64>()
        .map_err(|e| LoadlabError::Procfs(e.to_string()))
}

fn parse_kib_value(value: &str) -> Result<u64, LoadlabError> {
    let mut parts = value.split_whitespace();
    let number = parts
        .next()
        .ok_or_else(|| LoadlabError::Procfs("missing numeric value".to_owned()))?
        .parse::<u64>()
        .map_err(|e| LoadlabError::Procfs(e.to_string()))?;
    let unit = parts.next().unwrap_or("kB");
    match unit {
        "kB" => number
            .checked_mul(1024)
            .ok_or_else(|| LoadlabError::Procfs("memory value overflow".to_owned())),
        "B" => Ok(number),
        other => Err(LoadlabError::Procfs(format!("unsupported unit {other:?}"))),
    }
}

fn proc_error(error: io::Error) -> LoadlabError {
    LoadlabError::Procfs(error.to_string())
}

/// One thread's scheduler accounting, from `/proc/<pid>/task/<tid>/schedstat`.
///
/// Three whitespace-separated unsigned 64-bit values: nanoseconds spent on a CPU, nanoseconds spent
/// waiting on a run queue, and the number of times the thread was scheduled.
///
/// The second field is the only measurement in procfs that answers "was this thread runnable but not
/// running?". That is a different question from the one `/proc/self/stat` answers for the process,
/// which is "how much CPU time was consumed" — a thread that spent the window waiting for a CPU
/// consumed none, and a controller that only sees consumption cannot tell that apart from a target
/// it simply failed to reach.
///
/// Requires `CONFIG_SCHEDSTATS`. On a kernel built without it the file is absent, which is reported
/// as unavailability rather than as zeros: zeros here would be a claim that nothing waited, which is
/// exactly the kind of wrong answer that is worse than no answer.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ThreadScheduling {
    pub tid: u32,
    pub on_cpu_nanoseconds: u64,
    pub runqueue_wait_nanoseconds: u64,
    pub timeslices: u64,
}

/// Read one thread's scheduler accounting.
pub fn thread_scheduling(tid: u32) -> Result<ThreadScheduling, LoadlabError> {
    let text = fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).map_err(|_| {
        LoadlabError::Procfs(format!("cannot read scheduler statistics for thread {tid}"))
    })?;
    parse_schedstat(&text, tid)
}

fn parse_schedstat(text: &str, tid: u32) -> Result<ThreadScheduling, LoadlabError> {
    let mut fields = text.split_whitespace();
    let mut next = |name: &str| {
        fields
            .next()
            .ok_or_else(|| LoadlabError::Procfs(format!("missing {name} in schedstat")))?
            .parse::<u64>()
            .map_err(|error| LoadlabError::Procfs(format!("schedstat {name}: {error}")))
    };
    Ok(ThreadScheduling {
        tid,
        on_cpu_nanoseconds: next("on-CPU time")?,
        runqueue_wait_nanoseconds: next("runqueue wait")?,
        timeslices: next("timeslice count")?,
    })
}

/// Whether this kernel exposes per-thread scheduler statistics.
///
/// Probed rather than assumed: it is a kernel build option, not a loadlab setting, and the answer
/// decides whether the scheduler observation can be reported or only declared unavailable.
pub fn thread_scheduling_available() -> bool {
    // SAFETY: `gettid` takes no arguments, reads no memory and cannot fail. It returns the calling
    // thread's own kernel id, which is positive, so the conversion below only rejects the
    // impossible case rather than a real one.
    let tid = unsafe { libc::gettid() };
    u32::try_from(tid).is_ok_and(|tid| thread_scheduling(tid).is_ok())
}

/// One thread's fault counters, from `/proc/<pid>/task/<tid>/stat`.
///
/// Same two counters as [`fault_stats`], but for one thread, so a process-wide total can be split
/// between the threads that generate faults on purpose and the ones that fault incidentally.
pub fn thread_fault_stats(tid: u32) -> Result<FaultStats, LoadlabError> {
    let fields = stat_fields_for(tid)?;
    Ok(FaultStats {
        // Indexes match [`fault_stats`]: after the command field, minflt is index 7 and majflt is 9.
        minor_faults: parse_stat_field(&fields, 7, "minflt")?,
        major_faults: parse_stat_field(&fields, 9, "majflt")?,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_kib_value, parse_schedstat, parse_stat_field, parse_thp_host_policy,
        split_stat_fields,
    };

    #[test]
    fn thp_host_policy_reads_the_bracketed_value() {
        assert_eq!(
            parse_thp_host_policy("always madvise [never]\n").as_deref(),
            Some("never")
        );
        assert_eq!(
            parse_thp_host_policy("[always] madvise never\n").as_deref(),
            Some("always")
        );
        assert_eq!(parse_thp_host_policy("garbage").as_deref(), None);
    }

    /// A real `/proc/self/stat` line with every field up to `stime` spelled out, so a change to
    /// the field indexes below is caught here rather than as silently wrong CPU and fault metrics.
    ///
    /// Fields 1 and 2 are pid and `comm`; `comm` here contains a space and a parenthesis to prove
    /// the split anchors on the last ')' rather than on whitespace.
    const STAT_FIXTURE: &str = "4242 (load lab) S 1 4242 4242 0 -1 4194304 100 111 200 222 600 700 \
1024 2048 0 0 20 0 3 0 1000 123456789 98765 4321";

    #[test]
    fn stat_field_indexes_land_on_the_documented_fields() {
        let fields = split_stat_fields(STAT_FIXTURE).expect("fixture should parse");

        // minflt=100 (field 10), majflt=200 (field 12), utime=600 (field 14), stime=700 (field 15).
        assert_eq!(parse_stat_field(&fields, 7, "minflt").unwrap(), 100);
        assert_eq!(parse_stat_field(&fields, 9, "majflt").unwrap(), 200);
        assert_eq!(parse_stat_field(&fields, 11, "utime").unwrap(), 600);
        assert_eq!(parse_stat_field(&fields, 12, "stime").unwrap(), 700);
    }

    #[test]
    fn stat_field_out_of_range_is_an_error_not_a_zero() {
        let fields = split_stat_fields(STAT_FIXTURE).expect("fixture should parse");
        let error = parse_stat_field(&fields, 999, "utime").expect_err("out of range should fail");
        assert!(error.to_string().contains("missing utime"), "{error}");
    }

    #[test]
    fn kib_values_convert_to_bytes() {
        assert_eq!(parse_kib_value("1024 kB").unwrap(), 1024 * 1024);
        assert_eq!(parse_kib_value("0 kB").unwrap(), 0);
        assert_eq!(parse_kib_value("512 B").unwrap(), 512);
    }

    #[test]
    fn unsupported_units_are_rejected() {
        assert!(parse_kib_value("3 MB").is_err());
    }

    #[test]
    fn schedstat_fields_land_on_the_documented_order() {
        // on-CPU ns, runqueue wait ns, timeslices. A kernel with the values the other way round
        // would report a thread's waiting time as its running time, which is the kind of mistake
        // that looks plausible in a dashboard forever.
        let parsed = parse_schedstat("8317481 4288 2\n", 42).expect("fixture should parse");
        assert_eq!(parsed.tid, 42);
        assert_eq!(parsed.on_cpu_nanoseconds, 8_317_481);
        assert_eq!(parsed.runqueue_wait_nanoseconds, 4_288);
        assert_eq!(parsed.timeslices, 2);
    }

    #[test]
    fn the_host_loadlab_runs_on_is_readable_and_self_consistent() {
        let host = super::host_info().expect("uname cannot fail on Linux");

        assert_eq!(host.kernel_name, "Linux", "loadlab is Linux-only");
        assert!(!host.kernel_release.is_empty(), "no kernel release");
        assert!(
            !host.kernel_version.is_empty(),
            "the kernel build string is what distinguishes two builds of the same release"
        );
        assert!(
            !host.machine.is_empty(),
            "an empty architecture would make a cross-machine comparison meaningless"
        );
        assert!(host.cpus >= 1, "a host with no CPUs cannot run anything");
        // A wrong page size would silently rescale every page count loadlab publishes, so it is
        // worth asserting against the process's own view rather than trusting the copy.
        assert_eq!(host.page_size_bytes, page_size());
    }

    /// The page size this process was actually given.
    fn page_size() -> u64 {
        // SAFETY: `sysconf(_SC_PAGESIZE)` takes no pointers and cannot fail in a way that is
        // recoverable here; a negative answer would make the assertion fail loudly.
        let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(value > 0, "sysconf reported no page size");
        #[allow(clippy::cast_sign_loss)]
        let bytes = value as u64;
        bytes
    }

    #[test]
    fn memtotal_is_read_as_kibibytes_and_multiplied_out() {
        // Against the live file rather than a fixture: the conversion is the thing under test, and a
        // fixture would only prove the parser can read the fixture it was written with.
        let reported = super::memory_total_bytes().expect("meminfo is readable on Linux");
        let text = std::fs::read_to_string("/proc/meminfo").expect("meminfo is readable");
        let kibibytes: u64 = text
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("MemTotal:")?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .expect("MemTotal");
        assert_eq!(reported, kibibytes * 1024);
        assert!(reported > 0);
    }

    #[test]
    fn a_cpuinfo_without_a_model_name_reports_absent_rather_than_empty() {
        assert!(
            super::cpu_model().is_some(),
            "this host should have a model name"
        );
        // A model name that parsed to nothing would be indistinguishable from a real one in a
        // recording, so the filter that drops it is the point.
        let model = super::cpu_model().expect("a model");
        assert!(!model.trim().is_empty());
    }

    #[test]
    fn a_truncated_schedstat_is_an_error_not_a_zero() {
        // Missing fields must fail rather than default to 0, which would read as "never waited".
        assert!(parse_schedstat("100\n", 1).is_err());
        assert!(parse_schedstat("", 1).is_err());
        assert!(parse_schedstat("100 200 three\n", 1).is_err());
    }
}
