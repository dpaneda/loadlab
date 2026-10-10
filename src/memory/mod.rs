use std::ptr;

use crate::error::LoadlabError;

const CHUNK_SIZE: usize = 64 * 1024 * 1024;

/// The transparent-huge-page policy applied to the anonymous arena.
///
/// This is a *request*, not a guarantee: a process cannot force THP on a host whose
/// `/sys/kernel/mm/transparent_hugepage/enabled` is `never`, so the policy only advises the
/// kernel and the effective result has to be read back from `AnonHugePages`. The names match the
/// host's own setting so that `default` means "leave whatever the host does" and the rest are the
/// values loadlab would ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThpPolicy {
    /// Do not call `madvise` or `prctl`; inherit whatever the host is configured to do.
    #[default]
    Default,
    /// Ask the kernel to back the arena with huge pages where it can, and undo any process-wide
    /// disable. On a host set to `madvise` this is what turns huge pages on; on `always` it is
    /// redundant; on `never` it has no effect.
    Always,
    /// The same `MADV_HUGEPAGE` region advice as [`Self::Always`].
    ///
    /// Kept as a distinct name because the API mirrors the host's own vocabulary
    /// (`always`/`madvise`/`never`), not because loadlab makes a different syscall: a process
    /// cannot change the host's global mode without root, so both values mean "ask for huge pages
    /// on this arena". An operator picks whichever matches the host's setting.
    Madvise,
    /// Refuse huge pages for the arena with `MADV_NOHUGEPAGE` and disable them process-wide with
    /// `prctl(PR_SET_THP_DISABLE, 1)`, so the generated load uses base pages even on an `always`
    /// host.
    Never,
}

impl ThpPolicy {
    /// Every policy, in documentation order.
    pub const ALL: [Self; 4] = [Self::Default, Self::Always, Self::Madvise, Self::Never];

    /// The configuration name of this policy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Always => "always",
            Self::Madvise => "madvise",
            Self::Never => "never",
        }
    }

    /// Parse a policy name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_str() == value)
    }
}

#[derive(Debug)]
struct Mapping {
    ptr: *mut u8,
    len: usize,
    resident: usize,
}

// A Mapping is owned exclusively by the controller thread. It is never shared with another
// thread and never accessed concurrently. Marking it Send allows the whole controller to live
// on a dedicated worker thread without exposing the raw pointer to the HTTP runtime.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            // Best-effort cleanup. The mapping belongs exclusively to this object.
            let _ = libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

#[derive(Debug, Default)]
pub struct AnonymousArena {
    mappings: Vec<Mapping>,
    resident_bytes: usize,
    page_size: usize,
    thp_policy: ThpPolicy,
}

impl AnonymousArena {
    pub fn new() -> Result<Self, LoadlabError> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(LoadlabError::MemoryMap(
                "sysconf(_SC_PAGESIZE) returned an invalid value".to_owned(),
            ));
        }
        Ok(Self {
            page_size: page_size as usize,
            ..Self::default()
        })
    }

    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// The transparent-huge-page policy currently applied to the arena.
    #[must_use]
    pub fn thp_policy(&self) -> ThpPolicy {
        self.thp_policy
    }

    /// Apply a transparent-huge-page policy to the arena.
    ///
    /// Existing mappings are re-advised, so a policy change takes effect on memory that is already
    /// resident, and the policy is remembered for mappings created later. A switch back to
    /// [`ThpPolicy::Default`] clears a process-wide disable but cannot un-advise regions that a
    /// previous `never`/`madvise` already covered; those keep their advice until the arena drops
    /// and recreates them. That limitation is worth stating rather than papering over: the honest
    /// observation of what happened is `AnonHugePages`, not the requested policy.
    pub fn set_thp_policy(&mut self, policy: ThpPolicy) -> Result<(), LoadlabError> {
        if self.thp_policy == policy {
            return Ok(());
        }
        self.thp_policy = policy;
        self.apply_thp_advice()
    }

    fn apply_thp_advice(&self) -> Result<(), LoadlabError> {
        let advice = match self.thp_policy {
            ThpPolicy::Always | ThpPolicy::Madvise => Some(libc::MADV_HUGEPAGE),
            ThpPolicy::Never => Some(libc::MADV_NOHUGEPAGE),
            ThpPolicy::Default => None,
        };
        if let Some(advice) = advice {
            for mapping in &self.mappings {
                madvise_region(mapping.ptr, mapping.len, advice)?;
            }
        }
        // The process-wide switch only makes sense alongside the per-mapping advice, but it is
        // what covers anonymous memory loadlab did not map itself. `default` clears it so a
        // previous `never` does not outlive its configuration.
        set_thp_disabled(matches!(self.thp_policy, ThpPolicy::Never))
    }

    pub fn set_resident_bytes(&mut self, target: usize) -> Result<(), LoadlabError> {
        let target = align_down(target, self.page_size);
        if target == self.resident_bytes {
            return Ok(());
        }

        if target > self.resident_bytes {
            self.grow(target)?;
        } else {
            self.shrink(target)?;
        }

        Ok(())
    }

    fn grow(&mut self, target: usize) -> Result<(), LoadlabError> {
        self.ensure_capacity(target)?;

        let mut global_offset = self.resident_bytes;
        while global_offset < target {
            let index = global_offset / CHUNK_SIZE;
            let offset = global_offset % CHUNK_SIZE;
            let amount = (self.mappings[index].len - offset).min(target - global_offset);

            // Touch one byte in every page. This is deliberate: it turns virtual anonymous
            // mappings into resident pages and creates the minor-fault workload we want to test.
            unsafe {
                let ptr = self.mappings[index].ptr.add(offset);
                touch_pages(ptr, amount, self.page_size);
            }
            self.mappings[index].resident = (offset + amount).max(self.mappings[index].resident);
            global_offset += amount;
        }

        self.resident_bytes = target;
        Ok(())
    }

    fn shrink(&mut self, target: usize) -> Result<(), LoadlabError> {
        let mut global_offset = self.resident_bytes;
        while global_offset > target {
            // `global_offset > target >= 0` guarantees the subtraction cannot underflow.
            let index = (global_offset - 1) / CHUNK_SIZE;
            let chunk_start = index * CHUNK_SIZE;
            let current_end = self.mappings[index]
                .resident
                .min(global_offset - chunk_start);
            let new_end = target.saturating_sub(chunk_start).min(current_end);

            if new_end < current_end {
                unsafe {
                    let ptr = self.mappings[index].ptr.add(new_end);
                    madvise_dontneed(ptr, current_end - new_end)?;
                }
                self.mappings[index].resident = new_end;
            }

            // `chunk_start` is strictly less than the previous `global_offset`, so the loop
            // always makes progress even when a mapping had nothing left to drop.
            global_offset = chunk_start.max(target);
        }

        self.resident_bytes = target;

        // Unmap completely unused trailing mappings. This controls virtual address growth as
        // well as resident memory without depending on an allocator.
        while self
            .mappings
            .last()
            .is_some_and(|mapping| mapping.resident == 0)
        {
            self.mappings.pop();
        }

        Ok(())
    }

    fn ensure_capacity(&mut self, target: usize) -> Result<(), LoadlabError> {
        let required_mappings = target.div_ceil(CHUNK_SIZE);
        while self.mappings.len() < required_mappings {
            let len = CHUNK_SIZE;
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(LoadlabError::MemoryMap(
                    std::io::Error::last_os_error().to_string(),
                ));
            }
            let ptr: *mut u8 = ptr.cast();
            // A mapping created after the policy was set has to be advised too, or the policy only
            // ever applies to whatever happened to be resident at the moment it changed.
            match self.thp_policy {
                ThpPolicy::Always | ThpPolicy::Madvise => {
                    madvise_region(ptr, len, libc::MADV_HUGEPAGE)?;
                }
                ThpPolicy::Never => {
                    madvise_region(ptr, len, libc::MADV_NOHUGEPAGE)?;
                }
                ThpPolicy::Default => {}
            }
            self.mappings.push(Mapping {
                ptr,
                len,
                resident: 0,
            });
        }
        Ok(())
    }
}

fn align_down(value: usize, alignment: usize) -> usize {
    value / alignment * alignment
}

unsafe fn touch_pages(ptr: *mut u8, len: usize, page_size: usize) {
    let mut offset = 0;
    while offset < len {
        unsafe { ptr::write_volatile(ptr.add(offset), 1) };
        offset += page_size;
    }

    // Ensure the last partial page is resident too.
    if len > 0 {
        unsafe { ptr::write_volatile(ptr.add(len - 1), 1) };
    }
}

unsafe fn madvise_dontneed(ptr: *mut u8, len: usize) -> Result<(), LoadlabError> {
    if len == 0 {
        return Ok(());
    }
    madvise_region(ptr, len, libc::MADV_DONTNEED)
}

/// Apply one `madvise` advice to a region, mapping the errno to an error.
fn madvise_region(ptr: *mut u8, len: usize, advice: libc::c_int) -> Result<(), LoadlabError> {
    if len == 0 {
        return Ok(());
    }
    let result = unsafe { libc::madvise(ptr.cast(), len, advice) };
    if result != 0 {
        return Err(LoadlabError::MemoryMap(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}

/// Set or clear `PR_SET_THP_DISABLE` for the whole process, which covers anonymous allocations
/// beyond the arena.
fn set_thp_disabled(disabled: bool) -> Result<(), LoadlabError> {
    // SAFETY: `PR_SET_THP_DISABLE` reads its first variadic argument as a boolean int and ignores
    // the rest, so the values passed here are valid.
    let result = unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, i32::from(disabled), 0, 0, 0) };
    if result != 0 {
        return Err(LoadlabError::MemoryMap(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AnonymousArena, ThpPolicy};

    #[test]
    fn arena_can_grow_and_shrink() {
        let mut arena = AnonymousArena::new().unwrap();
        arena.set_resident_bytes(4 * 4096).unwrap();
        assert_eq!(arena.resident_bytes(), 4 * 4096);
        arena.set_resident_bytes(4096).unwrap();
        assert_eq!(arena.resident_bytes(), 4096);
        arena.set_resident_bytes(0).unwrap();
        assert_eq!(arena.resident_bytes(), 0);
    }

    // The policy is a request, not an assertion about the host: this checks that applying every
    // value succeeds and is remembered, including on a host where THP is unavailable. It
    // deliberately does not assert that huge pages appeared.
    #[test]
    fn thp_policy_is_applied_and_remembered() {
        let mut arena = AnonymousArena::new().unwrap();
        arena.set_resident_bytes(4 * 4096).unwrap();
        for policy in ThpPolicy::ALL {
            arena.set_thp_policy(policy).unwrap();
            assert_eq!(arena.thp_policy(), policy);
        }
        arena.set_thp_policy(ThpPolicy::Default).unwrap();
    }

    #[test]
    fn thp_policy_names_round_trip() {
        for policy in ThpPolicy::ALL {
            assert_eq!(ThpPolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(ThpPolicy::parse("bogus"), None);
    }
}
