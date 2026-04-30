use librespot::playback::player::PlayerEvent;
fn test(event: PlayerEvent) {
    if let PlayerEvent::TrackChanged { audio_item } = event {
        let _ = audio_item.name;
        let _ = audio_item.track_id;
        let _ = audio_item.duration_ms;
        // let _ = audio_item.artist; // does it have this?
        // let _ = audio_item.album; // does it have this?
    }
}
