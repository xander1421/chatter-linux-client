//! Call tile order. The video frame list is published in ascending
//! participant-id order and matched to tiles by position (`gl_call.rs`), so
//! every rebuild of the tile model must order the tiles the same way.

/// The local participant's own screen-share tile.
pub const LOCAL_SCREEN_TILE_NAME: &str = "Your screen";

/// Split the local tile — always the last one (the layout feeds the last row
/// the local camera) — off the remote tiles. Re-attach with [`order_tiles`].
pub fn split_local<T>(mut tiles: Vec<T>) -> (Vec<T>, Option<T>) {
    let local = tiles.pop();
    (tiles, local)
}

/// Remote tiles ascending by participant id, then the local tile last.
pub fn order_tiles<T>(mut remotes: Vec<T>, local: Option<T>, id: impl Fn(&T) -> &str) -> Vec<T> {
    remotes.sort_by(|a, b| id(a).cmp(id(b)));
    remotes.extend(local);
    remotes
}

/// Where the local screen-share tile goes among `tiles` tiles: just before
/// the local camera tile, which stays pinned last.
pub fn local_screen_slot(tiles: usize) -> usize {
    tiles.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_sort_by_id_and_local_stays_last() {
        let (remotes, local) = split_local(vec!["b", "a\u{1}screen", "a", "me"]);
        assert_eq!(local, Some("me"));
        assert_eq!(
            order_tiles(remotes, local, |s| s),
            ["a", "a\u{1}screen", "b", "me"]
        );
        assert_eq!(split_local(Vec::<&str>::new()), (vec![], None));
    }

    #[test]
    fn local_screen_goes_before_the_local_camera() {
        let mut tiles = vec!["a", "me"];
        tiles.insert(local_screen_slot(tiles.len()), "me\u{1}screen");
        assert_eq!(tiles, ["a", "me\u{1}screen", "me"]);
        assert_eq!(local_screen_slot(0), 0);
    }
}
