//! The boot-progress bridge to the hosting page — the wasm side of `web/boot.js`.
//!
//! The pre-login boot cannot show its own progress: every stage that matters (the Startup
//! catalog loads, the glue art, the first screen spawn) happens before — or *is* — the first
//! thing this app could draw, so the page's DOM overlay owns that UI (`web/boot.js`, decision
//! recorded in the boot-overlay PR). What the page cannot know is when the client is genuinely
//! *ready*: `init()` resolving only means the module instantiated, and the first frames after it
//! are exactly the heavy ones. So the app calls back.
//!
//! The contract mirrors [`crate::webenv`]'s in reverse: where webenv *reads* a global the page
//! set (`window.__wenilla_env`), this *calls* one the page may define —
//! `window.__wenilla_progress(stage)`. A page without the hook (an old copy, upstream's plain
//! index) costs one failed `Reflect::get` per signal and nothing else; every error is swallowed,
//! because a progress overlay must never be able to take the client down. Stages are plain
//! strings the page switch-cases on:
//!
//! - `"startup"` — the end of the first update's Startup pile (the single long task the
//!   catalogs run in). Sent from a `PostStartup` system; the page repaints only after the task
//!   yields, which is precisely the moment this lands.
//! - `"ready"`  — the first glue screen (login *or* character select: the env fast path skips
//!   Login entirely) has spawned. The page fades its overlay here. Sent once, ever —
//!   [`signal_ready_once`] — because both screens' `materialize_screen` run every frame.
//! - `"charselect"` — the character screens go up for the first time, so the page can cover the
//!   scene's first load as it covers the first glue screen: on the world server's authentication,
//!   in the frame the screen is put up, else on entering them. Sent once, ever.
//! - `"charcreate"` — Character Create opened before its models reached the page's disk
//!   ([`crate::web_glue_ahead`]): the page covers it while they download.
//! - `"charcreate-ready"` — that download ended: the page lifts the cover, if it shows one.
//! - `"exit"` — the client is going down (Exit Game, `QuitGame`): the page closes its tab where
//!   the browser lets a script do that, else covers the stopped canvas with a way back in.
//!
//! Native builds: every function is a no-op; there is no page.

use bevy::prelude::{AppExit, Commands, MessageReader, On};

use crate::cvars::CvarChanged;

/// Call `window.__wenilla_progress(stage)` if the page defined it; silently do nothing
/// otherwise. See the module header for the stage vocabulary.
pub(crate) fn signal(stage: &str) {
    call_page("__wenilla_progress", stage);
}

/// Call `window.<hook>(arg)` if the page defined it; every failure is swallowed.
#[cfg(target_arch = "wasm32")]
fn call_page(hook: &str, arg: &str) {
    use wasm_bindgen::JsCast;
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(hook) = js_sys::Reflect::get(&window, &hook.into()) else {
        return;
    };
    let Ok(hook) = hook.dyn_into::<js_sys::Function>() else {
        return; // absent (undefined) or not a function — a page that doesn't care
    };
    let _ = hook.call1(&wasm_bindgen::JsValue::NULL, &arg.into());
}

#[cfg(not(target_arch = "wasm32"))]
fn call_page(_hook: &str, _arg: &str) {}

/// The `locale` setting changed by the player: `window.__wenilla_locale(folder)`. The page's data
/// root is the locale, chosen before the client starts (`web/boot.js`), so a change takes effect
/// at the next load, as the reference's takes effect at the next start (the stock panel's
/// `CLIENT_RESTART_ALERT`): the page saves the choice and offers the reload. Observed from
/// `PostStartup` on, so the boot's own write of the running locale (`cvars::load_config`) is
/// not a change the player made.
pub(crate) fn on_locale_change(ev: On<CvarChanged>) {
    if ev.is("locale") && ev.old != ev.new {
        call_page("__wenilla_locale", &ev.new);
    }
}

/// The `PostStartup` system that starts [`on_locale_change`] observing.
pub(crate) fn observe_locale(mut commands: Commands) {
    commands.add_observer(on_locale_change);
}

/// `signal("ready")`, at most once per process — the callers run per-frame and "ready" is an
/// edge, not a state. Relaxed is enough: wasm is single-threaded and native is a no-op anyway.
pub(crate) fn signal_ready_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SENT: AtomicBool = AtomicBool::new(false);
    if !SENT.swap(true, Ordering::Relaxed) {
        signal("ready");
    }
}

/// The `"charselect"` stage, from the authentication edge and the `OnEnter` system: one edge per
/// process, since only the first visit loads the scene.
pub(crate) fn signal_charselect_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SENT: AtomicBool = AtomicBool::new(false);
    if !SENT.swap(true, Ordering::Relaxed) {
        signal("charselect");
    }
}

/// The `PostStartup` system behind the `"startup"` stage — registered in `lib.rs` beside the
/// app's other one-line wiring.
pub(crate) fn signal_startup() {
    signal("startup");
}

/// The `"exit"` stage, on the exit frame alongside the savers ([`crate::shutdown::on_app_exit`]);
/// the page acts on it once this frame's task ends, so their writes land first.
pub(crate) fn signal_exit(mut exits: MessageReader<AppExit>) {
    if exits.read().next().is_some() {
        signal("exit");
    }
}
