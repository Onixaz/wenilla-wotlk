//! The browser entry point — benilla's third launcher shim, after `benilla` and
//! `benilla-worldview` (decision 1160), and like them ~20 lines whose only job is to carry the
//! build-id stamp so a commit dirties this crate and not `benilla-app` (decision 0993).
//!
//! `web/index.html` loads the `wasm-bindgen --target web` glue this crate compiles to
//! (`scripts/web-build.sh`), which calls `init()` then this module's exported [`start`] —
//! `#[wasm_bindgen(start)]` also runs it automatically the moment the module finishes
//! instantiating, so the two are equivalent; the page calls it explicitly only to control
//! sequencing with `wasi_stubs.js`'s `bind(memory)` (see that file's own header for why the wasm
//! Lua VM needs it).

use wasm_bindgen::prelude::*;

use benilla_app::BuildId;

/// Grow the heap once, up front. The allocator otherwise grows linear memory a little at a time,
/// and a profile of the startup frame put 4 s of it inside the allocator, most under the spell
/// catalog's per-row vectors. Reserving halved that frame (3.8 s to 2.0 s) and took a world load
/// from 17.0 s to 14.6 s, and a login peaks past this size anyway, so it costs no memory. Freed,
/// the block stays with the allocator; a browser that refuses it leaves the heap to grow as usual.
#[cfg(target_arch = "wasm32")]
fn reserve_heap() {
    const BYTES: usize = 1 << 30;
    let mut block: Vec<u8> = Vec::new();
    let _ = block.try_reserve_exact(BYTES);
    std::hint::black_box(&mut block);
}

#[cfg(not(target_arch = "wasm32"))]
fn reserve_heap() {}

/// Set the panic hook (so a Rust panic reaches `console.error` with a real message instead of
/// the browser's bare "unreachable executed"), then hand off to [`benilla_app::run`] exactly like
/// the native `benilla` shim does — same `BuildId`, same entry point, so behaviour that differs
/// between the two targets differs inside `benilla_app`, never at this seam.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    reserve_heap();
    // `AppExit` is discarded deliberately: on wasm32, Bevy's winit runner hands the event loop to
    // the browser (`requestAnimationFrame`) and returns here immediately rather than blocking
    // until the app actually closes, so the value this call produces answers a question — "what
    // did the run exit with?" — that hasn't been decided yet. The native shims' `fn main() ->
    // AppExit` (where the return value IS the process exit code) has no wasm32 equivalent: a
    // browser tab has no exit code for a page to report.
    benilla_app::run(BuildId {
        version: env!("CARGO_PKG_VERSION"),
        describe: env!("BENILLA_GIT_DESCRIBE"),
        sha: env!("BENILLA_GIT_SHA"),
        short: env!("BENILLA_GIT_SHORT"),
        date: env!("BENILLA_GIT_DATE"),
        profile: env!("BENILLA_PROFILE"),
        project_dir: env!("BENILLA_PROJECT_DIR"),
        ..Default::default()
    });
}
