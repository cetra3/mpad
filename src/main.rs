use gtk4 as gtk;

use adw::{prelude::*, HeaderBar};
use eyre::Result;
use gtk::{Box, Orientation};
use multicast::setup;
use rand::Rng;
use tracing::*;
use tracing_subscriber::EnvFilter;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::multicast::TextChange;

mod multicast;

fn build_ui(application: &adw::Application) {
    let site_id = rand::thread_rng().gen();

    let text_view = gtk::TextView::builder()
        .monospace(true)
        .left_margin(5)
        .top_margin(5)
        .bottom_margin(5)
        .right_margin(5)
        .build();

    let scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&text_view)
        .build();

    let content = Box::new(Orientation::Vertical, 0);

    content.append(&HeaderBar::new());
    content.append(&scroll);

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title(&format!("Mpad - {site_id}"))
        .default_width(800)
        .default_height(600)
        .content(&content)
        .build();

    // Attach receiver to the main context and set the text buffer text from here
    let text_buffer = text_view.buffer();

    let in_change = Arc::new(AtomicBool::new(false));
    let insert_in_change = in_change.clone();
    let delete_in_change = in_change.clone();

    let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

    let send = setup(tx, site_id);

    let send_remove = send.clone();
    let span = span!(Level::INFO, "ui", site_id);

    let insert_span = span.clone();
    let delete_span = span.clone();

    text_buffer.connect_insert_text(move |_val, left, text| {
        if !insert_in_change.load(Ordering::Relaxed) {
            let _enter = insert_span.enter();
            debug!("Text inserted offset:{}, len:{}", left.offset(), text.len());

            send.try_send(TextChange::Insert {
                offset: left.offset() as usize,
                text: text.to_owned(),
            })
            .unwrap();
        }
    });

    text_buffer.connect_delete_range(move |_val, left, right| {
        if !delete_in_change.load(Ordering::Relaxed) {
            let _enter = delete_span.enter();
            debug!("Range deleted:{}-{}", left.offset(), right.offset());

            send_remove
                .try_send(TextChange::Remove {
                    offset: left.offset() as usize,
                    len: (right.offset() - left.offset()) as usize,
                })
                .unwrap();
        }
    });

    rx.attach(None, move |text| {
        let _enter = span.enter();
        debug!("Received update: {}", text.len());
        in_change.store(true, Ordering::Relaxed);

        text_buffer.set_text(&text);

        in_change.store(false, Ordering::Relaxed);

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

    let application = adw::Application::new(Some("io.github.cetra3.mpad"), Default::default());

    application.connect_activate(|app| {
        build_ui(app);
    });

    application.run();

    Ok(())
}
