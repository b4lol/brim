//! Dev helper: rasterize assets/brim.svg to /tmp/brim-preview.png for a
//! visual check. Not part of the shipped binary.

use gtk4::prelude::*;

fn main() {
    gtk4::init().expect("GTK init (needs a display)");
    let file = gtk4::gio::File::for_path("assets/brim.svg");
    let texture = gtk4::gdk::Texture::from_file(&file).expect("load SVG");
    texture
        .save_to_png("/tmp/brim-preview.png")
        .expect("save PNG");
    println!("{}x{}", texture.width(), texture.height());
}
