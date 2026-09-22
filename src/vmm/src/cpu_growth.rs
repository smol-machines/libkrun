//! Bookkeeping for irreversible host CPU creation.

use crate::vmm_config::machine_config::CpuFeaturesTemplate;

/// CPU discovery and CPUID policy that must survive a live checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuGrowthTopology {
    pub capacity: u8,
    pub ht_enabled: bool,
    pub nested_enabled: bool,
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

impl CpuGrowthTopology {
    /// Online quota can be smaller than the preserved KVM CPU slots. The
    /// checkpoint still needs every slot, including offline CPU MP state.
    #[cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tee")))]
    pub(crate) fn restored_slot_count(
        &self,
        configured_online: u8,
        saved_slots: usize,
    ) -> Result<u8, String> {
        let slots =
            u8::try_from(saved_slots).map_err(|_| "checkpoint CPU slot count is too large")?;
        if configured_online == 0 || configured_online > slots || slots > self.capacity {
            return Err("configured CPU quota does not fit checkpoint CPU topology".into());
        }
        Ok(slots)
    }
    pub(crate) fn encode(&self) -> [u8; 5] {
        [
            1,
            self.capacity,
            u8::from(self.ht_enabled),
            u8::from(self.nested_enabled),
            match self.cpu_template {
                None => 0,
                Some(CpuFeaturesTemplate::C3) => 1,
                Some(CpuFeaturesTemplate::T2) => 2,
                Some(CpuFeaturesTemplate::PortableV1) => 3,
            },
        ]
    }

    pub(crate) fn decode(bytes: &[u8], created: usize) -> Result<Self, String> {
        if bytes.len() != 5
            || bytes[0] != 1
            || bytes[1] == 0
            || created == 0
            || created > usize::from(bytes[1])
            || bytes[2] > 1
            || bytes[3] > 1
        {
            return Err("invalid checkpoint CPU growth topology".into());
        }
        let cpu_template = match bytes[4] {
            0 => None,
            1 => Some(CpuFeaturesTemplate::C3),
            2 => Some(CpuFeaturesTemplate::T2),
            3 => Some(CpuFeaturesTemplate::PortableV1),
            _ => return Err("unknown checkpoint CPU template".into()),
        };
        Ok(Self {
            capacity: bytes[1],
            ht_enabled: bytes[2] != 0,
            nested_enabled: bytes[3] != 0,
            cpu_template,
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tee")))]
#[derive(Default)]
pub(crate) struct CpuGrowthProgress {
    incomplete: Option<String>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tee")))]
impl CpuGrowthProgress {
    pub(crate) fn check(&self) -> Result<(), String> {
        match &self.incomplete {
            Some(error) => Err(format!(
                "CPU creation incomplete: {error}; cannot safely retry or checkpoint"
            )),
            None => Ok(()),
        }
    }

    pub(crate) fn begin_creation(&mut self, id: u8) {
        self.incomplete = Some(format!("creating vCPU {id}"));
    }

    pub(crate) fn finish(&mut self, result: &Result<(), String>) {
        match result {
            Ok(()) => self.incomplete = None,
            Err(error) if self.incomplete.is_some() => self.incomplete = Some(error.clone()),
            Err(_) => {} // Rejected before host creation: nothing changed.
        }
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "tee")
))]
mod tests {
    use super::*;

    #[test]
    fn shrunk_cpu_quota_preserves_offline_checkpoint_slots() {
        let topology = CpuGrowthTopology {
            capacity: 16,
            ht_enabled: false,
            nested_enabled: false,
            cpu_template: None,
        };
        assert_eq!(topology.restored_slot_count(2, 4).unwrap(), 4);
        assert_eq!(topology.restored_slot_count(4, 4).unwrap(), 4);
        for (online, slots) in [(0, 4), (5, 4), (2, 17), (1, 0), (1, 256)] {
            assert!(topology.restored_slot_count(online, slots).is_err());
        }
    }

    #[test]
    fn topology_roundtrip_preserves_policy_and_rejects_invalid_counts() {
        for cpu_template in [
            None,
            Some(CpuFeaturesTemplate::C3),
            Some(CpuFeaturesTemplate::T2),
            Some(CpuFeaturesTemplate::PortableV1),
        ] {
            let topology = CpuGrowthTopology {
                capacity: 16,
                ht_enabled: true,
                nested_enabled: false,
                cpu_template,
            };
            assert_eq!(
                CpuGrowthTopology::decode(&topology.encode(), 6).unwrap(),
                topology
            );
            assert!(CpuGrowthTopology::decode(&topology.encode(), 17).is_err());
            assert!(CpuGrowthTopology::decode(&topology.encode(), 0).is_err());
        }
        for bytes in [
            &[][..],
            &[1, 16, 0, 0][..],
            &[2, 16, 0, 0, 0][..],
            &[1, 16, 2, 0, 0][..],
            &[1, 16, 0, 0, 9][..],
        ] {
            assert!(CpuGrowthTopology::decode(bytes, 2).is_err());
        }
    }

    #[test]
    fn rejected_preflight_does_not_prevent_retry_or_checkpoint() {
        let mut state = CpuGrowthProgress::default();
        state.finish(&Err("target exceeds topology".into()));
        assert!(state.check().is_ok());
    }

    #[test]
    fn every_creation_stage_failure_stays_incomplete() {
        for stage in [
            "create",
            "configure",
            "spawn",
            "resume send",
            "resume acknowledgment",
        ] {
            let mut state = CpuGrowthProgress::default();
            state.begin_creation(2);
            assert!(state.check().is_err());
            state.finish(&Err(stage.into()));
            assert!(state.check().unwrap_err().contains(stage));
            // A handle may already have been retained. Checking count alone
            // must not turn an equal-target retry into successful completion.
            assert!(state.check().is_err());
        }
    }

    #[test]
    fn all_new_cpus_must_finish_before_checkpoint_is_allowed() {
        let mut state = CpuGrowthProgress::default();
        state.begin_creation(2);
        state.begin_creation(3);
        assert!(state.check().is_err());
        state.finish(&Ok(()));
        assert!(state.check().is_ok());
        state.begin_creation(4);
        state.finish(&Err("configuration failed".into()));
        assert!(state.check().is_err());
    }
}
