pub mod app;
pub mod domain;
pub mod jobs;
pub mod pages {
    pub mod branches;
    pub mod history;
    pub mod home;
    pub mod overview;
    pub mod search;
    pub mod sharing;
    pub mod sync;
    pub mod workbench;
}
pub mod services;
pub mod testing;
pub mod widgets {
    pub mod confirmation_sheet;
    pub mod home_screen;
    pub mod job_drawer;
    pub mod status_badge;
    pub mod workbench_shell;
}

pub use app::{PsycGuiApp, boot, boot_with_services};
pub use domain::{
    AppError, JobRecord, JobState, Message, PendingConfirmation, RecentRepositoryEntry,
    RepositoryHomeCard, RepositoryRegistry, Screen, WorkbenchPage,
};

pub fn run() -> iced::Result {
    iced::application(app::boot, app::update, app::view)
        .title("psyc-rust")
        .window_size(iced::Size::new(1200.0, 800.0))
        .run()
}
