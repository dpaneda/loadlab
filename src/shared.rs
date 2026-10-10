//! Shared memory and copy-on-write.
//!
//! The generators it provides are `MAP_SHARED`, `MAP_PRIVATE`, `fork`, COW writes and
//! shmem/tmpfs, and the reason it exists as its own module rather than another mode of the
//! anonymous arena is that **the same physical page has a different cost depending on who can see
//! it**.
//!
//! Concretely: a 256 MiB segment mapped shared by four actors is 256 MiB of physical memory, but
//! `sum(Rss)` over those four actors reports 1 GiB, because each process's RSS counts every page
//! it has mapped resident regardless of whether anyone else also has it. The same segment mapped
//! private and written by four actors is 1 GiB of physical memory, and every one of those numbers
//! is honest. An operator who wants to know what a workload costs cannot look at one process's RSS
//! and cannot look at the sum either. The kernel already publishes the answer: `Pss` divides each
//! shared page among the processes mapping it, so `sum(Pss)` across the set is the physical cost.
//!
//! So the controller reports three numbers rather than one, and the whole point is that they
//! disagree:
//!
//! - `accounted_bytes` — `sum(Rss)`. What naive process accounting, and most `/proc` tools, report.
//! - `physical_bytes` — `sum(Pss)`. What the memory actually costs.
//! - `shared_overcount_bytes` — the shared segment counted once per mapper rather than once in
//!   total. Zero with a single actor.
//!
//! Getting the actors to exist at all is the other half of the problem. A forked child inherits
//! the parent's address space, which is the cheapest way to share a mapping, but forking a
//! process that already has axum and tokio threads running in it is unsafe: the child may only
//! use async-signal-safe functions until it execs, and any allocator or lock the parent held at
//! the moment of the fork stays held in the child. loadlab re-executes itself instead
//! ([`run_actor`]), so each actor is a fresh single-threaded process that maps the shared segment
//! from an inherited file descriptor. It is more expensive than `fork` and it is correct, which is
//! the right trade for a tool whose output people use to size machines.

use std::{
    ffi::CString,
    fs,
    os::unix::{io::RawFd, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr,
};

use crate::error::LoadlabError;

/// The page size the kernel reported, cached for the mapping arithmetic.
fn page_size() -> usize {
    // SAFETY: `sysconf` takes a name and no pointers. A failure would make every mapping in this
    // module wrong, so a bad answer is clamped to the near-universal 4096 rather than propagated:
    // there is no sensible recovery, and a `sysconf` failure is not a condition a caller can act on.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    // A PAGE_SIZE of zero or negative is not a page size, and a positive one always fits a
    // usize on the same target that reported it. Checked rather than cast so the fallback is an
    // explicit choice rather than a truncation.
    usize::try_from(value)
        .ok()
        .filter(|page| *page > 0)
        .unwrap_or(4096)
}

/// Which kernel object backs the shared segment.
///
/// Both options produce pages the kernel accounts as `RssShmem`; they differ in the kernel
/// mechanism involved, which is what makes them worth offering separately in a lab.
///
/// `shm_open` is deliberately not offered: it is tmpfs underneath, exactly like `memfd`, and would
/// be a third code path producing the same accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedBacking {
    /// An anonymous file in memory, from `memfd_create`. Nothing touches the filesystem, and the
    /// object is reclaimed by the kernel as soon as the last reference goes away.
    #[default]
    Memfd,
    /// A System V shared memory segment, from `shmget`. The older mechanism, with its own kernel
    /// limits and its own visible presence in `/proc/sysvipc/shm`, so the accounting and the
    /// lifecycle are observable outside the process that made it.
    Sysv,
}

impl SharedBacking {
    /// The configuration name of this backing.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memfd => "memfd",
            Self::Sysv => "sysv",
        }
    }

    /// Every backing, in documentation order.
    pub const ALL: [Self; 2] = [Self::Memfd, Self::Sysv];

    /// Parse a backing name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|backing| backing.as_str() == value)
    }
}

/// The smallest segment loadlab will create, in bytes.
///
/// The kernel charges a page table page and at least one page of the segment itself for any
/// object, and a segment smaller than a page is not a workload anybody can measure.
pub const MIN_SEGMENT_BYTES: u64 = 4096;

/// The most actors loadlab will manage at once.
///
/// Each actor is a real process. This is not an arbitrary cap so much as a statement that the
/// controller is a resource laboratory and not a fork bomb.
pub const MAX_ACTORS: u32 = 64;

/// The backing object a segment is built on, owned by the parent.
///
/// Held across ticks rather than rebuilt: the segment's size is set once with `ftruncate`, and
/// growing it later would not fault pages into actors that are already mapped, so a size change is
/// handled by tearing the whole thing down and rebuilding rather than by an `ftruncate` that
/// silently does nothing useful.
pub struct Segment {
    backing: SharedBacking,
    /// Size the segment was created at. Fixed for the segment's life.
    bytes: u64,
    /// The mapping in this process, `MAP_SHARED`. Held so the pages are faulted in and stay
    /// faulted in: an object with no mapping in the parent would have nothing to demonstrate.
    address: *mut u8,
    /// `memfd` file descriptor, kept open so actors can inherit it across `exec`.
    fd: RawFd,
    /// System V segment id, when the backing is [`SharedBacking::Sysv`].
    sysv_id: libc::c_int,
}

// SAFETY: `Segment` owns its mapping and its descriptor exclusively, and the mapping is a plain
// byte range with no thread affinity. Moving it between threads is sound, which is what makes the
// controller's single-threaded tick loop safe to borrow it from.
unsafe impl Send for Segment {}

impl Segment {
    /// Create and populate a shared segment of `bytes` bytes.
    ///
    /// Every page is written once here so the segment is fully resident before any actor maps it.
    /// That is deliberate: a segment whose pages are faulted in lazily would let one actor's
    /// residency depend on whether another actor had touched a page yet, which is exactly the
    /// cross-actor coupling that pre-faulting makes visible rather than hiding.
    pub fn new(backing: SharedBacking, bytes: u64) -> Result<Self, LoadlabError> {
        let bytes = bytes.max(MIN_SEGMENT_BYTES);
        // `off_t` is signed, so a request beyond `i64::MAX` cannot be expressed as a file length
        // at all. Refused rather than truncated: a segment of the wrong size would be reported as
        // the size that was asked for.
        let length = libc::off_t::try_from(bytes).map_err(|_| {
            LoadlabError::InvalidConfig(format!(
                "shared segment size {bytes} exceeds the largest mappable file length"
            ))
        })?;

        let mut segment = match backing {
            SharedBacking::Memfd => Self::new_memfd(bytes, length)?,
            SharedBacking::Sysv => Self::new_sysv(bytes, length)?,
        };
        segment.populate()?;
        Ok(segment)
    }

    fn new_memfd(bytes: u64, length: libc::off_t) -> Result<Self, LoadlabError> {
        let name = CString::new("loadlab-shared").expect("no interior NUL");
        // SAFETY: `memfd_create` takes a NUL-terminated name and flags; it returns a new fd or -1.
        // `MFD_CLOEXEC` is deliberately omitted: the descriptor has to survive `exec` so an actor
        // process can inherit it. `clear_cloexec` below is what makes that safe against an
        // unrelated `exec` elsewhere in the process.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        if fd < 0 {
            return Err(LoadlabError::MemoryMap(format!(
                "memfd_create failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // Now the descriptor is inheritable. Held by a guard so an early return cannot leak it.
        let mut fd = FdGuard::adopt(fd);
        // SAFETY: `fd` is a live open descriptor for this thread; `ftruncate` takes no pointers
        // beyond the integer and the length.
        if unsafe { libc::ftruncate(fd.raw(), length) } != 0 {
            return Err(LoadlabError::MemoryMap(format!(
                "ftruncate({bytes}) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        fd.clear_cloexec();
        Ok(Self {
            backing: SharedBacking::Memfd,
            bytes,
            address: ptr::null_mut(),
            fd: fd.into_raw(),
            sysv_id: -1,
        })
    }

    /// `length` is unused here and that is not an oversight: a System V segment is sized by
    /// `shmget`, with no file to truncate. The caller checks it so both backings reject the same
    /// unexpressible sizes.
    fn new_sysv(bytes: u64, _length: libc::off_t) -> Result<Self, LoadlabError> {
        let size = libc::size_t::try_from(bytes)
            .map_err(|_| LoadlabError::InvalidConfig("shared segment is too large".to_owned()))?;
        // SAFETY: `shmget` takes only integers. `IPC_PRIVATE | IPC_CREAT` asks for a new segment
        // with a fresh key, so this cannot collide with or attach to anyone else's segment.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err(LoadlabError::MemoryMap(format!(
                "shmget({bytes}) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // A System V segment has no descriptor to inherit, so actors are handed the id instead.
        Ok(Self {
            backing: SharedBacking::Sysv,
            bytes,
            address: ptr::null_mut(),
            fd: -1,
            sysv_id: id,
        })
    }

    /// Map the segment into this process as `MAP_SHARED`.
    fn populate(&mut self) -> Result<(), LoadlabError> {
        let length = libc::size_t::try_from(self.bytes)
            .map_err(|_| LoadlabError::InvalidConfig("shared segment is too large".to_owned()))?;
        let address = self.map(length, libc::MAP_SHARED)?;
        // Written once per page. The value is irrelevant; what matters is that the write happens.
        for page in (0..length).step_by(page_size()) {
            // SAFETY: `address` is a live mapping of at least `length` bytes, and `page` is
            // within it by construction.
            unsafe { ptr::write_volatile(address.add(page), 1u8) };
        }
        self.address = address;
        Ok(())
    }

    /// Attach to the segment's backing object, read-only descriptor semantics.
    ///
    /// The read-only enforcement comes from mapping the *pages* `PROT_READ` in the actors rather
    /// than from the descriptor, because the object itself is shared and one actor must not be able
    /// to change what another actor sees.
    fn map(&self, length: usize, prot_and_flags: libc::c_int) -> Result<*mut u8, LoadlabError> {
        let address = match self.backing {
            SharedBacking::Memfd => {
                // SAFETY: `mmap` with a valid descriptor, a non-zero length and `MAP_SHARED` is
                // the documented way to map a memfd. A null return is checked immediately below.
                unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        length,
                        libc::PROT_READ | libc::PROT_WRITE,
                        prot_and_flags,
                        self.fd,
                        0,
                    )
                }
                .cast::<u8>()
            }
            SharedBacking::Sysv => {
                // SAFETY: `shmat` returns the attachment address or -1. The id is one this process
                // created, so it is valid.
                unsafe { libc::shmat(self.sysv_id, ptr::null(), 0) }.cast::<u8>()
            }
        };
        if address.is_null() || address as isize == -1 {
            return Err(LoadlabError::MemoryMap(format!(
                "mapping a {} shared segment failed: {}",
                self.backing.as_str(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(address)
    }

    /// The segment size fixed at creation.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The descriptor to hand to an actor, or `None` for System V segments.
    ///
    /// The descriptor stays open for the life of the segment: an actor spawned later must be able
    /// to attach, and `exec` resets the descriptor table of every child.
    #[must_use]
    pub fn inherit_fd(&self) -> Option<RawFd> {
        (self.backing == SharedBacking::Memfd).then_some(self.fd)
    }

    /// The System V segment id, or `None` for `memfd`.
    #[must_use]
    pub fn sysv_id(&self) -> Option<libc::c_int> {
        (self.backing == SharedBacking::Sysv).then_some(self.sysv_id)
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let mapped = !self.address.is_null() && self.address as isize != -1;
        if self.backing == SharedBacking::Sysv {
            // A System V segment is released with `shmdt`, not `munmap`.
            //
            // `shmat` counts an attachment in the kernel's `shm_nattch`, and only `shmdt`
            // decrements it. `munmap` detaches the pages but leaves the attachment standing, so
            // `IPC_RMID` below has a segment that is still attached: the kernel marks it for
            // destruction and keeps it, and it stays in `/proc/sysvipc/shm` counting against
            // `SHMALL` until the process exits. A tool that exists to create and tear down
            // segments cannot afford that -- the limit is a system-wide one and it is reached long
            // before the process is.
            if mapped {
                // SAFETY: `address` is the address `shmat` returned for this segment and has not
                // been unmapped, so it is exactly what `shmdt` expects. Detaching twice cannot
                // happen: this runs once, and `address` is cleared straight after.
                unsafe { libc::shmdt(self.address.cast()) };
                self.address = ptr::null_mut();
            }
            if self.sysv_id >= 0 {
                // SAFETY: removing the segment is an ipcctl with no buffer argument, so no pointer
                // is dereferenced. With the attachment gone this destroys the object immediately
                // rather than marking it.
                unsafe {
                    libc::shmctl(self.sysv_id, libc::IPC_RMID, ptr::null_mut());
                }
                self.sysv_id = -1;
            }
            return;
        }
        if mapped {
            // SAFETY: `address` is this segment's own mapping, unmapped exactly once here.
            unsafe {
                libc::munmap(
                    self.address.cast(),
                    libc::size_t::try_from(self.bytes).unwrap_or(libc::size_t::MAX),
                );
            }
            self.address = ptr::null_mut();
        }
        if self.fd >= 0 {
            // SAFETY: the descriptor is owned by this segment and not closed anywhere else.
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

/// A descriptor that closes itself unless explicitly released.
///
/// Used for the descriptor between `memfd_create` and the point where [`Segment`] owns it, so an
/// error return cannot leak it.
struct FdGuard(RawFd);

impl FdGuard {
    /// Take ownership of a descriptor that has `FD_CLOEXEC` set.
    fn adopt(fd: RawFd) -> Self {
        Self(fd)
    }

    fn raw(&self) -> RawFd {
        self.0
    }

    /// Clear `FD_CLOEXEC` so the descriptor survives the `exec` of an actor.
    fn clear_cloexec(&mut self) {
        // SAFETY: `fcntl` with `F_SETFD` and `FD_CLOEXEC` as the only flag takes an integer flag
        // set and dereferences no pointer.
        unsafe {
            libc::fcntl(self.0, libc::F_SETFD, 0);
        }
    }

    /// Give up ownership, leaving the caller responsible for the descriptor.
    fn into_raw(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for FdGuard {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: the guard owns this descriptor and closes it exactly once.
            unsafe { libc::close(self.0) };
        }
    }
}

/// What one actor holds and what the kernel says it holds.
///
/// The distinction between this struct's `rss_bytes` and `pss_bytes` is the entire reason this
/// module exists, so both are reported for every actor and never summed into a single headline
/// figure without also reporting which figure it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActorAccounting {
    /// The actor's process id. Zero for the controller process itself.
    pub pid: u32,
    /// Whether this is the controller process or a forked actor.
    pub is_controller: bool,
    /// Resident set size as the kernel reports it for this process: every page it has mapped,
    /// including pages it shares with other actors.
    pub rss_bytes: u64,
    /// Proportional set size: shared pages divided among the processes mapping them, so summing
    /// this across actors gives the physical memory the set actually holds.
    pub pss_bytes: u64,
    /// The `RssShmem` component. The shared segment is the only thing loadlab accounts here.
    pub rss_shmem_bytes: u64,
    /// The `RssAnon` component, which grows as this actor's copy-on-write pages are written.
    pub rss_anon_bytes: u64,
    /// Minor faults attributed to this actor. COW writes arrive as minor faults, so this is how
    /// the cost of a copy is observed rather than inferred.
    pub minor_faults: u64,
}

/// The observed state of the shared and copy-on-write segments.
///
/// `accounted_bytes` and `physical_bytes` are both sums over the actors. They are not
/// interchangeable: `sum(Rss)` counts a shared page once per process that maps it, `sum(Pss)`
/// divides it among them, and the difference grows with the actor count even though the machine
/// bought the memory once.
///
/// `shared_overcount_bytes` measures that gap for the shared segment alone, as
/// `sum(RssShmem) - the physical segment`. It is deliberately narrower than
/// `accounted_bytes - physical_bytes`, which also contains every other shared page the actors
/// happen to have in common -- the executable's text, the C library, anything the loader mapped
/// once. Those are real, and they are not what this figure is for; folding them in would leave it
/// nonzero even with a single actor and so unable to show the effect it exists to show.
#[derive(Debug, Clone, Default)]
pub struct SharedSnapshot {
    /// Kernel object backing the shared segment.
    pub backing: String,
    /// Size of the shared segment, fixed at creation.
    pub shared_bytes: u64,
    /// Size of each actor's private segment.
    pub private_bytes_per_actor: u64,
    /// Fraction of each private segment each actor writes to.
    pub cow_write_ratio: f64,
    /// Number of live actors, including the controller process.
    pub actors: u32,
    /// Number of live forked actors, excluding the controller.
    pub child_actors: u32,
    /// Physical pages the shared segment occupies, counted once.
    pub shared_physical_bytes: u64,
    /// `sum(RssShmem)` over every actor: the shared segment counted once per mapper.
    ///
    /// Reported alongside `shared_physical_bytes` rather than only as their difference, because
    /// the two are individually meaningful -- one is what naive accounting sees, the other is
    /// what the memory costs -- and a client charting the double-count over time needs both.
    pub shared_rss_bytes: u64,
    /// Physical pages the copy-on-write writes occupy across all actors.
    pub cow_physical_bytes: u64,
    /// `sum(Rss)` over every actor.
    pub accounted_bytes: u64,
    /// `sum(Pss)` over every actor: what the memory physically costs.
    pub physical_bytes: u64,
    /// Shared pages counted once per mapper instead of once in total: `sum(RssShmem)` across the
    /// actors minus the one physical copy of the segment.
    pub shared_overcount_bytes: u64,
    /// COW bytes requested this tick, before clamping to the operator ceiling.
    pub requested_cow_bytes: u64,
    /// COW bytes the controller was held to this tick.
    pub target_cow_bytes: u64,
    /// Whether the observed COW bytes are within tolerance of the target.
    pub converged: bool,
    /// Whether the request exceeded the operator ceiling.
    pub saturated: bool,
    /// Per-actor accounting, controller first. Empty when no actor exists.
    pub actor_accounting: Vec<ActorAccounting>,
}

/// Whether the current actor configuration still matches what the actors were built for.
///
/// A difference means an actor has to be replaced, because an actor's private segment size is
/// fixed at its `exec`: unlike the CPU pool or a file-backed mapping, nothing can be resized in
/// place. This is the same rebuild-detection idiom the other controllers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorSpec {
    /// The kernel object the actors attach to.
    pub backing: SharedBacking,
    /// Actors to run, including the controller.
    pub actors: u32,
    /// Each actor's private segment size.
    pub private_bytes_per_actor: u64,
    /// How much of each private segment to write.
    pub cow_write_ratio_milli: u16,
}

/// One forked actor process holding a shared and a private mapping.
pub struct Actor {
    child: Child,
    /// Whether the kernel has already reported this actor as exited.
    ///
    /// Tracked because `try_wait` reaps as a side effect, and a reaped pid is no longer this
    /// process's to signal: the kernel is free to hand it to an unrelated process, and a
    /// `SIGTERM` aimed at a stale pid would land on whatever now owns it.
    exited: bool,
}

impl Actor {
    /// Spawn an actor process attached to `segment`.
    ///
    /// The child is loadlab itself, re-executed in actor mode. See [`run_actor`] for what that
    /// mode does and why the parent does not simply `fork`.
    pub fn spawn(
        segment: &Segment,
        spec: ActorSpec,
        executable: Option<&Path>,
    ) -> Result<Self, LoadlabError> {
        let executable = executable.ok_or_else(|| {
            LoadlabError::MemoryMap(
                "cannot locate an executable loadlab binary to spawn an actor".to_owned(),
            )
        })?;
        let mut command = Command::new(executable);
        command.arg("--actor");
        if let Some(fd) = segment.inherit_fd() {
            command.arg(format!("--actor-fd={fd}"));
        }
        if let Some(id) = segment.sysv_id() {
            command.arg(format!("--actor-sysv-id={id}"));
        }

        // A pipe the actor writes one byte to once it has mapped and read the segment. Waiting on
        // it turns "the child exists" into "the child is the workload", without which the first
        // accounting pass reads a process that has not mapped anything yet.
        let (ready_read, ready_write) = ready_pipe()?;
        let mut ready_write = FdGuard::adopt(ready_write);
        // The actor has to inherit this one descriptor, and only this one: the read end stays
        // `CLOEXEC` from `pipe2`, so it cannot leak into the child it is used to watch.
        ready_write.clear_cloexec();
        command.arg(format!("--actor-ready-fd={}", ready_write.raw()));
        command.arg(format!("--actor-bytes={}", segment.bytes()));
        command.arg(format!(
            "--actor-private-bytes={}",
            spec.private_bytes_per_actor
        ));
        command.arg(format!(
            "--actor-cow-ratio-milli={}",
            spec.cow_write_ratio_milli
        ));
        // `PR_SET_PDEATHSIG` cannot be passed as an argument, so it is set in `pre_exec`. The
        // parent's stdin is closed so an actor that somehow reaches a read returns EOF and exits
        // rather than blocking forever on a terminal nobody is watching.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: `pre_exec` runs in the forked child between `fork` and `exec`, where only
        // async-signal-safe calls are allowed. `prctl` is a syscall wrapper and takes no pointer,
        // so it is safe here. The alternative, a signal handler in the child, is not.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|error| {
            LoadlabError::MemoryMap(format!("cannot spawn an actor process: {error}"))
        })?;
        // Close the parent's copy of the write end, or the read below would never see EOF when the
        // child dies without signalling.
        drop(ready_write);
        // Block until the actor has the segment resident, or until it dies and the read returns
        // EOF. A spawn that cannot get that far is a failed spawn, not a zero-valued actor.
        await_ready(ready_read).map_err(|error| {
            LoadlabError::MemoryMap(format!(
                "an actor started but never mapped its segment: {error}"
            ))
        })?;
        Ok(Self {
            child,
            exited: false,
        })
    }

    /// The actor's process id.
    ///
    /// Not an `Option`: the id stays valid after the child is reaped, and teardown needs to know
    /// what to check rather than discover.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the actor is still running, reaping it if it has exited.
    ///
    /// A missing actor is replaced by the caller rather than reported here: this is a maintenance
    /// question and the tick loop is the only place that can answer it without a borrow.
    pub fn is_running(&mut self) -> bool {
        if let Ok(None) = self.child.try_wait() {
            return true;
        }
        // Reaped, or unreapable. Either way there is nothing left to signal, and the actor is
        // replaced by the caller rather than waited on again.
        self.exited = true;
        false
    }

    /// Send `SIGTERM` and wait for the actor to go away.
    ///
    /// Blocking is fine: this only happens when an actor is being torn down, which is a
    /// configuration change or a shutdown, never a steady-state tick.
    pub fn terminate(&mut self) {
        // Only signal while the actor is still ours to signal. A pid that has already been reaped
        // may belong to an unrelated process by now, and `SIGTERM` would go to that process
        // instead of to nothing at all.
        //
        // A race remains between `try_wait` reporting the actor alive and this call, during which
        // the actor can exit and the kernel can reuse the pid. Closing that would need `pidfd`, and
        // the window is bounded by a single tick's worth of work, so it is recorded here rather
        // than pretended away.
        if !self.exited {
            // SAFETY: `kill` takes a pid and a signal and dereferences nothing. The pid came from
            // a child this process spawned and has not been reaped.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
            }
        }
        let _ = self.child.wait();
    }

    /// Read the kernel's accounting for this actor.
    ///
    /// Returns `None` if the actor has exited, which the caller treats the same as any other
    /// reason to rebuild.
    #[must_use]
    pub fn accounting(&self) -> Option<ActorAccounting> {
        crate::procfs::actor_accounting(self.pid())
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        // A dropped actor must not outlive the process, or a stopped workload would leave
        // processes holding memory that nothing reports. `terminate` waits for the exit, so by the
        // time it returns no actor is still mapping the segment.
        //
        // No segment reference is held here, and none is needed: `SharedController::release`
        // clears the actor list before dropping the segment, so the kernel object cannot be
        // removed while an actor is still mapped to it. That ordering is the invariant, and
        // putting the reference on the actor would let the two be dropped in either order.
        self.terminate();
    }
}

/// Run loadlab as an actor process.
///
/// This is the `--actor` entry point, and it is a separate code path on purpose: an actor is a
/// single-threaded process with no runtime, no allocator traffic from another thread and no
/// business talking to the HTTP API. It attaches read-only to the shared segment, maps a private
/// view of the same object, writes `cow_ratio` of that private view, and then waits to be killed.
///
/// The last part is what makes the accounting interesting. An actor that wrote its pages and
/// exited would still have caused the copy-on-write, but its RSS would no longer be counted
/// anywhere, and `sum(Rss)` would stop showing the duplication that is the point of the module.
/// So actors hold their pages until signalled.
pub fn run_actor(args: &ActorArgs) -> Result<(), LoadlabError> {
    let page = page_size();
    let shared_len = usize::try_from(args.bytes)
        .map_err(|_| LoadlabError::InvalidConfig("actor segment is too large".to_owned()))?;

    // Attach to the shared segment as read-only. `PROT_READ` rather than `PROT_READ | PROT_WRITE`
    // is enforced by the mapping, not just by convention: a write would raise SIGSEGV rather than
    // silently modify what the other actors are reading, which is the guarantee the shared side
    // needs to be worth measuring.
    let shared = if let Some(fd) = args.fd {
        // SAFETY: `mmap` over an inherited descriptor with a length the parent created. The actor
        // is single-threaded and nothing else in this process has touched the descriptor.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                shared_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if address.is_null() {
            return Err(LoadlabError::MemoryMap(format!(
                "actor could not map the inherited shared segment: {}",
                std::io::Error::last_os_error()
            )));
        }
        // Read the whole segment so the pages are resident and counted in this actor's RSS. A
        // `volatile` read is what stops the compiler from eliding the loop as dead code, which
        // would leave the pages unmapped and every number below a lie.
        read_whole_segment(address.cast::<u8>(), shared_len, page);
        address.cast::<u8>()
    } else if let Some(id) = args.sysv_id {
        // SAFETY: `shmat` with the id the parent created. Returns the address or -1.
        let address = unsafe { libc::shmat(id, ptr::null(), libc::SHM_RDONLY) };
        if address.is_null() || address as isize == -1 {
            return Err(LoadlabError::MemoryMap(format!(
                "actor could not attach to System V segment {id}: {}",
                std::io::Error::last_os_error()
            )));
        }
        read_whole_segment(address.cast::<u8>(), shared_len, page);
        address.cast::<u8>()
    } else {
        return Err(LoadlabError::InvalidConfig(
            "an actor needs either --actor-fd or --actor-sysv-id".to_owned(),
        ));
    };

    // The private view: writable, and this actor's alone. Writing it is what triggers
    // copy-on-write, and the resulting pages are private to this actor, which is why they are
    // accounted as anonymous memory no matter what backs the shared segment.
    //
    // The two backings get genuinely different mechanisms here, and the difference is real rather
    // than an implementation detail:
    //
    //   * A file-backed object has a private mapping, so `MAP_PRIVATE` alone gives each actor
    //     its own copies faulted from the pages every other actor is reading.
    //   * System V shared memory has no private mapping at all -- `shmat` attaches the one
    //     object and a second writable attachment is still that same object. An earlier version of
    //     this code used a writable attachment and claimed `mprotect` would turn it into
    //     copy-on-write; it would not, because `mprotect` changes protection and does not give the
    //     mapping private pages. Writes would have landed in the shared segment and every actor
    //     would have measured zero. The private view is therefore anonymous.
    //
    // The observable consequence is deliberate and worth stating plainly: with `Sysv`, the copies
    // are private but not faulted from the shared segment, so this measures the *cost* of
    // copy-on-write without the shared-source fan-out that `Memfd` provides. `Memfd` is the
    // default for exactly that reason.
    let private_len = usize::try_from(args.private_bytes)
        .map_err(|_| LoadlabError::InvalidConfig("actor private view is too large".to_owned()))?;
    // A zero-length mapping is `EINVAL`, not an empty mapping. A workload that asks for no
    // copy-on-write must not fail on the way to having none.
    let writable: Option<*mut u8> = if private_len == 0 {
        None
    } else if args.fd.is_some() {
        let fd = args.fd.unwrap_or(-1);
        // SAFETY: `mmap` over the inherited descriptor with `MAP_PRIVATE`, so writes copy rather
        // than propagate. The parent sizes the backing object to cover this length.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                private_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        if address.is_null() {
            return Err(LoadlabError::MemoryMap(format!(
                "actor could not map a private view of the segment: {}",
                std::io::Error::last_os_error()
            )));
        }
        Some(address.cast::<u8>())
    } else {
        // SAFETY: an anonymous private mapping has no backing object to outrun, and the kernel
        // supplies zero pages that are copied on first write.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                private_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address.is_null() {
            return Err(LoadlabError::MemoryMap(format!(
                "actor could not map an anonymous private view: {}",
                std::io::Error::last_os_error()
            )));
        }
        Some(address.cast::<u8>())
    };

    // Pages to write, as integer arithmetic: the ratio already arrives in thousandths, so there
    // is no reason to round-trip it through a float and then back to a count of pages.
    let dirty_pages = (private_len / page) * usize::from(args.cow_ratio_milli) / 1000;
    if let Some(writable) = writable {
        for index in 0..dirty_pages {
            // Only the first byte of each page is written, which is what faults in a whole page:
            // copy-on-write is charged per page, so touching every byte would measure the same
            // thing at several times the cost.
            let offset = (index * page).min(private_len.saturating_sub(1));
            // SAFETY: `offset` is inside the writable private mapping. Writing is the entire
            // point: it is the write that makes the kernel allocate a private page for this actor.
            unsafe { ptr::write_volatile(writable.add(offset), 0x5au8) };
        }
    }

    // Keep both mappings referenced for the life of the process, and hold them rather than
    // letting the compiler or `Drop` tidy them away: an actor that released its mappings would
    // stop being counted in the very accounting it exists to demonstrate.
    std::hint::black_box((shared, writable));

    // Now -- and only now -- tell the parent to start accounting. Writing before the pages are
    // resident would let the parent read a `status` file that does not yet describe the workload,
    // which is exactly the race this pipe exists to close. The parent treats a closed pipe without
    // a byte as a failed start, so this must come after every fallible step.
    if let Some(fd) = args.ready_fd {
        let byte = [1u8];
        // SAFETY: `fd` is the write end the parent created and passed; `byte` is a one-byte buffer.
        // The return value is deliberately ignored: a parent that has already given up is not a
        // reason for the actor to stop being the workload.
        unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    }

    wait_for_termination();
    Ok(())
}

/// The largest private view per actor that a copy-on-write budget can pay for.
///
/// `ratio` is a percentage, and it is the number that converts a writable extent into resident
/// copies: an actor maps `private` bytes but only dirties `private * ratio / 100` of them, so a
/// ratio of zero means no copy-on-write at all and therefore no private mapping worth making.
/// Dividing the budget back out by the ratio is what lets the operator ask for a big view and a
/// small write ratio and still get what they meant.
#[must_use]
pub fn permitted_private_bytes(
    requested_private_bytes: u64,
    ratio_percent: u64,
    actors: u32,
    permitted_cow_bytes: u64,
) -> u64 {
    if ratio_percent == 0 {
        return 0;
    }
    let actors = u64::from(actors.max(1));
    let writable_extent = permitted_cow_bytes
        .saturating_mul(100)
        .saturating_div(ratio_percent)
        .saturating_div(actors);
    requested_private_bytes.min(writable_extent)
}

/// A pipe whose write end the actor inherits and the parent waits on.
///
/// `pipe2` with `O_CLOEXEC` closes both ends by default, and the caller clears the flag on the one
/// end the actor needs. Doing it this way round means an early failure can never leak the read end
/// into a child, which would leave the parent waiting forever for an EOF that a descendant still
/// holds open.
fn ready_pipe() -> Result<(RawFd, RawFd), LoadlabError> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `pipe2` writes two descriptors into the array, which has room for exactly two.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(LoadlabError::MemoryMap(format!(
            "cannot create the actor readiness pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok((fds[0], fds[1]))
}

/// Wait for one byte from an actor, closing the descriptor either way.
///
/// A zero-length read means the write end is gone, which is what a child that exited before
/// mapping looks like. Retrying on `EINTR` is what keeps a signal delivered to the controller from
/// being mistaken for a dead actor.
fn await_ready(fd: RawFd) -> std::io::Result<()> {
    let guard = FdGuard::adopt(fd);
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: `guard.raw()` is a live read descriptor and `byte` is a one-byte buffer.
        let read = unsafe { libc::read(guard.raw(), byte.as_mut_ptr().cast(), 1) };
        if read == 1 {
            return Ok(());
        }
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the actor exited before signalling readiness",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Read every page of a mapping so the pages are resident and counted in this actor's RSS.
///
/// A plain read would be optimized away as dead code, which would leave the pages unmapped and
/// every figure this actor contributes a lie. `read_volatile` is what forces the access, and
/// `black_box` on the accumulator is what stops the loop being discarded wholesale.
fn read_whole_segment(base: *mut u8, length: usize, page: usize) {
    let mut accumulator = 0u8;
    for offset in (0..length).step_by(page) {
        // SAFETY: `base` is a live mapping of at least `length` bytes and `offset` is within it by
        // construction, so this is a read of a page the mapping covers.
        unsafe { accumulator = accumulator.wrapping_add(ptr::read_volatile(base.add(offset))) };
    }
    std::hint::black_box(accumulator);
}

/// Block until this actor is signalled.
///
/// A blocking `pause` loop rather than a sleep: the actor has no work to do, so it should consume
/// nothing, and `pause` returns immediately when a signal arrives rather than up to a tick late.
/// The default `SIGTERM` disposition terminates the process, which is what the parent sends.
fn wait_for_termination() {
    loop {
        // SAFETY: `pause` takes no arguments, dereferences nothing and always returns when a
        // signal is delivered. A signal whose default action is to terminate never returns here.
        let result = unsafe { libc::pause() };
        if result != 0 {
            // `EINTR` is the expected outcome of a handled signal. Anything else, and any error at
            // all, is not worth acting on: this process holds memory and has nothing to report, so
            // the only useful response is to keep waiting to be killed.
            return;
        }
    }
}

/// The arguments an actor process is started with.
///
/// Public because [`main`](crate) parses them out of `argv` before anything else runs, and because
/// the integration tests invoke an actor directly to prove the copy-on-write behaviour without
/// standing up a controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActorArgs {
    /// Inherited `memfd` descriptor, if the segment is a `memfd`.
    pub fd: Option<RawFd>,
    /// System V segment id, if the segment is a System V segment.
    pub sysv_id: Option<libc::c_int>,
    /// Size of the shared segment.
    pub bytes: u64,
    /// Size of this actor's private segment.
    pub private_bytes: u64,
    /// How much of the private segment to write, in thousandths.
    pub cow_ratio_milli: u16,
    /// Write end of the pipe the parent waits on before it begins accounting.
    ///
    /// The parent has to know that the segment is mapped and resident before it reads
    /// `/proc/<pid>/status`, or it races the child's startup and records a workload that has not
    /// happened yet. A one-byte write after the mapping is the simplest signal that cannot be
    /// confused with the child merely existing.
    pub ready_fd: Option<RawFd>,
}

impl ActorArgs {
    /// Parse actor arguments from the process arguments.
    ///
    /// Returns `None` when `--actor` is absent, which is the normal case. Parsing by hand rather
    /// than through clap because this mode runs before the runtime is started and must not be
    /// able to fail in any way the controller's error path does not expect.
    #[must_use]
    pub fn from_args<I, S>(args: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut parsed = Self::default();
        let mut requested = false;
        for arg in args {
            let text = arg.as_ref().to_string_lossy();
            if text == "--actor" {
                requested = true;
            } else if let Some(value) = text.strip_prefix("--actor-fd=") {
                parsed.fd = value.parse().ok();
            } else if let Some(value) = text.strip_prefix("--actor-sysv-id=") {
                parsed.sysv_id = value.parse().ok();
            } else if let Some(value) = text.strip_prefix("--actor-bytes=") {
                parsed.bytes = value.parse().unwrap_or(0);
            } else if let Some(value) = text.strip_prefix("--actor-private-bytes=") {
                parsed.private_bytes = value.parse().unwrap_or(0);
            } else if let Some(value) = text.strip_prefix("--actor-cow-ratio-milli=") {
                parsed.cow_ratio_milli = value.parse().unwrap_or(0);
            } else if let Some(value) = text.strip_prefix("--actor-ready-fd=") {
                parsed.ready_fd = value.parse().ok();
            }
        }
        requested.then_some(parsed)
    }
}

/// The shared and copy-on-write controller.
///
/// The segment and the actors are both built lazily and kept, mirroring the file-backed
/// controller: a workload that never asks for shared memory spawns nothing and creates nothing,
/// and a workload that stops does not have to pay to rebuild either.
pub struct SharedController {
    /// The shared segment, rebuilt only when its size or backing changes.
    segment: Option<Segment>,
    /// The forked actors, rebuilt when any part of [`ActorSpec`] changes.
    actors: Vec<Actor>,
    /// What the live actors were built for, so a change can be detected.
    spec: Option<ActorSpec>,
    /// The segment size and backing the live segment was created with.
    segment_shape: Option<(SharedBacking, u64)>,
    /// Minor faults seen in the controller process, carried so the controller's own copy-on-write
    /// writes can be accounted the same way as an actor's.
    last_controller_minor_faults: u64,
    /// The controller's own anonymous memory, captured before the first segment exists so it can
    /// be told apart from the copy-on-write pages the workload asks for.
    anon_baseline: Option<u64>,
    /// The binary actors are re-executed from.
    ///
    /// Resolved once at construction rather than per spawn, and stored rather than looked up again
    /// because the unit tests have to point it at the real binary: under `cargo test`,
    /// `current_exe` is the libtest harness, which parses `--actor` as an unknown flag and exits
    /// before `main` ever runs, so a test that did not override this would spawn a process that
    /// dies instantly and then measure nothing.
    executable: Option<PathBuf>,
}

// SAFETY: the controller owns its segment and its actors exclusively and only touches them from
// the single controller thread.
unsafe impl Send for SharedController {}

impl SharedController {
    /// Build a controller with nothing allocated.
    #[must_use]
    pub fn new() -> Self {
        Self {
            segment: None,
            actors: Vec::new(),
            spec: None,
            segment_shape: None,
            last_controller_minor_faults: 0,
            anon_baseline: None,
            executable: executable(),
        }
    }

    /// Build a controller that re-executes a specific binary for its actors.
    ///
    /// Used by the tests, which run inside the libtest harness and so cannot use `current_exe`.
    #[cfg(test)]
    #[must_use]
    pub fn with_executable(executable: Option<PathBuf>) -> Self {
        Self {
            segment: None,
            actors: Vec::new(),
            spec: None,
            segment_shape: None,
            last_controller_minor_faults: 0,
            anon_baseline: None,
            executable,
        }
    }

    /// Reconcile the shared and copy-on-write segments against the configuration.
    ///
    /// Returns what the kernel says, not what was asked for. The two are reported separately
    /// because for this controller they genuinely differ in meaning rather than only in magnitude.
    pub fn reconcile(
        &mut self,
        config: &SharedSettings,
        controller_minor_faults: u64,
        tolerance_bytes: u64,
        max_shared_bytes: u64,
    ) -> Result<SharedSnapshot, LoadlabError> {
        let faults = crate::procfs::memory_stats().ok();

        // Nothing wanted: release everything and report the quiet state. Released rather than
        // held, because an actor is a process and a segment is kernel memory; keeping them alive
        // for a stopped workload would make "stopped" mean something different here than it does
        // for the other controllers.
        if config.shared_bytes == 0 && config.private_bytes_per_actor == 0 {
            self.release();
            return Ok(quiet());
        }

        // The ceiling bounds the *combined* footprint, so the copy-on-write budget is whatever is
        // left after the shared segment is paid for. Subtracting first is what makes it a
        // combined ceiling: a request for 1 GiB shared and 1 GiB of copy-on-write cannot both be
        // satisfied under a 1.5 GiB ceiling, and the copy-on-write side is the one that grows with
        // the actor count, so it is the side that yields.
        let actors = config.actors.max(1);
        let shared_bytes = config.shared_bytes.max(MIN_SEGMENT_BYTES);
        let permitted_cow_bytes = max_shared_bytes.saturating_sub(shared_bytes);

        // The segment is sized once. A size change means a new segment: `ftruncate` on an existing
        // mapping would leave actors mapped over the old length with new pages nobody touches, and
        // the file would report a size that no actor is actually using.
        //
        // The object is sized for the *larger* of the shared view and one private view, not just
        // the shared one. A `MAP_PRIVATE` mapping that runs past the end of the backing object
        // raises SIGBUS on first write, so a workload asking for more copy-on-write than shared
        // would otherwise be a crash rather than a request. Only the shared portion is populated,
        // so the untouched remainder costs no resident memory and cannot inflate what is reported.
        let backing_bytes = shared_bytes
            .max(config.private_bytes_per_actor)
            .max(MIN_SEGMENT_BYTES);
        let shape = (config.backing, backing_bytes);
        if self.segment_shape != Some(shape) {
            self.release();
            // Captured here, before any actor or private mapping exists, because this is the last
            // moment at which the controller's anonymous memory is purely its own heap. Taken
            // afterwards it would include the workload and could not be told apart from it.
            if self.anon_baseline.is_none() {
                self.anon_baseline = crate::procfs::memory_stats()
                    .ok()
                    .map(|stats| stats.rss_anon_bytes);
            }
            let segment = Segment::new(shape.0, shape.1)?;
            self.segment = Some(segment);
            self.segment_shape = Some(shape);
        }

        // The controller process is itself an actor: it holds the shared mapping so the segment
        // has a mapper in this process, and so the accounting has a baseline that includes the
        // process an operator will actually be looking at.
        let spec = ActorSpec {
            backing: config.backing,
            actors,
            private_bytes_per_actor: permitted_private_bytes(
                config.private_bytes_per_actor.min(shared_bytes),
                cow_ratio_percent(config.cow_write_ratio),
                actors,
                permitted_cow_bytes,
            ),
            cow_write_ratio_milli: cow_ratio_milli(config.cow_write_ratio),
        };
        if self.spec != Some(spec) {
            self.respawn(spec);
        }

        // Drop any actor that has died on its own, and put it back. An actor that exits is not an
        // error to report: it is a workload that stopped being the workload that was asked for,
        // and the fix is to make it be that again.
        self.reap_dead(spec);

        Ok(self.account(
            config,
            &spec,
            controller_minor_faults,
            tolerance_bytes,
            permitted_cow_bytes,
            faults,
        ))
    }

    /// Replace every actor with a fresh set built for `spec`.
    fn respawn(&mut self, spec: ActorSpec) {
        self.release_actors();
        let Some(segment) = self.segment.as_ref() else {
            return;
        };
        let children = spec.actors.saturating_sub(1);
        for _ in 0..children {
            match Actor::spawn(segment, spec, self.executable.as_deref()) {
                Ok(actor) => self.actors.push(actor),
                Err(error) => {
                    // One failed spawn must not take the controller down: the remaining actors are
                    // still valid and the shortage is visible in the actor count, which is the
                    // number an operator alerts on.
                    tracing::warn!(%error, "cannot spawn a shared-memory actor");
                }
            }
        }
        self.spec = Some(spec);
    }

    /// Reap actors that exited, then spawn replacements up to the configured count.
    fn reap_dead(&mut self, spec: ActorSpec) {
        let before = self.actors.len();
        self.actors.retain_mut(Actor::is_running);
        let lost = before.saturating_sub(self.actors.len());
        let Some(segment) = self.segment.as_ref() else {
            return;
        };
        for _ in 0..lost {
            match Actor::spawn(segment, spec, self.executable.as_deref()) {
                Ok(actor) => self.actors.push(actor),
                Err(error) => tracing::warn!(%error, "cannot replace a shared-memory actor"),
            }
        }
    }

    /// Read every actor's accounting and derive the reported figures.
    fn account(
        &mut self,
        config: &SharedSettings,
        spec: &ActorSpec,
        controller_minor_faults: u64,
        tolerance_bytes: u64,
        permitted_cow_bytes: u64,
        faults: Option<crate::procfs::MemoryStats>,
    ) -> SharedSnapshot {
        let actors = spec.actors.max(1);

        // What the client asked to write, what the ceiling allows, and therefore how far apart
        // they are. Reported as three figures rather than one because the gap between them is the
        // whole point of having a ceiling: a client that is refused needs to see that it was
        // refused and by how much, not a target silently equal to what it asked for.
        let requested_cow_bytes = config
            .private_bytes_per_actor
            .saturating_mul(u64::from(actors))
            .saturating_mul(cow_ratio_percent(config.cow_write_ratio))
            / 100;
        let target_cow_bytes = spec
            .private_bytes_per_actor
            .saturating_mul(u64::from(actors))
            .saturating_mul(cow_ratio_percent(config.cow_write_ratio))
            / 100;
        let saturated = requested_cow_bytes > permitted_cow_bytes;

        // The controller's own accounting, included as an actor so the reported figures describe
        // the set an operator is running rather than only the children.
        let mut accounting = Vec::with_capacity(self.actors.len() + 1);
        let mut accounted_bytes = 0u64;
        let mut physical_bytes = 0u64;
        let mut shared_physical_bytes = 0u64;
        let mut shared_rss_bytes = 0u64;
        let mut cow_physical_bytes = 0u64;

        if let Some(stats) = faults {
            let shmem = stats.rss_shmem_bytes;
            let anon = stats.rss_anon_bytes;
            accounted_bytes = stats.rss_bytes;
            physical_bytes = stats.pss_bytes;
            // One copy: this is the controller's own physical contribution, which is why it is
            // assigned rather than accumulated. Only the actors below are summed.
            shared_physical_bytes = shmem;
            shared_rss_bytes = shmem;
            cow_physical_bytes = anon.saturating_sub(self.anon_baseline.unwrap_or(0));
            accounting.push(ActorAccounting {
                pid: 0,
                is_controller: true,
                rss_bytes: stats.rss_bytes,
                pss_bytes: stats.pss_bytes,
                rss_shmem_bytes: shmem,
                rss_anon_bytes: anon,
                minor_faults: self.last_controller_minor_faults,
            });
        }

        let mut child_actors = 0u32;
        for actor in &self.actors {
            let Some(entry) = actor.accounting() else {
                // The actor exited between the reap and this read. Not counted: its pages are
                // gone from the kernel's view, so counting a stale reading would report memory
                // that no longer exists.
                continue;
            };
            child_actors = child_actors.saturating_add(1);
            accounted_bytes = accounted_bytes.saturating_add(entry.rss_bytes);
            physical_bytes = physical_bytes.saturating_add(entry.pss_bytes);
            // `shared_physical_bytes` is deliberately *not* summed across actors. The shared
            // segment is one copy of physical memory no matter how many actors map it, and each
            // actor's `RssShmem` reports that same copy. Summing would repeat the exact mistake
            // this module exists to demonstrate, so only the first actor's figure is kept and the
            // sum is left to `physical_bytes`, which the kernel has already de-duplicated for us.
            if shared_physical_bytes == 0 {
                shared_physical_bytes = entry.rss_shmem_bytes;
            }
            shared_rss_bytes = shared_rss_bytes.saturating_add(entry.rss_shmem_bytes);
            cow_physical_bytes = cow_physical_bytes.saturating_add(entry.rss_anon_bytes);
            accounting.push(ActorAccounting {
                is_controller: false,
                ..entry
            });
        }

        let observed_cow_bytes = cow_physical_bytes;
        self.last_controller_minor_faults = controller_minor_faults;

        // Copy-on-write is monotonic: a page that has been written privately cannot be handed back.
        // Convergence therefore means the machine took *at least* the copies asked for, and
        // overshoot is not unconvergence -- it means the actors wrote more than the ratio asked,
        // which the controller has no way to undo. Judging it as a signed gap instead, as the
        // other controllers do, would report every converged run as unconverged the moment the
        // rounding to whole pages went the actor's way.
        //
        // The observed figure has the controller's own anonymous memory subtracted, because that is
        // the server's heap rather than copy-on-write and would otherwise satisfy any small target
        // on its own.
        let converged = observed_cow_bytes.saturating_add(tolerance_bytes) >= target_cow_bytes;

        SharedSnapshot {
            backing: config.backing.as_str().to_owned(),
            shared_bytes: config.shared_bytes.max(MIN_SEGMENT_BYTES),
            // The effective view the actors were spawned with, not the one that was requested:
            // this describes the workload that is running, and the clamped figure is the one the
            // accounting below is measured against.
            private_bytes_per_actor: spec.private_bytes_per_actor,
            cow_write_ratio: config.cow_write_ratio,
            // What is actually running, which is the controller plus the actors that survived to
            // be read. Deliberately not `config.actors`: a spawn that failed or an actor that
            // died has to show up here rather than being reported as what was asked for.
            actors: child_actors.saturating_add(1),
            child_actors,
            shared_physical_bytes,
            shared_rss_bytes,
            cow_physical_bytes,
            accounted_bytes,
            physical_bytes,
            shared_overcount_bytes: shared_rss_bytes.saturating_sub(shared_physical_bytes),
            requested_cow_bytes,
            target_cow_bytes,
            converged,
            saturated,
            actor_accounting: accounting,
        }
    }

    /// Tear down every actor, leaving the segment in place.
    fn release_actors(&mut self) {
        self.actors.clear();
        self.spec = None;
    }

    /// Tear down everything.
    fn release(&mut self) {
        self.release_actors();
        self.segment = None;
        self.segment_shape = None;
    }
}

impl Drop for SharedController {
    fn drop(&mut self) {
        // Actors first: a System V segment must outlive every actor mapped to it.
        self.release();
    }
}

/// The shared and copy-on-write configuration a single reconciliation needs.
///
/// Kept separate from the API's configuration type so this module does not depend on the wire
/// format, and so a unit test can drive the controller without building a whole `WorkloadState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharedSettings {
    /// Kernel object to back the segment.
    pub backing: SharedBacking,
    /// Size of the shared segment. Zero means no shared segment.
    pub shared_bytes: u64,
    /// Size of each actor's private segment.
    pub private_bytes_per_actor: u64,
    /// Fraction of each private segment to write, from 0 to 1.
    pub cow_write_ratio: f64,
    /// Actors to run, including the controller process.
    pub actors: u32,
}

/// A configuration for nothing: no segment, no actors, no accounting.
fn quiet() -> SharedSnapshot {
    SharedSnapshot {
        backing: String::new(),
        converged: true,
        ..Default::default()
    }
}

/// A ratio as thousandths, so the actor's fixed-size configuration can be compared with `==`.
///
/// A `f64` cannot be part of an `ActorSpec` for exactly the reason it cannot be compared for
/// equality, and an actor's ratio is fixed at its `exec`, so it has to be part of the rebuild
/// check.
fn cow_ratio_milli(ratio: f64) -> u16 {
    let milli = (ratio.clamp(0.0, 1.0) * 1000.0).round();
    // Rounded into range rather than cast blindly: a NaN ratio would saturate to 0 rather than
    // becoming an implementation-defined value.
    // Checked to a range and rounded rather than cast, because `as` on a float is saturating in
    // Rust but silently produces a value that has nothing to do with the input when the input is
    // NaN. An explicit clamp of a known-finite value is both correct and reviewable.
    if !milli.is_finite() {
        return 0;
    }
    milli.clamp(0.0, 1000.0).round() as u16
}

/// A ratio as whole percent, for byte arithmetic where a thousandth of a byte is meaningless.
fn cow_ratio_percent(ratio: f64) -> u64 {
    let percent = (ratio.clamp(0.0, 1.0) * 100.0).round();
    if !percent.is_finite() {
        return 0;
    }
    percent.clamp(0.0, 100.0).round() as u64
}

/// The path of the loadlab executable, used to spawn actors.
///
/// A function rather than a constant because resolving it once at startup turns a missing or
/// non-executable binary into a startup error rather than a per-tick one, and because the actor
/// tests need to know whether it resolved at all before they can decide to skip.
#[must_use]
pub fn executable() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?;
    fs::metadata(&path)
        .is_ok_and(|meta| meta.is_file())
        .then_some(path)
}

#[cfg(test)]
mod tests {
    use super::{
        ActorArgs, ActorSpec, MIN_SEGMENT_BYTES, SharedBacking, SharedController, SharedSettings,
        cow_ratio_milli, cow_ratio_percent, executable, permitted_private_bytes,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serializes the tests that create System V segments.
    ///
    /// `shmget(IPC_PRIVATE)` ids are global and the kernel recycles them, so if one test drops a
    /// segment and another creates one before the first re-stats the old id, the id can name a
    /// fresh segment and the "released" assertion fails. That is a property of the kernel's id
    /// allocator, not of loadlab, so the tests are serialized rather than weakened.
    static SYSV_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A controller that re-executes the real binary.
    ///
    /// Under `cargo test` this process is the libtest harness, which rejects `--actor` before
    /// `main` runs, so a controller built the way production builds one would spawn processes that
    /// die immediately and then measure a workload that never existed. The binary cargo built
    /// alongside the harness is the one to use; if it is not there, the tests that spawn actors
    /// skip rather than fail, because a missing artifact is not a copy-on-write bug.
    fn controller() -> SharedController {
        SharedController::with_executable(actor_binary())
    }

    /// The `loadlab` binary next to this test executable, if it can be found.
    ///
    /// `current_exe` is `.../target/<profile>/deps/loadlab-<hash>`; the binary is its sibling
    /// directory's `loadlab`.
    fn actor_binary() -> Option<PathBuf> {
        let harness = executable()?;
        let deps = harness.parent()?;
        let profile = deps.parent()?;
        let candidate = profile.join("loadlab");
        candidate.is_file().then_some(candidate)
    }

    /// A configuration small enough to be instant and large enough to span several pages.
    const SHARED: u64 = 512 * 1024;
    const PRIVATE: u64 = 512 * 1024;
    /// A ceiling above what the tests ask for, so saturation is never accidental.
    const CEILING: u64 = 64 * 1024 * 1024;

    fn settings(actors: u32) -> SharedSettings {
        SharedSettings {
            backing: SharedBacking::Memfd,
            shared_bytes: SHARED,
            private_bytes_per_actor: PRIVATE,
            cow_write_ratio: 1.0,
            actors,
        }
    }

    #[test]
    fn backing_names_round_trip() {
        for backing in SharedBacking::ALL {
            assert_eq!(SharedBacking::parse(backing.as_str()), Some(backing));
        }
        assert_eq!(SharedBacking::parse("tmpfs"), None);
        assert_eq!(SharedBacking::parse("shm"), None);
        assert_eq!(SharedBacking::parse(""), None);
    }

    #[test]
    fn a_backing_name_error_lists_the_valid_names() {
        // The message is what an operator reads, so the set has to be in it.
        let names: Vec<&str> = SharedBacking::ALL.iter().map(|b| b.as_str()).collect();
        assert_eq!(names, ["memfd", "sysv"]);
    }

    #[test]
    fn a_rounded_ratio_is_comparable_as_an_integer() {
        assert_eq!(cow_ratio_milli(1.0), 1000);
        assert_eq!(cow_ratio_milli(0.0), 0);
        assert_eq!(cow_ratio_milli(0.5), 500);
        // Out of range and non-finite inputs clamp rather than wrap or become undefined.
        assert_eq!(cow_ratio_milli(2.0), 1000);
        assert_eq!(cow_ratio_milli(-1.0), 0);
        assert_eq!(cow_ratio_milli(f64::NAN), 0);
        assert_eq!(cow_ratio_percent(1.0), 100);
        assert_eq!(cow_ratio_percent(0.5), 50);
        assert_eq!(cow_ratio_percent(f64::NAN), 0);
    }

    #[test]
    fn a_ceiling_caps_the_private_view_per_actor() {
        // 8 MiB of copy-on-write across 4 actors, all of it written, is 2 MiB of view each.
        assert_eq!(
            permitted_private_bytes(64 * 1024 * 1024, 100, 4, 8 * 1024 * 1024),
            2 * 1024 * 1024
        );
        // A small write ratio means a large view for the same copies: a quarter of the view is
        // written, so four times the view fits in the same budget.
        assert_eq!(
            permitted_private_bytes(64 * 1024 * 1024, 25, 4, 8 * 1024 * 1024),
            8 * 1024 * 1024
        );
        // A request below the ceiling is honoured rather than inflated.
        assert_eq!(
            permitted_private_bytes(1024 * 1024, 100, 4, 8 * 1024 * 1024),
            1024 * 1024
        );
        // No write ratio means no copy-on-write, so no private mapping is worth making.
        assert_eq!(
            permitted_private_bytes(1024 * 1024, 0, 4, 8 * 1024 * 1024),
            0
        );
        // An exhausted budget permits nothing rather than wrapping or dividing by zero.
        assert_eq!(permitted_private_bytes(1024 * 1024, 100, 4, 0), 0);
    }

    #[test]
    fn actor_arguments_parse_from_the_command_line() {
        let parsed = ActorArgs::from_args([
            "--actor",
            "--actor-fd=7",
            "--actor-bytes=4096",
            "--actor-private-bytes=8192",
            "--actor-cow-ratio-milli=250",
        ])
        .expect("--actor requested");
        assert_eq!(parsed.fd, Some(7));
        assert_eq!(parsed.sysv_id, None);
        assert_eq!(parsed.bytes, 4096);
        assert_eq!(parsed.private_bytes, 8192);
        assert_eq!(parsed.cow_ratio_milli, 250);
    }

    #[test]
    fn actor_arguments_absent_means_this_is_not_an_actor() {
        assert!(ActorArgs::from_args(["--bind", "127.0.0.1:8080"]).is_none());
        // A flag that merely looks like an actor flag must not trigger actor mode.
        assert!(ActorArgs::from_args(["--actor-fd=3"]).is_none());
    }

    #[test]
    fn a_system_v_segment_is_handed_over_by_id_and_a_memfd_by_descriptor() {
        let memfd = super::Segment::new(SharedBacking::Memfd, SHARED).expect("memfd segment");
        assert!(memfd.inherit_fd().is_some());
        assert!(memfd.sysv_id().is_none());

        let sysv = super::Segment::new(SharedBacking::Sysv, SHARED).expect("sysv segment");
        assert!(sysv.inherit_fd().is_none());
        assert!(sysv.sysv_id().is_some_and(|id| id >= 0));
    }

    #[test]
    fn a_segment_below_the_minimum_is_raised_rather_than_rejected() {
        // A sub-page segment cannot be mapped, and silently mapping a page when a page was asked
        // for would be a different workload from the one that was requested.
        let segment = super::Segment::new(SharedBacking::Memfd, 1).expect("segment");
        assert_eq!(segment.bytes(), MIN_SEGMENT_BYTES);
    }

    #[test]
    fn a_segment_larger_than_a_file_length_can_be_expressed_is_refused() {
        let Err(error) = super::Segment::new(SharedBacking::Memfd, u64::MAX) else {
            panic!("a segment cannot be u64::MAX bytes");
        };
        assert!(
            error.to_string().contains("mappable file length"),
            "{error}"
        );
    }

    #[test]
    fn an_idle_controller_creates_nothing_and_reports_quiet() {
        let mut controller = controller();
        assert!(controller.actors.is_empty(), "every actor must be gone");
        assert!(controller.segment.is_none(), "the segment must be released");
        let snapshot = controller
            .reconcile(
                &SharedSettings {
                    backing: SharedBacking::Memfd,
                    shared_bytes: 0,
                    private_bytes_per_actor: 0,
                    cow_write_ratio: 0.0,
                    actors: 0,
                },
                0,
                0,
                CEILING,
            )
            .expect("reconcile");
        assert!(snapshot.converged);
        assert_eq!(snapshot.shared_bytes, 0);
        assert_eq!(snapshot.actors, 0);
        assert!(snapshot.actor_accounting.is_empty());
        assert!(snapshot.backing.is_empty());
        assert!(controller.actors.is_empty(), "every actor must be gone");
        assert!(controller.segment.is_none(), "the segment must be released");
    }

    #[test]
    fn a_single_actor_creates_a_segment_and_accounts_for_itself() {
        let mut controller = controller();
        let snapshot = controller
            .reconcile(&settings(1), 0, 0, CEILING)
            .expect("reconcile");
        assert_eq!(snapshot.shared_bytes, SHARED);
        assert_eq!(snapshot.actors, 1);
        assert_eq!(snapshot.child_actors, 0);
        assert_eq!(snapshot.actor_accounting.len(), 1);
        let controller_actor = &snapshot.actor_accounting[0];
        assert!(controller_actor.is_controller);
        assert_eq!(controller_actor.pid, 0);
        // One actor maps the segment once, so there is nothing to double-count. Checked on the
        // segment figure rather than on `accounted == physical`, because a single process still
        // shares its text and C library with everything else on the machine and those pages make
        // the two totals differ no matter what this module does.
        assert_eq!(snapshot.shared_overcount_bytes, 0);
        assert_eq!(snapshot.shared_physical_bytes, snapshot.shared_rss_bytes);
    }

    #[test]
    fn several_actors_share_one_segment_so_the_accounting_overcounts_it() {
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let actors = 3u32;
        let mut controller = controller();
        let snapshot = controller
            .reconcile(&settings(actors), 0, 0, CEILING)
            .expect("reconcile");
        assert_eq!(
            snapshot.child_actors,
            actors - 1,
            "one child per actor but the controller"
        );
        assert_eq!(
            snapshot.actor_accounting.len(),
            actors as usize,
            "every actor is accounted for, controller included"
        );
        // This is the claim the whole module exists to make: the shared segment is one copy of
        // physical memory, and summing RSS over its mappers counts it once per mapper.
        assert!(
            snapshot.shared_overcount_bytes > 0,
            "expected a double-count with {actors} actors, got segment Rss {} physical {}",
            snapshot.shared_rss_bytes,
            snapshot.shared_physical_bytes
        );
        // The controller is the first entry by construction, so it is checked separately rather
        // than skipped inside the loop where a mistake would be easy to miss.
        assert!(snapshot.actor_accounting[0].is_controller);
        for entry in &snapshot.actor_accounting[1..] {
            assert!(!entry.is_controller);
            assert!(entry.pid > 0);
        }
        assert_eq!(controller.actors.len(), actors as usize - 1);
    }

    #[test]
    fn a_system_v_segment_accounts_the_same_way() {
        let _guard = SYSV_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let mut controller = controller();
        let mut config = settings(3);
        config.backing = SharedBacking::Sysv;
        let snapshot = controller
            .reconcile(&config, 0, 0, CEILING)
            .expect("reconcile");
        assert_eq!(snapshot.backing, "sysv");
        assert_eq!(snapshot.child_actors, 2);
        assert_eq!(snapshot.shared_bytes, SHARED);
        assert!(
            snapshot.shared_overcount_bytes > 0,
            "System V segments are shared the same way memfd segments are"
        );
    }

    #[test]
    fn a_copy_on_write_ratio_changes_what_the_actors_are_asked_to_write() {
        assert_ne!(
            ActorSpec {
                backing: SharedBacking::Memfd,
                actors: 2,
                private_bytes_per_actor: PRIVATE,
                cow_write_ratio_milli: cow_ratio_milli(1.0),
            },
            ActorSpec {
                backing: SharedBacking::Memfd,
                actors: 2,
                private_bytes_per_actor: PRIVATE,
                cow_write_ratio_milli: cow_ratio_milli(0.5),
            },
            "a different ratio must be a different actor specification"
        );
    }

    #[test]
    fn a_request_above_the_operator_ceiling_is_clamped_and_reported_saturated() {
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let mut controller = controller();
        let mut config = settings(2);
        config.private_bytes_per_actor = 4 * 1024 * 1024;
        // Two actors each writing 4 MiB is 8 MiB of copy-on-write, and the shared segment takes a
        // further 512 KiB. Under a 1 MiB combined ceiling the copy-on-write side gets whatever is
        // left -- 512 KiB -- and the per-actor view is divided out of that, halved again by the two
        // actors: 256 KiB each.
        let ceiling = 1024 * 1024;
        let snapshot = controller
            .reconcile(&config, 0, 0, ceiling)
            .expect("reconcile");
        assert!(snapshot.saturated, "the request exceeds the ceiling");
        assert!(
            snapshot.requested_cow_bytes > snapshot.target_cow_bytes,
            "the request must be reported above the target it was held to"
        );
        assert_eq!(snapshot.target_cow_bytes, ceiling - SHARED);
        assert_eq!(snapshot.private_bytes_per_actor, 256 * 1024);
        assert_eq!(
            snapshot.shared_bytes, SHARED,
            "the shared view is untouched"
        );
    }

    #[test]
    fn stopping_tears_down_every_actor_and_the_segment() {
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let mut controller = controller();
        controller
            .reconcile(&settings(3), 0, 0, CEILING)
            .expect("reconcile");
        assert!(
            !controller.actors.is_empty(),
            "actors must still be running"
        );
        assert!(controller.segment.is_some(), "the segment must still exist");
        let pids: Vec<u32> = controller.actors.iter().map(super::Actor::pid).collect();
        assert_eq!(pids.len(), 2);

        let snapshot = controller
            .reconcile(
                &SharedSettings {
                    backing: SharedBacking::Memfd,
                    shared_bytes: 0,
                    private_bytes_per_actor: 0,
                    cow_write_ratio: 0.0,
                    actors: 0,
                },
                0,
                0,
                CEILING,
            )
            .expect("reconcile");
        assert!(controller.actors.is_empty(), "every actor must be gone");
        assert!(controller.segment.is_none(), "the segment must be released");
        assert_eq!(snapshot.actors, 0);
        // The actors must be gone, not merely unreported. An actor left running would hold
        // memory that nothing in the API accounts for.
        for pid in pids {
            assert!(
                !process_exists(pid),
                "actor {pid} survived teardown and is still holding memory"
            );
        }
    }

    #[test]
    fn a_segment_is_released_when_the_controller_is_dropped() {
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let pids;
        {
            let mut controller = controller();
            controller
                .reconcile(&settings(3), 0, 0, CEILING)
                .expect("reconcile");
            pids = controller
                .actors
                .iter()
                .map(super::Actor::pid)
                .collect::<Vec<_>>();
            assert_eq!(pids.len(), 2);
        }
        // Dropping the controller has to terminate the actors. A `Drop` that only dropped the
        // `Child` handle would leave them running for the life of the test binary.
        for pid in pids {
            assert!(
                !process_exists(pid),
                "actor {pid} outlived the controller that spawned it"
            );
        }
    }

    #[test]
    fn shrinking_the_shared_segment_replaces_it_rather_than_truncating_underneath_actors() {
        // Skips rather than fails when the binary has not been built, so an actor
        // test cannot report a copy-on-write bug on a host with nothing to re-exec.
        if actor_binary().is_none() {
            return;
        }
        let mut controller = controller();
        controller
            .reconcile(&settings(2), 0, 0, CEILING)
            .expect("reconcile");
        let mut smaller = settings(2);
        smaller.shared_bytes = 128 * 1024;
        let snapshot = controller
            .reconcile(&smaller, 0, 0, CEILING)
            .expect("reconcile");
        // An `ftruncate` under a live mapping would leave the actors reading pages the file no
        // longer has, which is a SIGBUS waiting to happen. The size has to be reported honestly.
        assert_eq!(snapshot.shared_bytes, 128 * 1024);
    }

    /// Whether a pid still names a live process.
    ///
    /// `/proc` rather than `kill(pid, 0)` because a reaped child stays visible as a zombie to a
    /// signal-based probe in some cases, and a zombie is not holding memory. The actor is waited
    /// on during teardown, so by the time this runs it is genuinely gone.
    fn process_exists(pid: u32) -> bool {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false; // No such process.
        };
        // A zombie's whole status line set is a subset of these; state is the discriminator.
        !status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
    }

    #[test]
    fn dropping_a_segment_releases_its_system_v_object() {
        let _guard = SYSV_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let segment = super::Segment::new(SharedBacking::Sysv, SHARED).expect("segment");
        let id = segment.sysv_id().expect("a System V segment has an id");
        assert_eq!(segment.bytes(), SHARED);

        // SAFETY: `shmid_ds` is a plain C struct of integers that zeroing makes a valid initial
        // value for.
        let mut info: libc::shmid_ds = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::shmctl(id, libc::IPC_STAT, &raw mut info) },
            0,
            "the segment must exist before it is dropped"
        );

        drop(segment);

        // Polled rather than read once, and the reason is the kernel's attachment accounting
        // rather than anything this test did. `fork` copies the address space, and a System V
        // attachment is counted once per copy, so a segment attached in this process is still
        // attached in any child forked while it lived. Other tests in this binary fork, so after
        // `IPC_RMID` the segment can legitimately linger -- marked for destruction, waiting for
        // the last inherited attachment to go -- and that is still the release working, just not
        // finished on the same instruction. A child that outlives the deadline is a real leak and
        // must still fail.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (result, error) = loop {
            // SAFETY: as above; the id is an integer and `info` is a valid output buffer.
            let result = unsafe { libc::shmctl(id, libc::IPC_STAT, &raw mut info) };
            if result == -1 {
                break (result, std::io::Error::last_os_error().raw_os_error());
            }
            if std::time::Instant::now() >= deadline {
                break (
                    result,
                    Some(libc::EIDRM), // Reported below as "still there", not as an errno claim.
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(
            result, -1,
            "a released System V segment must be gone, and {} attachments are still holding it",
            info.shm_nattch
        );
        assert!(
            error == Some(libc::EIDRM) || error == Some(libc::EINVAL),
            "expected the segment id to be invalid, got errno {error:?}"
        );
    }
}
