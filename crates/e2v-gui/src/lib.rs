pub mod app;
pub mod domain;
pub mod services;
pub mod testing;

pub use app::{PsycGuiApp, boot, boot_with_services};
pub use domain::{
    AppError, Message, RecentRepositoryEntry, RepositoryHomeCard, RepositoryRegistry, Screen,
};

pub fn run() -> iced::Result {
    iced::application(app::boot, app::update, app::view)
        .title("psyc-rust")
        .window_size(iced::Size::new(1200.0, 800.0))
        .run()
}
