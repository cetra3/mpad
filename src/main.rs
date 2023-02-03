use gtk4 as gtk;

use eyre::Result;
use gio::prelude::*;
use gtk::prelude::*;
use multicast::setup_channels;
use tracing::*;
use tracing_subscriber::EnvFilter;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

mod multicast;

fn build_ui(application: &gtk::Application) {
    let text_view = gtk::TextView::builder()
        .monospace(true)
        .margin_bottom(5)
        .margin_end(5)
        .margin_start(5)
        .margin_top(5)
        .build();

    let scroll = gtk::ScrolledWindow::builder().child(&text_view).build();

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Mpad")
        .default_width(800)
        .default_height(600)
        .child(&scroll)
        .build();

    // Attach receiver to the main context and set the text buffer text from here
    let text_buffer = text_view.buffer();

    let is_changing = Arc::new(AtomicBool::new(false));
    let changing = is_changing.clone();

    let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

    let (send, recv) = setup_channels().unwrap();

    thread::spawn(move || loop {
        let val = match recv.recv() {
            Ok(val) => val,
            Err(err) => {
                error!("Could not receive value:{}", err);
                break;
            }
        };

        if let Err(err) = tx.send(val) {
            error!("Could not send data to channel:{}", err);
            break;
        }
    });

    text_buffer.connect_changed(move |val| {
        if !changing.load(Ordering::Relaxed) {
            let (left, right) = val.bounds();

            let text = val.text(&left, &right, false).to_string();

            debug!("Buffer: {}, Len:{}", text, text.len());

            send.send(text).unwrap();
        }
    });

    rx.attach(None, move |text| {
        is_changing.store(true, Ordering::Relaxed);

        text_buffer.set_text(&text);

        is_changing.store(false, Ordering::Relaxed);

        glib::Continue(true)
    });

    window.show();
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_level(true)
        .with_env_filter(
            EnvFilter::builder()
                .with_env_var("RUST_LOG")
                .with_default_directive("mpad=debug".parse()?)
                .from_env_lossy(),
        )
        .init();

    let application = gtk::Application::new(Some("io.github.cetra3.mpad"), Default::default());

    application.connect_activate(|app| {
        build_ui(app);
    });

    application.run();

    Ok(())
}
