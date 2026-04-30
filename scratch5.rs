use librespot::core::spotify_id::SpotifyId;
fn test(id: SpotifyId) {
    let s: String = id.to_base62().unwrap();
}
