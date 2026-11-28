//! The guest's 2D framebuffers, recorded so a restored GPU device can recreate
//! them.
//!
//! A checkpoint or fork restores the GPU device with fresh host state: the
//! renderer's resources are live host objects that are not serialized. The
//! guest, though, resumes with every resource it created still in use, and a
//! compositor keeps flipping between the framebuffers it made before the
//! snapshot. On the restored device those resource ids do not exist, every
//! SET_SCANOUT and RESOURCE_FLUSH fails, and the display never shows another
//! frame (smolvm#2475).
//!
//! A 2D resource is fully described by its creation parameters and the guest
//! memory backing it, and guest memory is restored with the machine. Recording
//! those, and which resource each scanout shows, is enough to recreate the
//! display on the restored device before the guest sends its next command.
//! Resources created through a 3D context or as blobs hold host GPU state that
//! cannot be rebuilt this way, so they are not recorded; a display that shows
//! one is not replayed.

/// One guest 2D resource: what RESOURCE_CREATE_2D asked for, and the guest
/// memory RESOURCE_ATTACH_BACKING gave it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Resource2d {
    pub id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// `(guest address, length)` entries, in the order the guest gave them.
    /// Empty while no backing is attached.
    pub backing: Vec<(u64, u32)>,
}

/// What a scanout shows, as the guest last set it with SET_SCANOUT.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanoutBinding {
    pub scanout_id: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
}

/// The guest's 2D resources and scanout bindings, as the device last accepted
/// them. Updated only after a command succeeds, so it mirrors the host state a
/// restore has to rebuild.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DisplayReplay {
    pub resources: Vec<Resource2d>,
    pub scanouts: Vec<ScanoutBinding>,
}

impl DisplayReplay {
    fn resource_mut(&mut self, id: u32) -> Option<&mut Resource2d> {
        self.resources.iter_mut().find(|r| r.id == id)
    }

    /// RESOURCE_CREATE_2D succeeded.
    pub fn created(&mut self, id: u32, format: u32, width: u32, height: u32) {
        self.unref(id);
        self.resources.push(Resource2d {
            id,
            format,
            width,
            height,
            backing: Vec::new(),
        });
    }

    /// RESOURCE_ATTACH_BACKING succeeded. Ignored for a resource that is not a
    /// recorded 2D one.
    pub fn attached(&mut self, id: u32, backing: Vec<(u64, u32)>) {
        if let Some(resource) = self.resource_mut(id) {
            resource.backing = backing;
        }
    }

    /// RESOURCE_DETACH_BACKING succeeded.
    pub fn detached(&mut self, id: u32) {
        if let Some(resource) = self.resource_mut(id) {
            resource.backing.clear();
        }
    }

    /// RESOURCE_UNREF succeeded: the resource, and any scanout showing it, go.
    pub fn unref(&mut self, id: u32) {
        self.resources.retain(|r| r.id != id);
        self.scanouts.retain(|s| s.resource_id != id);
    }

    /// SET_SCANOUT succeeded. A resource id of 0 disables the scanout; one that
    /// is not a recorded 2D resource leaves the scanout unrecorded, since it
    /// could not be replayed.
    pub fn scanout(&mut self, scanout_id: u32, resource_id: u32, width: u32, height: u32) {
        self.scanouts.retain(|s| s.scanout_id != scanout_id);
        if resource_id != 0 && self.resources.iter().any(|r| r.id == resource_id) {
            self.scanouts.push(ScanoutBinding {
                scanout_id,
                resource_id,
                width,
                height,
            });
        }
    }

    /// The scanout now shows something this record cannot replay (a blob).
    pub fn scanout_cleared(&mut self, scanout_id: u32) {
        self.scanouts.retain(|s| s.scanout_id != scanout_id);
    }

    /// Whether there is anything to recreate.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scanout_records_only_a_known_2d_resource_and_follows_its_lifetime() {
        let mut d = DisplayReplay::default();
        d.created(7, 2, 1280, 800);
        d.attached(7, vec![(0x1000, 4096), (0x3000, 8192)]);
        d.scanout(0, 7, 1280, 800);
        assert_eq!(d.scanouts.len(), 1);
        assert_eq!(d.resources[0].backing, vec![(0x1000, 4096), (0x3000, 8192)]);

        // A resource the record does not know (3D, blob) is not replayable.
        d.scanout(0, 99, 1280, 800);
        assert!(d.scanouts.is_empty());

        d.scanout(0, 7, 1280, 800);
        d.detached(7);
        assert!(d.resources[0].backing.is_empty());
        d.unref(7);
        assert!(d.is_empty());
        assert!(
            d.scanouts.is_empty(),
            "a scanout of a freed resource goes too"
        );
    }

    #[test]
    fn disabling_or_replacing_a_scanout_drops_its_old_binding() {
        let mut d = DisplayReplay::default();
        d.created(1, 2, 64, 64);
        d.created(2, 2, 64, 64);
        d.scanout(0, 1, 64, 64);
        d.scanout(0, 2, 64, 64);
        assert_eq!(
            d.scanouts,
            vec![ScanoutBinding {
                scanout_id: 0,
                resource_id: 2,
                width: 64,
                height: 64
            }]
        );
        d.scanout(0, 0, 0, 0);
        assert!(d.scanouts.is_empty());
        d.scanout(0, 1, 64, 64);
        d.scanout_cleared(0);
        assert!(d.scanouts.is_empty());
    }

    #[test]
    fn recreating_an_id_replaces_the_old_resource() {
        let mut d = DisplayReplay::default();
        d.created(3, 2, 64, 64);
        d.attached(3, vec![(0x1000, 16384)]);
        d.scanout(0, 3, 64, 64);
        d.created(3, 1, 128, 128);
        assert_eq!(d.resources.len(), 1);
        assert!(d.resources[0].backing.is_empty());
        assert!(d.scanouts.is_empty());
    }
}
