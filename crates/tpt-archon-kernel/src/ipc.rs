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

use tpt_archon_bridge::capability::{Capability, Resource, Right};

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
#[derive(Debug, Default)]
pub struct MessageRouter {
    inboxes: BTreeMap<u64, Vec<Message>>,
}

impl MessageRouter {
    /// Creates an empty router.
    pub fn new() -> Self {
        Self {
            inboxes: BTreeMap::new(),
        }
    }

    /// Registers a channel with an empty inbox.
    pub fn register_channel(&mut self, channel: u64) {
        self.inboxes.entry(channel).or_default();
    }

    /// Sends `message` if `cap` authorizes writing `message.channel`.
    pub fn send(&mut self, cap: &Capability, message: Message) -> Result<(), IpcError> {
        if !cap.authorizes(Resource::Channel(message.channel), Right::Write) {
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
        if !cap.authorizes(Resource::Channel(channel), Right::Read) {
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
    use tpt_archon_bridge::capability::CapabilityIssuer;

    #[test]
    fn authorized_send_and_receive() {
        let mut issuer = CapabilityIssuer::new();
        let mut router = MessageRouter::new();
        router.register_channel(7);

        let send_cap = issuer.mint(Resource::Channel(7), Right::Write);
        let recv_cap = issuer.mint(Resource::Channel(7), Right::Read);

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
        let mut issuer = CapabilityIssuer::new();
        let mut router = MessageRouter::new();
        router.register_channel(1);
        let read_only = issuer.mint(Resource::Channel(1), Right::Read);
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
        let mut issuer = CapabilityIssuer::new();
        let mut router = MessageRouter::new();
        let cap = issuer.mint(Resource::Channel(99), Right::Write);
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
}
