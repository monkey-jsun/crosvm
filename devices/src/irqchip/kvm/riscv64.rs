// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use base::errno_result;
use base::error;
use base::ioctl_with_ref;
use base::ioctl_with_val;
use base::AsRawDescriptor;
use base::Error as BaseError;
use base::Event;
use base::RawDescriptor;
use base::Result;
use base::SafeDescriptor;
use hypervisor::kvm::KvmVcpu;
use hypervisor::kvm::KvmVm;
use hypervisor::DeviceKind;
use hypervisor::IrqRoute;
use hypervisor::Vcpu;
use hypervisor::Vm;
use kvm_sys::*;
use sync::Mutex;

use crate::IrqChip;
use crate::IrqChipRiscv64;
use crate::IrqEventIndex;
use crate::IrqEventSource;

const RISCV_IRQCHIP: u64 = 0x0800_0000;

const KVM_DEV_RISCV_AIA_ADDR_APLIC: u64 = 0;

pub const AIA_IMSIC_BASE: u64 = RISCV_IRQCHIP;
const KVM_DEV_RISCV_IMSIC_SIZE: u64 = 0x1000;

pub const fn aia_addr_imsic(vcpu_id: u64) -> u64 {
    1 + vcpu_id
}

pub const fn aia_imsic_addr(hart: usize) -> u64 {
    AIA_IMSIC_BASE + (hart as u64) * KVM_DEV_RISCV_IMSIC_SIZE
}

pub const fn aia_imsic_size(num_harts: usize) -> u64 {
    num_harts as u64 * KVM_DEV_RISCV_IMSIC_SIZE
}

pub const fn aia_aplic_addr(num_harts: usize) -> u64 {
    AIA_IMSIC_BASE + (num_harts as u64) * KVM_DEV_RISCV_IMSIC_SIZE
}
pub const AIA_APLIC_SIZE: u32 = 0x4000;

// Connstants for get/set attributes calls.
const KVM_DEV_RISCV_AIA_GRP_CONFIG: u32 = 0;

const KVM_DEV_RISCV_AIA_CONFIG_MODE: u64 = 0;
const KVM_DEV_RISCV_AIA_CONFIG_IDS: u64 = 1;
const KVM_DEV_RISCV_AIA_CONFIG_SRCS: u64 = 2;
const KVM_DEV_RISCV_AIA_CONFIG_HART_BITS: u64 = 5;

pub const IMSIC_MAX_INT_IDS: u64 = 2047;

// CONFIG_MODE values
const AIA_MODE_HWACCEL: u32 = 1;
const AIA_MODE_AUTO: u32 = 2;

const KVM_DEV_RISCV_AIA_GRP_ADDR: u32 = 1;

const KVM_DEV_RISCV_AIA_GRP_CTRL: u32 = 2;

struct AiaDescriptor(SafeDescriptor);

impl AiaDescriptor {
    fn try_clone(&self) -> Result<AiaDescriptor> {
        self.0.try_clone().map(AiaDescriptor)
    }

    fn aia_init(&self) -> Result<()> {
        let init_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_CTRL,
            attr: KVM_DEV_RISCV_AIA_CTRL_INIT as u64,
            addr: 0,
            flags: 0,
        };

        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_aia_mode is
        // pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_DEVICE_ATTR, &init_attr) };
        if ret != 0 {
            return errno_result();
        }
        Ok(())
    }

    fn get_num_ids(&self) -> Result<u32> {
        let mut aia_num_ids = 0;
        let raw_num_ids = &mut aia_num_ids as *mut u32;

        let aia_num_ids_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_CONFIG,
            attr: KVM_DEV_RISCV_AIA_CONFIG_IDS,
            addr: raw_num_ids as u64,
            flags: 0,
        };

        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_num_ids is
        // pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_GET_DEVICE_ATTR, &aia_num_ids_attr) };
        if ret != 0 {
            return errno_result();
        }
        Ok(aia_num_ids)
    }

    fn get_aia_mode(&self) -> Result<u32> {
        let mut aia_mode: u32 = AIA_MODE_HWACCEL;
        let raw_aia_mode = &mut aia_mode as *mut u32;
        let aia_mode_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_CONFIG,
            attr: KVM_DEV_RISCV_AIA_CONFIG_MODE,
            addr: raw_aia_mode as u64,
            flags: 0,
        };
        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_aia_mode is
        // pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_GET_DEVICE_ATTR, &aia_mode_attr) };
        if ret != 0 {
            return errno_result();
        }
        Ok(aia_mode)
    }

    fn set_num_sources(&self, num_sources: u32) -> Result<()> {
        let raw_num_sources = &num_sources as *const u32;
        let kvm_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_CONFIG,
            attr: KVM_DEV_RISCV_AIA_CONFIG_SRCS,
            addr: raw_num_sources as u64,
            flags: 0,
        };
        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_aia_mode is
        // pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_DEVICE_ATTR, &kvm_attr) };
        if ret != 0 {
            return errno_result();
        }
        Ok(())
    }

    fn set_hart_bits(&self, hart_bits: u32) -> Result<()> {
        let raw_hart_bits = &hart_bits as *const u32;
        let kvm_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_CONFIG,
            attr: KVM_DEV_RISCV_AIA_CONFIG_HART_BITS,
            addr: raw_hart_bits as u64,
            flags: 0,
        };
        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_aia_mode is
        // pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_DEVICE_ATTR, &kvm_attr) };
        if ret != 0 {
            return errno_result();
        }
        Ok(())
    }

    fn set_aplic_addrs(&self, num_vcpus: usize) -> Result<()> {
        /* Set AIA device addresses */
        let aplic_addr = aia_aplic_addr(num_vcpus);
        let raw_aplic_addr = &aplic_addr as *const u64;
        let kvm_attr = kvm_device_attr {
            group: KVM_DEV_RISCV_AIA_GRP_ADDR,
            attr: KVM_DEV_RISCV_AIA_ADDR_APLIC,
            addr: raw_aplic_addr as u64,
            flags: 0,
        };
        // SAFETY: Safe because we allocated the struct that's being passed in, and raw_aplic_addr
        // is pointing to a uniquely owned local, mutable variable.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_DEVICE_ATTR, &kvm_attr) };
        if ret != 0 {
            return errno_result();
        }
        for i in 0..num_vcpus {
            let imsic_addr = aia_imsic_addr(i);
            let raw_imsic_addr = &imsic_addr as *const u64;
            let kvm_attr = kvm_device_attr {
                group: KVM_DEV_RISCV_AIA_GRP_ADDR,
                attr: aia_addr_imsic(i as u64),
                addr: raw_imsic_addr as u64,
                flags: 0,
            };
            // SAFETY: Safe because we allocated the struct that's being passed in, and
            // raw_imsic_addr is pointing to a uniquely owned local, mutable variable.
            let ret = unsafe { ioctl_with_ref(self, KVM_SET_DEVICE_ATTR, &kvm_attr) };
            if ret != 0 {
                return errno_result();
            }
        }
        Ok(())
    }
}

impl AsRawDescriptor for AiaDescriptor {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.0.as_raw_descriptor()
    }
}

/// IrqChip implementation where the entire IrqChip is emulated by KVM.
///
/// This implementation will use the KVM API to create and configure the in-kernel irqchip.
/// Stored IRQ event for userspace PLIC handling.
struct PlicIrqEvent {
    irq: u32,
    trigger: Event,
    resample: Option<Event>,
}

use crate::irqchip::riscv64_plic::Plic;
use crate::irqchip::riscv64_plic::PLIC_NUM_SOURCES;

pub struct KvmKernelIrqChip {
    pub(super) vm: KvmVm,
    pub(super) vcpus: Arc<Mutex<Vec<Option<KvmVcpu>>>>,
    num_vcpus: usize,
    num_ids: usize,     // number of imsics ids
    num_sources: usize, // number of aplic sources
    aia: Option<AiaDescriptor>,
    device_kind: DeviceKind,
    pub(super) routes: Arc<Mutex<Vec<IrqRoute>>>,
    /// PLIC IRQ events (userspace handling).
    plic_events: Arc<Mutex<Vec<PlicIrqEvent>>>,
    /// PLIC state.
    pub(crate) plic: Arc<Mutex<Plic>>,
}

impl KvmKernelIrqChip {
    /// Construct a new KvmKernelIrqchip.
    pub fn new(vm: KvmVm, num_vcpus: usize) -> Result<KvmKernelIrqChip> {
        // Create KVM AIA device for proper vCPU interrupt infrastructure.
        // Without AIA, KVM doesn't set up IMSIC interrupt files and timer
        // interrupts don't work correctly. The FDT exposes PLIC (not AIA)
        // to the guest, but AIA runs internally in KVM.
        let aia = AiaDescriptor(vm.create_device(DeviceKind::RiscvAia)?);

        let aia_mode = aia.get_aia_mode()?;
        if aia_mode != AIA_MODE_HWACCEL && aia_mode != AIA_MODE_AUTO {
            return Err(BaseError::new(libc::ENOTSUP));
        }

        const NUM_SOURCES: u32 = 64;
        aia.set_num_sources(NUM_SOURCES)?;

        let num_ids = aia.get_num_ids()?;

        let max_hart_idx = num_vcpus as u64 - 1;
        let num_hart_bits = std::cmp::max(1, 64 - max_hart_idx.leading_zeros());
        aia.set_hart_bits(num_hart_bits)?;

        Ok(KvmKernelIrqChip {
            vm,
            vcpus: Arc::new(Mutex::new((0..num_vcpus).map(|_| None).collect())),
            num_vcpus,
            num_ids: num_ids as usize,
            num_sources: NUM_SOURCES as usize,
            aia: Some(aia),
            device_kind: DeviceKind::RiscvAia,
            routes: Arc::new(Mutex::new(kvm_default_irq_routing_table(
                NUM_SOURCES as usize,
            ))),
            plic_events: Arc::new(Mutex::new(Vec::new())),
            plic: Arc::new(Mutex::new(Plic::new(PLIC_NUM_SOURCES, num_vcpus))),
        })
    }

    /// Attempt to create a shallow clone of this riscv64 KvmKernelIrqChip instance.
    /// This is the arch-specific impl used by `KvmKernelIrqChip::clone()`.
    pub(super) fn arch_try_clone(&self) -> Result<Self> {
        Ok(KvmKernelIrqChip {
            vm: self.vm.try_clone()?,
            vcpus: self.vcpus.clone(),
            num_vcpus: self.num_vcpus,
            num_ids: self.num_ids,
            num_sources: self.num_sources,
            aia: match &self.aia { Some(a) => Some(a.try_clone()?), None => None },
            device_kind: self.device_kind,
            routes: self.routes.clone(),
            plic_events: self.plic_events.clone(),
            plic: self.plic.clone(),
        })
    }

    /// Register an IRQ event for PLIC userspace handling.
    pub fn register_irq_event(
        &mut self,
        irq: u32,
        trigger: Event,
        resample: Option<Event>,
    ) -> Result<Option<IrqEventIndex>> {
        let mut events = self.plic_events.lock();
        let index = events.len();
        events.push(PlicIrqEvent { irq, trigger, resample });
        Ok(Some(index))
    }

    /// Unregister an IRQ event.
    pub fn unregister_irq_event(&self, _irq: u32) -> Result<()> {
        // For simplicity, don't remove — just ignore
        Ok(())
    }

    /// Return IRQ event tokens for polling.
    pub fn plic_irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, IrqEventSource, Event)>> {
        let events = self.plic_events.lock();
        let mut tokens = Vec::new();
        for (i, evt) in events.iter().enumerate() {
            tokens.push((
                i,
                IrqEventSource {
                    device_id: vm_control::DeviceId::PlatformDeviceId(
                        vm_control::PlatformDeviceId::Serial,
                    ),
                    queue_id: evt.irq as usize,
                    device_name: format!("plic-irq-{}", evt.irq),
                },
                evt.trigger.try_clone()?,
            ));
        }
        Ok(tokens)
    }

    /// Service an IRQ: update PLIC state.
    /// Holds plic lock across KVM_INTERRUPT to prevent TOCTOU race where
    /// a stale UNSET from a concurrent complete clobbers a newer SET.
    pub fn plic_service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        let mut plic = self.plic.lock();
        plic.set_irq(irq, level);
        let changes = plic.update();
        // Keep plic locked while issuing KVM_INTERRUPT (lock order: plic → vcpus)
        let vcpus = self.vcpus.lock();
        for (ctx, assert) in changes {
            if let Some(Some(vcpu)) = vcpus.get(ctx) {
                let interrupt = kvm_interrupt {
                    irq: if assert { KVM_INTERRUPT_SET } else { KVM_INTERRUPT_UNSET } as u32,
                };
                let _ = unsafe { ioctl_with_ref(vcpu, KVM_INTERRUPT, &interrupt) };
            }
        }
        Ok(())
    }

    /// Service an IRQ event: read the event, update PLIC, signal vCPUs.
    /// Holds plic lock across KVM_INTERRUPT to prevent TOCTOU race where
    /// a stale UNSET from a concurrent complete clobbers a newer SET.
    pub fn plic_service_irq_event(&mut self, event_index: IrqEventIndex) -> Result<()> {
        let events = self.plic_events.lock();
        if let Some(evt) = events.get(event_index) {
            // Read the eventfd to acknowledge/consume the event
            let _ = evt.trigger.wait();
            let irq = evt.irq;
            drop(events);

            // Edge-triggered: assert then immediately deassert.
            // Pending bit stays set (only cleared by claim), but source_level
            // goes back to false so complete() won't re-pend.
            let mut plic = self.plic.lock();
            plic.set_irq(irq, true);
            plic.set_irq(irq, false);
            let changes = plic.update();
            // Keep plic locked while issuing KVM_INTERRUPT (lock order: plic → vcpus)
            let vcpus = self.vcpus.lock();
            for (ctx, assert) in changes {
                if let Some(Some(vcpu)) = vcpus.get(ctx) {
                    let interrupt = kvm_interrupt {
                        irq: if assert { KVM_INTERRUPT_SET } else { KVM_INTERRUPT_UNSET } as u32,
                    };
                    let _ = unsafe { ioctl_with_ref(vcpu, KVM_INTERRUPT, &interrupt) };
                }
            }
        }
        Ok(())
    }

    /// Inject pending PLIC interrupts for a vCPU.
    pub fn plic_inject_interrupts(&self, _vcpu: &dyn Vcpu) -> Result<()> {
        // Interrupt injection is handled in plic_service_irq and PlicBusDevice.
        // The KVM_INTERRUPT is called when IRQ state changes, not on every vCPU entry.
        Ok(())
    }
}

impl IrqChipRiscv64 for KvmKernelIrqChip {
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipRiscv64>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn as_irq_chip(&self) -> &dyn IrqChip {
        self
    }

    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip {
        self
    }

    fn finalize(&self) -> Result<()> {
        // PLIC mode: no AIA to finalize.
        if let Some(ref aia) = self.aia {
            aia.set_aplic_addrs(self.num_vcpus)?;
            aia.aia_init()?;
        }
        Ok(())
    }

    fn get_num_ids_sources(&self) -> (usize, usize) {
        // Report 0 sources to FDT so APLIC node is omitted.
        // KVM has 64 sources internally for proper AIA initialization,
        // but the guest kernel can't use APLIC (driver fails to probe).
        (self.num_ids, 0)
    }

    fn get_plic(&self) -> Arc<Mutex<Plic>> {
        self.plic.clone()
    }

    fn get_vcpus(&self) -> Arc<Mutex<Vec<Option<KvmVcpu>>>> {
        self.vcpus.clone()
    }
}

/// Default RiscV routing table.
fn kvm_default_irq_routing_table(num_sources: usize) -> Vec<IrqRoute> {
    let mut routes: Vec<IrqRoute> = Vec::new();

    for i in 0..num_sources {
        routes.push(IrqRoute::aia_irq_route(i as u32));
    }

    routes
}
