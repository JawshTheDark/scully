// Throwaway: does GtkMediaFile play a remote .mov on this machine? Prints
// every state/error transition instead of a silent blank widget.
use gtk::prelude::*;

fn main() {
    let app = gtk::Application::builder().application_id("io.jawsh.MediaProbe").build();
    app.connect_activate(|app| {
        let url = std::env::args().nth(1)
            .unwrap_or_else(|| "https://cdn.lurker.chat/7OgC1fvMBftQ.mov".into());
        let win = gtk::ApplicationWindow::builder()
            .application(app).default_width(600).default_height(520).build();
        let media = gtk::MediaFile::for_file(&gtk::gio::File::for_uri(&url));
        media.connect_error_notify(|m| {
            eprintln!("PROBE error: {:?}", m.error().map(|e| e.to_string()));
        });
        media.connect_prepared_notify(|m| {
            eprintln!("PROBE prepared: {} has_video={} duration={}",
                m.is_prepared(), m.has_video(), m.duration());
        });
        media.connect_playing_notify(|m| eprintln!("PROBE playing: {}", m.is_playing()));
        let video = gtk::Video::builder().media_stream(&media).build();
        win.set_child(Some(&video));
        win.present();
        media.play();
        eprintln!("PROBE started for {url}");
    });
    app.run_with_args::<&str>(&[]);
}
