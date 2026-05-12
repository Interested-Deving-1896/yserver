//! One composited frame for one output.
//!
//! Phase 1 carries just enough state to push into `InFlight`. The
//! command buffer, descriptors, and the wait dependency on
//! `PaintBatch` arrive in phase 2/3/4.

use ash::vk;

#[derive(Debug)]
pub struct OutputFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub composite_fence: vk::Fence,
    pub bo_slot: Option<usize>,
}

impl OutputFrame {
    pub fn new(
        output_idx: usize,
        frame_id: u64,
        submitted_gen: u64,
        composite_fence: vk::Fence,
        bo_slot: Option<usize>,
    ) -> Self {
        Self {
            output_idx,
            frame_id,
            submitted_gen,
            composite_fence,
            bo_slot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_records_all_fields() {
        let f = OutputFrame::new(0, 1, 7, vk::Fence::null(), Some(2));
        assert_eq!(f.output_idx, 0);
        assert_eq!(f.frame_id, 1);
        assert_eq!(f.submitted_gen, 7);
        assert_eq!(f.bo_slot, Some(2));
    }
}
