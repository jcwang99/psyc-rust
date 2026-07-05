#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultRow {
    pub path: String,
    pub source: String,
    pub file_object_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query_text: String,
    pub path_prefix_text: String,
    pub validation_error: Option<String>,
    pub results: Vec<SearchResultRow>,
}

#[derive(Debug, Clone)]
pub enum SearchMessage {
    SetQueryText(String),
    SetPathPrefixText(String),
    SubmitSearch,
}

pub fn update_search(
    app: &mut crate::app::PsycGuiApp,
    message: SearchMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        SearchMessage::SetQueryText(value) => {
            app.workbench.search.query_text = value;
            iced::Task::none()
        }
        SearchMessage::SetPathPrefixText(value) => {
            app.workbench.search.path_prefix_text = value;
            iced::Task::none()
        }
        SearchMessage::SubmitSearch => {
            if app.workbench.search.query_text.trim().is_empty() {
                app.workbench.search.validation_error = Some("Search query is required".into());
                return iced::Task::none();
            }

            let Some(repo_root) = app.selected_repository.clone() else {
                return iced::Task::none();
            };

            let rows = app
                .services
                .search
                .search(
                    repo_root,
                    app.workbench.branch_token.clone(),
                    app.workbench.overview.head_snapshot_id.clone(),
                    crate::services::SearchQuery {
                        query_text: app.workbench.search.query_text.trim().to_owned(),
                        path_prefix: (!app.workbench.search.path_prefix_text.trim().is_empty())
                            .then(|| app.workbench.search.path_prefix_text.trim().to_owned()),
                    },
                )
                .unwrap_or_default();

            app.workbench.search.validation_error = None;
            app.workbench.search.results = rows;
            iced::Task::none()
        }
    }
}

pub fn view_search(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, text, text_input};

    let results = if app.workbench.search.results.is_empty() {
        column![text("No search results yet.")]
    } else {
        app.workbench
            .search
            .results
            .iter()
            .fold(column![].spacing(8), |column, row| {
                column.push(text(format!("{} [{}]", row.path, row.source)))
            })
    };

    let content = {
        let base = column![
            text("Search").size(28),
            text_input("Filename or extension", &app.workbench.search.query_text)
                .on_input(SearchMessage::SetQueryText)
                .padding(10),
            text_input(
                "Optional path prefix",
                &app.workbench.search.path_prefix_text
            )
            .on_input(SearchMessage::SetPathPrefixText)
            .padding(10),
            button("Search").on_press(SearchMessage::SubmitSearch),
            results,
        ]
        .spacing(12);

        if let Some(error) = app.workbench.search.validation_error.as_ref() {
            base.push(text(error))
        } else {
            base
        }
    };

    let page: iced::Element<'_, SearchMessage> = container(content).padding(20).into();
    page.map(crate::domain::Message::from)
}
