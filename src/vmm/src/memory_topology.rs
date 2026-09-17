//! Boot RAM layout metadata, distinct from the live usable RAM total.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryGrowthTopology {
    pub boot_memory_mib: u64,
}

impl MemoryGrowthTopology {
    pub(crate) fn encode(&self) -> [u8; 9] {
        let mut bytes = [0; 9];
        bytes[0] = 1;
        bytes[1..].copy_from_slice(&self.boot_memory_mib.to_le_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 9 || bytes[0] != 1 {
            return Err("unsupported checkpoint RAM topology".into());
        }
        let boot_memory_mib = u64::from_le_bytes(bytes[1..].try_into().unwrap());
        if boot_memory_mib == 0 || boot_memory_mib.checked_mul(1 << 20).is_none() {
            return Err("invalid checkpoint boot RAM size".into());
        }
        Ok(Self { boot_memory_mib })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boot_memory_topology_roundtrips_and_rejects_invalid_sizes() {
        let topology = MemoryGrowthTopology {
            boot_memory_mib: 1024,
        };
        assert_eq!(
            MemoryGrowthTopology::decode(&topology.encode()).unwrap(),
            topology
        );
        for size in [0, u64::MAX] {
            assert!(
                MemoryGrowthTopology::decode(
                    &MemoryGrowthTopology {
                        boot_memory_mib: size
                    }
                    .encode()
                )
                .is_err()
            );
        }
        assert!(MemoryGrowthTopology::decode(&[1; 8]).is_err());
        let mut wrong_version = topology.encode();
        wrong_version[0] = 2;
        assert!(MemoryGrowthTopology::decode(&wrong_version).is_err());
    }
}
