use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::widget::{container, text};
use iced::{Element, Task};

use crate::domain::{Message, Screen};
use crate::pages::home::{HomeJobResult, HomeState};
use crate::services::AppServices;

#[derive(Debug)]
pub struct PsycGuiApp {
    pub screen: Screen,
    pub selected_repository: Option<PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: AppServices,
    pub home: HomeState,
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
            home: HomeState::default(),
        },
        Task::none(),
    )
}

pub fn update(app: &mut PsycGuiApp, message: Message) -> Task<Message> {
    match message {
        Message::Home(message) => crate::pages::home::update_home(app, message),
        Message::HomeJobFinished(result) => {
            handle_home_job_result(app, result);
            Task::none()
        }
        Message::NoOp => Task::none(),
    }
}

pub fn view(app: &PsycGuiApp) -> Element<'_, Message> {
    let title = match app.screen {
        Screen::Home => "Repositories",
        Screen::Workbench => "Workbench",
    };

    container(text(title).size(32)).into()
}

fn handle_home_job_result(
    app: &mut PsycGuiApp,
    result: Result<HomeJobResult, crate::domain::AppError>,
) {
    match result {
        Ok(HomeJobResult::RepositoryLoaded(card)) => {
            app.home.validation_error = None;
            upsert_home_card(&mut app.home.cards, card.clone());
            app.registry
                .touch_recent(card.repo_root.clone(), current_unix_ms());
            app.selected_repository = Some(card.repo_root);
            app.screen = Screen::Workbench;
        }
        Err(error) => {
            app.home.validation_error = Some(error.message);
        }
    }
}

fn upsert_home_card(
    cards: &mut Vec<crate::domain::RepositoryHomeCard>,
    card: crate::domain::RepositoryHomeCard,
) {
    cards.retain(|existing| existing.repo_root != card.repo_root);
    cards.insert(0, card);
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
