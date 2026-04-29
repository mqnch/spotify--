use rspotify::model::{SimplifiedPlaylist, PlaylistItem};
fn test(p: SimplifiedPlaylist, i: PlaylistItem) {
    let _ = p.items;
    let _ = i.item;
}
