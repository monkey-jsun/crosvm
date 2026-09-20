// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest ISA export for riscv64.
//!
//! Policy: the guest is advertised as **RVA23U64-compatible**, not as "whatever the host
//! has".  Each profile extension is emitted only if KVM confirms it is enabled on the vCPU,
//! so the advertised set is deterministic across RVA23 hosts, honest on hosts that lack
//! something, and never wider than the profile.
//!
//! Why not the raw host set: it is not portable across hosts, several extensions carry
//! device-tree obligations beyond their name (Zicbom/Zicboz/Zicbop need block sizes, Sscofpmf
//! wants a PMU node), and a malformed `riscv,isa` string makes the guest kernel panic — the
//! exact failure that forced the previous retreat to a base-only string.
//!
//! Profile members that are implementation *properties* rather than KVM-enableable
//! extensions (Ziccif, Ziccamoa, Zicclsm, Za64rs, Zic64b) are not emitted: KVM has no id for
//! them and they gate nothing.

/// One profile extension: its DT name and its `KVM_RISCV_ISA_EXT_*` id.
///
/// Ids are the position of each entry in the uapi `enum KVM_RISCV_ISA_EXT_ID`
/// (`arch/riscv/include/uapi/asm/kvm.h`), which is append-only and therefore stable.
/// Values below are from Linux 6.18.
#[derive(Copy, Clone, Debug)]
pub struct IsaExt {
    pub name: &'static str,
    pub kvm_id: u64,
}

const fn ext(name: &'static str, kvm_id: u64) -> IsaExt {
    IsaExt { name, kvm_id }
}

/// RVA23U64 mandatory extensions that KVM can enable per-vCPU.  Single-letter extensions
/// (I, M, A, F, D, C, V, B) come from `misa` and are not listed here.
pub const RVA23U64: &[IsaExt] = &[
    // Zi*
    ext("zicbom", 11),
    ext("zicbop", 71),
    ext("zicboz", 12),
    ext("ziccrse", 68),
    ext("zicntr", 19),
    ext("zicond", 24),
    ext("zicsr", 20),
    ext("zifencei", 21),
    ext("zihintntl", 48),
    ext("zihintpause", 10),
    ext("zihpm", 22),
    ext("zimop", 55),
    // Za*
    ext("zawrs", 61),
    // Zf*
    ext("zfa", 51),
    ext("zfhmin", 47),
    // Zc*
    ext("zcb", 57),
    ext("zcmop", 60),
    // Zb*  (B = Zba + Zbb + Zbs)
    ext("zba", 17),
    ext("zbb", 13),
    ext("zbs", 18),
    // Zk*
    ext("zkt", 35),
    // Zv*
    ext("zvbb", 36),
    ext("zvfhmin", 50),
    ext("zvkt", 45),
    // S*  — Supm (user-mode pointer masking) is satisfied by Ssnpm; the guest kernel
    // derives `supm` from it.
    ext("ssnpm", 63),
];

/// Extensions that need a `riscv,<name>-block-size` CPU-node property or the guest kernel
/// disables them with `pr_err("... disabling as no ...-block-size found")`.
pub const ZICBOM: &str = "zicbom";
pub const ZICBOZ: &str = "zicboz";
pub const ZICBOP: &str = "zicbop";

/// Canonical order of single-letter extensions per the RISC-V ISA manual: base, then
/// m, a, f, d, q, c, b, then the rest alphabetically.  Also used to order multi-letter
/// extensions by the letter following their `z` prefix.
const CANONICAL_ORDER: &[u8] = b"iemafdqcbghjklnoprstuvwxyz";

fn canonical_rank(c: u8) -> usize {
    CANONICAL_ORDER
        .iter()
        .position(|&x| x == c)
        .unwrap_or(CANONICAL_ORDER.len())
}

/// Single-letter extensions present in `misa`, in canonical order.
pub fn misa_letters(misa: u64) -> Vec<char> {
    CANONICAL_ORDER
        .iter()
        .filter(|&&c| misa & (1u64 << (c - b'a')) != 0)
        .map(|&c| c as char)
        .collect()
}

/// Sort multi-letter extensions into the manual's canonical order: `Z*` grouped by the
/// letter after `z` (in single-letter canonical order), alphabetical within a group; then
/// `S*` (supervisor), alphabetical; then anything else.
pub fn sort_canonical(names: &mut [&str]) {
    names.sort_by_key(|n| {
        let b = n.as_bytes();
        match b.first() {
            Some(b'z') => (
                0usize,
                canonical_rank(*b.get(1).unwrap_or(&b'z')),
                n.to_string(),
            ),
            Some(b's') => (1usize, 0, n.to_string()),
            _ => (2usize, 0, n.to_string()),
        }
    });
}

/// Build the legacy `riscv,isa` string: `rv64` + misa letters + `_ext` for each multi-letter
/// extension, which must already be in canonical order.
pub fn legacy_isa_string(misa: u64, multi_canonical: &[&str]) -> String {
    let mut s = String::from("rv64");
    for c in misa_letters(misa) {
        s.push(c);
    }
    for ext in multi_canonical {
        s.push('_');
        s.push_str(ext);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn misa_of(letters: &str) -> u64 {
        letters.bytes().fold(0, |m, c| m | 1u64 << (c - b'a'))
    }

    #[test]
    fn letters_are_canonical() {
        // K3 misa order as KVM reports it is irrelevant; output must be i m a f d c b v.
        let m = misa_of("vbcdfami");
        assert_eq!(misa_letters(m).iter().collect::<String>(), "imafdcbv");
    }

    #[test]
    fn multi_letter_canonical_order() {
        let mut v = vec![
            "zvbb", "ssnpm", "zba", "zicbom", "zfa", "zawrs", "zcb", "zkt", "zicsr",
        ];
        sort_canonical(&mut v);
        // Zi*, Za*, Zf*, Zc*, Zb*, Zk*, Zv*, then S*
        assert_eq!(
            v,
            vec!["zicbom", "zicsr", "zawrs", "zfa", "zcb", "zba", "zkt", "zvbb", "ssnpm"]
        );
    }

    #[test]
    fn legacy_string_shape() {
        let m = misa_of("imafdcv");
        let mut multi = vec!["ssaia", "zicsr", "smaia"];
        sort_canonical(&mut multi);
        assert_eq!(
            legacy_isa_string(m, &multi),
            "rv64imafdcv_zicsr_smaia_ssaia"
        );
    }

    #[test]
    fn base_only_matches_previous_behaviour() {
        // With nothing verified, the string is exactly what crosvm emitted before this
        // change (modulo the AIA suffix the caller appends).
        assert_eq!(legacy_isa_string(misa_of("imafdc"), &[]), "rv64imafdc");
    }

    #[test]
    fn table_ids_are_unique_and_names_lowercase() {
        let mut ids: Vec<u64> = RVA23U64.iter().map(|e| e.kvm_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            RVA23U64.len(),
            "duplicate KVM id in RVA23U64 table"
        );
        for e in RVA23U64 {
            assert_eq!(e.name, e.name.to_ascii_lowercase());
            assert!(e.name.starts_with('z') || e.name.starts_with('s'));
        }
    }
}
