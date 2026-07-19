//! Capability-scoped multi-tenant demo.
//!
//! Shows the `tpt-archon-bridge` capability system enforcing isolation between
//! two tenants sharing one unified page cache: each tenant is handed read/write
//! capabilities scoped to *their own* pages, and any access outside that scope
//! is denied by the cache — no second buffer pool, no OS process boundary.
//!
//! Run with: `cargo run -p tpt-archon-bridge --example multi_tenant`

use tpt_archon_bridge::capability::{CapabilityIssuer, Resource, Right};
use tpt_archon_bridge::page_cache::{CorePageCache, UnifiedPageCache};
use tpt_archon_core::block::InMemoryBlockDevice;
use tpt_archon_core::page::BufferPool;

/// A tenant: a name plus the capabilities it was issued over its own pages.
struct Tenant {
    name: &'static str,
    caps: Vec<tpt_archon_bridge::capability::Capability>,
}

impl Tenant {
    fn write_page(&self, cache: &mut CorePageCache<InMemoryBlockDevice>, block: u64, byte: u8) {
        // Find the capability this tenant holds for `block` (if any).
        let cap = self
            .caps
            .iter()
            .find(|c| c.resource() == Resource::Page(block))
            .expect("tenant must hold a capability for its own page");
        {
            let page = cache.map_write(cap, block).expect("authorized write");
            page.as_bytes_mut()[0] = byte;
        }
        cache.unmap(block);
    }

    fn read_page(&self, cache: &mut CorePageCache<InMemoryBlockDevice>, block: u64) -> Option<u8> {
        let cap = self
            .caps
            .iter()
            .find(|c| c.resource() == Resource::Page(block))?;
        let page = cache.map_read(cap, block).ok()?;
        let v = page.as_bytes()[0];
        cache.unmap(block);
        Some(v)
    }
}

fn main() {
    let mut issuer = CapabilityIssuer::new();
    let mut cache = CorePageCache::new(BufferPool::new(InMemoryBlockDevice::new(8), 4));

    // Issuer grants each tenant a capability scoped to exactly one page.
    let alice = Tenant {
        name: "alice",
        caps: vec![issuer.mint(Resource::Page(0), Right::ReadWrite)],
    };
    let bob = Tenant {
        name: "bob",
        caps: vec![issuer.mint(Resource::Page(1), Right::ReadWrite)],
    };

    // Each tenant writes only to its own page — same cache, no cross-talk.
    alice.write_page(&mut cache, 0, 0xAA);
    bob.write_page(&mut cache, 1, 0xBB);

    assert_eq!(alice.read_page(&mut cache, 0), Some(0xAA));
    assert_eq!(bob.read_page(&mut cache, 1), Some(0xBB));
    println!(
        "{}@page0 = {:02X}",
        alice.name,
        alice.read_page(&mut cache, 0).unwrap()
    );
    println!(
        "{}@page1   = {:02X}",
        bob.name,
        bob.read_page(&mut cache, 1).unwrap()
    );

    // A tenant cannot read or write another tenant's page: the cache denies it.
    let alice_tries_bob = alice.read_page(&mut cache, 1);
    assert_eq!(
        alice_tries_bob, None,
        "alice has no capability for bob's page"
    );
    println!(
        "alice denied access to bob@page1: {}",
        alice_tries_bob.is_none()
    );

    // Revocation: a real kernel re-validates a capability against its issuer on
    // every use, so a revoked capability is rejected before it ever reaches the
    // cache. The cache itself trusts the capability's embedded authorization
    // (it cannot see the issuer), so the issuer check is the enforcer.
    let alice_cap = alice.caps[0];
    assert!(issuer.validate(&alice_cap));
    issuer.revoke(&alice_cap);
    assert!(
        !issuer.validate(&alice_cap),
        "revoked cap no longer vouched for"
    );

    // Simulate the kernel's gate: only present the cap to the cache if the
    // issuer still validates it.
    let allowed = issuer.validate(&alice_cap);
    let read_after_revoke = if allowed {
        cache.map_read(&alice_cap, 0).map(|p| p.as_bytes()[0]).ok()
    } else {
        None
    };
    assert_eq!(
        read_after_revoke, None,
        "revoked capability must not reach the page"
    );
    println!(
        "alice's revoked capability blocked by issuer gate: {}",
        read_after_revoke.is_none()
    );

    println!("multi-tenant isolation demo complete.");
}
