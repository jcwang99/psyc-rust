use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct NewRepositoryForm {
    pub repo_root_text: String,
    pub password_text: String,
    pub branch_name_text: String,
}

impl Default for NewRepositoryForm {
    fn default() -> Self {
        Self {
            repo_root_text: String::new(),
            password_text: String::new(),
            branch_name_text: "main".into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HomeState {
    pub cards: Vec<crate::domain::RepositoryHomeCard>,
    pub open_repository_path: String,
    pub validation_error: Option<String>,
    pub new_repository: NewRepositoryForm,
}

#[derive(Debug, Clone)]
pub enum HomeMessage {
    SubmitCreateRepository,
    SubmitOpenRepository,
    SelectRepository(PathBuf),
}

#[derive(Debug, Clone)]
pub enum HomeJobResult {
    RepositoryLoaded(crate::domain::RepositoryHomeCard),
}

pub fn update_home(
    app: &mut crate::app::PsycGuiApp,
    message: HomeMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        HomeMessage::SubmitCreateRepository => {
            if app.home.new_repository.password_text.trim().is_empty() {
                app.home.validation_error = Some("Password is required".into());
                return iced::Task::none();
            }

            let repo_root_text = app.home.new_repository.repo_root_text.trim();
            if repo_root_text.is_empty() {
                app.home.validation_error = Some("Repository path is required".into());
                return iced::Task::none();
            }

            app.home.validation_error = None;
            let repo_root = PathBuf::from(repo_root_text);
            let password = app.home.new_repository.password_text.clone();
            let branch_name = if app.home.new_repository.branch_name_text.trim().is_empty() {
                "main".to_owned()
            } else {
                app.home.new_repository.branch_name_text.trim().to_owned()
            };
            let service = app.services.repository.clone();

            crate::jobs::spawn_blocking_job(
                move || service.init_repository(repo_root, password, branch_name),
                |result| {
                    crate::domain::Message::HomeJobFinished(
                        result.map(HomeJobResult::RepositoryLoaded),
                    )
                },
            )
        }
        HomeMessage::SubmitOpenRepository => {
            let repo_root_text = app.home.open_repository_path.trim();
            if repo_root_text.is_empty() {
                app.home.validation_error = Some("Repository path is required".into());
                return iced::Task::none();
            }

            app.home.validation_error = None;
            let repo_root = PathBuf::from(repo_root_text);
            let service = app.services.repository.clone();

            crate::jobs::spawn_blocking_job(
                move || service.open_repository(repo_root),
                |result| {
                    crate::domain::Message::HomeJobFinished(
                        result.map(HomeJobResult::RepositoryLoaded),
                    )
                },
            )
        }
        HomeMessage::SelectRepository(repo_root) => {
            if let Some(card) = app
                .home
                .cards
                .iter()
                .find(|card| card.repo_root == repo_root)
                .cloned()
            {
                crate::app::activate_repository(app, card);
            } else {
                app.registry
                    .touch_recent(repo_root.clone(), current_unix_ms());
                app.selected_repository = Some(repo_root);
                app.screen = crate::domain::Screen::Workbench;
            }
            iced::Task::none()
        }
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
