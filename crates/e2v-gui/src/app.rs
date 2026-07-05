use std::path::PathBuf;

use iced::widget::{container, text};
use iced::{Element, Task};

use crate::domain::{Message, Screen};
use crate::services::AppServices;

#[derive(Debug)]
pub struct PsycGuiApp {
    pub screen: Screen,
    pub selected_repository: Option<PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: AppServices,
}

pub fn boot() -> (PsycGuiApp, Task<Message>) {
    boot_with_services(AppServices::real())
}

pub fn boot_with_services(services: AppServices) -> (PsycGuiApp, Task<Message>) {
    (
        PsycGuiApp {
            screen: Screen::Home,
            selected_repository: None,
            registry: crate::domain::RepositoryRegistry::default(),
            services,
        },
        Task::none(),
    )
}

pub fn update(_app: &mut PsycGuiApp, _message: Message) -> Task<Message> {
    Task::none()
}

pub fn view(app: &PsycGuiApp) -> Element<'_, Message> {
    let title = match app.screen {
        Screen::Home => "Repositories",
        Screen::Workbench => "Workbench",
    };

    container(text(title).size(32)).into()
}
