//! Capability-scoped multi-tenant demo.
//!
//! Shows the `tpt-archon-bridge` capability system enforcing isolation between
//! two tenants sharing one unified page cache: each tenant is handed read/write
//! capabilities scoped to *their own* pages, and any access outside that scope
//! is denied by the cache — no second buffer pool, no OS process boundary.
//!
//! Run with: `cargo run -p tpt-archon-bridge --example multi_tenant`

use std::cell::RefCell;
use std::rc::Rc;

use tpt_archon_bridge::capability::{CapabilityIssuer, Resource, Right};
use tpt_archon_bridge::page_cache::{CacheError, CorePageCache, UnifiedPageCache};
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
    let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
    let mut cache = CorePageCache::new(
        BufferPool::new(InMemoryBlockDevice::new(8), 4),
        issuer.clone(),
    );

    // Issuer grants each tenant a capability scoped to exactly one page.
    let alice = Tenant {
        name: "alice",
        caps: vec![issuer
            .borrow_mut()
            .mint(Resource::Page(0), Right::ReadWrite)],
    };
    let bob = Tenant {
        name: "bob",
        caps: vec![issuer
            .borrow_mut()
            .mint(Resource::Page(1), Right::ReadWrite)],
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

    // Revocation: the cache shares `issuer` (a `Rc<RefCell<CapabilityIssuer>>`)
    // and checks liveness on every `map_read`/`map_write` call, so `revoke`
    // takes effect immediately at the real enforcement point — no separate,
    // easy-to-forget "ask the issuer first" step required by the caller.
    let alice_cap = alice.caps[0];
    assert!(issuer.borrow().validate(&alice_cap));
    issuer.borrow_mut().revoke(&alice_cap);
    assert!(
        !issuer.borrow().validate(&alice_cap),
        "revoked cap no longer vouched for"
    );

    // The same, structurally-unchanged capability is now rejected by the
    // cache itself, without any extra gating logic at the call site.
    let read_after_revoke = cache.map_read(&alice_cap, 0).map(|p| p.as_bytes()[0]);
    let was_denied = read_after_revoke.is_err();
    assert_eq!(
        read_after_revoke.err(),
        Some(CacheError::Denied),
        "revoked capability must be denied by the cache directly"
    );
    println!("alice's revoked capability blocked by the cache directly: {was_denied}");

    println!("multi-tenant isolation demo complete.");
}
