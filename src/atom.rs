//! LV2 atom sequence layout, on plain byte buffers.
//!
//! These buffers are shared with plugin code: Studio writes input sequences
//! that plugins read, and parses output sequences that plugins wrote, sizes
//! included. Everything here must therefore stay in bounds for *any* buffer
//! contents; the Kani harnesses in `verification.rs` check exactly that.
//!
//! Layout: `LV2_Atom_Sequence` = atom header { size: u32, type: u32 } +
//! body { unit: u32, pad: u32 }, then events, each { frames: i64,
//! atom header { size, type }, body } padded to 8 bytes. `size` in the
//! sequence header counts everything after the 8-byte header.

/// Size of an LV2_Atom header and of the sequence body header.
pub const HEADER: usize = 8;

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn write_u32(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

/// View a word buffer (8-aligned, as LV2 requires) as bytes.
pub fn as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: u8 has alignment 1 and no invalid values; the byte length is
    // exactly the words' size in bytes.
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, std::mem::size_of_val(words)) }
}

/// Mutable byte view of a word buffer.
pub fn as_bytes_mut(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: as for `as_bytes`; the borrow is unique.
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, std::mem::size_of_val(words)) }
}

/// Make `bytes` an empty input sequence of type `seq`.
pub fn seq_reset(bytes: &mut [u8], seq: u32) {
    write_u32(bytes, 0, HEADER as u32); // size = sizeof(LV2_Atom_Sequence_Body)
    write_u32(bytes, 4, seq);
    bytes[8..16].fill(0); // unit, pad
}

/// Prepare an output buffer: a Chunk atom advertising the capacity.
pub fn chunk_reset(bytes: &mut [u8], chunk: u32) {
    write_u32(bytes, 0, (bytes.len() - HEADER) as u32);
    write_u32(bytes, 4, chunk);
}

/// Append an event to the sequence in `bytes`. Returns false (and changes
/// nothing) when it does not fit.
pub fn seq_append(bytes: &mut [u8], frame: i64, type_: u32, body: &[u8]) -> bool {
    // The sequence atom's size counts its body; events start after it.
    let size = read_u32(bytes, 0) as usize;
    let at = 8 + size;
    let total = 16 + body.len().div_ceil(8) * 8;
    if at + total > bytes.len() {
        return false;
    }
    bytes[at..at + 8].copy_from_slice(&frame.to_ne_bytes());
    write_u32(bytes, at + 8, body.len() as u32);
    write_u32(bytes, at + 12, type_);
    bytes[at + 16..at + 16 + body.len()].copy_from_slice(body);
    write_u32(bytes, 0, (size + total) as u32);
    true
}

/// Call `f` with every event atom (header + body) of the sequence a plugin
/// wrote into `bytes`, if it is a sequence of type `seq`. Truncated events
/// are cut at the end of the sequence.
pub fn for_each_event(bytes: &[u8], seq: u32, mut f: impl FnMut(&[u8])) {
    let size = read_u32(bytes, 0) as usize;
    if read_u32(bytes, 4) != seq {
        return; // the plugin wrote nothing this cycle
    }
    let end = (8 + size).min(bytes.len());
    let mut at = 16; // atom header + sequence body
    while at + 16 <= end {
        let asize = read_u32(bytes, at + 8) as usize;
        f(&bytes[at + 8..(at + 16 + asize).min(end)]);
        at += 16 + asize.div_ceil(8) * 8;
    }
}
