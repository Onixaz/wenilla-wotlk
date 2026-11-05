//! wasm32: the mixer's kira backend in the browser — Web Audio, scheduled here.
//!
//! kira's renderer runs on the page's one thread, so the mix reaches the speakers as chunks
//! rendered ahead and queued on the `AudioContext` clock (`AudioBufferSourceNode`s started back to
//! back). Whatever stalls that thread longer than the queue is silence. Two things keep that rare
//! and short, which cpal's Web Audio stream (two fixed 2048-frame buffers, a cursor that never
//! looks at the clock) did not:
//!
//! - **Resync.** A refill that finds the cursor behind the clock restarts it just ahead of the
//!   clock: one gap, where a cursor left in the past starts every later chunk the moment it is
//!   queued, on top of the one still playing, for the rest of the session.
//! - **An adaptive queue** ([`super::web_queue`]): short while playback is smooth, so a sound
//!   effect waits no longer than it did; deeper after each underrun, so a stretch of hitches (a
//!   world load, a city) stops cutting the music; shrinking back once it is smooth again.
//!
//! The queue is refilled by a [`PUMP_MS`] interval and by every chunk's `ended` event.

use std::cell::RefCell;
use std::rc::Rc;

use kira::backend::{Backend, Renderer};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::AudioContext;

use super::web_queue::{Lead, Refill, MIN_LEAD_S};

/// The context to play on, made where the mixer probes the sample rate (`mixer::backend_settings`)
/// so the rate the mix is built for is the one it plays at; `None` makes one here.
pub(super) type WebBackendSettings = Option<AudioContext>;

/// Frames per queued chunk: ~21 ms at 48 kHz, so the queue's depth moves in small steps.
const CHUNK_FRAMES: usize = 1024;
/// Output channels: the mix is stereo.
const CHANNELS: usize = 2;
/// Where a (re)started cursor lands past the clock, seconds: time to queue the first chunk.
const RESTART_S: f64 = 0.02;
/// The refill interval, milliseconds.
const PUMP_MS: i32 = 5;

thread_local! {
    /// The running queue's context and cursor, for [`queued_secs`].
    static QUEUE: RefCell<Option<(AudioContext, f64)>> = const { RefCell::new(None) };
}

/// How much rendered mix is queued ahead of what is heard now, seconds: the rendered position
/// of a sound runs this far ahead of its audible one. 0 with no running queue.
pub(super) fn queued_secs() -> f64 {
    QUEUE.with(|q| {
        q.borrow()
            .as_ref()
            .map_or(0.0, |(ctx, cursor)| (cursor - ctx.current_time()).max(0.0))
    })
}

/// The renderer and the queue it feeds.
struct Scheduler {
    ctx: AudioContext,
    renderer: Renderer,
    rate: f32,
    /// Where the next chunk starts, on the context clock; 0 before the first.
    cursor: f64,
    lead: Lead,
    /// The interleaved render buffer and one de-interleaved channel.
    mix: Vec<f32>,
    channel: Vec<f32>,
    underruns: u64,
}

impl Scheduler {
    fn pump(&mut self, refill: &js_sys::Function) {
        let now = self.ctx.current_time();
        let was = self.lead.secs;
        match self.lead.refill(now, self.cursor) {
            Refill::OnTime => {
                if self.lead.secs < was && self.lead.secs <= MIN_LEAD_S {
                    bevy::log::info!("audio: queue back to {:.0} ms", self.lead.secs * 1000.0);
                }
            }
            Refill::Start => self.cursor = now + RESTART_S,
            Refill::Underrun => {
                self.underruns += 1;
                bevy::log::warn!(
                    "audio: output ran dry {:.0} ms ago (underrun {}) — restarted, queue now {:.0} ms",
                    (now - self.cursor) * 1000.0,
                    self.underruns,
                    self.lead.target() * 1000.0,
                );
                self.cursor = now + RESTART_S;
            }
        }
        while self.cursor < now + self.lead.target() {
            if let Err(e) = self.queue_chunk(refill) {
                bevy::log::warn!("audio: queueing a chunk failed: {e:?}");
                break;
            }
        }
        let cursor = self.cursor;
        QUEUE.with(|q| match &mut *q.borrow_mut() {
            Some((_, c)) => *c = cursor,
            slot => *slot = Some((self.ctx.clone(), cursor)),
        });
    }

    fn queue_chunk(&mut self, refill: &js_sys::Function) -> Result<(), wasm_bindgen::JsValue> {
        self.renderer.on_start_processing();
        self.renderer.process(&mut self.mix, CHANNELS as u16);
        let buffer = self
            .ctx
            .create_buffer(CHANNELS as u32, CHUNK_FRAMES as u32, self.rate)?;
        for ch in 0..CHANNELS {
            for (out, frame) in self
                .channel
                .iter_mut()
                .zip(self.mix.as_chunks::<CHANNELS>().0)
            {
                *out = frame[ch];
            }
            buffer.copy_to_channel(&self.channel, ch as i32)?;
        }
        let source = self.ctx.create_buffer_source()?;
        source.set_buffer(Some(&buffer));
        source.connect_with_audio_node(&self.ctx.destination())?;
        source.add_event_listener_with_callback("ended", refill)?;
        source.start_with_when(self.cursor)?;
        self.cursor += CHUNK_FRAMES as f64 / f64::from(self.rate);
        Ok(())
    }
}

enum State {
    Setup(AudioContext),
    Running {
        _refill: Closure<dyn FnMut()>,
        interval: i32,
        scheduler: Rc<RefCell<Scheduler>>,
    },
    Empty,
}

/// kira's [`Backend`] over the page's `AudioContext`.
pub(super) struct WebBackend {
    state: State,
}

impl WebBackend {
    /// Hold the deep queue while a loading cover is up ([`super::web_queue::HOLD_LEAD_S`]).
    pub(super) fn set_hold(&mut self, hold: bool) {
        if let State::Running { scheduler, .. } = &self.state {
            if let Ok(mut s) = scheduler.try_borrow_mut() {
                s.lead.set_hold(hold);
            }
        }
    }
}

impl Backend for WebBackend {
    type Settings = WebBackendSettings;
    type Error = anyhow::Error;

    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
    ) -> Result<(Self, u32), Self::Error> {
        let ctx = match settings {
            Some(ctx) => ctx,
            None => AudioContext::new().map_err(|e| anyhow::anyhow!("no AudioContext: {e:?}"))?,
        };
        let rate = ctx.sample_rate() as u32;
        Ok((
            Self {
                state: State::Setup(ctx),
            },
            rate,
        ))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        let State::Setup(ctx) = std::mem::replace(&mut self.state, State::Empty) else {
            anyhow::bail!("the audio backend was started twice");
        };
        let rate = ctx.sample_rate();
        let now = ctx.current_time();
        let scheduler = Rc::new(RefCell::new(Scheduler {
            ctx,
            renderer,
            rate,
            cursor: 0.0,
            lead: Lead::new(now),
            mix: vec![0.0; CHUNK_FRAMES * CHANNELS],
            channel: vec![0.0; CHUNK_FRAMES],
            underruns: 0,
        }));
        // One closure refills from the interval and from every chunk's `ended`; it reaches itself
        // through `slot` to hand the chunks it queues.
        let slot: Rc<RefCell<Option<js_sys::Function>>> = Rc::new(RefCell::new(None));
        let refill = {
            let scheduler = Rc::clone(&scheduler);
            let slot = Rc::clone(&slot);
            Closure::<dyn FnMut()>::new(move || {
                let Some(f) = slot.borrow().clone() else {
                    return;
                };
                if let Ok(mut s) = scheduler.try_borrow_mut() {
                    s.pump(&f);
                }
            })
        };
        let function: js_sys::Function =
            refill.as_ref().unchecked_ref::<js_sys::Function>().clone();
        *slot.borrow_mut() = Some(function.clone());
        let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
        let interval = window
            .set_interval_with_callback_and_timeout_and_arguments_0(&function, PUMP_MS)
            .map_err(|e| anyhow::anyhow!("setInterval: {e:?}"))?;
        scheduler.borrow_mut().pump(&function);
        self.state = State::Running {
            _refill: refill,
            interval,
            scheduler,
        };
        Ok(())
    }
}

impl Drop for WebBackend {
    fn drop(&mut self) {
        if let State::Running {
            interval,
            scheduler,
            ..
        } = &self.state
        {
            if let Some(window) = web_sys::window() {
                window.clear_interval_with_handle(*interval);
            }
            let _ = scheduler.borrow().ctx.close();
        }
    }
}
