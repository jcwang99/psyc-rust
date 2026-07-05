#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchNavItem {
    pub label: &'static str,
    pub page: crate::domain::WorkbenchPage,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchShellModel {
    pub repo_title: String,
    pub nav_items: Vec<WorkbenchNavItem>,
}

pub fn build_workbench_shell_model(app: &crate::app::PsycGuiApp) -> WorkbenchShellModel {
    let active_page = app.workbench.active_page;
    let nav = [
        (crate::domain::WorkbenchPage::Overview, "Overview"),
        (crate::domain::WorkbenchPage::History, "History"),
        (crate::domain::WorkbenchPage::Branches, "Branches"),
        (crate::domain::WorkbenchPage::Sync, "Sync"),
        (crate::domain::WorkbenchPage::Search, "Search"),
        (crate::domain::WorkbenchPage::Sharing, "Sharing"),
        (crate::domain::WorkbenchPage::Preview, "Preview"),
        (crate::domain::WorkbenchPage::Advanced, "Advanced"),
    ];

    WorkbenchShellModel {
        repo_title: app
            .selected_repository
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No repository selected".into()),
        nav_items: nav
            .into_iter()
            .map(|(page, label)| WorkbenchNavItem {
                label,
                page,
                active: page == active_page,
            })
            .collect(),
    }
}

pub fn view_workbench(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text};

    let model = build_workbench_shell_model(app);
    let nav =
        model.nav_items.into_iter().fold(
            column![text(model.repo_title).size(24)].spacing(10),
            |column, item| {
                let label = if item.active {
                    format!("> {}", item.label)
                } else {
                    item.label.to_owned()
                };
                column.push(button(text(label)).on_press(
                    crate::pages::workbench::WorkbenchMessage::SelectPage(item.page).into(),
                ))
            },
        );

    let body = crate::pages::workbench::view_active_page(app);
    let jobs = crate::widgets::job_drawer::view_job_drawer(&app.jobs);
    let main_content = {
        let base = column![body, jobs].spacing(16);
        if let Some(sheet) =
            crate::widgets::confirmation_sheet::view_confirmation_sheet(&app.pending_confirmation)
        {
            base.push(sheet)
        } else {
            base
        }
    };

    container(
        row![
            container(nav).width(220).padding(16),
            container(main_content).width(iced::Length::Fill),
        ]
        .spacing(20),
    )
    .padding(20)
    .into()
}
