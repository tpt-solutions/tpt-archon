//! Capability-based access control types.
//!
//! A [`Capability`] is a strongly-typed, unforgeable token granting a specific
//! [`Right`] over a specific resource (a storage page or an IPC channel).
//! Capabilities cannot be constructed from raw integers by external code — only
//! a [`CapabilityIssuer`] mints them — which is what "unforgeable" means here.
//! They are also revocable: an issuer can revoke a previously minted
//! capability, after which validation fails.
//!
//! This is the type-level security layer `tpt-eidos` is intended to reinforce;
//! the guarantees here are enforced by Rust's privacy (private constructor +
//! sealed token) rather than dependent types, until that dependency is wired
//! in.

use alloc::collections::BTreeSet;

/// The kind of access a [`Capability`] grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Right {
    /// Permission to read a resource.
    Read,
    /// Permission to write a resource.
    Write,
    /// Permission to both read and write a resource.
    ReadWrite,
}

impl Right {
    /// Whether this right permits reading.
    pub fn allows_read(self) -> bool {
        matches!(self, Right::Read | Right::ReadWrite)
    }

    /// Whether this right permits writing.
    pub fn allows_write(self) -> bool {
        matches!(self, Right::Write | Right::ReadWrite)
    }
}

/// The kind of resource a capability refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    /// A storage page identified by block id.
    Page(u64),
    /// An IPC channel identified by channel id.
    Channel(u64),
}

/// An unforgeable, revocable capability token.
///
/// The inner serial is private, so a `Capability` can only be obtained from a
/// [`CapabilityIssuer`] — it cannot be fabricated from arbitrary data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    serial: u64,
    resource: Resource,
    right: Right,
}

impl Capability {
    /// The resource this capability refers to.
    pub fn resource(&self) -> Resource {
        self.resource
    }

    /// The right this capability grants.
    pub fn right(&self) -> Right {
        self.right
    }

    /// Whether this capability authorizes `right` on `resource`.
    pub fn authorizes(&self, resource: Resource, right: Right) -> bool {
        self.resource == resource
            && match right {
                Right::Read => self.right.allows_read(),
                Right::Write => self.right.allows_write(),
                Right::ReadWrite => self.right.allows_read() && self.right.allows_write(),
            }
    }
}

/// Mints and revokes [`Capability`] tokens.
///
/// Each minted capability gets a unique serial; a capability validates only if
/// its serial is still live (i.e. was minted here and not revoked).
#[derive(Debug, Default)]
pub struct CapabilityIssuer {
    next_serial: u64,
    live: BTreeSet<u64>,
}

impl CapabilityIssuer {
    /// Creates a fresh issuer.
    pub fn new() -> Self {
        Self {
            next_serial: 0,
            live: BTreeSet::new(),
        }
    }

    /// Mints a new capability granting `right` over `resource`.
    pub fn mint(&mut self, resource: Resource, right: Right) -> Capability {
        let serial = self.next_serial;
        self.next_serial += 1;
        self.live.insert(serial);
        Capability {
            serial,
            resource,
            right,
        }
    }

    /// Revokes `cap`. Subsequent [`validate`](Self::validate) calls fail.
    pub fn revoke(&mut self, cap: &Capability) {
        self.live.remove(&cap.serial);
    }

    /// Whether `cap` was minted here and is still live.
    pub fn validate(&self, cap: &Capability) -> bool {
        self.live.contains(&cap.serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_capability_validates_and_authorizes() {
        let mut issuer = CapabilityIssuer::new();
        let cap = issuer.mint(Resource::Page(3), Right::ReadWrite);
        assert!(issuer.validate(&cap));
        assert!(cap.authorizes(Resource::Page(3), Right::Read));
        assert!(cap.authorizes(Resource::Page(3), Right::Write));
        assert!(!cap.authorizes(Resource::Page(4), Right::Read));
    }

    #[test]
    fn read_only_capability_denies_write() {
        let mut issuer = CapabilityIssuer::new();
        let cap = issuer.mint(Resource::Channel(1), Right::Read);
        assert!(cap.authorizes(Resource::Channel(1), Right::Read));
        assert!(!cap.authorizes(Resource::Channel(1), Right::Write));
    }

    #[test]
    fn revoked_capability_fails_validation() {
        let mut issuer = CapabilityIssuer::new();
        let cap = issuer.mint(Resource::Page(0), Right::Read);
        assert!(issuer.validate(&cap));
        issuer.revoke(&cap);
        assert!(!issuer.validate(&cap));
    }

    #[test]
    fn capabilities_from_other_issuers_do_not_validate() {
        let mut a = CapabilityIssuer::new();
        let b = CapabilityIssuer::new();
        let cap = a.mint(Resource::Page(0), Right::Read);
        // b never minted this serial (b is empty).
        assert!(!b.validate(&cap));
    }
}
