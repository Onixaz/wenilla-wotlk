//! wasm32: music, ambience beds and narration play as native Web Audio voices.
//!
//! The rest of the mix is rendered by kira on the page's one thread and queued ahead
//! (`web_backend`); a stall longer than that queue is silence, and a world load stalls the page for
//! seconds (a cold GPU shader cache: 3.2 s under the login cover, measured). These long sounds are
//! already decoded whole on the page (`web_load`), so the browser can play them itself, on its
//! audio thread, where no stall of the page reaches them: an `AudioBufferSourceNode` through a fade
//! gain (fade-in, pause and stop ramps) and a volume gain (the per-frame volume feed) into one
//! [`MusicBus`], whose gain is the master volume times the output gate, into the speakers.
//!
//! Deviation: these voices bypass kira's main chain, so the output limiter and the level meters do
//! not see them; the master volume and the window-focus mute still apply, through the bus.

use kira::sound::PlaybackState;
use kira::Decibels;
use web_sys::{
    AudioBuffer, AudioBufferSourceNode, AudioContext, AudioParam, AudioScheduledSourceNode,
    GainNode,
};

use super::mixer::StaticSoundData;
use super::web_voice_clock::VoiceClock;

/// The bus's own de-click ramp on a master or gate change, seconds (`mixer::glide`'s 15 ms).
const BUS_RAMP_S: f64 = 0.015;

/// The shared gain every native voice ends in: master volume times the output gate.
pub(super) struct MusicBus {
    ctx: AudioContext,
    gain: GainNode,
    master: f32,
    open: bool,
}

impl MusicBus {
    pub(super) fn new(ctx: AudioContext) -> Result<Self, String> {
        let gain = ctx.create_gain().map_err(js_err)?;
        gain.connect_with_audio_node(&ctx.destination())
            .map_err(js_err)?;
        Ok(Self {
            ctx,
            gain,
            master: 1.0,
            open: true,
        })
    }

    pub(super) fn set_master(&mut self, amp: f32) {
        self.master = amp;
        self.apply();
    }

    pub(super) fn set_open(&mut self, open: bool) {
        self.open = open;
        self.apply();
    }

    fn apply(&self) {
        let target = if self.open { self.master } else { 0.0 };
        ramp(
            &self.gain.gain(),
            self.ctx.current_time(),
            target,
            BUS_RAMP_S,
        );
    }

    /// Start `data` now, as kira would have played it: its fixed volume, its fade-in, its start
    /// position and its loop region.
    pub(super) fn play(&self, data: StaticSoundData) -> Result<NativeVoice, String> {
        let ctx = &self.ctx;
        let rate = data.sample_rate as f32;
        let len = data.frames.len();
        let buffer = ctx
            .create_buffer(2, len.max(1) as u32, rate)
            .map_err(js_err)?;
        let mut channel = vec![0.0f32; len];
        let picks: [fn(&kira::Frame) -> f32; 2] = [|f| f.left, |f| f.right];
        for (ch, pick) in picks.into_iter().enumerate() {
            for (out, frame) in channel.iter_mut().zip(data.frames.iter()) {
                *out = pick(frame);
            }
            buffer
                .copy_to_channel(&channel, ch as i32)
                .map_err(js_err)?;
        }
        drop(channel);
        let duration = len as f64 / f64::from(rate);
        let settings = data.settings;
        let seconds = |p: kira::sound::PlaybackPosition| match p {
            kira::sound::PlaybackPosition::Seconds(s) => s,
            kira::sound::PlaybackPosition::Samples(n) => n as f64 / f64::from(rate),
        };
        let offset = seconds(settings.start_position).clamp(0.0, duration);
        let loop_start = settings.loop_region.map(|r| seconds(r.start));

        let fade = ctx.create_gain().map_err(js_err)?;
        let volume = ctx.create_gain().map_err(js_err)?;
        fade.connect_with_audio_node(&volume).map_err(js_err)?;
        volume.connect_with_audio_node(&self.gain).map_err(js_err)?;
        let now = ctx.current_time();
        if let kira::Value::Fixed(db) = settings.volume {
            volume.gain().set_value(amplitude(db));
        }
        if let Some(tween) = settings.fade_in_tween {
            fade.gain().set_value_at_time(0.0, now).map_err(js_err)?;
            ramp(&fade.gain(), now, 1.0, tween.duration.as_secs_f64());
        }
        let mut voice = NativeVoice {
            ctx: ctx.clone(),
            buffer,
            fade,
            volume,
            source: None,
            loop_start,
            clock: VoiceClock::new(now, offset, duration, loop_start),
        };
        voice.start_source(now, offset)?;
        Ok(voice)
    }
}

/// One native voice: the handle behind `mixer::StreamingSoundHandle` on wasm32. Dropping it does
/// not stop the sound, as dropping a kira handle does not.
pub(crate) struct NativeVoice {
    ctx: AudioContext,
    buffer: AudioBuffer,
    fade: GainNode,
    volume: GainNode,
    /// The source playing now; a pause ends it and a resume starts a new one at the held offset.
    source: Option<AudioBufferSourceNode>,
    loop_start: Option<f64>,
    clock: VoiceClock,
}

impl NativeVoice {
    fn start_source(&mut self, now: f64, offset: f64) -> Result<(), String> {
        let source = self.ctx.create_buffer_source().map_err(js_err)?;
        source.set_buffer(Some(&self.buffer));
        if let Some(start) = self.loop_start {
            source.set_loop(true);
            source.set_loop_start(start);
        }
        source.connect_with_audio_node(&self.fade).map_err(js_err)?;
        source
            .start_with_when_and_grain_offset(now, offset)
            .map_err(js_err)?;
        self.source = Some(source);
        Ok(())
    }

    fn now(&self) -> f64 {
        self.ctx.current_time()
    }

    pub(crate) fn set_volume(&mut self, db: Decibels, fade_secs: f64) {
        ramp(&self.volume.gain(), self.now(), amplitude(db), fade_secs);
    }

    pub(crate) fn state(&self) -> PlaybackState {
        self.clock.state(self.now())
    }

    pub(crate) fn position(&self) -> f64 {
        self.clock.position(self.now())
    }

    pub(crate) fn stop(&mut self, fade_secs: f64) {
        let now = self.now();
        let at = self.clock.stop(now, fade_secs);
        ramp(&self.fade.gain(), now, 0.0, at - now);
        if let Some(source) = &self.source {
            let _ = AudioScheduledSourceNode::stop_with_when(source, at);
        }
    }

    pub(crate) fn pause(&mut self, fade_secs: f64) {
        let now = self.now();
        let Some(at) = self.clock.pause(now, fade_secs) else {
            return;
        };
        ramp(&self.fade.gain(), now, 0.0, at - now);
        if let Some(source) = &self.source {
            let _ = AudioScheduledSourceNode::stop_with_when(source, at);
        }
    }

    pub(crate) fn resume(&mut self, fade_secs: f64) {
        let now = self.now();
        self.clock.settle(now);
        let Some(offset) = self.clock.resume(now) else {
            return;
        };
        if let Some(old) = self.source.take() {
            let _ = AudioScheduledSourceNode::stop_with_when(&old, now);
        }
        if let Err(e) = self.start_source(now, offset) {
            bevy::log::warn!("audio: resuming a native voice failed: {e}");
            return;
        }
        let _ = self.fade.gain().set_value_at_time(0.0, now);
        ramp(&self.fade.gain(), now, 1.0, fade_secs);
    }
}

/// Move `param` from where it is now to `target` over `secs` (a step when `secs` is 0), replacing
/// whatever ramp was running.
fn ramp(param: &AudioParam, now: f64, target: f32, secs: f64) {
    let current = param.value();
    let _ = param.cancel_scheduled_values(now);
    let _ = param.set_value_at_time(current, now);
    if secs > 0.0 {
        let _ = param.linear_ramp_to_value_at_time(target, now + secs);
    } else {
        let _ = param.set_value_at_time(target, now);
    }
}

fn amplitude(db: Decibels) -> f32 {
    db.as_amplitude()
}

fn js_err(e: wasm_bindgen::JsValue) -> String {
    format!("{e:?}")
}
