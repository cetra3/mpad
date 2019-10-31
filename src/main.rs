use std::env;

mod multicast;

#[allow(dead_code)]
mod ditto;

use log::*;

use gio::prelude::*;
use gtk::prelude::*;

use std::env::args;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

fn build_ui(application: &gtk::Application) {
    let window = gtk::ApplicationWindow::new(application);

    window.set_title("Mpad");
    window.set_position(gtk::WindowPosition::Center);
    window.set_default_size(800, 600);

    let text_view = gtk::TextView::new();
    let scroll = gtk::ScrolledWindow::new(gtk::NONE_ADJUSTMENT, gtk::NONE_ADJUSTMENT);
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.add(&text_view);

    let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

    let (send, recv) = multicast::setup_channels().expect("Could not create multicast channel");

    thread::spawn(move || loop {

        let val = match recv.recv() {
            Ok(val) => val,
            Err(err) => {
                error!("Could not receive value:{}", err);
                continue;
            }
        };

        if let Err(err) = tx.send(val) {
            error!("Could not send data to channel:{}", err);
        }

    });

    // Attach receiver to the main context and set the text buffer text from here
    let text_buffer = text_view
        .get_buffer()
        .expect("Couldn't get buffer from text_view");

    let is_changing = Arc::new(AtomicBool::new(false));
    let changing = is_changing.clone();

    text_buffer.connect_changed(move |val| {
        if !changing.load(Ordering::Relaxed) {
            let (left, right) = val.get_bounds();

            let text = val.get_text(&left, &right, false).unwrap().to_string();

            debug!("Buffer: {}, Len:{}", text, text.len());

            if let Err(err) = send.send(text) {
                error!("{}", err);
            }
        }
    });

    rx.attach(None, move |text| {
        is_changing.store(true, Ordering::Relaxed);

        text_buffer.set_text(&text);

        is_changing.store(false, Ordering::Relaxed);

        glib::Continue(true)
    });

    window.add(&scroll);
    window.show_all();
}

fn main() {
    if let Err(_) = env::var("RUST_LOG") {
        env::set_var("RUST_LOG", "mpad=INFO");
    }

    pretty_env_logger::init_timed();
    let application = gtk::Application::new(
        Some("com.github.gtk-rs.examples.multithreading_context"),
        Default::default(),
    )
    .expect("Initialization failed...");

    application.connect_activate(|app| {
        build_ui(app);
    });

    application.run(&args().collect::<Vec<_>>());
}
