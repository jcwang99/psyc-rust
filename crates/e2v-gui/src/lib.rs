pub mod app;
pub mod domain;

pub use app::{PsycGuiApp, boot};
pub use domain::{Message, Screen};

pub fn run() -> iced::Result {
    iced::application(app::boot, app::update, app::view)
        .title("psyc-rust")
        .window_size(iced::Size::new(1200.0, 800.0))
        .run()
}
