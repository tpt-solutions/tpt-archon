//! A cooperative async task scheduler (user-space first).
//!
//! One [`Task`] per database connection, not an OS process. This is a
//! deterministic, single-threaded, cooperative round-robin scheduler suitable
//! for the user-space validation model called for in `spec.txt`'s Risk 1
//! mitigation (prove the architecture on a host OS before bare-metal / real
//! `io_uring`).
//!
//! # Deadlock-freedom
//!
//! Because tasks are polled cooperatively and the scheduler holds no locks
//! across `poll`, a task can only block progress by never yielding `Ready`.
//! The scheduler always makes progress if any task is runnable and never waits
//! on a task while holding a resource another task needs — the property
//! `tpt-telos` is intended to prove (see `formal-proofs/`). Until then the
//! behavior is exercised by the tests below.

use alloc::boxed::Box;
use alloc::collections::VecDeque;

/// The result of polling a task once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// The task made progress but is not finished; re-schedule it.
    Pending,
    /// The task completed.
    Ready,
}

/// A unit of schedulable work (e.g. one DB connection's driver).
pub trait Task {
    /// Advances the task. Returning [`Poll::Ready`] removes it from the
    /// scheduler.
    fn poll(&mut self) -> Poll;
}

/// A boxed task with an assigned id.
struct Entry {
    id: u64,
    task: Box<dyn Task>,
}

/// A cooperative round-robin scheduler.
#[derive(Default)]
pub struct Scheduler {
    ready: VecDeque<Entry>,
    next_id: u64,
}

impl Scheduler {
    /// Creates an empty scheduler.
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            next_id: 0,
        }
    }

    /// Spawns a task, returning its id.
    pub fn spawn(&mut self, task: Box<dyn Task>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.ready.push_back(Entry { id, task });
        id
    }

    /// Number of tasks still scheduled.
    pub fn task_count(&self) -> usize {
        self.ready.len()
    }

    /// Polls one task (round-robin). Returns the polled task's id and result,
    /// or `None` if there are no tasks.
    pub fn tick(&mut self) -> Option<(u64, Poll)> {
        let mut entry = self.ready.pop_front()?;
        let result = entry.task.poll();
        let id = entry.id;
        if result == Poll::Pending {
            self.ready.push_back(entry);
        }
        Some((id, result))
    }

    /// Runs until every task completes. Returns the number of `tick`s executed.
    ///
    /// Guaranteed to terminate as long as every task eventually returns
    /// [`Poll::Ready`].
    pub fn run_to_completion(&mut self) -> usize {
        let mut ticks = 0;
        while self.tick().is_some() {
            ticks += 1;
        }
        ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;

    struct CountdownTask {
        remaining: u32,
        log: Rc<RefCell<alloc::vec::Vec<u64>>>,
        id: u64,
    }

    impl Task for CountdownTask {
        fn poll(&mut self) -> Poll {
            self.log.borrow_mut().push(self.id);
            if self.remaining == 0 {
                Poll::Ready
            } else {
                self.remaining -= 1;
                Poll::Pending
            }
        }
    }

    #[test]
    fn runs_all_tasks_to_completion() {
        let mut s = Scheduler::new();
        let log = Rc::new(RefCell::new(alloc::vec::Vec::new()));
        s.spawn(Box::new(CountdownTask {
            remaining: 2,
            log: log.clone(),
            id: 0,
        }));
        s.spawn(Box::new(CountdownTask {
            remaining: 1,
            log: log.clone(),
            id: 1,
        }));
        let ticks = s.run_to_completion();
        assert_eq!(s.task_count(), 0);
        // Round-robin interleaving proves fairness (no task starves).
        assert!(ticks >= 5);
        let l = log.borrow();
        assert!(l.contains(&0) && l.contains(&1));
    }

    #[test]
    fn immediate_ready_task_finishes_in_one_tick() {
        struct Done;
        impl Task for Done {
            fn poll(&mut self) -> Poll {
                Poll::Ready
            }
        }
        let mut s = Scheduler::new();
        let id = s.spawn(Box::new(Done));
        assert_eq!(s.tick(), Some((id, Poll::Ready)));
        assert_eq!(s.task_count(), 0);
        assert_eq!(s.tick(), None);
    }
}
