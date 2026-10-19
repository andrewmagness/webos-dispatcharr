//! **The running binary's identity, written to the crash log at startup** — `src/main.c` calls
//! [`plx_crash_write_image_marker`] before the signal tracer (`src/crashtrace.c`) is armed, so
//! every later crash record in `dispatcharrtv-crash.log` is paired with the build id, load base
//! and size of the executable that actually crashed, even if another binary has been deployed
//! since. That is what lets a developer symbolicate a crash offline against the matching
//! `dispatcharrtv.debug` (`crash-triage` skill). Nothing is sent anywhere.

/// **`image_addr` for this binary is the lowest `PT_LOAD` vaddr — `0x10000` — and NOT zero.**
///
/// Device-adjacent measurement, 2026-08-29, against the real Sentry EU project with a real armv7
/// DIF uploaded. This is the number that decides whether a crash report is a source line or a bare
/// address, and getting it wrong fails SILENTLY in the worst way: the image still resolves, the
/// event carries no processing error, and the frame simply comes back
/// `symbolicatorStatus: "missing_symbol"` — indistinguishable from "we never uploaded symbols".
///
/// Both were tried, same address, same DIF, minutes apart:
///
/// | `image_addr` | result |
/// |---|---|
/// | `0x0` | `missing_symbol`, no error reported |
/// | `0x10000` | `symbolicated` -> `plx_crash_install`, `crashtrace.c:290` |
///
/// The reason is that Symbolicator computes `rva = instruction_addr - image_addr` and an ELF's
/// symbol addresses are relative to its own load base, which for this non-PIE executable is the
/// first `PT_LOAD`'s vaddr rather than 0. The plan this was built to said "since the binary is
/// ET_EXEC, frames are absolute `instruction_addr` — no `addr_mode`", which is correct about the
/// FRAME and silent about the image, and the silence is the whole trap: absolute frames plus a zero
/// image base looks obviously right and yields nothing.
///
/// **This is the FALLBACK, not the source of truth** — [`image_addr`] derives the number from the
/// running binary's own program headers. It stays written down because it is the measured value
/// above, so a build whose `/proc/self/exe` is unreadable still reports the number that was proven
/// to symbolicate rather than a zero that silently would not.
///
/// This doc used to say deriving it "is not possible here — the running process cannot read its own
/// program headers without parsing `/proc/self/exe`", which names the method and then treats it as
/// a reason not to. `paths::app_dir` has read that link since long before this module existed.
pub const IMAGE_ADDR: &str = "0x10000";

/// **The image base this binary actually links at**, as the `0x…` string a Sentry debug image wants.
///
/// Derived rather than declared: `Symbolicator` computes `rva = instruction_addr - image_addr`, so
/// this number decides whether a crash is a source line or nothing at all, and it is a property of
/// the LINK — a linker script change, a switch to PIE, a different toolchain default would each
/// move it with no other symptom. Resolved once per process; the read is ~64 KiB of the head of
/// `/proc/self/exe`, not the whole 7 MB binary.
///
/// Falls back to [`IMAGE_ADDR`] whenever the answer cannot be trusted — no `/proc` (the macOS
/// simulator), a Mach-O rather than an ELF, a big-endian object, or `ET_DYN`, where the vaddr in
/// the program header is an offset from a load bias this function cannot see. A wrong non-zero
/// base and a zero base fail identically and invisibly, so "I could not tell" must resolve to the
/// value that was measured to work, never to a guess.
pub fn image_addr() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        read_head("/proc/self/exe")
            .as_deref()
            .map(image_addr_of)
            .unwrap_or_else(|| IMAGE_ADDR.to_string())
    })
}

/// The pure half of [`image_addr`], so a test grades the expression the app actually uses rather
/// than a re-derivation of it.
fn image_addr_of(buf: &[u8]) -> String {
    lowest_load_vaddr(buf)
        .map(|v| format!("0x{v:x}"))
        .unwrap_or_else(|| IMAGE_ADDR.to_string())
}

/// The head of a file — enough to hold an ELF header and every program header after it. Bounded
/// rather than a whole read: this runs at boot on a set with 1.68 GB, and the binary is 7 MB.
fn read_head(path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// **The GNU build id of the running binary, as lowercase hex** — the only thing that can pair a
/// crash report with the `dispatcharrtv.debug` a release cut and uploaded.
///
/// `-Wl,--build-id=sha1` is unconditional on every link (`docs/agent-reference.md`: it costs 20 bytes and `strip`
/// preserves it), and a debuginfo build and a plain one produce DIFFERENT ids from identical
/// sources — which is why `SYMBOLS` is in the `RUST_CFG` stamp, and why this has to be read from
/// the binary that is actually running rather than baked in at compile time by anything.
///
/// Empty when it cannot be read, and a caller must then send NO debug image: an image carrying a
/// wrong or absent id is what produces `missing_symbol` with no error, the failure mode
/// [`IMAGE_ADDR`]'s doc records from the other direction.
pub fn build_id() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        read_head("/proc/self/exe")
            .as_deref()
            .and_then(gnu_build_id)
            .unwrap_or_default()
    })
}

/// The mapped span of this executable's loadable ELF segments.
///
/// Sentry's debug-image schema uses a size as well as an address. Like [`image_addr`], this is a
/// link property and is read from the binary that is actually running, not from the next build
/// that happens to process its crash log.
pub fn image_size() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        read_head("/proc/self/exe")
            .as_deref()
            .and_then(load_span)
            .unwrap_or(0)
    })
}

/// Append the running image's identity to the crash log before any signal can arrive.
///
/// The signal handler itself cannot parse ELF or allocate. This normal-startup marker associates
/// every later fallback record with the binary that actually crashed even if another binary is
/// deployed before the log is read.
#[no_mangle]
pub extern "C" fn plx_crash_write_image_marker(fd: std::os::raw::c_int) {
    if fd < 0 {
        return;
    }
    let line = if build_id().is_empty() {
        // Still delimit this process. Otherwise a rare ELF-read failure would let a later crash
        // inherit the preceding process's perfectly valid, but now wrong, build identity.
        "img: unavailable\n".to_string()
    } else {
        format!(
            "img: build_id={} image_addr={} image_size=0x{:x}\n",
            build_id(),
            image_addr(),
            image_size()
        )
    };
    unsafe {
        let _ = libc::write(fd, line.as_ptr().cast(), line.len());
    }
}

/// Walk an ELF's `PT_NOTE` segments for `NT_GNU_BUILD_ID`. Pure, for the reason
/// [`lowest_load_vaddr`] is.
///
/// Note records are `[namesz][descsz][type]` then the name and the descriptor, each padded to four
/// bytes — and the PADDING is the part that is easy to drop, because `"GNU\0"` is exactly four
/// bytes and so a parser that forgets to round up reads correctly on every ELF anyone would test
/// with and wrongly on the one that has a different note first.
pub fn gnu_build_id(buf: &[u8]) -> Option<String> {
    const PT_NOTE: u32 = 4;
    const NT_GNU_BUILD_ID: u32 = 3;
    if buf.len() < 64 || &buf[..4] != b"\x7fELF" || buf[5] != 1 {
        return None;
    }
    let is64 = buf[4] == 2;
    let u16at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    let u32at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let u64at =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?)) };
    let (phoff, phentsize, phnum) = if is64 {
        (
            u64at(32)? as usize,
            u16at(54)? as usize,
            u16at(56)? as usize,
        )
    } else {
        (
            u32at(28)? as usize,
            u16at(42)? as usize,
            u16at(44)? as usize,
        )
    };
    for i in 0..phnum {
        let base = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32at(base)? != PT_NOTE {
            continue;
        }
        let (off, size) = if is64 {
            (u64at(base + 8)? as usize, u64at(base + 32)? as usize)
        } else {
            (u32at(base + 4)? as usize, u32at(base + 16)? as usize)
        };
        let mut at = off;
        let end = off.checked_add(size)?;
        while at + 12 <= end && at + 12 <= buf.len() {
            let namesz = u32at(at)? as usize;
            let descsz = u32at(at + 4)? as usize;
            let kind = u32at(at + 8)?;
            let name_at = at + 12;
            let desc_at = name_at.checked_add(namesz.next_multiple_of(4))?;
            let desc_end = desc_at.checked_add(descsz)?;
            if kind == NT_GNU_BUILD_ID && buf.get(name_at..name_at + namesz) == Some(b"GNU\0") {
                let d = buf.get(desc_at..desc_end)?;
                return Some(d.iter().map(|b| format!("{b:02x}")).collect());
            }
            let next = desc_at.checked_add(descsz.next_multiple_of(4))?;
            if next <= at {
                break; // a zero-length note would otherwise spin here forever
            }
            at = next;
        }
    }
    None
}

/// The lowest `PT_LOAD` virtual address in an ELF image. Pure — the caller owns the read, which is
/// what makes every branch here host-testable against a hand-built header.
///
/// `None` for anything this cannot answer honestly: not an ELF, big-endian, `ET_DYN` (see
/// [`image_addr`]), a truncated header, or no `PT_LOAD` at all.
pub fn lowest_load_vaddr(buf: &[u8]) -> Option<u64> {
    const PT_LOAD: u32 = 1;
    const ET_EXEC: u16 = 2;
    if buf.len() < 64 || &buf[..4] != b"\x7fELF" {
        return None;
    }
    let is64 = match buf[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    if buf[5] != 1 {
        return None; // big-endian: every field below is read little-endian
    }
    let u16at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    let u32at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let u64at =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?)) };

    if u16at(16)? != ET_EXEC {
        return None;
    }
    let (phoff, phentsize, phnum) = if is64 {
        (
            u64at(32)? as usize,
            u16at(54)? as usize,
            u16at(56)? as usize,
        )
    } else {
        (
            u32at(28)? as usize,
            u16at(42)? as usize,
            u16at(44)? as usize,
        )
    };
    // `p_vaddr` is the field after `p_offset` in both classes, at different widths and — because
    // ELF64 moves `p_flags` up to second — different offsets. Getting this pair wrong reads a file
    // offset as a load address, which on this binary happens to be a plausible small number.
    let vaddr_at = if is64 { 16 } else { 8 };
    let min_entry = if is64 { 24 } else { 12 };
    if phentsize < min_entry || phnum == 0 {
        return None;
    }
    (0..phnum)
        .filter_map(|i| {
            let base = phoff.checked_add(i.checked_mul(phentsize)?)?;
            if u32at(base)? != PT_LOAD {
                return None;
            }
            if is64 {
                u64at(base + vaddr_at)
            } else {
                u32at(base + vaddr_at).map(u64::from)
            }
        })
        .min()
}

/// Size of the address range covered by all `PT_LOAD` segments in one `ET_EXEC` ELF.
fn load_span(buf: &[u8]) -> Option<u64> {
    const PT_LOAD: u32 = 1;
    const ET_EXEC: u16 = 2;
    if buf.len() < 64 || &buf[..4] != b"\x7fELF" || buf[5] != 1 {
        return None;
    }
    let is64 = match buf[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let u16at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    let u32at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let u64at =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?)) };
    if u16at(16)? != ET_EXEC {
        return None;
    }
    let (phoff, phentsize, phnum) = if is64 {
        (
            u64at(32)? as usize,
            u16at(54)? as usize,
            u16at(56)? as usize,
        )
    } else {
        (
            u32at(28)? as usize,
            u16at(42)? as usize,
            u16at(44)? as usize,
        )
    };
    let minimum = if is64 { 48 } else { 24 };
    if phentsize < minimum || phnum == 0 {
        return None;
    }
    let mut low = u64::MAX;
    let mut high = 0u64;
    for i in 0..phnum {
        let base = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32at(base)? != PT_LOAD {
            continue;
        }
        let (vaddr, memsz) = if is64 {
            (u64at(base + 16)?, u64at(base + 40)?)
        } else {
            (u64::from(u32at(base + 8)?), u64::from(u32at(base + 20)?))
        };
        low = low.min(vaddr);
        high = high.max(vaddr.checked_add(memsz)?);
    }
    (low != u64::MAX && high > low).then_some(high - low)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal little-endian ELF32 `ET_EXEC` image: header, `n` program headers, and whatever
    /// note bytes a caller wants appended. Hand-built rather than a checked-in fixture so every
    /// offset this parser reads is stated once, here, where a wrong one is visible.
    fn elf32(phdrs: &[[u32; 8]], notes: &[u8], note_off: u32) -> Vec<u8> {
        const EHDR: usize = 52;
        const PHENT: usize = 32;
        let mut v = vec![0u8; EHDR];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 1; // ELFCLASS32
        v[5] = 1; // ELFDATA2LSB
        v[6] = 1;
        v[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        v[18..20].copy_from_slice(&40u16.to_le_bytes()); // EM_ARM
        v[28..32].copy_from_slice(&(EHDR as u32).to_le_bytes()); // e_phoff
        v[40..42].copy_from_slice(&(EHDR as u16).to_le_bytes()); // e_ehsize
        v[42..44].copy_from_slice(&(PHENT as u16).to_le_bytes()); // e_phentsize
        v[44..46].copy_from_slice(&(phdrs.len() as u16).to_le_bytes()); // e_phnum
        for ph in phdrs {
            for w in ph {
                v.extend_from_slice(&w.to_le_bytes());
            }
        }
        v.resize(note_off.max(v.len() as u32) as usize, 0);
        v.extend_from_slice(notes);
        v.resize(v.len().max(64), 0); // the parser refuses anything shorter than an ELF64 header
        v
    }

    /// One ELF note: `[namesz][descsz][type]`, then the name and the descriptor, **each padded to
    /// four bytes**. The padding is the part a hand-rolled walker forgets, and forgetting it reads
    /// correctly on every object anyone would test with — `"GNU\0"` is exactly four bytes — and
    /// wrongly on the one whose first note has a name that is not.
    fn note(name: &[u8], kind: u32, desc: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(name);
        v.resize(v.len().next_multiple_of(4), 0);
        v.extend_from_slice(desc);
        v.resize(v.len().next_multiple_of(4), 0);
        v
    }

    /// **The measured `image_addr`.** Pinned as a number because the failure it prevents is silent:
    /// with `0x0` the same address, the same DIF and the same project return `missing_symbol` with
    /// no error attached, which reads exactly like never having uploaded symbols at all. Verified
    /// end to end against the live EU project on 2026-08-29 — `0x10000` resolved
    /// `0x88ef8` to `plx_crash_install` at `crashtrace.c:290`, matching `addr2line` locally.
    #[test]
    fn the_image_base_is_the_load_vaddr_not_zero() {
        assert_eq!(IMAGE_ADDR, "0x10000");
        assert_ne!(
            IMAGE_ADDR, "0x0",
            "a zero image base silently yields missing_symbol"
        );
    }

    /// **The image base is the LOWEST `PT_LOAD`, not the first one written down.** A link that
    /// emits its segments out of order, or adds one below the text, would otherwise move the base
    /// and every frame with it — and the failure is `missing_symbol` with no error, per the table
    /// above.
    #[test]
    fn the_image_base_is_the_lowest_load_segment() {
        //          p_type      offset  vaddr     paddr  filesz  memsz  flags  align
        let data = [1u32, 0x8000, 0x30000, 0, 0x100, 0x100, 6, 0x1000];
        let text = [1u32, 0x0000, 0x10000, 0, 0x8000, 0x8000, 5, 0x1000];
        let phdr = [6u32, 0x34, 0x10034, 0, 0x40, 0x40, 4, 4];
        // Deliberately NOT in address order, and with a non-LOAD segment first.
        let b = elf32(&[phdr, data, text], &[], 0);
        assert_eq!(lowest_load_vaddr(&b), Some(0x10000));
        assert_eq!(
            load_span(&b),
            Some(0x20100),
            "size reaches the end of the highest LOAD"
        );
        assert_eq!(
            image_addr_of(&b),
            "0x10000",
            "and it agrees with the measured constant"
        );
    }

    /// **The build id is found past a note that precedes it**, which is the padding case above. A
    /// walker that advances by `namesz` instead of `align4(namesz)` finds the GNU note only when it
    /// happens to be first.
    #[test]
    fn the_build_id_note_is_found_behind_another_note() {
        let id: Vec<u8> = (0u8..20).collect();
        let mut notes = note(b"Linux\0", 0x100, &[1, 2, 3]); // namesz 6 -> two bytes of padding
        notes.extend_from_slice(&note(b"GNU\0", 3, &id));
        let off = 52 + 32 * 2; // after two program headers
        let seg = [4u32, off, 0, 0, notes.len() as u32, 0, 4, 4]; // PT_NOTE
        let text = [1u32, 0, 0x10000, 0, 0x8000, 0x8000, 5, 0x1000];
        let b = elf32(&[seg, text], &notes, off);
        assert_eq!(
            gnu_build_id(&b).as_deref(),
            Some("000102030405060708090a0b0c0d0e0f10111213")
        );
    }
}
