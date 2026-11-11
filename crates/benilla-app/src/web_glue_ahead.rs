//! The character screens' head start on the page. The reference reads a race's body and its
//! creation stage off the disk when the player clicks the race; on the page each is a model and
//! some fifty animation files over the network. So the moment the creation tables are loaded, at
//! the login screen, every playable race's bodies and stages download to the page's disk, while
//! the player types. They go ahead of the world-entry fetches until the account turns out to have
//! characters, whose world entry is then the likelier next step. Registered only on the page.

use std::sync::atomic::{AtomicBool, Ordering};

use bevy::prelude::*;

use crate::char_select::Roster;
use crate::entities::{CharCreate, Creatures};
use crate::portrait::GlueScene;

/// The page's store tag: a returning player's finished download is not walked again.
const TAG: &str = "glue-create";

/// Whether the set goes ahead of the queued fetches ahead: until a roster with characters.
static FIRST: AtomicBool = AtomicBool::new(true);

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

/// Start the download once per page, as soon as the creation tables and the display catalog are in.
pub(crate) fn fetch_create_set_once(
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
    benilla_formats::web::fill_trees(names, TAG, || FIRST.load(Ordering::Relaxed));
}

/// Follow the roster: an account with characters sends its world entry first.
pub(crate) fn follow_roster(roster: Option<Res<Roster>>) {
    if let Some(roster) = roster.filter(|r| r.is_changed()) {
        FIRST.store(roster.is_empty(), Ordering::Relaxed);
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
}
