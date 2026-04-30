// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::error;
use base::Error;
use base::Event;
use base::Result;
#[cfg(target_arch = "x86_64")]
use hypervisor::kvm::KvmCap;
use hypervisor::kvm::KvmVcpu;
use hypervisor::IrqRoute;
use hypervisor::MPState;
use hypervisor::Vcpu;
use kvm_sys::kvm_mp_state;
use resources::SystemAllocator;

use crate::Bus;
use crate::IrqEdgeEvent;
use crate::IrqEventSource;
use crate::IrqLevelEvent;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

use crate::IrqChip;
use crate::IrqChipCap;
use crate::IrqEventIndex;
use crate::VcpuRunState;

/// This IrqChip only works with Kvm so we only implement it for KvmVcpu.
impl IrqChip for KvmKernelIrqChip {
    /// Add a vcpu to the irq chip.
    fn add_vcpu(&mut self, vcpu_id: usize, vcpu: &dyn Vcpu) -> Result<()> {
        let vcpu: &KvmVcpu = vcpu
            .downcast_ref()
            .expect("KvmKernelIrqChip::add_vcpu called with non-KvmVcpu");
        self.vcpus.lock()[vcpu_id] = Some(vcpu.try_clone()?);
        Ok(())
    }

    /// Register an event with edge-trigger semantic that can trigger an interrupt
    /// for a particular GSI.
    fn register_edge_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqEdgeEvent,
        _source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        #[cfg(not(target_arch = "riscv64"))]
        {
            self.vm.register_irqfd(irq, irq_event.get_trigger(), None)?;
            Ok(None)
        }
        #[cfg(target_arch = "riscv64")]
        {
            // PLIC mode: return event index so main loop adds it to poll context.
            // The IrqSetup::Event handler in linux.rs adds it to wait_ctx immediately.
            self.register_irq_event(irq, irq_event.get_trigger().try_clone()?, None)
        }
    }

    /// Unregister an event with edge-trigger semantic for a particular GSI.
    fn unregister_edge_irq_event(&mut self, irq: u32, irq_event: &IrqEdgeEvent) -> Result<()> {
        #[cfg(not(target_arch = "riscv64"))]
        { self.vm.unregister_irqfd(irq, irq_event.get_trigger()) }
        #[cfg(target_arch = "riscv64")]
        { self.unregister_irq_event(irq) }
    }

    /// Register an event with level-trigger semantic that can trigger an interrupt
    /// for a particular GSI.
    fn register_level_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqLevelEvent,
        _source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        #[cfg(not(target_arch = "riscv64"))]
        {
            self.vm
                .register_irqfd(irq, irq_event.get_trigger(), Some(irq_event.get_resample()))?;
            Ok(None)
        }
        #[cfg(target_arch = "riscv64")]
        {
            self.register_irq_event(
                irq,
                irq_event.get_trigger().try_clone()?,
                Some(irq_event.get_resample().try_clone()?),
            )
        }
    }

    /// Unregister an event with level-trigger semantic for a particular GSI.
    fn unregister_level_irq_event(&mut self, irq: u32, irq_event: &IrqLevelEvent) -> Result<()> {
        #[cfg(not(target_arch = "riscv64"))]
        { self.vm.unregister_irqfd(irq, irq_event.get_trigger()) }
        #[cfg(target_arch = "riscv64")]
        { self.unregister_irq_event(irq) }
    }

    /// Route an IRQ line to an interrupt controller, or to a particular MSI vector.
    fn route_irq(&mut self, route: IrqRoute) -> Result<()> {
        let mut routes = self.routes.lock();
        routes.retain(|r| r.gsi != route.gsi);
        routes.push(route);

        #[cfg(not(target_arch = "riscv64"))]
        { self.vm.set_gsi_routing(&routes) }
        #[cfg(target_arch = "riscv64")]
        { Ok(()) } // PLIC mode: no KVM GSI routing
    }

    /// Replace all irq routes with the supplied routes
    fn set_irq_routes(&mut self, routes: &[IrqRoute]) -> Result<()> {
        let mut current_routes = self.routes.lock();
        *current_routes = routes.to_vec();

        #[cfg(not(target_arch = "riscv64"))]
        { self.vm.set_gsi_routing(&current_routes) }
        #[cfg(target_arch = "riscv64")]
        { Ok(()) } // PLIC mode: no KVM GSI routing
    }

    /// Return a vector of all registered irq numbers and their associated events and event
    /// indices.
    fn irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, IrqEventSource, Event)>> {
        #[cfg(not(target_arch = "riscv64"))]
        { Ok(Vec::new()) }
        #[cfg(target_arch = "riscv64")]
        { self.plic_irq_event_tokens() }
    }

    /// Either assert or deassert an IRQ line.
    fn service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        #[cfg(not(target_arch = "riscv64"))]
        { self.vm.set_irq_line(irq, level) }
        #[cfg(target_arch = "riscv64")]
        { self.plic_service_irq(irq, level) }
    }

    /// Service an IRQ event by asserting then deasserting an IRQ line.
    fn service_irq_event(&mut self, event_index: IrqEventIndex) -> Result<()> {
        #[cfg(not(target_arch = "riscv64"))]
        {
            let _ = event_index;
            error!("service_irq_event should never be called for KvmKernelIrqChip");
            Ok(())
        }
        #[cfg(target_arch = "riscv64")]
        { self.plic_service_irq_event(event_index) }
    }

    /// Broadcast an end of interrupt.
    /// This should never be called on a KvmKernelIrqChip because a KVM vcpu should never exit
    /// with the KVM_EXIT_EOI_BROADCAST reason when an in-kernel irqchip exists.
    fn broadcast_eoi(&self, _vector: u8) -> Result<()> {
        error!("broadcast_eoi should never be called for KvmKernelIrqChip");
        Ok(())
    }

    /// Injects any pending interrupts for `vcpu`.
    fn inject_interrupts(&self, vcpu: &dyn Vcpu) -> Result<()> {
        #[cfg(not(target_arch = "riscv64"))]
        { let _ = vcpu; Ok(()) }
        #[cfg(target_arch = "riscv64")]
        { self.plic_inject_interrupts(vcpu) }
    }

    /// Notifies the irq chip that the specified VCPU has executed a halt instruction.
    /// For KvmKernelIrqChip this is a no-op because KVM handles VCPU blocking.
    fn halted(&self, _vcpu_id: usize) {}

    /// Blocks until `vcpu` is in a runnable state or until interrupted by
    /// `IrqChip::kick_halted_vcpus`.  Returns `VcpuRunState::Runnable if vcpu is runnable, or
    /// `VcpuRunState::Interrupted` if the wait was interrupted.
    /// For KvmKernelIrqChip this is a no-op and always returns Runnable because KVM handles VCPU
    /// blocking.
    fn wait_until_runnable(&self, _vcpu: &dyn Vcpu) -> Result<VcpuRunState> {
        Ok(VcpuRunState::Runnable)
    }

    /// Makes unrunnable VCPUs return immediately from `wait_until_runnable`.
    /// For KvmKernelIrqChip this is a no-op because KVM handles VCPU blocking.
    fn kick_halted_vcpus(&self) {}

    /// Get the current MP state of the specified VCPU.
    fn get_mp_state(&self, vcpu_id: usize) -> Result<MPState> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => Ok(MPState::from(&vcpu.get_mp_state()?)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Set the current MP state of the specified VCPU.
    fn set_mp_state(&mut self, vcpu_id: usize, state: &MPState) -> Result<()> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => vcpu.set_mp_state(&kvm_mp_state::from(state)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Attempt to clone this IrqChip instance.
    fn try_clone(&self) -> Result<Self> {
        // Because the KvmKernelIrqchip struct contains arch-specific fields we leave the
        // cloning to arch-specific implementations
        self.arch_try_clone()
    }

    /// Finalize irqchip setup. Should be called once all devices have registered irq events and
    /// been added to the io_bus and mmio_bus.
    /// KvmKernelIrqChip does not need to do anything here.
    fn finalize_devices(
        &mut self,
        _resources: &mut SystemAllocator,
        _io_bus: &Bus,
        _mmio_bus: &Bus,
    ) -> Result<()> {
        Ok(())
    }

    /// The KvmKernelIrqChip doesn't process irq events itself so this function does nothing.
    fn process_delayed_irq_events(&mut self) -> Result<()> {
        Ok(())
    }

    fn irq_delayed_event_token(&self) -> Result<Option<Event>> {
        Ok(None)
    }

    fn check_capability(&self, c: IrqChipCap) -> bool {
        match c {
            #[cfg(target_arch = "x86_64")]
            IrqChipCap::TscDeadlineTimer => self.vm.check_raw_capability(KvmCap::TscDeadlineTimer),
            #[cfg(target_arch = "x86_64")]
            IrqChipCap::X2Apic => true,
            IrqChipCap::MpStateGetSet => true,
        }
    }
}
