use std::path::PathBuf;

use iced::widget::{container, text};
use iced::{Element, Task};

use crate::domain::{Message, Screen};

#[derive(Debug)]
pub struct PsycGuiApp {
    pub screen: Screen,
    pub selected_repository: Option<PathBuf>,
}

pub fn boot() -> (PsycGuiApp, Task<Message>) {
    (
        PsycGuiApp {
            screen: Screen::Home,
            selected_repository: None,
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
