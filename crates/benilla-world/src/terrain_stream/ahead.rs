//! Fetch-ahead for the web build: the tiles the residency window will want a few seconds from now,
//! along the viewer's motion, fetched before the streamer asks for them. Each tile's ADT is
//! fetched with its own reads walked as it lands (`benilla_formats::web::prefetch_parents`), so
//! by the time a tile enters the window its layers, models and buildings are in the page's store
//! and its spawn costs parse, not round trips. Natively there is nothing to fetch: the system is
//! registered on `wasm32` alone.

use std::collections::HashSet;

use bevy::prelude::*;

use super::{StreamWindow, TerrainStreamer, ViewFocus};
use crate::view::{ViewDistance, WorldCamera};
use benilla_assets::WdtIndex;

/// How far ahead, in seconds of the viewer's current motion, the window is predicted.
const LOOKAHEAD_S: f32 = 6.0;
/// A jump longer than this between frames is a teleport, not motion: it resets the estimate.
const TELEPORT_YD: f32 = 120.0;
/// The velocity estimate's smoothing time constant, seconds.
const SMOOTH_S: f32 = 0.5;

/// The motion estimate and the tiles already sent ahead, per map.
#[derive(Default)]
pub(super) struct Ahead {
    /// The last focus sample, wow `(x, y)`, and its time in seconds.
    last: Option<([f32; 2], f32)>,
    /// Smoothed velocity, yards per second.
    vel: [f32; 2],
    /// Tiles fetched ahead and still near: never sent twice while they stay near.
    sent: HashSet<(i32, i32)>,
    /// The map directory the state is for.
    map: Option<String>,
}

impl Ahead {
    /// Fold the focus at `t` seconds and return where it is predicted to be [`LOOKAHEAD_S`] on.
    fn observe(&mut self, pos: [f32; 2], t: f32) -> [f32; 2] {
        if let Some((last, t0)) = self.last {
            let dt = t - t0;
            let d = [pos[0] - last[0], pos[1] - last[1]];
            if d[0].hypot(d[1]) > TELEPORT_YD {
                self.vel = [0.0, 0.0];
            } else if dt > 0.0 {
                let a = 1.0 - (-dt / SMOOTH_S).exp();
                for (v, d) in self.vel.iter_mut().zip(d) {
                    *v += (d / dt - *v) * a;
                }
            }
        }
        self.last = Some((pos, t));
        [
            pos[0] + self.vel[0] * LOOKAHEAD_S,
            pos[1] + self.vel[1] * LOOKAHEAD_S,
        ]
    }

    /// The tiles the window at `predicted` wants that are neither held (`held`) nor sent yet,
    /// marking them sent; a sent tile both windows have left is forgotten, so a return re-fetches.
    fn take_new(
        &mut self,
        now: &StreamWindow,
        predicted: &StreamWindow,
        exists: impl Fn((i32, i32)) -> bool,
        held: impl Fn((i32, i32)) -> bool,
    ) -> Vec<(i32, i32)> {
        self.sent.retain(|&t| now.keeps(t) || predicted.keeps(t));
        let fresh: Vec<_> = predicted
            .wanted_tiles()
            .into_iter()
            .filter(|&t| exists(t) && !held(t) && !self.sent.contains(&t))
            .collect();
        self.sent.extend(fresh.iter().copied());
        fresh
    }
}

/// What a viewer arriving at wow `(x, y)` on map directory `dir` streams first, nearest first: the
/// map's WDT and its window's tiles. The web build's world entry fetches these ahead of the login.
pub fn entry_files(dir: &str, farclip: f32, x: f32, y: f32) -> Vec<String> {
    let window = StreamWindow::at(farclip, x, y);
    let (fx, fy) = window.focus_tile();
    let mut tiles = window.wanted_tiles();
    tiles.sort_by_key(|&(tx, ty)| (tx - fx).abs().max((ty - fy).abs()));
    std::iter::once(format!("World\\Maps\\{dir}\\{dir}.wdt"))
        .chain(
            tiles
                .into_iter()
                .map(|(tx, ty)| format!("World\\Maps\\{dir}\\{dir}_{tx}_{ty}.adt")),
        )
        .collect()
}

/// Send the predicted window's new tiles to the page's store, nearest the prediction first.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(super) fn prefetch_ahead(
    state: Res<TerrainStreamer>,
    focus: Res<ViewFocus>,
    camera: Query<&Transform, With<WorldCamera>>,
    view: Res<ViewDistance>,
    wdts: Res<Assets<WdtIndex>>,
    time: Res<Time>,
    mut ahead: Local<Ahead>,
) {
    let Some(dir) = state.map_dir.as_deref() else {
        return;
    };
    if ahead.map.as_deref() != Some(dir) {
        *ahead = Ahead {
            map: Some(dir.to_string()),
            ..default()
        };
    }
    // Only a WDT-gated map: ungated probing would fetch ahead tiles that do not exist.
    let Some(wdt) = state.wdt.as_ref().and_then(|h| wdts.get(h)) else {
        return;
    };
    let center = focus.resolve(camera.single().ok().map(|c| c.translation));
    let predicted = ahead.observe([center[0], center[1]], time.elapsed_secs());
    let now = StreamWindow::at(view.farclip, center[0], center[1]);
    let then = StreamWindow::at(view.farclip, predicted[0], predicted[1]);
    if then == now && state.tiles.len() >= now.wanted_tiles().len() {
        return; // standing still with the window full: nothing ahead to fetch
    }
    let mut fresh = ahead.take_new(
        &now,
        &then,
        |(tx, ty)| wdt.has_tile(tx as u32, ty as u32),
        |t| state.tiles.contains_key(&t),
    );
    let (fx, fy) = then.focus_tile();
    fresh.sort_by_key(|&(tx, ty)| (tx - fx).abs().max((ty - fy).abs()));
    benilla_formats::web::prefetch_parents(
        fresh
            .into_iter()
            .map(|(tx, ty)| format!("World\\Maps\\{dir}\\{dir}_{tx}_{ty}.adt")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_formats::tile_to_world;

    /// The WDT leads, then the tile underfoot, then its neighbours.
    #[test]
    fn an_entry_fetches_the_wdt_and_then_the_tiles_outward_from_the_character() {
        let window = StreamWindow::at(500.0, -9450.0, 60.0);
        let (fx, fy) = window.focus_tile();
        let files = entry_files("Azeroth", 500.0, -9450.0, 60.0);
        assert_eq!(files[0], "World\\Maps\\Azeroth\\Azeroth.wdt");
        assert_eq!(
            files[1],
            format!("World\\Maps\\Azeroth\\Azeroth_{fx}_{fy}.adt")
        );
        assert_eq!(files.len(), window.wanted_tiles().len() + 1);
    }

    /// Motion at a steady 10 yd/s converges on a prediction 60 yd ahead; a teleport resets it.
    #[test]
    fn the_prediction_follows_motion_and_drops_on_a_teleport() {
        let mut a = Ahead::default();
        let mut p = [0.0, 0.0];
        for i in 0..400 {
            p = a.observe([i as f32 * 0.1, 0.0], i as f32 * 0.01);
        }
        assert!((p[0] - (39.9 + 60.0)).abs() < 1.0, "{p:?}");
        assert!(p[1].abs() < 1e-3);
        let q = a.observe([5000.0, 0.0], 4.01);
        assert_eq!(q, [5000.0, 0.0]);
    }

    /// The window ahead sends its tiles once, skips held and absent ones, and forgets the ones left
    /// behind.
    #[test]
    fn tiles_ahead_are_sent_once_and_forgotten_when_left() {
        let mut a = Ahead::default();
        let at = |tx: i32, ty: i32| {
            let (x, y) = tile_to_world(tx as u32, ty as u32);
            // The tile's middle: tile_to_world gives its north-west corner.
            StreamWindow::at(
                350.0,
                x - benilla_formats::TILE_SIZE / 2.0,
                y - benilla_formats::TILE_SIZE / 2.0,
            )
        };
        let now = at(32, 48);
        let then = at(33, 48);
        let first = a.take_new(&now, &then, |_| true, |t| t == (33, 48));
        assert!(!first.is_empty());
        assert!(!first.contains(&(33, 48)), "a held tile is not sent");
        assert!(a
            .take_new(&now, &then, |_| true, |_| false)
            .contains(&(33, 48)));
        assert!(a.take_new(&now, &then, |_| true, |_| false).is_empty());
        let far = at(10, 10);
        let back = a.take_new(&far, &far, |t| t != (10, 10), |_| false);
        assert!(!back.contains(&(10, 10)), "an absent tile is not sent");
        assert!(a.sent.iter().all(|&t| far.keeps(t)));
        assert!(!a.take_new(&now, &then, |_| true, |_| false).is_empty());
    }
}
