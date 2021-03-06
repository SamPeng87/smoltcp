// Heads up! Before working on this file you should read, at least,
// the parts of RFC 1122 that discuss ARP.

use managed::ManagedMap;

use wire::{EthernetAddress, IpAddress};

/// A cached neighbor.
///
/// A neighbor mapping translates from a protocol address to a hardware address,
/// and contains the timestamp past which the mapping should be discarded.
#[derive(Debug, Clone, Copy)]
pub struct Neighbor {
    hardware_addr: EthernetAddress,
    expires_at:    u64,
}

/// An answer to a neighbor cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The neighbor address is in the cache and not expired.
    Found(EthernetAddress),
    /// The neighbor address is not in the cache, or has expired.
    NotFound,
    /// The neighbor address is not in the cache, or has expired,
    /// and a lookup has been made recently.
    Hushed
}

/// A neighbor cache backed by a map.
///
/// # Examples
///
/// On systems with heap, this cache can be created with:
/// ```rust
/// use std::collections::BTreeMap;
/// use smoltcp::iface::NeighborCache;
/// let mut neighbor_cache = NeighborCache::new(BTreeMap::new());
/// ```
///
/// On systems without heap, use:
/// ```rust
/// use smoltcp::iface::NeighborCache;
/// let mut neighbor_cache_storage = [None; 8];
/// let mut neighbor_cache = NeighborCache::new(&mut neighbor_cache_storage[..]);
/// ```
#[derive(Debug)]
pub struct Cache<'a> {
    storage:      ManagedMap<'a, IpAddress, Neighbor>,
    hushed_until: u64,
}

impl<'a> Cache<'a> {
    /// Minimum delay between discovery requests, in milliseconds.
    pub(crate) const SILENT_TIME: u64 = 1_000;

    /// Neighbor entry lifetime, in milliseconds.
    pub(crate) const ENTRY_LIFETIME: u64 = 60_000;

    /// Create a cache. The backing storage is cleared upon creation.
    ///
    /// # Panics
    /// This function panics if `storage.len() == 0`.
    pub fn new<T>(storage: T) -> Cache<'a>
            where T: Into<ManagedMap<'a, IpAddress, Neighbor>> {
        let mut storage = storage.into();
        storage.clear();

        Cache { storage, hushed_until: 0 }
    }

    pub(crate) fn fill(&mut self, protocol_addr: IpAddress, hardware_addr: EthernetAddress,
                       timestamp: u64) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        let neighbor = Neighbor {
            expires_at: timestamp + Self::ENTRY_LIFETIME, hardware_addr
        };
        match self.storage.insert(protocol_addr, neighbor) {
            Ok(Some(old_neighbor)) => {
                if old_neighbor.hardware_addr != hardware_addr {
                    net_trace!("replaced {} => {} (was {})",
                               protocol_addr, hardware_addr, old_neighbor.hardware_addr);
                }
            }
            Ok(None) => {
                net_trace!("filled {} => {} (was empty)", protocol_addr, hardware_addr);
            }
            Err((protocol_addr, neighbor)) => {
                // If we're going down this branch, it means that a fixed-size cache storage
                // is full, and we need to evict an entry.
                let old_protocol_addr = match self.storage {
                    ManagedMap::Borrowed(ref mut pairs) => {
                        pairs
                            .iter()
                            .min_by_key(|pair_opt| {
                                let (_protocol_addr, neighbor) = pair_opt.unwrap();
                                neighbor.expires_at
                            })
                            .expect("empty neighbor cache storage") // unwraps min_by_key
                            .unwrap() // unwraps pair
                            .0
                    }
                    // Owned maps can extend themselves.
                    #[cfg(any(feature = "std", feature = "alloc"))]
                    ManagedMap::Owned(_) => unreachable!()
                };

                let _old_neighbor =
                    self.storage.remove(&old_protocol_addr).unwrap();
                match self.storage.insert(protocol_addr, neighbor) {
                    Ok(None) => {
                        net_trace!("filled {} => {} (evicted {} => {})",
                                   protocol_addr, hardware_addr,
                                   old_protocol_addr, _old_neighbor.hardware_addr);
                    }
                    // We've covered everything else above.
                    _ => unreachable!()
                }

            }
        }
    }

    pub(crate) fn lookup_pure(&self, protocol_addr: &IpAddress, timestamp: u64) ->
                             Option<EthernetAddress> {
        if protocol_addr.is_broadcast() {
            return Some(EthernetAddress::BROADCAST)
        }

        match self.storage.get(protocol_addr) {
            Some(&Neighbor { expires_at, hardware_addr }) => {
                if timestamp < expires_at {
                    return Some(hardware_addr)
                }
            }
            None => ()
        }

        None
    }

    pub(crate) fn lookup(&mut self, protocol_addr: &IpAddress, timestamp: u64) -> Answer {
        match self.lookup_pure(protocol_addr, timestamp) {
            Some(hardware_addr) =>
                Answer::Found(hardware_addr),
            None if timestamp < self.hushed_until =>
                Answer::Hushed,
            None => {
                self.hushed_until = timestamp + Self::SILENT_TIME;
                Answer::NotFound
            }
        }
    }
}

#[cfg(test)]
mod test {
    use wire::Ipv4Address;
    use super::*;

    const HADDR_A: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 1]);
    const HADDR_B: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 2]);
    const HADDR_C: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 3]);
    const HADDR_D: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 4]);

    const PADDR_A: IpAddress = IpAddress::Ipv4(Ipv4Address([1, 0, 0, 1]));
    const PADDR_B: IpAddress = IpAddress::Ipv4(Ipv4Address([1, 0, 0, 2]));
    const PADDR_C: IpAddress = IpAddress::Ipv4(Ipv4Address([1, 0, 0, 3]));
    const PADDR_D: IpAddress = IpAddress::Ipv4(Ipv4Address([1, 0, 0, 4]));

    #[test]
    fn test_fill() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        assert_eq!(cache.lookup_pure(&PADDR_A, 0), None);
        assert_eq!(cache.lookup_pure(&PADDR_B, 0), None);

        cache.fill(PADDR_A, HADDR_A, 0);
        assert_eq!(cache.lookup_pure(&PADDR_A, 0), Some(HADDR_A));
        assert_eq!(cache.lookup_pure(&PADDR_B, 0), None);
        assert_eq!(cache.lookup_pure(&PADDR_A, 2 * Cache::ENTRY_LIFETIME), None);

        cache.fill(PADDR_A, HADDR_A, 0);
        assert_eq!(cache.lookup_pure(&PADDR_B, 0), None);
    }

    #[test]
    fn test_expire() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(PADDR_A, HADDR_A, 0);
        assert_eq!(cache.lookup_pure(&PADDR_A, 0), Some(HADDR_A));
        assert_eq!(cache.lookup_pure(&PADDR_A, 2 * Cache::ENTRY_LIFETIME), None);
    }

    #[test]
    fn test_replace() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(PADDR_A, HADDR_A, 0);
        assert_eq!(cache.lookup_pure(&PADDR_A, 0), Some(HADDR_A));
        cache.fill(PADDR_A, HADDR_B, 0);
        assert_eq!(cache.lookup_pure(&PADDR_A, 0), Some(HADDR_B));
    }

    #[test]
    fn test_evict() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(PADDR_A, HADDR_A, 100);
        cache.fill(PADDR_B, HADDR_B, 50);
        cache.fill(PADDR_C, HADDR_C, 200);
        assert_eq!(cache.lookup_pure(&PADDR_B, 1000), Some(HADDR_B));
        assert_eq!(cache.lookup_pure(&PADDR_D, 1000), None);

        cache.fill(PADDR_D, HADDR_D, 300);
        assert_eq!(cache.lookup_pure(&PADDR_B, 1000), None);
        assert_eq!(cache.lookup_pure(&PADDR_D, 1000), Some(HADDR_D));
    }

    #[test]
    fn test_hush() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        assert_eq!(cache.lookup(&PADDR_A, 0), Answer::NotFound);
        assert_eq!(cache.lookup(&PADDR_A, 100), Answer::Hushed);
        assert_eq!(cache.lookup(&PADDR_A, 2000), Answer::NotFound);
    }
}

