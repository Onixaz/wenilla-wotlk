//! The character creation set on the page. The reference reads a race's body and its creation
//! stage off the disk when the player clicks the race; on the page each is a model and some fifty
//! animation files over the network. So the moment the creation tables are loaded, at the login
//! screen, every playable race's bodies and stages start downloading to the page's disk while the
//! player types, ahead of the world-entry fetches: a new account goes there next. An account that
//! turns out to have characters stops that download, and Character Create, if the player opens
//! it, fetches the rest under the page's cover (`charcreate` in `web/boot.js`) with the screen
//! waiting on it, so every race, class and dial after it reads the disk. Registered only on the
//! page.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use benilla_formats::web::FillLane;
use bevy::prelude::*;

use crate::char_select::Roster;
use crate::entities::{CharCreate, Creatures};
use crate::portrait::{GlueLook, GluePreview, GlueScene};

/// The page's store tag: a returning player's finished download is not walked again.
const TAG: &str = "glue-create";

/// The walk's lane ([`lane_for`]), as `FillLane as u8`, read at each of its steps.
static LANE: AtomicU8 = AtomicU8::new(FillLane::First as u8);
/// Whether a walk is going; at most one is.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Whether the whole set is on the page's disk.
static DONE: AtomicBool = AtomicBool::new(false);

/// The set's models, kept for a walk that starts again when Character Create opens.
#[derive(Resource)]
pub(crate) struct CreateSet(Vec<String>);

/// A creation stage's model, as the glue booth loads it.
fn scene_model(scene: GlueScene) -> String {
    let token = scene.token();
    format!("Interface\\Glues\\Models\\UI_{token}\\UI_{token}.mdx")
}

/// Each race's bodies, then its stage, in the race table's order; the Death Knight's stage last.
/// `body` names a (race, sex) body's model, `None` for a race nobody may create.
fn create_set(
    races: impl IntoIterator<Item = u8>,
    body: impl Fn(u8, u8) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for race in races {
        let bodies: Vec<String> = (0..2).filter_map(|sex| body(race, sex)).collect();
        if bodies.is_empty() {
            continue;
        }
        out.extend(bodies);
        out.push(scene_model(GlueScene::of_race(race)));
    }
    out.push(scene_model(GlueScene::DeathKnight));
    let mut seen = std::collections::HashSet::new();
    out.retain(|n| seen.insert(n.to_ascii_lowercase()));
    out
}

/// The walk's lane: with the screen waiting while Character Create shows, else ahead of the
/// world-entry fetches until the account turns out to have characters, then stopped.
fn lane_for(in_create: bool, has_characters: bool) -> FillLane {
    match (in_create, has_characters) {
        (true, _) => FillLane::Now,
        (false, false) => FillLane::First,
        (false, true) => FillLane::Off,
    }
}

fn lane() -> FillLane {
    match LANE.load(Ordering::Relaxed) {
        x if x == FillLane::Now as u8 => FillLane::Now,
        x if x == FillLane::First as u8 => FillLane::First,
        x if x == FillLane::Behind as u8 => FillLane::Behind,
        _ => FillLane::Off,
    }
}

/// A walk ended, finished or stopped: the page lifts Character Create's cover if it shows one.
fn walked(all_on_disk: bool) {
    RUNNING.store(false, Ordering::Relaxed);
    if all_on_disk {
        DONE.store(true, Ordering::Relaxed);
    }
    crate::webprogress::signal("charcreate-ready");
}

fn walk(models: &[String]) {
    if DONE.load(Ordering::Relaxed) || RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    benilla_formats::web::fill_trees(models.to_vec(), TAG, lane, walked);
}

/// Start the download once per page, as soon as the creation tables and the display catalog are in.
pub(crate) fn fetch_create_set_once(
    mut commands: Commands,
    mut started: Local<bool>,
    create: Option<Res<CharCreate>>,
    creatures: Option<Res<Creatures>>,
) {
    if *started {
        return;
    }
    let (Some(create), Some(creatures)) = (create, creatures) else {
        return;
    };
    *started = true;
    let catalog = &create.0;
    let names = create_set(catalog.race_rows().iter().map(|r| r.id), |race, sex| {
        creatures.model_path(catalog.body_display(race, sex)?)
    });
    info!("glue ahead: {} creation models", names.len());
    walk(&names);
    commands.insert_resource(CreateSet(names));
}

/// Follow the screen and the roster into the walk's lane; Character Create opening with the set
/// not yet on disk raises the page's cover and starts a stopped walk again.
pub(crate) fn steer(
    mut was_in_create: Local<bool>,
    preview: Option<Res<GluePreview>>,
    roster: Option<Res<Roster>>,
    set: Option<Res<CreateSet>>,
) {
    let in_create = preview.is_some_and(|p| matches!(p.look, Some(GlueLook::Create(_))));
    let has_characters = roster.is_some_and(|r| !r.is_empty());
    LANE.store(lane_for(in_create, has_characters) as u8, Ordering::Relaxed);
    let opened = in_create && !*was_in_create;
    *was_in_create = in_create;
    if opened && !DONE.load(Ordering::Relaxed) {
        if let Some(set) = set {
            crate::webprogress::signal("charcreate");
            walk(&set.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_race_brings_its_bodies_and_its_stage_once() {
        let got = create_set([1, 3, 7, 9], |race, sex| {
            (race != 9).then(|| format!("Character\\R{race}\\S{sex}.mdx"))
        });
        assert_eq!(
            got,
            [
                "Character\\R1\\S0.mdx",
                "Character\\R1\\S1.mdx",
                "Interface\\Glues\\Models\\UI_Human\\UI_Human.mdx",
                "Character\\R3\\S0.mdx",
                "Character\\R3\\S1.mdx",
                "Interface\\Glues\\Models\\UI_Dwarf\\UI_Dwarf.mdx",
                // A Gnome stands in the Dwarf's stage, fetched once; race 9 creates nothing.
                "Character\\R7\\S0.mdx",
                "Character\\R7\\S1.mdx",
                "Interface\\Glues\\Models\\UI_DeathKnight\\UI_DeathKnight.mdx",
            ]
        );
    }

    #[test]
    fn the_set_goes_ahead_for_a_new_account_and_waits_for_the_screen_otherwise() {
        assert_eq!(
            lane_for(false, false),
            FillLane::First,
            "login, or no characters"
        );
        assert_eq!(
            lane_for(false, true),
            FillLane::Off,
            "characters: nothing up front"
        );
        assert_eq!(
            lane_for(true, true),
            FillLane::Now,
            "the player opened Character Create"
        );
        assert_eq!(lane_for(true, false), FillLane::Now);
    }
}
