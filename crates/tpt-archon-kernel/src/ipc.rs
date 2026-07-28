//! Capability-bearing IPC message passing.
//!
//! The microkernel handles only IPC, scheduling, and memory. Services
//! (filesystems, network, drivers) run as isolated user-space endpoints and
//! communicate exclusively through capability-bearing [`Message`]s routed by
//! the [`MessageRouter`]. A message can only be delivered to a channel the
//! sender holds a write [`Capability`] for, so resource access is mediated by
//! the capability system rather than ambient authority.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tpt_archon_bridge::capability::{Capability, Resource, Right, SharedIssuer};

/// A capability-bearing message addressed to a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The destination channel id.
    pub channel: u64,
    /// The message payload.
    pub payload: Vec<u8>,
}

/// Errors from message routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// The sender's capability does not authorize writing this channel.
    Denied,
    /// No such channel is registered.
    NoSuchChannel,
}

/// Routes messages between registered channels.
///
/// Each channel has an inbox; [`send`](Self::send) enqueues a message iff the
/// sender presents a capability authorizing a write to that channel.
#[derive(Debug)]
pub struct MessageRouter {
    inboxes: BTreeMap<u64, Vec<Message>>,
    issuer: SharedIssuer,
}

impl MessageRouter {
    /// Creates an empty router gated by `issuer` — capabilities presented to
    /// `send`/`receive` are checked for live revocation against it, not just
    /// their structural (resource, right) shape.
    pub fn new(issuer: SharedIssuer) -> Self {
        Self {
            inboxes: BTreeMap::new(),
            issuer,
        }
    }

    /// Registers a channel with an empty inbox.
    pub fn register_channel(&mut self, channel: u64) {
        self.inboxes.entry(channel).or_default();
    }

    /// Sends `message` if `cap` authorizes writing `message.channel`.
    pub fn send(&mut self, cap: &Capability, message: Message) -> Result<(), IpcError> {
        if !self
            .issuer
            .borrow()
            .authorizes(cap, Resource::Channel(message.channel), Right::Write)
        {
            return Err(IpcError::Denied);
        }
        let inbox = self
            .inboxes
            .get_mut(&message.channel)
            .ok_or(IpcError::NoSuchChannel)?;
        inbox.push(message);
        Ok(())
    }

    /// Receives (drains) all messages for `channel` if `cap` authorizes reading
    /// it.
    pub fn receive(&mut self, cap: &Capability, channel: u64) -> Result<Vec<Message>, IpcError> {
        if !self
            .issuer
            .borrow()
            .authorizes(cap, Resource::Channel(channel), Right::Read)
        {
            return Err(IpcError::Denied);
        }
        let inbox = self
            .inboxes
            .get_mut(&channel)
            .ok_or(IpcError::NoSuchChannel)?;
        Ok(core::mem::take(inbox))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use tpt_archon_bridge::capability::CapabilityIssuer;

    fn shared_issuer() -> Rc<RefCell<CapabilityIssuer>> {
        Rc::new(RefCell::new(CapabilityIssuer::new()))
    }

    #[test]
    fn authorized_send_and_receive() {
        let issuer = shared_issuer();
        let mut router = MessageRouter::new(issuer.clone());
        router.register_channel(7);

        let send_cap = issuer.borrow_mut().mint(Resource::Channel(7), Right::Write);
        let recv_cap = issuer.borrow_mut().mint(Resource::Channel(7), Right::Read);

        router
            .send(
                &send_cap,
                Message {
                    channel: 7,
                    payload: alloc::vec![1, 2, 3],
                },
            )
            .unwrap();

        let msgs = router.receive(&recv_cap, 7).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, alloc::vec![1, 2, 3]);
        // Drained.
        assert!(router.receive(&recv_cap, 7).unwrap().is_empty());
    }

    #[test]
    fn send_without_write_capability_is_denied() {
        let issuer = shared_issuer();
        let mut router = MessageRouter::new(issuer.clone());
        router.register_channel(1);
        let read_only = issuer.borrow_mut().mint(Resource::Channel(1), Right::Read);
        assert_eq!(
            router.send(
                &read_only,
                Message {
                    channel: 1,
                    payload: alloc::vec![]
                }
            ),
            Err(IpcError::Denied)
        );
    }

    #[test]
    fn unknown_channel_errors() {
        let issuer = shared_issuer();
        let mut router = MessageRouter::new(issuer.clone());
        let cap = issuer
            .borrow_mut()
            .mint(Resource::Channel(99), Right::Write);
        assert_eq!(
            router.send(
                &cap,
                Message {
                    channel: 99,
                    payload: alloc::vec![]
                }
            ),
            Err(IpcError::NoSuchChannel)
        );
    }

    #[test]
    fn revoked_capability_is_denied_at_send_and_receive() {
        // Regression test for security-audit finding 1: `revoke` must be
        // enforced by `MessageRouter` itself, not only by calling
        // `CapabilityIssuer::validate` out-of-band.
        let issuer = shared_issuer();
        let mut router = MessageRouter::new(issuer.clone());
        router.register_channel(3);
        let cap = issuer
            .borrow_mut()
            .mint(Resource::Channel(3), Right::ReadWrite);

        router
            .send(
                &cap,
                Message {
                    channel: 3,
                    payload: alloc::vec![9],
                },
            )
            .unwrap();

        issuer.borrow_mut().revoke(&cap);
        assert_eq!(
            router.send(
                &cap,
                Message {
                    channel: 3,
                    payload: alloc::vec![9]
                }
            ),
            Err(IpcError::Denied)
        );
        assert_eq!(router.receive(&cap, 3), Err(IpcError::Denied));
    }
}
