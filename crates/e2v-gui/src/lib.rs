pub mod app;
pub mod domain;
pub mod jobs;
pub mod pages {
    pub mod branches;
    pub mod history;
    pub mod home;
    pub mod overview;
    pub mod workbench;
}
pub mod services;
pub mod testing;
pub mod widgets {
    pub mod job_drawer;
}

pub use app::{PsycGuiApp, boot, boot_with_services};
pub use domain::{
    AppError, JobRecord, JobState, Message, RecentRepositoryEntry, RepositoryHomeCard,
    RepositoryRegistry, Screen, WorkbenchPage,
};

pub fn run() -> iced::Result {
    iced::application(app::boot, app::update, app::view)
        .title("psyc-rust")
        .window_size(iced::Size::new(1200.0, 800.0))
        .run()
}
