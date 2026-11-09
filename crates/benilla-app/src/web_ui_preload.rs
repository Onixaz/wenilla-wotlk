//! The page's UI preload. The reference reads its interface files off the disk when a window
//! opens; on the page that read of an absent file is a network round trip, so the files come
//! ahead of the player's click. `LoadAddOn` must answer within the call, so the addons' code waits
//! in memory; art draws when it lands, so it only has to be on the disk.
//!
//! Registered only on the page. The login and character screens' art and the world map stay out
//! of the art list: the first is read at those screens, the second is by far the largest part and
//! is fetched last.

use std::collections::HashSet;

use bevy::prelude::*;

use benilla_world::world_map::CurrentMap;

use crate::ui_world_map::WorldMapCatalogRes;

const BLIZZARD_ADDONS: &str = r"interface\addons\blizzard_";
const INTERFACE: &str = r"interface\";
const WORLD_MAP: &str = r"interface\worldmap\";
const GLUES: &str = r"interface\glues\";
const ICONS: &str = r"interface\icons\";

/// How many frames in the world the map's catalog may take before the art starts without it.
const CATALOG_WAIT_FRAMES: u32 = 600;

/// What to fetch, in index spelling and fetch order.
#[derive(Debug, Default, PartialEq)]
struct Lists {
    code: Vec<String>,
    art: Vec<String>,
    map: Vec<String>,
}

/// Sort the chain's index into the three lists. `here` holds the lowercased `WorldMapArea` art
/// folders of the player's continent, whose map tiles go first; icons follow the windows' own art.
fn lists(index: impl IntoIterator<Item = String>, here: &HashSet<String>) -> Lists {
    let mut out = Lists::default();
    for name in index {
        if !name.starts_with(INTERFACE) {
            continue;
        }
        if name.starts_with(BLIZZARD_ADDONS) {
            if [".lua", ".xml", ".toc"].iter().any(|e| name.ends_with(e)) {
                out.code.push(name);
            }
        } else if name.starts_with(WORLD_MAP) {
            out.map.push(name);
        } else if !name.starts_with(GLUES) {
            out.art.push(name);
        }
    }
    out.code.sort();
    out.art.sort_by_key(|n| (n.starts_with(ICONS), n.clone()));
    let far = |n: &String| !here.contains(n[WORLD_MAP.len()..].split('\\').next().unwrap_or(""));
    out.map.sort_by_key(|n| (far(n), n.clone()));
    out
}

/// The art folders of the continent `map` is drawn on, lowercased; empty off every continent.
fn continent_folders(catalog: &benilla_ui::script::WorldMapCatalog, map: u32) -> HashSet<String> {
    catalog
        .continents
        .iter()
        .filter(|c| c.map_id == map)
        .flat_map(|c| std::iter::once(&c.map_file).chain(c.zones.iter().map(|z| &z.map_file)))
        .map(|f| f.to_ascii_lowercase())
        .collect()
}

/// Bring the addons' code into memory at the character screens, once per page.
pub(crate) fn pin_ui_code_once(mut done: Local<bool>) {
    if std::mem::replace(&mut *done, true) {
        return;
    }
    let code = lists(benilla_formats::web::index_names(), &HashSet::new()).code;
    info!("ui preload: {} code files", code.len());
    benilla_formats::web::prefetch_pinned(&code);
}

/// Start the art's download once in the world, once per page: the windows' art, then the map.
pub(crate) fn fill_ui_art_once(
    mut started: Local<bool>,
    mut waited: Local<u32>,
    map: Option<Res<CurrentMap>>,
    catalog: Option<Res<WorldMapCatalogRes>>,
) {
    if *started {
        return;
    }
    let Some(map) = map else {
        return;
    };
    *waited += 1;
    if catalog.is_none() && *waited < CATALOG_WAIT_FRAMES {
        return;
    }
    *started = true;
    let here = catalog.map_or_else(HashSet::new, |c| continent_folders(&c.0, map.0));
    let Lists { art, map, .. } = lists(benilla_formats::web::index_names(), &here);
    info!(
        "ui preload: {} art files, {} map files",
        art.len(),
        map.len()
    );
    benilla_formats::web::fill_in_turn(vec![("ui-art", art), ("world-map", map)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_index_sorts_into_code_art_and_map() {
        let got = lists(
            index(&[
                r"interface\icons\spell_a.blp",
                r"interface\addons\blizzard_talentui\blizzard_talentui.lua",
                r"interface\addons\blizzard_talentui\blizzard_talentui.xml",
                r"interface\addons\blizzard_talentui\blizzard_talentui.toc",
                r"interface\addons\blizzard_talentui\notes.txt",
                r"interface\talentframe\ui-talentframe.blp",
                r"interface\glues\models\ui_human\ui_human.m2",
                r"interface\worldmap\elwynn\elwynn1.blp",
                r"world\maps\azeroth\azeroth_1_1.adt",
            ]),
            &HashSet::new(),
        );
        assert_eq!(
            got.code,
            index(&[
                r"interface\addons\blizzard_talentui\blizzard_talentui.lua",
                r"interface\addons\blizzard_talentui\blizzard_talentui.toc",
                r"interface\addons\blizzard_talentui\blizzard_talentui.xml",
            ])
        );
        assert_eq!(
            got.art,
            index(&[
                r"interface\talentframe\ui-talentframe.blp",
                r"interface\icons\spell_a.blp",
            ]),
            "the windows' own art goes before the icons, and the glue screens not at all"
        );
        assert_eq!(got.map, index(&[r"interface\worldmap\elwynn\elwynn1.blp"]));
    }

    #[test]
    fn the_players_continent_leads_the_map_tiles() {
        let here: HashSet<String> = ["tirisfal".to_string()].into_iter().collect();
        let got = lists(
            index(&[
                r"interface\worldmap\ashenvale\ashenvale1.blp",
                r"interface\worldmap\tirisfal\tirisfal1.blp",
                r"interface\worldmap\tirisfal_extra\x.blp",
            ]),
            &here,
        );
        assert_eq!(
            got.map,
            index(&[
                r"interface\worldmap\tirisfal\tirisfal1.blp",
                r"interface\worldmap\ashenvale\ashenvale1.blp",
                r"interface\worldmap\tirisfal_extra\x.blp",
            ])
        );
    }
}
