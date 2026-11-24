//! The capture side on Windows: desktop duplication into the ring, with the slot bookkeeping.

use std::sync::Arc;
use std::time::Instant;

use crate::dxgi_ffi::{self, Output};
use crate::ring::{SharedFrame, SharedRing, SlotTable};
use crate::{CaptureError, Grab};

pub(crate) struct Duplication {
    output: Output,
    ring: Arc<SharedRing>,
    slots: SlotTable,
}

impl Duplication {
    pub(crate) fn open(device_name: &str) -> Result<Duplication, CaptureError> {
        let (output, handles) = dxgi_ffi::open(device_name)?;
        let ring = Arc::new(SharedRing::new(output.width, output.height, output.adapter, false, handles));
        Ok(Duplication { output, ring, slots: SlotTable::default() })
    }

    pub(crate) fn ring(&self) -> &Arc<SharedRing> {
        &self.ring
    }

    pub(crate) fn grab(&mut self, now: Instant) -> Result<Grab, CaptureError> {
        if let Some(reason) = self.ring.fallback() {
            return Err(CaptureError::Fallback(reason));
        }
        if !self.output.acquire()? {
            return Ok(Grab::Unchanged);
        }
        // Every texture is still being read: the picture stays held and is copied next time.
        let Some(slot) = self.slots.pick(self.output.released(), now) else { return Ok(Grab::Unchanged) };
        let Some(ready) = self.slots.fill(slot, now) else { return Err(CaptureError::Lost("the fence values ran out".into())) };
        self.output.copy_held(slot, ready)?;
        SharedFrame::new(Arc::clone(&self.ring), slot, ready)
            .map(Grab::Frame)
            .ok_or_else(|| CaptureError::Failed("no such texture in the ring".into()))
    }
}
