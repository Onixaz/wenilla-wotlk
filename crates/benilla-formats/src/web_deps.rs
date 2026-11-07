//! The chain names a file's loader reads next, read off the file's own bytes: what the web build
//! fetches ahead the moment a parent's bytes arrive, so a loader's serial `await`s (a WMO's groups,
//! an M2's skin and `.anim` files, an ADT's layer textures) find their bytes already fetched
//! instead of paying one round trip each. Plain chunk walking with no browser dependency, so it
//! is tested natively against the install.
//!
//! The lists are a superset of what is certainly read only where the loader's own choice depends
//! on existence (an ADT layer's `_s` variant); the caller drops names the chain index lacks.

use crate::{anim_paths, skin_path};

/// What [`deps`] found in a file: `leaves` are read and done, `parents` carry deps of their own
/// (an ADT's models and WMO roots) and are worth walking in turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deps {
    pub leaves: Vec<String>,
    pub parents: Vec<String>,
}

/// The names `name`'s loader reads after it, from `bytes`, by the name's extension. An unknown
/// extension or an unreadable file has none.
pub fn deps(name: &str, bytes: &[u8]) -> Deps {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".adt") {
        adt_deps(bytes)
    } else if lower.ends_with(".wmo") && !is_wmo_group(&lower) {
        wmo_root_deps(&lower, bytes)
    } else if lower.ends_with(".m2") {
        m2_deps(&lower, bytes)
    } else {
        Deps::default()
    }
}

/// Whether a lowercase `.wmo` name is a group file (`…_NNN.wmo`) rather than a root.
fn is_wmo_group(lower: &str) -> bool {
    let stem = &lower[..lower.len() - 4];
    stem.len() > 4 && {
        let (head, digits) = stem.split_at(stem.len() - 3);
        head.ends_with('_') && digits.bytes().all(|b| b.is_ascii_digit())
    }
}

/// A model reference (`.mdx`/`.mdl`, as ADTs and WMOs write them) as its archive `.m2` name.
pub(crate) fn model_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((stem, "mdx" | "mdl" | "m2")) => format!("{stem}.m2"),
        _ => lower,
    }
}

/// The `(reversed magic, payload)` chunks of a chunked file, stopping at the first truncation.
fn chunks(b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let head = b.get(at..at + 8)?;
        let magic = [head[0], head[1], head[2], head[3]];
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let data = b.get(at + 8..at + 8 + size)?;
        at += 8 + size;
        Some((magic, data))
    })
}

/// The non-empty names of a NUL-separated blob (MTEX, MMDX, MWMO, MOTX).
fn names(blob: &[u8]) -> impl Iterator<Item = String> + '_ {
    blob.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

/// An ADT's layer textures (each with its `_s` variant, which the terrain loader tries first),
/// its doodad models and its WMO roots.
fn adt_deps(b: &[u8]) -> Deps {
    let mut out = Deps::default();
    for (magic, data) in chunks(b) {
        match &magic {
            b"XETM" => {
                for t in names(data) {
                    if let Some(stem) = t
                        .len()
                        .checked_sub(4)
                        .filter(|&i| t.is_char_boundary(i) && t[i..].eq_ignore_ascii_case(".blp"))
                        .map(|i| &t[..i])
                    {
                        out.leaves.push(format!("{stem}_s.blp"));
                    }
                    out.leaves.push(t);
                }
            }
            b"XDMM" => out.parents.extend(names(data).map(|m| model_name(&m))),
            b"OMWM" => out.parents.extend(names(data)),
            _ => {}
        }
    }
    out
}

/// A WMO root's group files (`MOHD.nGroups`, named `<stem>_NNN.wmo`) and its textures (`MOTX`).
/// The doodads (`MODN`) are left out: a building spawns one doodad set, not the whole list.
fn wmo_root_deps(lower: &str, b: &[u8]) -> Deps {
    let stem = &lower[..lower.len() - 4];
    let mut out = Deps::default();
    for (magic, data) in chunks(b) {
        match &magic {
            b"DHOM" => {
                if let Some(n) = data.get(4..8) {
                    let n = u32::from_le_bytes([n[0], n[1], n[2], n[3]]).min(512);
                    out.leaves
                        .extend((0..n).map(|gi| format!("{stem}_{gi:03}.wmo")));
                }
            }
            b"XTOM" => out.leaves.extend(names(data)),
            _ => {}
        }
    }
    out
}

/// An M2's skin profile 0, its sequences' `.anim` files and its file-named textures (type 0).
fn m2_deps(lower: &str, b: &[u8]) -> Deps {
    /// `M2Header.textures`: `(count, offset)` at `0x50`; each record is 16 bytes, type at 0 and
    /// the filename's `(count, offset)` at 8.
    const TEXTURES: usize = 0x50;
    let u32_at = |o: usize| {
        b.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let mut out = Deps::default();
    if b.get(..4) != Some(b"MD20") {
        return out;
    }
    out.leaves.push(skin_path(lower));
    if let Ok(files) = anim_paths(b, lower) {
        out.leaves.extend(files.into_iter().map(|(_, f)| f));
    }
    if let (Some(count), Some(ofs)) = (u32_at(TEXTURES), u32_at(TEXTURES + 4)) {
        for r in (0..count.min(256)).map(|i| ofs + i * 16) {
            let (Some(0), Some(len), Some(at)) = (u32_at(r), u32_at(r + 8), u32_at(r + 12)) else {
                continue;
            };
            let Some(raw) = b.get(at..at + len) else {
                continue;
            };
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            if end > 0 {
                out.leaves
                    .push(String::from_utf8_lossy(&raw[..end]).into_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(magic: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = magic.to_vec();
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn group_names_are_told_from_roots() {
        assert!(is_wmo_group("world\\wmo\\a\\b_000.wmo"));
        assert!(is_wmo_group("b_123.wmo"));
        assert!(!is_wmo_group("world\\wmo\\a\\b.wmo"));
        assert!(!is_wmo_group("b_12.wmo"));
        assert!(!is_wmo_group("_000.wmo"));
    }

    #[test]
    fn an_adt_names_its_layers_models_and_buildings() {
        let mut b = chunk(b"REVM", &18u32.to_le_bytes());
        b.extend(chunk(b"XETM", b"Tileset\\A.blp\0Tileset\\B.blp\0"));
        b.extend(chunk(b"XDMM", b"World\\Tree.MDX\0World\\Rock.m2\0"));
        b.extend(chunk(b"OMWM", b"World\\wmo\\Hut.wmo\0"));
        let d = deps("World\\Maps\\Azeroth\\Azeroth_32_48.adt", &b);
        assert_eq!(
            d.leaves,
            [
                "Tileset\\A_s.blp",
                "Tileset\\A.blp",
                "Tileset\\B_s.blp",
                "Tileset\\B.blp"
            ]
        );
        assert_eq!(
            d.parents,
            ["world\\tree.m2", "world\\rock.m2", "World\\wmo\\Hut.wmo"]
        );
    }

    #[test]
    fn a_wmo_root_names_its_groups_and_textures_and_a_group_nothing() {
        let mut mohd = vec![0u8; 64];
        mohd[4..8].copy_from_slice(&3u32.to_le_bytes());
        let mut b = chunk(b"REVM", &17u32.to_le_bytes());
        b.extend(chunk(b"DHOM", &mohd));
        b.extend(chunk(b"XTOM", b"A.blp\0\0\0B.blp\0"));
        let d = deps("World\\wmo\\Hut.wmo", &b);
        assert_eq!(
            d.leaves,
            [
                "world\\wmo\\hut_000.wmo",
                "world\\wmo\\hut_001.wmo",
                "world\\wmo\\hut_002.wmo",
                "A.blp",
                "B.blp"
            ]
        );
        assert!(d.parents.is_empty());
        assert_eq!(deps("World\\wmo\\Hut_001.wmo", &b), Deps::default());
    }

    #[test]
    fn a_truncated_file_names_what_it_can() {
        let mut b = chunk(b"XETM", b"A.blp\0");
        b.extend_from_slice(b"XDMM\xff\xff\x00\x00short");
        assert_eq!(deps("x.adt", &b).leaves, ["A_s.blp", "A.blp"]);
        assert_eq!(deps("x.m2", b"MD2"), Deps::default());
        assert_eq!(deps("x.blp", b"BLP2"), Deps::default());
    }

    /// The real Goldshire tile and a model on it: every name the walk returns is in the chain,
    /// but for the `_s` variants it lists without knowing, which the caller filters.
    #[test]
    fn real_files_name_what_their_loaders_read() {
        let data = crate::wow_data_or_skip!();
        let chain = crate::Chain::open(&data).expect("open chain");
        let adt_name = "World\\Maps\\Azeroth\\Azeroth_32_48.adt";
        let adt = chain.read(adt_name).expect("read Goldshire's ADT");
        let d = deps(adt_name, &adt);
        assert!(d.leaves.len() >= 4, "{:?}", d.leaves);
        assert!(!d.parents.is_empty());
        for n in d
            .leaves
            .iter()
            .filter(|n| !n.to_ascii_lowercase().ends_with("_s.blp"))
        {
            assert!(chain.contains(n), "{n} not in the chain");
        }
        for n in &d.parents {
            assert!(chain.contains(n), "{n} not in the chain");
        }
        let m2_name = d
            .parents
            .iter()
            .find(|n| n.ends_with(".m2"))
            .expect("a doodad model");
        let m2 = chain.read(m2_name).expect("read the model");
        let md = deps(m2_name, &m2);
        assert!(md.leaves[0].ends_with("00.skin"));
        for n in &md.leaves {
            assert!(chain.contains(n), "{m2_name}: {n} not in the chain");
        }
        let wmo_name = d
            .parents
            .iter()
            .find(|n| n.to_ascii_lowercase().ends_with(".wmo"))
            .expect("a building");
        let wmo = chain.read(wmo_name).expect("read the WMO root");
        let wd = deps(wmo_name, &wmo);
        assert!(wd.leaves.iter().any(|n| n.ends_with("_000.wmo")));
        for n in &wd.leaves {
            assert!(chain.contains(n), "{wmo_name}: {n} not in the chain");
        }
    }
}
