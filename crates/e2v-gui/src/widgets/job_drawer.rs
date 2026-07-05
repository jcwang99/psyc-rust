pub fn running_jobs(jobs: &[crate::domain::JobRecord]) -> usize {
    jobs.iter()
        .filter(|job| matches!(job.state, crate::domain::JobState::Running))
        .count()
}

pub fn view_job_drawer(
    jobs: &[crate::domain::JobRecord],
) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{column, container, text};

    let content = if jobs.is_empty() {
        column![
            text("Background jobs").size(20),
            text("No background jobs yet."),
        ]
        .spacing(8)
    } else {
        jobs.iter().fold(
            column![text(format!("Background jobs ({})", running_jobs(jobs))).size(20)].spacing(8),
            |column, job| {
                let state = match &job.state {
                    crate::domain::JobState::Running => "Running".to_owned(),
                    crate::domain::JobState::Succeeded => "Succeeded".to_owned(),
                    crate::domain::JobState::Failed(message) => format!("Failed: {message}"),
                };
                column.push(text(format!("{}: {}", job.label, state)))
            },
        )
    };

    container(content).padding(16).into()
}
