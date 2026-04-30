// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(target_os = "android", target_os = "linux"))]
use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

use arch::apply_device_tree_overlays;
use arch::fdt::create_memory_node;
use arch::DtbOverlay;
#[cfg(any(target_os = "android", target_os = "linux"))]
use arch::PlatformBusResources;
use arch::serial::SerialDeviceInfo;
use cros_fdt::Error;
use cros_fdt::Fdt;
use cros_fdt::Result;
use devices::PciAddress;
use devices::PciInterruptPin;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

// Serial clock frequency for the ns16550a UART.
const RISCV64_SERIAL_SPEED: u32 = 1843200;

// This is the start of DRAM in the physical address space.
use crate::RISCV64_PHYS_MEM_START;

// CPUs are assigned phandles starting with this number.
const PHANDLE_CPU0: u32 = 0x100;

const PHANDLE_PLIC: u32 = 2;
const PHANDLE_CPU_INTC_BASE: u32 = 4;

// PLIC constants matching QEMU virt machine
const PLIC_BASE: u64 = 0x0C00_0000;
const PLIC_NUM_SOURCES: u32 = 95; // riscv,ndev value
const PLIC_PCI_IRQ_BASE: u32 = 32; // PCI INTA=32, INTB=33, INTC=34, INTD=35

fn create_cpu_nodes(
    fdt: &mut Fdt,
    num_vcpus: u32,
    timebase_frequency: u32,
    isa_string: &str,
    mmu_type: &str,
) -> Result<()> {
    let cpus_node = fdt.root_mut().subnode_mut("cpus")?;
    cpus_node.set_prop("#address-cells", 0x1u32)?;
    cpus_node.set_prop("#size-cells", 0x0u32)?;
    cpus_node.set_prop("timebase-frequency", timebase_frequency)?;

    for vcpu_id in 0..num_vcpus {
        let cpu_name = format!("cpu@{vcpu_id:x}");
        let cpu_node = cpus_node.subnode_mut(&cpu_name)?;
        cpu_node.set_prop("device_type", "cpu")?;
        cpu_node.set_prop("compatible", "riscv")?;
        cpu_node.set_prop("mmu-type", mmu_type)?;
        cpu_node.set_prop("riscv,isa", isa_string)?;
        cpu_node.set_prop("status", "okay")?;
        cpu_node.set_prop("reg", vcpu_id)?;
        cpu_node.set_prop("phandle", PHANDLE_CPU0 + vcpu_id)?;

        // Add interrupt controller node
        let intc_node = cpu_node.subnode_mut("interrupt-controller")?;
        intc_node.set_prop("compatible", "riscv,cpu-intc")?;
        intc_node.set_prop("#interrupt-cells", 1u32)?;
        intc_node.set_prop("interrupt-controller", ())?;
        intc_node.set_prop("phandle", PHANDLE_CPU_INTC_BASE + vcpu_id)?;
    }
    Ok(())
}

fn create_serial_node(fdt: &mut Fdt, addr: u64, size: u64, irq: u32) -> Result<()> {
    let serial_node = fdt.root_mut().subnode_mut(&format!("U6_16550A@{addr:x}"))?;
    serial_node.set_prop("compatible", "ns16550a")?;
    serial_node.set_prop("reg", &[addr, size])?;
    serial_node.set_prop("clock-frequency", RISCV64_SERIAL_SPEED)?;
    serial_node.set_prop("interrupt-parent", PHANDLE_PLIC)?;
    serial_node.set_prop("interrupts", irq)?;
    Ok(())
}

fn create_serial_nodes(fdt: &mut Fdt, serial_devices: &[SerialDeviceInfo]) -> Result<()> {
    for dev in serial_devices {
        create_serial_node(fdt, dev.address, dev.size, dev.irq)?;
    }
    Ok(())
}

fn create_chosen_node(
    fdt: &mut Fdt,
    cmdline: &str,
    initrd: Option<(GuestAddress, u32)>,
    stdout_path: Option<&str>,
) -> Result<()> {
    let chosen_node = fdt.root_mut().subnode_mut("chosen")?;
    chosen_node.set_prop("linux,pci-probe-only", 1u32)?;
    chosen_node.set_prop("bootargs", cmdline)?;
    if let Some(stdout_path) = stdout_path {
        chosen_node.set_prop("stdout-path", stdout_path)?;
    }

    let kaslr_seed: u64 = rand::random();
    chosen_node.set_prop("kaslr-seed", kaslr_seed)?;

    let rng_seed_bytes: [u8; 256] = rand::random();
    chosen_node.set_prop("rng-seed", &rng_seed_bytes)?;

    if let Some((initrd_addr, initrd_size)) = initrd {
        let initrd_start = initrd_addr.offset();
        let initrd_end = initrd_start + initrd_size as u64;
        chosen_node.set_prop("linux,initrd-start", initrd_start)?;
        chosen_node.set_prop("linux,initrd-end", initrd_end)?;
    }

    Ok(())
}

/// Create a SiFive PLIC node in the FDT.
/// This replaces the AIA (IMSIC+APLIC) nodes for guests with CONFIG_SIFIVE_PLIC=y
/// but no IMSIC driver (kernel < 6.10).
fn create_plic_node(
    fdt: &mut Fdt,
    num_vcpus: usize,
) -> Result<()> {
    // S-mode only: one context per hart
    let num_contexts = num_vcpus;
    let plic_size = 0x200000 + (num_contexts as u64) * 0x1000;

    let name = format!("plic@{PLIC_BASE:#x}");
    let plic_node = fdt.root_mut().subnode_mut(&name)?;
    plic_node.set_prop("compatible", vec!["sifive,plic-1.0.0".to_string(), "riscv,plic0".to_string()])?;
    plic_node.set_prop("reg", &[0u32, PLIC_BASE as u32, 0u32, plic_size as u32])?;
    plic_node.set_prop("#interrupt-cells", 1u32)?;
    plic_node.set_prop("#address-cells", 0u32)?;
    plic_node.set_prop("interrupt-controller", ())?;
    plic_node.set_prop("riscv,ndev", PLIC_NUM_SOURCES)?;
    plic_node.set_prop("phandle", PHANDLE_PLIC)?;

    // interrupts-extended: one entry per hart, S-mode external IRQ (9)
    const S_MODE_EXT_IRQ: u32 = 9;
    let mut intc_regs: Vec<u32> = Vec::with_capacity(num_vcpus * 2);
    for hart in 0..num_vcpus {
        intc_regs.push(PHANDLE_CPU_INTC_BASE + hart as u32);
        intc_regs.push(S_MODE_EXT_IRQ);
    }
    plic_node.set_prop("interrupts-extended", intc_regs)?;

    Ok(())
}

/// PCI host controller address range.
///
/// This represents a single entry in the "ranges" property for a PCI host controller.
///
/// See [PCI Bus Binding to Open Firmware](https://www.openfirmware.info/data/docs/bus.pci.pdf)
/// and https://www.kernel.org/doc/Documentation/devicetree/bindings/pci/host-generic-pci.txt
/// for more information.
#[derive(Copy, Clone)]
pub struct PciRange {
    pub space: PciAddressSpace,
    pub bus_address: u64,
    pub cpu_physical_address: u64,
    pub size: u64,
    pub prefetchable: bool,
}

/// PCI address space.
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub enum PciAddressSpace {
    /// PCI configuration space
    Configuration = 0b00,
    /// I/O space
    Io = 0b01,
    /// 32-bit memory space
    Memory = 0b10,
    /// 64-bit memory space
    Memory64 = 0b11,
}

/// Location of memory-mapped PCI configuration space.
#[derive(Copy, Clone)]
pub struct PciConfigRegion {
    /// Physical address of the base of the memory-mapped PCI configuration region.
    pub base: u64,
    /// Size of the PCI configuration region in bytes.
    pub size: u64,
}

fn create_pci_nodes(
    fdt: &mut Fdt,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    cfg: PciConfigRegion,
    ranges: &[PciRange],
) -> Result<()> {
    // Add devicetree nodes describing a PCI generic host controller.
    // See Documentation/devicetree/bindings/pci/host-generic-pci.txt in the kernel
    // and "PCI Bus Binding to IEEE Std 1275-1994".
    let ranges: Vec<u32> = ranges
        .iter()
        .flat_map(|r| {
            let ss = r.space as u32;
            let p = r.prefetchable as u32;
            [
                // BUS_ADDRESS(3) encoded as defined in OF PCI Bus Binding
                (ss << 24) | (p << 30),
                (r.bus_address >> 32) as u32,
                r.bus_address as u32,
                // CPU_PHYSICAL(2)
                (r.cpu_physical_address >> 32) as u32,
                r.cpu_physical_address as u32,
                // SIZE(2)
                (r.size >> 32) as u32,
                r.size as u32,
            ]
        })
        .collect();

    let bus_range = [0u32, 0u32]; // Only bus 0
    let reg = [cfg.base, cfg.size];

    // PCI interrupt-map: one entry per device, using actual GSI numbers from crosvm.
    // PLIC has #interrupt-cells=1, so each entry is 6 cells:
    //   PCI_ADDR(3) + PIN(1) + PLIC_PHANDLE(1) + IRQ_NUM(1)
    let mut interrupts: Vec<u32> = Vec::new();
    for (address, irq_num, irq_pin) in pci_irqs.iter() {
        // PCI_DEVICE(3)
        interrupts.push(address.to_config_address(0, 8));
        interrupts.push(0);
        interrupts.push(0);
        // INT# pin
        interrupts.push(irq_pin.to_mask() + 1);
        // PLIC phandle + IRQ number (actual GSI from crosvm)
        interrupts.push(PHANDLE_PLIC);
        interrupts.push(*irq_num);
    }

    let mask: &[u32] = &[
        0xf800, // bits 11-15 (device number, all 32 slots)
        0, 0,
        0x7, // INT# pin (1-4)
    ];

    let pci_node = fdt.root_mut().subnode_mut("pci")?;
    pci_node.set_prop("compatible", "pci-host-cam-generic")?;
    pci_node.set_prop("device_type", "pci")?;
    pci_node.set_prop("ranges", ranges)?;
    pci_node.set_prop("bus-range", &bus_range)?;
    pci_node.set_prop("#address-cells", 3u32)?;
    pci_node.set_prop("#size-cells", 2u32)?;
    pci_node.set_prop("reg", &reg)?;
    pci_node.set_prop("#interrupt-cells", 1u32)?;
    pci_node.set_prop("interrupt-map", interrupts)?;
    pci_node.set_prop("interrupt-map-mask", mask)?;
    pci_node.set_prop("dma-coherent", ())?;
    Ok(())
}

/// Creates a flattened device tree containing all of the parameters for the
/// kernel and loads it into the guest memory at the specified offset.
///
/// # Arguments
///
/// * `fdt_max_size` - The amount of space reserved for the device tree
/// * `guest_mem` - The guest memory object
/// * `pci_irqs` - List of PCI device address to PCI interrupt number and pin mappings
/// * `pci_cfg` - Location of the memory-mapped PCI configuration space.
/// * `pci_ranges` - Memory ranges accessible via the PCI host controller.
/// * `num_vcpus` - Number of virtual CPUs the guest will have
/// * `fdt_load_offset` - The offset into physical memory for the device tree
/// * `cmdline` - The kernel commandline
/// * `initrd` - An optional tuple of initrd guest physical address and size
/// * `timebase_frequency` - The time base frequency for the VM.
pub fn create_fdt(
    fdt_max_size: usize,
    guest_mem: &GuestMemory,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    pci_cfg: PciConfigRegion,
    pci_ranges: &[PciRange],
    #[cfg(any(target_os = "android", target_os = "linux"))] platform_dev_resources: Vec<
        PlatformBusResources,
    >,
    num_vcpus: u32,
    fdt_load_offset: u64,
    cmdline: &str,
    initrd: Option<(GuestAddress, u32)>,
    timebase_frequency: u32,
    isa_string: &str,
    mmu_type: &str,
    android_fstab: Option<File>,
    serial_devices: &[SerialDeviceInfo],
    dump_device_tree_blob: Option<PathBuf>,
    device_tree_overlays: Vec<DtbOverlay>,
) -> Result<()> {
    let mut fdt = Fdt::new(&[]);

    // The whole thing is put into one giant node with some top level properties
    let root_node = fdt.root_mut();
    root_node.set_prop("compatible", "linux,dummy-virt")?;
    root_node.set_prop("#address-cells", 0x2u32)?;
    root_node.set_prop("#size-cells", 0x2u32)?;
    if let Some(android_fstab) = android_fstab {
        arch::android::create_android_fdt(&mut fdt, android_fstab)?;
    }
    let stdout_path = serial_devices
        .first()
        .map(|first_serial| format!("/U6_16550A@{:x}", first_serial.address));
    create_chosen_node(&mut fdt, cmdline, initrd, stdout_path.as_deref())?;
    create_memory_node(&mut fdt, guest_mem)?;
    create_cpu_nodes(&mut fdt, num_vcpus, timebase_frequency, isa_string, mmu_type)?;
    create_plic_node(&mut fdt, num_vcpus as usize)?;
    create_serial_nodes(&mut fdt, serial_devices)?;
    create_pci_nodes(&mut fdt, pci_irqs, pci_cfg, pci_ranges)?;

    // Done writing base FDT, now apply DT overlays
    apply_device_tree_overlays(
        &mut fdt,
        device_tree_overlays,
        #[cfg(any(target_os = "android", target_os = "linux"))]
        platform_dev_resources,
        #[cfg(any(target_os = "android", target_os = "linux"))]
        &BTreeMap::new(),
    )?;

    let fdt_final = fdt.finish()?;
    if fdt_final.len() > fdt_max_size {
        return Err(Error::TotalSizeTooLarge);
    }

    if let Some(file_path) = dump_device_tree_blob {
        std::fs::write(&file_path, &fdt_final)
            .map_err(|_| Error::FdtGuestMemoryWriteError)?;
    }

    let fdt_address = GuestAddress(RISCV64_PHYS_MEM_START + fdt_load_offset);
    let written = guest_mem
        .write_at_addr(fdt_final.as_slice(), fdt_address)
        .map_err(|_| Error::FdtGuestMemoryWriteError)?;
    if written < fdt_final.len() {
        return Err(Error::FdtGuestMemoryWriteError);
    }

    Ok(())
}
