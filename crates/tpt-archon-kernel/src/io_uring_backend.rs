//! Real `io_uring`-backed async I/O reactor (Linux only).
//!
//! [`scheduler`](crate::scheduler) is a hand-rolled, synchronous-poll,
//! cooperative round-robin scheduler with no I/O concept of its own — a
//! [`Task`](crate::scheduler::Task)'s `poll` either finishes synchronously or
//! yields [`Poll::Pending`](crate::scheduler::Poll::Pending) and is re-queued.
//! This module plugs a real Linux `io_uring` completion queue into that same
//! model without changing it: [`Reactor`] owns the ring, [`Reactor::poll_completions`]
//! submits queued SQEs and reaps whatever CQEs are ready *without blocking*,
//! and [`IoReadTask`]/[`IoWriteTask`] are ordinary [`Task`](crate::scheduler::Task)s
//! that poll the reactor once per tick and report `Pending` until their own
//! completion shows up.
//!
//! # Why this preserves the scheduler's deadlock-freedom argument
//!
//! `formal-proofs/scheduler.telos` models the ready queue and proves progress
//! holds as long as a `Pending` poll never removes a task from eventual
//! re-scheduling and no lock is held across `poll`. Both hold here: a
//! `Pending` result from an I/O task always leaves it in the scheduler's ready
//! queue (same as any other pending task — there is no separate "awaiting I/O"
//! queue to fall out of), and `poll_completions` never blocks — it only
//! submits already-queued SQEs and drains whatever the kernel has already
//! completed, so no task can stall another by holding a lock.
//!
//! # Buffer safety
//!
//! `io_uring` requires a submitted buffer's memory to stay valid until its
//! completion is reaped. Each task owns its buffer as a `Vec<u8>` stored
//! inline in the task struct; moving the task (e.g. the scheduler popping its
//! `Box<dyn Task>` off the front of the ready queue) only moves the `Box`
//! pointer, never the `Vec`'s heap allocation, so the buffer address stays
//! stable across ticks. A task must not be dropped while its operation is
//! still in flight (this module never does so internally, since a task is
//! only removed from the scheduler after it returns `Ready`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::os::unix::io::RawFd;
use std::rc::Rc;

use io_uring::{opcode, types, IoUring};

use crate::scheduler::{Poll, Task};

/// The outcome of one submitted operation.
enum IoOutcome {
    /// Submitted, not yet observed as complete.
    Pending,
    /// Completed; `res` is the raw `io_uring` CQE result (bytes transferred,
    /// or a negative `-errno` on failure — same convention as the underlying
    /// syscall).
    Done { res: i32 },
}

/// Owns the `io_uring` submission/completion rings and correlates completions
/// back to the caller that submitted them.
///
/// Single-threaded by design, matching the rest of the crate's
/// `Rc<RefCell<_>>` concurrency model (see [`crate::ipc`]) — share one
/// `Reactor` across tasks via `Rc<RefCell<Reactor>>`.
pub struct Reactor {
    ring: IoUring,
    next_op_id: u64,
    pending: BTreeMap<u64, Rc<RefCell<IoOutcome>>>,
}

impl Reactor {
    /// Creates a reactor with a submission/completion ring of the given
    /// capacity (rounded up to a power of two by the kernel).
    pub fn new(entries: u32) -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(entries)?,
            next_op_id: 0,
            pending: BTreeMap::new(),
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_op_id;
        self.next_op_id += 1;
        id
    }

    /// Queues a read of `buf.len()` bytes from `fd` at `offset`. Returns a
    /// handle the caller polls via [`Reactor::poll_completions`] +
    /// [`Reactor::take_result`].
    ///
    /// # Safety
    /// `buf` must remain valid (not moved, freed, or aliased) until this
    /// operation's result has been observed via [`Reactor::take_result`].
    pub unsafe fn submit_read(&mut self, fd: RawFd, buf: &mut [u8], offset: u64) -> u64 {
        let op_id = self.next_id();
        let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .user_data(op_id);
        self.push(entry);
        self.pending
            .insert(op_id, Rc::new(RefCell::new(IoOutcome::Pending)));
        op_id
    }

    /// Queues a write of `buf` to `fd` at `offset`. Same safety contract as
    /// [`Reactor::submit_read`].
    ///
    /// # Safety
    /// `buf` must remain valid until the result is observed.
    pub unsafe fn submit_write(&mut self, fd: RawFd, buf: &[u8], offset: u64) -> u64 {
        let op_id = self.next_id();
        let entry = opcode::Write::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .user_data(op_id);
        self.push(entry);
        self.pending
            .insert(op_id, Rc::new(RefCell::new(IoOutcome::Pending)));
        op_id
    }

    fn push(&mut self, entry: io_uring::squeue::Entry) {
        // SAFETY: the entry's buffer pointer validity is the caller's
        // contract per `submit_read`/`submit_write`'s own safety doc.
        while unsafe { self.ring.submission().push(&entry) }.is_err() {
            // Submission queue full: flush what's queued to make room. This
            // still never blocks waiting on a *completion*.
            let _ = self.ring.submit();
        }
    }

    /// Submits any queued SQEs and reaps whatever CQEs the kernel has already
    /// produced. Never blocks (does not wait for new completions). Returns
    /// the number of completions reaped.
    pub fn poll_completions(&mut self) -> io::Result<usize> {
        self.ring.submit()?;
        let mut completed = 0;
        // Collect first (completion queue borrows `self.ring` mutably) so we
        // can then update `self.pending` without a second live borrow.
        let cqes: std::vec::Vec<(u64, i32)> = self
            .ring
            .completion()
            .map(|cqe| (cqe.user_data(), cqe.result()))
            .collect();
        for (op_id, res) in cqes {
            if let Some(outcome) = self.pending.remove(&op_id) {
                *outcome.borrow_mut() = IoOutcome::Done { res };
            }
            completed += 1;
        }
        Ok(completed)
    }

    /// If `op_id`'s operation has completed, removes and returns its raw
    /// result (bytes transferred, or negative `-errno`). Returns `None` if
    /// still pending or unknown.
    fn take_result(&mut self, op_id: u64) -> Option<i32> {
        match self.pending.get(&op_id) {
            Some(outcome) => match &*outcome.borrow() {
                IoOutcome::Done { res } => Some(*res),
                IoOutcome::Pending => None,
            },
            None => None,
        }
    }
}

/// Which phase of its (single) operation an I/O task is in.
enum Phase {
    NotSubmitted,
    Submitted(u64),
    Done(i32),
}

/// A [`Task`] that reads `buf.len()` bytes from `fd` at `offset` via the
/// shared [`Reactor`], yielding `Pending` until the read completes.
pub struct IoReadTask {
    reactor: Rc<RefCell<Reactor>>,
    fd: RawFd,
    offset: u64,
    buf: std::vec::Vec<u8>,
    phase: Phase,
}

impl IoReadTask {
    /// Creates a task reading `len` bytes from `fd` at `offset`.
    pub fn new(reactor: Rc<RefCell<Reactor>>, fd: RawFd, offset: u64, len: usize) -> Self {
        Self {
            reactor,
            fd,
            offset,
            buf: alloc::vec![0u8; len],
            phase: Phase::NotSubmitted,
        }
    }

    /// The raw `io_uring` result once the task has reached [`Poll::Ready`]:
    /// bytes read on success, or a negative `-errno` on failure. Panics if
    /// called before completion.
    pub fn result(&self) -> i32 {
        match self.phase {
            Phase::Done(res) => res,
            _ => panic!("IoReadTask::result called before completion"),
        }
    }

    /// The read buffer. Only meaningful bytes up to `result()` (if positive)
    /// were actually filled by the kernel.
    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }
}

impl Task for IoReadTask {
    fn poll(&mut self) -> Poll {
        match self.phase {
            Phase::NotSubmitted => {
                // SAFETY: `self.buf` is owned by this task and not moved or
                // touched again until `take_result` reports completion.
                let op_id = unsafe {
                    self.reactor
                        .borrow_mut()
                        .submit_read(self.fd, &mut self.buf, self.offset)
                };
                self.phase = Phase::Submitted(op_id);
                let _ = self.reactor.borrow_mut().poll_completions();
                Poll::Pending
            }
            Phase::Submitted(op_id) => {
                let _ = self.reactor.borrow_mut().poll_completions();
                match self.reactor.borrow_mut().take_result(op_id) {
                    Some(res) => {
                        self.phase = Phase::Done(res);
                        Poll::Ready
                    }
                    None => Poll::Pending,
                }
            }
            Phase::Done(_) => Poll::Ready,
        }
    }
}

/// A [`Task`] that writes `buf` to `fd` at `offset` via the shared
/// [`Reactor`], yielding `Pending` until the write completes.
pub struct IoWriteTask {
    reactor: Rc<RefCell<Reactor>>,
    fd: RawFd,
    offset: u64,
    buf: std::vec::Vec<u8>,
    phase: Phase,
}

impl IoWriteTask {
    /// Creates a task writing `data` to `fd` at `offset`.
    pub fn new(
        reactor: Rc<RefCell<Reactor>>,
        fd: RawFd,
        offset: u64,
        data: std::vec::Vec<u8>,
    ) -> Self {
        Self {
            reactor,
            fd,
            offset,
            buf: data,
            phase: Phase::NotSubmitted,
        }
    }

    /// The raw `io_uring` result once the task has reached [`Poll::Ready`]:
    /// bytes written on success, or a negative `-errno` on failure. Panics if
    /// called before completion.
    pub fn result(&self) -> i32 {
        match self.phase {
            Phase::Done(res) => res,
            _ => panic!("IoWriteTask::result called before completion"),
        }
    }
}

impl Task for IoWriteTask {
    fn poll(&mut self) -> Poll {
        match self.phase {
            Phase::NotSubmitted => {
                // SAFETY: `self.buf` is owned by this task and not moved or
                // touched again until `take_result` reports completion.
                let op_id = unsafe {
                    self.reactor
                        .borrow_mut()
                        .submit_write(self.fd, &self.buf, self.offset)
                };
                self.phase = Phase::Submitted(op_id);
                let _ = self.reactor.borrow_mut().poll_completions();
                Poll::Pending
            }
            Phase::Submitted(op_id) => {
                let _ = self.reactor.borrow_mut().poll_completions();
                match self.reactor.borrow_mut().take_result(op_id) {
                    Some(res) => {
                        self.phase = Phase::Done(res);
                        Poll::Ready
                    }
                    None => Poll::Pending,
                }
            }
            Phase::Done(_) => Poll::Ready,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    /// A named, then immediately unlinked, temp file — the open fd stays
    /// valid (standard POSIX semantics) for as long as `f` is held, which is
    /// all `io_uring` needs, and nothing is left behind on disk.
    fn temp_file_with(contents: &[u8]) -> std::fs::File {
        let path = std::env::temp_dir().join(format!(
            "tpt-archon-kernel-io-uring-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        f.write_all(contents).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f
    }

    #[test]
    fn read_task_yields_correct_bytes() {
        let f = temp_file_with(b"hello io_uring");
        let reactor = Rc::new(RefCell::new(
            Reactor::new(8).expect("io_uring not available"),
        ));
        let mut task = IoReadTask::new(reactor, f.as_raw_fd(), 0, b"hello io_uring".len());
        let mut ticks = 0;
        while task.poll() == Poll::Pending {
            ticks += 1;
            assert!(ticks < 100_000, "read never completed");
        }
        assert_eq!(task.result(), b"hello io_uring".len() as i32);
        assert_eq!(task.buffer(), b"hello io_uring");
        drop(f);
    }

    #[test]
    fn read_task_completes_through_scheduler() {
        let f = temp_file_with(b"round trip through the scheduler");
        let reactor = Rc::new(RefCell::new(
            Reactor::new(8).expect("io_uring not available"),
        ));
        let mut scheduler = Scheduler::new();
        let fd = f.as_raw_fd();
        let task = IoReadTask::new(reactor, fd, 0, b"round trip through the scheduler".len());
        let id = scheduler.spawn(alloc::boxed::Box::new(task));
        let mut ticks = 0;
        loop {
            match scheduler.tick() {
                Some((got_id, Poll::Ready)) => {
                    assert_eq!(got_id, id);
                    break;
                }
                Some((_, Poll::Pending)) => {
                    ticks += 1;
                    assert!(ticks < 100_000, "read never completed");
                }
                None => panic!("task disappeared before completing"),
            }
        }
        drop(f);
    }

    #[test]
    fn write_task_persists_data() {
        let f = temp_file_with(b"");
        let reactor = Rc::new(RefCell::new(
            Reactor::new(8).expect("io_uring not available"),
        ));
        let mut write = IoWriteTask::new(
            reactor.clone(),
            f.as_raw_fd(),
            0,
            b"written via io_uring".to_vec(),
        );
        let mut ticks = 0;
        while write.poll() == Poll::Pending {
            ticks += 1;
            assert!(ticks < 100_000, "write never completed");
        }
        assert_eq!(write.result(), b"written via io_uring".len() as i32);

        let mut read = IoReadTask::new(reactor, f.as_raw_fd(), 0, b"written via io_uring".len());
        ticks = 0;
        while read.poll() == Poll::Pending {
            ticks += 1;
            assert!(ticks < 100_000, "read-back never completed");
        }
        assert_eq!(read.buffer(), b"written via io_uring");
        drop(f);
    }
}
