use librespot::playback::player::PlayerEvent;
fn test(event: PlayerEvent) {
    if let PlayerEvent::TrackChanged { audio_item } = event {
        let _ = audio_item.name;
        // let _ = audio_item.artists; 
    }
}
