//! Length-prefixed messages over lock-free byte queues (`rtrb`).
//!
//! Used wherever bytes cross threads at the plugin boundary: GUI → audio
//! thread (atom messages), audio thread → GUI (plugin notifications), and
//! LV2 worker requests/responses. A message is a fixed-size header whose
//! last four bytes are the body length (u32, native endian), then the body.
//! Checked by the Kani harnesses in `verification.rs`.

/// Push `head` + `body` as one message. Returns false (writing nothing)
/// when the queue lacks space.
pub fn push(tx: &mut rtrb::Producer<u8>, head: &[u8], body: &[u8]) -> bool {
    let need = head.len() + body.len();
    if tx.slots() < need {
        return false;
    }
    let Ok(mut chunk) = tx.write_chunk_uninit(need) else { return false };
    let msg = head.iter().chain(body).copied();
    let (a, b) = chunk.as_mut_slices();
    for (dst, v) in a.iter_mut().chain(b.iter_mut()).zip(msg) {
        dst.write(v);
    }
    // SAFETY: every slot of the chunk was written above.
    unsafe { chunk.commit_all() };
    true
}

/// Pop one message with an `H`-byte header (body length in its last four
/// bytes). `f` gets the header and the body as up to two slices (the queue
/// may wrap). Returns `None` if no complete message is queued.
pub fn pop_with<const H: usize, R>(rx: &mut rtrb::Consumer<u8>, f: impl FnOnce([u8; H], &[u8], &[u8]) -> R) -> Option<R> {
    let avail = rx.slots();
    if avail < H {
        return None;
    }
    // Peek at the header (an uncommitted read consumes nothing).
    let mut head = [0u8; H];
    {
        let peek = rx.read_chunk(H).ok()?;
        let (a, b) = peek.as_slices();
        for (d, s) in head.iter_mut().zip(a.iter().chain(b)) {
            *d = *s;
        }
    }
    let len = u32::from_ne_bytes([head[H - 4], head[H - 3], head[H - 2], head[H - 1]]) as usize;
    if avail < H + len {
        return None; // message not fully written yet
    }
    let chunk = rx.read_chunk(H + len).ok()?;
    let (a, b) = chunk.as_slices();
    // Skip the header, which may itself span the wrap point.
    let (a, b) = if a.len() >= H { (&a[H..], b) } else { (&b[H - a.len()..], &[][..]) };
    let r = f(head, a, b);
    chunk.commit_all();
    Some(r)
}

/// Copy a body given as two slices into `out`; returns the bytes copied.
pub fn copy_body(a: &[u8], b: &[u8], out: &mut [u8]) -> usize {
    let n = (a.len() + b.len()).min(out.len());
    for (d, s) in out[..n].iter_mut().zip(a.iter().chain(b)) {
        *d = *s;
    }
    n
}
