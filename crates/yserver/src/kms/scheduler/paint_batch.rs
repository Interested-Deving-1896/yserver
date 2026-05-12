//! A frame's accumulated paint work.
//!
//! Phase 1: shell. The batch is opened at the start of a composite
//! cycle and closed at the end. Recorders still call
//! `run_one_shot_op` directly in phase 1, so the batch carries no
//! Vulkan work yet — only the frame_id and the set of outputs that
//! will composite this cycle.
//!
//! Phase 2 fills in the per-frame primary command buffer,
//! descriptor pool, and scratch arena. Phase 3 migrates recorders
//! to append into the batch instead of submitting directly.

#[derive(Debug)]
pub struct PaintBatch {
    pub frame_id: u64,
    /// Outputs that will composite from this batch (`C(F)` in the
    /// HLD). Captured at batch close time. Phase 1: populated for
    /// shape but not yet load-bearing.
    pub dirty_outputs: Vec<usize>,
}

impl PaintBatch {
    pub fn new(frame_id: u64) -> Self {
        Self {
            frame_id,
            dirty_outputs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_batch_has_empty_output_set() {
        let b = PaintBatch::new(42);
        assert_eq!(b.frame_id, 42);
        assert!(b.dirty_outputs.is_empty());
    }
}
