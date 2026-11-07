//! World entry's head start on the web build: the character select record says where the selected
//! character stands, so that map's tiles are fetched, with what each one reads, while the player
//! is still on the select screen. Entry then reads them from memory or the page's cache instead of
//! waiting on the network behind the loading screen. A new selection replaces the old one's
//! unsent fetches, so it never queues behind a character the player left; what landed stays in
//! the page's cache. Natively there is nothing to fetch.

use benilla_assets::MapCatalogRes;
use benilla_ui::script::VmKind;
use benilla_world::view::ViewDistance;
use bevy::prelude::*;

use super::glue_mgr::GlueMgr;
use super::UiScript;

/// Send the selected character's streaming window to the page's store, once per selection.
pub(super) fn fetch_selected_world(
    script: Option<NonSend<UiScript>>,
    mgr: Res<GlueMgr>,
    catalog: Option<Res<MapCatalogRes>>,
    view: Option<Res<ViewDistance>>,
    mut last: Local<super::VmMemo<Option<u64>>>,
) {
    let Some(script) = script.filter(|s| s.vm_kind() == VmKind::Glue) else {
        return;
    };
    let Some(guid) = script.glue_selected_guid() else {
        return;
    };
    let (Some(catalog), Some(view)) = (catalog, view) else {
        return;
    };
    let last = last.get(&script);
    if *last == Some(guid) {
        return;
    }
    let Some(picked) = mgr.roster_character(guid) else {
        return;
    };
    let Some(dir) = catalog.0.directory(picked.map) else {
        return;
    };
    if last.replace(guid).is_some() {
        benilla_formats::web::drop_ahead();
    }
    benilla_formats::web::prefetch_parents(benilla_world::terrain_stream::entry_files(
        dir,
        view.farclip,
        picked.position.x,
        picked.position.y,
    ));
}
