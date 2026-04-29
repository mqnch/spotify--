use rspotify::{prelude::*, AuthCodeSpotify, model::{PlaylistId, PlayableId, TrackId, UserId}};
async fn test(spotify: &AuthCodeSpotify) {
    let stream = spotify.current_user_saved_tracks(None);
    let user_id = UserId::from_id("test").unwrap();
    let pl = spotify.user_playlist_create(user_id, "name", Some(false), Some(false), Some("desc")).await.unwrap();
    let pl_id = PlaylistId::from_id("test").unwrap();
    let track_id = TrackId::from_id("test").unwrap();
    let playable = PlayableId::Track(track_id);
    spotify.playlist_add_items(pl_id.clone(), vec![playable.clone()], None).await.unwrap();
    // spotify.playlist_remove_all_occurrences_of_items(pl_id, vec![playable], None).await.unwrap();
}
