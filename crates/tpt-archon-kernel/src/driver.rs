//! User-space driver framework, v1 (sandbox-testable mock).
//!
//! Phase 2b deferred item (closed in Phase 11.2). This is a *v1* that proves
//! the end-to-end plumbing with no real hardware:
//!
//! ```text
//! interrupt --(MockInterruptSource, capability-checked)--> MessageRouter
//!      --> Driver::handle_interrupt --> capability-checked IPC response
//! ```
//!
//! A [`DriverTask<D: Driver>`] adapts a driver into an ordinary
//! [`scheduler::Task`], so the scheduler's deadlock-freedom argument already
//! covers it (a pending `DriverTask` simply re-polls until its interrupt queue
//! drains, holding no resource another task needs).
//!
//! No real interrupt controller, UIO/VFIO device wrapper, or bare-metal target
//! exists anywhere in this repo, so v1 uses a [`MockInterruptSource`] instead
//! of a real line. The explicit v2 follow-ups (real UIO/VFIO, real interrupt
//! controllers, bare-metal targets, `no_std` interrupt handlers, device
//! discovery tables) are tracked in `TODO.md` Phase 11.2 — not silently
//! dropped.
//!
//! # Capability gating
//!
//! - Standing up a [`DriverTask`] requires a live [`Resource::Device`] *read*
//!   capability (a driver *reads* device state). [`stand_up`] returns `None`
//!   without one, or if it has been revoked.
//! - Injecting an interrupt into a [`MockInterruptSource`] requires a live
//!   *write* capability over the same device (you may only *drive* the line if
//!   authorized to write the device).

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use tpt_archon_bridge::capability::{Capability, Resource, Right, SharedIssuer};

use crate::ipc::{Message, MessageRouter};
use crate::scheduler::{Poll, Task};

/// Errors from driver-framework operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverError {
    /// The caller lacks a write capability over the device, so may not inject
    /// interrupts into its line.
    InjectDenied,
}

/// A device driver: turns an inbound interrupt into zero or more outbound IPC
/// messages (e.g. an acknowledgement or a read result) delivered through
/// `router` using the driver task's response capability.
pub trait Driver {
    /// The device this driver is bound to.
    fn device(&self) -> u64;

    /// Handle one inbound interrupt message. `cap` is the driver task's
    /// response capability (authorizing writes to its response channel(s));
    /// `router.send(cap, …)` is how the driver emits IPC.
    fn handle_interrupt(
        &mut self,
        msg: &Message,
        router: &mut MessageRouter,
        cap: &Capability,
    ) -> Vec<Message>;
}

/// A mock interrupt source: an injectable queue standing in for a real
/// interrupt controller (v2 work).
///
/// `inject` models an edge-triggered interrupt line and is capability-checked;
/// `next`/`pending` drain/inspect the queue (no check — a driver that has
/// already proven it may run just consumes its own pending work).
#[derive(Default)]
pub struct MockInterruptSource {
    queue: VecDeque<Message>,
}

impl MockInterruptSource {
    /// Creates an empty source (no pending interrupts).
    pub fn new() -> Self {
        Self::default()
    }

    /// Injects an interrupt `msg` onto `device`'s line.
    ///
    /// Requires a live *write* capability over [`Resource::Device(device)`];
    /// without one this returns [`DriverError::InjectDenied`] and injects
    /// nothing — the line is capability-gated, not ambiently writable.
    pub fn inject(
        &mut self,
        issuer: &SharedIssuer,
        cap: &Capability,
        device: u64,
        msg: Message,
    ) -> Result<(), DriverError> {
        if !(issuer
            .borrow()
            .authorizes(cap, Resource::Device(device), Right::Write)
            || issuer
                .borrow()
                .authorizes(cap, Resource::Device(device), Right::ReadWrite))
        {
            return Err(DriverError::InjectDenied);
        }
        self.queue.push_back(msg);
        Ok(())
    }

    /// Pops the next pending interrupt, or `None` if the line is quiet.
    pub fn pop_interrupt(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    /// Whether any interrupt is still pending.
    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }
}

/// Adapts a [`Driver`] into a [`scheduler::Task`].
///
/// Spawned once per driver. Each `poll` drains one pending interrupt and hands
/// it to the driver; the task stays [`Poll::Pending`] while interrupts remain,
/// and returns [`Poll::Ready`] once the queue is drained.
///
/// The router is shared via `Rc<RefCell<_>>` (matching the crate's existing
/// single-threaded, `no_std`-friendly concurrency model) so a test harness can
/// inspect the delivered IPC after the task has run.
pub struct DriverTask<D: Driver> {
    driver: D,
    source: MockInterruptSource,
    router: Rc<RefCell<MessageRouter>>,
    cap: Capability,
}

impl<D: Driver> DriverTask<D> {
    /// Creates a driver task drawing interrupts from `source`, delivering
    /// responses through `router` using `cap` (the response-channel write
    /// capability). Prefer [`stand_up`], which enforces the device-capability
    /// gate.
    pub fn new(
        source: MockInterruptSource,
        router: Rc<RefCell<MessageRouter>>,
        cap: Capability,
        driver: D,
    ) -> Self {
        Self {
            driver,
            source,
            router,
            cap,
        }
    }
}

impl<D: Driver> Task for DriverTask<D> {
    fn poll(&mut self) -> Poll {
        match self.source.pop_interrupt() {
            Some(msg) => {
                self.driver
                    .handle_interrupt(&msg, &mut self.router.borrow_mut(), &self.cap);
                Poll::Pending
            }
            None => Poll::Ready,
        }
    }
}

/// Authorization gate for standing up a [`DriverTask`].
///
/// Returns `Some(task)` only if `device_cap` is live and authorizes *reading*
/// [`Resource::Device(device)`] (a driver reads device state). A revoked or
/// missing device capability yields `None` — the device capability is the gate,
/// not just documentation.
pub fn stand_up<D: Driver>(
    issuer: &SharedIssuer,
    device_cap: &Capability,
    device: u64,
    source: MockInterruptSource,
    router: Rc<RefCell<MessageRouter>>,
    response_cap: Capability,
    driver: D,
) -> Option<DriverTask<D>> {
    if issuer
        .borrow()
        .authorizes(device_cap, Resource::Device(device), Right::Read)
        || issuer
            .borrow()
            .authorizes(device_cap, Resource::Device(device), Right::ReadWrite)
    {
        Some(DriverTask::new(source, router, response_cap, driver))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use tpt_archon_bridge::capability::CapabilityIssuer;

    fn shared_issuer() -> SharedIssuer {
        Rc::new(RefCell::new(CapabilityIssuer::new()))
    }

    /// A driver that echoes the interrupt payload back onto a response channel.
    struct EchoDriver {
        device_id: u64,
        response_channel: u64,
    }

    impl Driver for EchoDriver {
        fn device(&self) -> u64 {
            self.device_id
        }

        fn handle_interrupt(
            &mut self,
            msg: &Message,
            router: &mut MessageRouter,
            cap: &Capability,
        ) -> Vec<Message> {
            let out = Message {
                channel: self.response_channel,
                payload: msg.payload.clone(),
            };
            // The response cap authorizes writing the response channel; the
            // router enforces it (see `ipc::MessageRouter::send`).
            let _ = router.send(cap, out.clone());
            alloc::vec![out]
        }
    }

    #[test]
    fn mock_interrupt_wakes_driver_task_via_scheduler() {
        let issuer = shared_issuer();
        let router = Rc::new(RefCell::new(MessageRouter::new(issuer.clone())));
        router.borrow_mut().register_channel(9);

        // Device read cap (run the driver) + channel write cap (respond).
        let device_cap = issuer
            .borrow_mut()
            .mint(Resource::Device(5), Right::ReadWrite);
        let response_cap = issuer.borrow_mut().mint(Resource::Channel(9), Right::Write);

        let mut source = MockInterruptSource::new();
        source
            .inject(
                &issuer,
                &device_cap,
                5,
                Message {
                    channel: 5,
                    payload: alloc::vec![0xDE, 0xAD],
                },
            )
            .unwrap();

        let task = stand_up(
            &issuer,
            &device_cap,
            5,
            source,
            router.clone(),
            response_cap,
            EchoDriver {
                device_id: 5,
                response_channel: 9,
            },
        )
        .expect("authorized driver stands up");

        let mut scheduler = Scheduler::new();
        scheduler.spawn(Box::new(task));
        scheduler.run_to_completion();

        // The driver delivered the echoed payload to channel 9 (router is the
        // shared one, so we can inspect it here after the task ran).
        let read_cap = issuer.borrow_mut().mint(Resource::Channel(9), Right::Read);
        let msgs = router.borrow_mut().receive(&read_cap, 9).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, alloc::vec![0xDE, 0xAD]);
    }

    #[test]
    fn stand_up_denied_without_read_device_capability() {
        let issuer = shared_issuer();
        let router = Rc::new(RefCell::new(MessageRouter::new(issuer.clone())));
        // Only a *channel* cap, never a device cap.
        let channel_cap = issuer.borrow_mut().mint(Resource::Channel(9), Right::Write);

        let task = stand_up(
            &issuer,
            &channel_cap,
            5,
            MockInterruptSource::new(),
            router,
            channel_cap,
            EchoDriver {
                device_id: 5,
                response_channel: 9,
            },
        );
        assert!(task.is_none());
    }

    #[test]
    fn stand_up_denied_with_revoked_device_capability() {
        let issuer = shared_issuer();
        let router = Rc::new(RefCell::new(MessageRouter::new(issuer.clone())));
        let device_cap = issuer.borrow_mut().mint(Resource::Device(5), Right::Read);
        issuer.borrow_mut().revoke(&device_cap);
        let response_cap = issuer.borrow_mut().mint(Resource::Channel(9), Right::Write);

        let task = stand_up(
            &issuer,
            &device_cap,
            5,
            MockInterruptSource::new(),
            router,
            response_cap,
            EchoDriver {
                device_id: 5,
                response_channel: 9,
            },
        );
        assert!(task.is_none());
    }

    #[test]
    fn inject_denied_without_write_device_capability() {
        let issuer = shared_issuer();
        let mut source = MockInterruptSource::new();
        // Read-only device cap may run a driver but may NOT inject interrupts.
        let read_cap = issuer.borrow_mut().mint(Resource::Device(5), Right::Read);
        assert_eq!(
            source.inject(
                &issuer,
                &read_cap,
                5,
                Message {
                    channel: 5,
                    payload: alloc::vec![1]
                }
            ),
            Err(DriverError::InjectDenied)
        );
        assert!(!source.pending());

        // A write cap (or read-write) may inject.
        let write_cap = issuer.borrow_mut().mint(Resource::Device(5), Right::Write);
        assert!(source
            .inject(
                &issuer,
                &write_cap,
                5,
                Message {
                    channel: 5,
                    payload: alloc::vec![2]
                }
            )
            .is_ok());
        assert!(source.pending());
    }
}
