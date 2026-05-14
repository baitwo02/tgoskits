use ax_plat::mem::{Aligned4K, pa};

use crate::config::plat::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET};

#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[unsafe(link_section = ".data")]
static mut BOOT_PT_SV39: Aligned4K<[u64; 512]> = Aligned4K::new([0; 512]);

// SG2002 / CV181x XuanTie C906 PTE attribute bits (beyond standard RISC-V):
//   bit[63] SO    – Strongly Ordered (device / non-cacheable)
//   bit[62] CACHE – Cacheable
//   bit[61] BUF   – Bufferable
//   bit[60] SHARE – Shareable
//
// PAGE_IOREMAP: SO=1, SHARE=1  (uncacheable, strongly-ordered)
// PAGE_KERNEL:  CACHE=1, BUF=1, SHARE=1  (write-back, shareable)
#[allow(clippy::identity_op)] // (0x0 << 10) here makes sense because it's an address
unsafe fn init_boot_page_table() {
    unsafe {
        // 0x0000_0000..0x4000_0000, device/MMIO, 1G block  (SO | SHARE)
        BOOT_PT_SV39[0] = (0x0 << 10) | 0xef | (1 << 63) | (1 << 60);
        // 0x4000_0000..0x8000_0000, device/MMIO, 1G block  (SO | SHARE)
        BOOT_PT_SV39[1] = (0x40000 << 10) | 0xef | (1 << 63) | (1 << 60);
        // 0x8000_0000..0xC000_0000, kernel RAM, 1G block   (CACHE | BUF | SHARE)
        BOOT_PT_SV39[2] = (0x80000 << 10) | 0xef | (0x7 << 60);
        // 0xffff_ffc0_0000_0000..0xffff_ffc0_4000_0000, device/MMIO (SO | SHARE)
        BOOT_PT_SV39[0x100] = (0x0 << 10) | 0xef | (1 << 63) | (1 << 60);
        // 0xffff_ffc0_4000_0000..0xffff_ffc0_8000_0000, device/MMIO (SO | SHARE)
        BOOT_PT_SV39[0x101] = (0x40000 << 10) | 0xef | (1 << 63) | (1 << 60);
        // 0xffff_ffc0_8000_0000..0xffff_ffc0_C000_0000, kernel RAM  (CACHE | BUF | SHARE)
        BOOT_PT_SV39[0x102] = (0x80000 << 10) | 0xef | (0x7 << 60);
    }
}

unsafe fn init_mmu() {
    unsafe {
        ax_cpu::asm::write_kernel_page_table(pa!(&raw const BOOT_PT_SV39 as usize));
        ax_cpu::asm::flush_tlb(None);
    }
}

/// Early UART output before MMU/driver init (direct MMIO write to UART0).
/// Outputs "Boot\r\n" so we know the CPU has reached this point.
#[unsafe(naked)]
unsafe extern "C" fn early_tests() {
    core::arch::naked_asm!(
        // UART0 base address (SG2002: 0x0414_0000)
        "li t0, 0x4140000",

        // output 'B'
        "li t1, 'B'",
        "sb t1, 0(t0)",
        // output 'o'
        "li t1, 'o'",
        "sb t1, 0(t0)",
        // output 'o'
        "li t1, 'o'",
        "sb t1, 0(t0)",
        // output 't'
        "li t1, 't'",
        "sb t1, 0(t0)",
        // output '\r'
        "li t1, 0x0d",
        "sb t1, 0(t0)",
        // output '\n'
        "li t1, 0x0a",
        "sb t1, 0(t0)",

        "ret",
    );
}

/// The earliest entry point for the primary CPU.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    // PC = 0x8020_0000
    // a0 = hartid
    // a1 = dtb
    core::arch::naked_asm!(
        "
        mv      s0, a0                  // save hartid
        mv      s1, a1                  // save DTB pointer
        la      sp, {boot_stack}
        li      t0, {boot_stack_size}
        add     sp, sp, t0              // setup boot stack

        call    {init_boot_page_table}

        call    {early_tests}           // early UART test (before MMU)
        call    {init_mmu}              // setup boot page table and enable MMU
        call    {early_tests}           // early UART test (after MMU)

        li      s2, {phys_virt_offset}  // fix up virtual high address
        add     sp, sp, s2

        mv      a0, s0
        addi    a0, a0, -1              // map hart1 -> cpu0
        mv      a1, s1
        la      a2, {entry}
        add     a2, a2, s2
        jalr    a2                      // call_main(cpu_id, dtb)
        j       .",
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        boot_stack_size = const BOOT_STACK_SIZE,
        boot_stack = sym BOOT_STACK,
        init_boot_page_table = sym init_boot_page_table,
        init_mmu = sym init_mmu,
        entry = sym ax_plat::call_main,
        early_tests = sym early_tests,
    )
}

/// The earliest entry point for secondary CPUs.
#[cfg(feature = "smp")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    // a0 = hartid
    // a1 = SP
    core::arch::naked_asm!(
        "
        mv      s0, a0                  // save hartid
        mv      sp, a1                  // set SP

        call    {init_mmu}              // setup boot page table and enable MMU

        li      s1, {phys_virt_offset}  // fix up virtual high address
        add     a1, a1, s1
        add     sp, sp, s1

        mv      a0, s0
        addi    a0, a0, -1              // map hart(n+1) -> cpu(n)
        la      a1, {entry}
        add     a1, a1, s1
        jalr    a1                      // call_secondary_main(cpu_id)
        j       .",
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        init_mmu = sym init_mmu,
        entry = sym ax_plat::call_secondary_main,
    )
}
