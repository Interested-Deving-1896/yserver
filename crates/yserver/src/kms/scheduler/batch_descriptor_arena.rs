//! Per-batch descriptor pool for paint-side recorders.
//!
//! Phase 3D migrations (render, text) route descriptor allocations
//! through this arena so multiple recorder appends in one CB don't
//! invalidate each other's descriptor sets via shared-pool reset.
//!
//! Sizing: each batch gets one pool sized for a typical batch
//! (256 sets, 1024 COMBINED_IMAGE_SAMPLER, 256 UNIFORM_BUFFER,
//! 64 STORAGE_BUFFER). Growth allocates an additional pool chunk
//! when the active pool is exhausted — recorded sets in earlier
//! chunks stay valid because pools are released only at batch
//! retirement.
//!
//! # Paint-side descriptor pool catalogue (for 3D plan author)
//!
//! Audit grep:
//! ```text
//! rg -nB1 -A4 'create_descriptor_pool|allocate_descriptor_sets|reset_descriptor_pool' \
//!    crates/yserver/src/kms/vk/ | grep -v 'composite_pool_ring\|pipeline.rs\b'
//! ```
//!
//! Results (as of 2026-05-13):
//!
//! - `render_pipeline.rs:392` — `RenderPipelineCache::new` calls `create_descriptor_pool`
//!   (one pool per cache instance, sized for COMBINED_IMAGE_SAMPLER descriptors).
//! - `render_pipeline.rs:451` — `RenderPipelineCache::reset_descriptors` calls
//!   `reset_descriptor_pool`. This is the shared-pool reset that's unsafe across
//!   multiple recorder appends in one CB — the primary motivation for this arena.
//! - `render_pipeline.rs:471` — `RenderPipelineCache::allocate_descriptor_for_views`
//!   calls `allocate_descriptor_sets` from the shared pool.
//! - `text_pipeline.rs:273` — `TextPipeline::new` calls `create_descriptor_pool`
//!   (one pool per pipeline instance, for the glyph atlas sampler set).
//! - `text_pipeline.rs:289` — `TextPipeline::new` calls `allocate_descriptor_sets`
//!   immediately after pool creation (one pre-allocated set for atlas binding).
//!
//! The `compositor.rs:156` hit is from the composite path and routes through
//! `CompositePoolRing` (phase 2) — NOT a paint-side pool; 3D leaves it alone.
//!
//! `dst_readback.rs`, `logic_fill_pipeline.rs`, and the `ops/` directory have
//! no descriptor pool calls.
//!
//! 3D migration plan: route `RenderPipelineCache::allocate_descriptor_for_views`
//! and `TextPipeline`'s atlas-set allocation through `BatchDescriptorArena`
//! instead of the per-pipeline shared pools. The shared pools can then be removed
//! from those pipeline types once all their callers are migrated.

use std::sync::Arc;

use ash::vk;

use crate::kms::{scheduler::paint_batch::BatchResource, vk::device::VkContext};

pub struct BatchDescriptorArena {
    vk: Arc<VkContext>,
    pools: Vec<vk::DescriptorPool>,
    /// Approximate sets remaining in the active pool. When 0, the
    /// next `allocate_set` grows. This is heuristic — Vulkan
    /// returns `OUT_OF_POOL_MEMORY` if a specific descriptor type
    /// is exhausted before the set count is.
    sets_remaining_in_active: u32,
}

impl std::fmt::Debug for BatchDescriptorArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchDescriptorArena")
            .field("pools", &self.pools.len())
            .field("sets_remaining_in_active", &self.sets_remaining_in_active)
            .finish_non_exhaustive()
    }
}

const SETS_PER_POOL: u32 = 256;
const SAMPLERS_PER_POOL: u32 = 1024;
const UNIFORMS_PER_POOL: u32 = 256;
const STORAGE_PER_POOL: u32 = 64;

impl BatchDescriptorArena {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            sets_remaining_in_active: 0,
        }
    }

    /// Allocate one descriptor set with `layout`. Grows the pool
    /// if the active one is exhausted (or if allocation returns
    /// `OUT_OF_POOL_MEMORY`).
    pub fn allocate_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        if self.sets_remaining_in_active == 0 {
            self.grow()?;
        }
        let pool = *self.pools.last().expect("just grew");
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        match unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => {
                self.sets_remaining_in_active -= 1;
                Ok(sets[0])
            }
            Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY) | Err(vk::Result::ERROR_FRAGMENTED_POOL) => {
                // Pool is full despite our counter; force grow + retry once.
                self.sets_remaining_in_active = 0;
                self.grow()?;
                let pool = *self.pools.last().expect("just grew");
                let alloc_info = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts);
                let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
                self.sets_remaining_in_active -= 1;
                Ok(sets[0])
            }
            Err(e) => Err(e),
        }
    }

    fn grow(&mut self) -> Result<(), vk::Result> {
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(SAMPLERS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(UNIFORMS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(STORAGE_PER_POOL),
        ];
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SETS_PER_POOL)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { self.vk.device.create_descriptor_pool(&info, None)? };
        self.pools.push(pool);
        self.sets_remaining_in_active = SETS_PER_POOL;
        Ok(())
    }
}

impl BatchResource for BatchDescriptorArena {
    fn release(self: Box<Self>, vk: &VkContext) {
        for p in self.pools {
            unsafe { vk.device.destroy_descriptor_pool(p, None) };
        }
    }
}
