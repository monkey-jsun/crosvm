// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SiFive PLIC (Platform Level Interrupt Controller) emulation for riscv64.
//!
//! This implements a userspace PLIC for use with KVM on riscv64, allowing
//! guests with CONFIG_SIFIVE_PLIC=y (but no IMSIC driver) to receive
//! wired interrupts from PCI devices.
//!
//! Based on the SiFive PLIC specification and QEMU's sifive_plic.c.

use std::sync::Arc;
use sync::Mutex;

use crate::BusAccessInfo;
use crate::BusDevice;
use crate::Suspendable;

// PLIC constants matching QEMU virt machine and Linux irq-sifive-plic.c
pub const PLIC_BASE: u64 = 0x0C00_0000;
pub const PLIC_NUM_SOURCES: u32 = 96; // 95 usable (0 is reserved)
pub const PLIC_NUM_PRIORITIES: u32 = 7;

// PCI INTx sources (matching QEMU convention)
pub const PLIC_PCI_IRQ_BASE: u32 = 32;

// Register offsets
const PRIORITY_BASE: u64 = 0x0;
const PENDING_BASE: u64 = 0x1000;
const ENABLE_BASE: u64 = 0x2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_BASE: u64 = 0x20_0000;
const CONTEXT_STRIDE: u64 = 0x1000;
const CONTEXT_THRESHOLD: u64 = 0x0;
const CONTEXT_CLAIM: u64 = 0x4;

// S-mode external interrupt number for KVM_INTERRUPT
const IRQ_S_EXT: u32 = 9;

/// State for a single PLIC context (one per hart in S-mode).
#[derive(Clone)]
struct PlicContext {
    /// Priority threshold — only IRQs with priority > threshold are delivered.
    threshold: u32,
}

/// SiFive PLIC interrupt controller state.
pub struct Plic {
    /// Number of interrupt sources (max source ID + 1).
    num_sources: u32,
    /// Number of contexts (one per hart for S-mode only).
    num_contexts: usize,
    /// Priority for each source (0 = disabled).
    priority: Vec<u32>,
    /// Pending bits (one bit per source).
    pending: Vec<u32>,
    /// Claimed/in-service bits (one bit per source).
    claimed: Vec<u32>,
    /// Enable bits per context (num_contexts * bitfield_words).
    enable: Vec<Vec<u32>>,
    /// Per-context state.
    contexts: Vec<PlicContext>,
    /// Current level of each source (for level-triggered re-assertion).
    source_level: Vec<bool>,
}

impl Plic {
    pub fn new(num_sources: u32, num_contexts: usize) -> Self {
        let words = ((num_sources + 31) / 32) as usize;
        Plic {
            num_sources,
            num_contexts,
            priority: vec![0; num_sources as usize],
            pending: vec![0; words],
            claimed: vec![0; words],
            enable: vec![vec![0u32; words]; num_contexts],
            contexts: vec![PlicContext { threshold: 0 }; num_contexts],
            source_level: vec![false; num_sources as usize],
        }
    }

    /// Set or clear a source's level. Sets pending on rising edge.
    pub fn set_irq(&mut self, irq: u32, level: bool) {
        if irq == 0 || irq >= self.num_sources {
            return;
        }
        let was = self.source_level[irq as usize];
        self.source_level[irq as usize] = level;

        if level && !was {
            // Rising edge: set pending
            let word = (irq / 32) as usize;
            let bit = irq % 32;
            self.pending[word] |= 1 << bit;
        }
    }

    /// Find the best (highest priority) pending+enabled+unclaimed IRQ for a context.
    fn best_irq(&self, context: usize) -> Option<u32> {
        let threshold = self.contexts[context].threshold;
        let mut best_irq: Option<u32> = None;
        let mut best_pri: u32 = 0;

        for word_idx in 0..self.pending.len() {
            let candidates = self.pending[word_idx]
                & self.enable[context][word_idx]
                & !self.claimed[word_idx];
            if candidates == 0 {
                continue;
            }
            for bit in 0..32 {
                if candidates & (1 << bit) == 0 {
                    continue;
                }
                let irq = (word_idx as u32) * 32 + bit;
                if irq == 0 || irq >= self.num_sources {
                    continue;
                }
                let pri = self.priority[irq as usize];
                if pri > threshold && pri > best_pri {
                    best_pri = pri;
                    best_irq = Some(irq);
                }
            }
        }
        best_irq
    }

    /// Evaluate whether each context should have its external IRQ asserted.
    /// Returns Vec<(context_id, should_assert)> for ALL contexts unconditionally.
    /// This avoids the desync bug where change-tracking caused stuck ext_irq_pending.
    pub fn update(&mut self) -> Vec<(usize, bool)> {
        let mut result = Vec::new();
        for ctx in 0..self.num_contexts {
            let best = self.best_irq(ctx);
            let should_assert = best.is_some();
            result.push((ctx, should_assert));
        }
        result
    }

    /// Claim the highest priority IRQ for a context.
    pub fn claim(&mut self, context: usize) -> u32 {
        if let Some(irq) = self.best_irq(context) {
            let word = (irq / 32) as usize;
            let bit = irq % 32;
            self.pending[word] &= !(1 << bit);
            self.claimed[word] |= 1 << bit;
            irq
        } else {
            0 // No interrupt to claim
        }
    }

    /// Complete (EOI) an IRQ for a context.
    pub fn complete(&mut self, context: usize, irq: u32) {
        if irq == 0 || irq >= self.num_sources {
            return;
        }
        let word = (irq / 32) as usize;
        let bit = irq % 32;
        self.claimed[word] &= !(1 << bit);

        // Re-pend if source is still asserted (level-triggered)
        let repend = self.source_level[irq as usize];
        if repend {
            self.pending[word] |= 1 << bit;
        }
        // repend handled above
    }

    /// Read a PLIC MMIO register.
    pub fn read(&self, offset: u64) -> u32 {
        if offset < PENDING_BASE {
            // Priority registers
            let irq = (offset / 4) as usize;
            if irq < self.num_sources as usize {
                return self.priority[irq];
            }
        } else if offset < ENABLE_BASE {
            // Pending registers (read-only)
            let word = ((offset - PENDING_BASE) / 4) as usize;
            if word < self.pending.len() {
                return self.pending[word];
            }
        } else if offset < CONTEXT_BASE {
            // Enable registers
            let ctx_offset = offset - ENABLE_BASE;
            let context = (ctx_offset / ENABLE_STRIDE) as usize;
            let word = ((ctx_offset % ENABLE_STRIDE) / 4) as usize;
            if context < self.num_contexts && word < self.enable[context].len() {
                return self.enable[context][word];
            }
        } else {
            // Context registers (threshold + claim)
            let ctx_offset = offset - CONTEXT_BASE;
            let context = (ctx_offset / CONTEXT_STRIDE) as usize;
            let reg = ctx_offset % CONTEXT_STRIDE;
            if context < self.num_contexts {
                match reg {
                    CONTEXT_THRESHOLD => return self.contexts[context].threshold,
                    CONTEXT_CLAIM => {
                        // Can't claim in read() since we need &mut self
                        // This will be handled specially in the BusDevice impl
                        return 0;
                    }
                    _ => {}
                }
            }
        }
        0
    }

    /// Write a PLIC MMIO register.
    pub fn write(&mut self, offset: u64, value: u32) {
        if offset < PENDING_BASE {
            // Priority registers
            let irq = (offset / 4) as usize;
            if irq > 0 && irq < self.num_sources as usize {
                self.priority[irq] = value & PLIC_NUM_PRIORITIES;
            }
        } else if offset < ENABLE_BASE {
            // Pending registers are read-only
        } else if offset < CONTEXT_BASE {
            // Enable registers
            let ctx_offset = offset - ENABLE_BASE;
            let context = (ctx_offset / ENABLE_STRIDE) as usize;
            let word = ((ctx_offset % ENABLE_STRIDE) / 4) as usize;
            if context < self.num_contexts && word < self.enable[context].len() {
                self.enable[context][word] = value;
            }
        } else {
            // Context registers
            let ctx_offset = offset - CONTEXT_BASE;
            let context = (ctx_offset / CONTEXT_STRIDE) as usize;
            let reg = ctx_offset % CONTEXT_STRIDE;
            if context < self.num_contexts {
                match reg {
                    CONTEXT_THRESHOLD => {
                        self.contexts[context].threshold = value & PLIC_NUM_PRIORITIES;
                    }
                    CONTEXT_CLAIM => {
                        // Write to claim register = complete/EOI
                        self.complete(context, value);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Total MMIO aperture size.
    pub fn aperture_size(&self) -> u64 {
        CONTEXT_BASE + (self.num_contexts as u64) * CONTEXT_STRIDE
    }
}

/// PLIC as a BusDevice for MMIO access from the guest.
pub struct PlicBusDevice {
    plic: Arc<Mutex<Plic>>,
    /// Callback to inject/clear external interrupt on a vCPU.
    vcpu_kick: Arc<dyn Fn(usize, bool) + Send + Sync>,
    /// Callback to signal resample for an IRQ on EOI (so devices can re-assert).
    eoi_callback: Arc<dyn Fn(u32) + Send + Sync>,
}

impl PlicBusDevice {
    pub fn new(
        plic: Arc<Mutex<Plic>>,
        vcpu_kick: Arc<dyn Fn(usize, bool) + Send + Sync>,
        eoi_callback: Arc<dyn Fn(u32) + Send + Sync>,
    ) -> Self {
        PlicBusDevice { plic, vcpu_kick, eoi_callback }
    }
}

impl Suspendable for PlicBusDevice {}

impl BusDevice for PlicBusDevice {
    fn debug_label(&self) -> String {
        "PLIC".to_string()
    }

    fn device_id(&self) -> vm_control::DeviceId {
        vm_control::DeviceId::PlatformDeviceId(vm_control::PlatformDeviceId::Serial)
    }

    /// Read a PLIC register.  For claim reads, holds plic lock across
    /// vcpu_kick to prevent TOCTOU race (lock order: plic → vcpus).
    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        if data.len() != 4 {
            return;
        }
        let offset = info.offset;
        let mut plic = self.plic.lock();

        let value = if offset >= CONTEXT_BASE {
            let ctx_offset = offset - CONTEXT_BASE;
            let context = (ctx_offset / CONTEXT_STRIDE) as usize;
            let reg = ctx_offset % CONTEXT_STRIDE;
            if reg == CONTEXT_CLAIM && context < plic.num_contexts {
                // Claim: atomically find best IRQ, clear pending, set claimed
                let irq = plic.claim(context);
                let changes = plic.update();
                // Keep plic locked while kicking vCPUs
                for (ctx, assert) in changes {
                    (self.vcpu_kick)(ctx, assert);
                }
                irq
            } else {
                plic.read(offset)
            }
        } else {
            plic.read(offset)
        };

        data.copy_from_slice(&value.to_le_bytes());
    }

    /// Write a PLIC register.  Holds plic lock across vcpu_kick to
    /// prevent TOCTOU race (lock order: plic → vcpus).
    /// On EOI (complete), signals resample so devices can re-assert.
    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        if data.len() != 4 {
            return;
        }
        let offset = info.offset;
        let value = u32::from_le_bytes(data.try_into().unwrap());

        // Detect EOI (complete) writes to signal resample afterward
        let mut eoi_irq: Option<u32> = None;
        if offset >= CONTEXT_BASE {
            let ctx_offset = offset - CONTEXT_BASE;
            let reg = ctx_offset % CONTEXT_STRIDE;
            if reg == CONTEXT_CLAIM && value != 0 {
                eoi_irq = Some(value);
            }
        }

        let mut plic = self.plic.lock();
        plic.write(offset, value);
        let changes = plic.update();
        // Keep plic locked while kicking vCPUs
        for (ctx, assert) in changes {
            (self.vcpu_kick)(ctx, assert);
        }
        drop(plic);

        // Signal resample AFTER releasing plic lock to avoid deadlock
        // (resample thread may call trigger → service_irq_event → plic lock)
        if let Some(irq) = eoi_irq {
            (self.eoi_callback)(irq);
        }
    }
}
