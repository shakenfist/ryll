//! Proves GLZ cross-image references share the dictionary's source
//! buffer instead of cloning it per fragment.
//!
//! Sharing the source and cloning it produce identical pixels, so no
//! output-comparing test can tell them apart: the only observable
//! difference is the allocation. This file installs a counting global
//! allocator, and it is an integration test so that allocator is
//! confined to this one test binary rather than replacing the
//! allocator under every unit test in the crate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use shakenfist_spice_compression::{decompress_glz, GlzDictionary};

thread_local! {
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Counts the bytes this thread allocates. A growing `realloc` is
/// charged its growth and a shrinking one nothing, so the count is the
/// most memory the thread asked for, not the number of bytes copied.
struct Counting;

// SAFETY: every method forwards to `System` with the layout it was
// given; the counter is a thread-local `Cell<usize>` with a const
// initialiser, so no allocation happens inside the allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn add(bytes: usize) {
    // `try_with` rather than `with`: during thread teardown the
    // thread-local is gone and an allocation there must not panic.
    let _ = BYTES.try_with(|counter| counter.set(counter.get().saturating_add(bytes)));
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f`, returning its value and the bytes allocated on this thread
/// while it ran. Counting is per thread, so tests running in parallel
/// do not interfere.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = BYTES.with(Cell::get);
    let value = f();
    let after = BYTES.with(Cell::get);
    (value, after.saturating_sub(before))
}

/// One top-down RGB32 GLZ row of `width` pixels, image id 1, with
/// `payload` as its compressed body.
fn glz_rgb32_row(width: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"  ZL");
    data.extend_from_slice(&[0, 1, 0, 1, 0x18]); // GLZ 1.1, top-down RGB32
    for value in [width, 1, width * 4] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    data.extend_from_slice(&1u64.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(payload);
    data
}

/// 128 two-pixel fragments against one 64 KiB source: enough that
/// cloning the source per fragment is megabytes of allocation, while
/// sharing the dictionary's own buffer costs the 1 KiB output and
/// little else.
fn many_fragments_one_large_source() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    const FRAGMENTS: usize = 128;
    const SOURCE_PIXELS: usize = 16 * 1024;
    let source: Vec<u8> = (0..SOURCE_PIXELS)
        .flat_map(|pixel| [pixel as u8, (pixel >> 8) as u8, 7, 255])
        .collect();
    let mut payload = Vec::new();
    let mut expected = Vec::new();
    for fragment in 0..FRAGMENTS {
        let pixel_offset = fragment * 2;
        // ctrl 0x40: length two pixels, source in a previous image,
        // low nibble of the pixel offset; then the rest of the offset,
        // then image_dist 1.
        payload.push(0x40 | (pixel_offset & 0x0F) as u8);
        payload.push((pixel_offset >> 4) as u8);
        payload.push(0x01);
        expected.extend_from_slice(&source[pixel_offset * 4..pixel_offset * 4 + 8]);
    }
    (
        glz_rgb32_row((FRAGMENTS * 2) as u32, &payload),
        source,
        expected,
    )
}

#[test]
fn fragmented_cross_references_share_the_source_allocation() {
    let (data, source, expected) = many_fragments_one_large_source();
    let source_bytes = source.len();
    let dict = GlzDictionary::with_cap(1024 * 1024);
    dict.insert(0, source);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime");

    let (image, allocated) = measure(|| rt.block_on(decompress_glz(&data, &dict)).expect("decode"));

    assert_eq!(image.pixels, expected);
    // Cloning the source once per fragment would allocate 128 x 64 KiB
    // = 8 MiB. Sharing it allocates the output buffer and the decode's
    // own small structures. The threshold sits far from both, so this
    // fails on a returning clone rather than on an allocator detail.
    assert!(
        allocated < source_bytes,
        "decode allocated {allocated} bytes against a {source_bytes}-byte source: \
         a per-fragment clone of the source has come back"
    );
}
