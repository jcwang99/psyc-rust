#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeCardModel {
    pub display_name: String,
    pub repo_root: std::path::PathBuf,
    pub branch_name: String,
    pub head_snapshot_id: Option<String>,
    pub remote_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeScreenModel {
    pub cards: Vec<HomeCardModel>,
    pub validation_error: Option<String>,
}

pub fn build_home_screen_model(app: &crate::app::PsycGuiApp) -> HomeScreenModel {
    HomeScreenModel {
        cards: app
            .home
            .cards
            .iter()
            .map(|card| HomeCardModel {
                display_name: card.display_name.clone(),
                repo_root: card.repo_root.clone(),
                branch_name: card.branch_name.clone(),
                head_snapshot_id: card.head_snapshot_id.clone(),
                remote_configured: card.remote_configured,
            })
            .collect(),
        validation_error: app.home.validation_error.clone(),
    }
}

pub fn view_home(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, scrollable, text, text_input};

    let model = build_home_screen_model(app);

    let cards = if model.cards.is_empty() {
        column![
            text("Repositories").size(32),
            text("No repositories loaded yet."),
        ]
        .spacing(16)
    } else {
        model.cards.into_iter().fold(
            column![text("Repositories").size(32)].spacing(16),
            |column, card| {
                let head = card.head_snapshot_id.unwrap_or_else(|| "none".into());
                let remote = if card.remote_configured {
                    "Remote: configured"
                } else {
                    "Remote: missing"
                };
                column.push(
                    button(
                        column![
                            text(card.display_name).size(22),
                            text(card.repo_root.display().to_string()),
                            row![
                                crate::widgets::status_badge::view_status_badge(format!(
                                    "Branch: {}",
                                    card.branch_name
                                )),
                                crate::widgets::status_badge::view_status_badge(format!(
                                    "Head: {}",
                                    head
                                )),
                                crate::widgets::status_badge::view_status_badge(remote),
                            ]
                            .spacing(8),
                        ]
                        .spacing(6),
                    )
                    .on_press(
                        crate::pages::home::HomeMessage::SelectRepository(card.repo_root),
                    ),
                )
            },
        )
    };

    let forms = {
        let base = column![
            text("Open existing repository").size(22),
            text_input("Open repository path", &app.home.open_repository_path)
                .on_input(crate::pages::home::HomeMessage::SetOpenRepositoryPath)
                .padding(10),
            button("Open repository")
                .on_press(crate::pages::home::HomeMessage::SubmitOpenRepository),
            text("Create new repository").size(22),
            text_input(
                "Create repository path",
                &app.home.new_repository.repo_root_text
            )
            .on_input(crate::pages::home::HomeMessage::SetNewRepositoryPath)
            .padding(10),
            text_input("Password", &app.home.new_repository.password_text)
                .on_input(crate::pages::home::HomeMessage::SetNewRepositoryPassword)
                .padding(10),
            text_input("Branch", &app.home.new_repository.branch_name_text)
                .on_input(crate::pages::home::HomeMessage::SetNewRepositoryBranch)
                .padding(10),
            button("Create repository")
                .on_press(crate::pages::home::HomeMessage::SubmitCreateRepository),
        ]
        .spacing(12);

        if let Some(error) = model.validation_error.clone() {
            base.push(text(error))
        } else {
            base
        }
    };

    let page: iced::Element<'_, crate::pages::home::HomeMessage> = container(
        row![
            scrollable(cards).width(iced::Length::FillPortion(2)),
            container(forms).width(iced::Length::FillPortion(1)),
        ]
        .spacing(20),
    )
    .padding(24)
    .into();
    page.map(crate::domain::Message::from)
}
