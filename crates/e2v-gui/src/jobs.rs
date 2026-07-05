pub fn spawn_blocking_job<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, crate::domain::AppError> + Send + 'static,
    map: impl FnOnce(Result<T, crate::domain::AppError>) -> crate::domain::Message + Send + 'static,
) -> iced::Task<crate::domain::Message> {
    iced::Task::perform(
        async move {
            match tokio::task::spawn_blocking(work).await {
                Ok(result) => result,
                Err(error) => Err(crate::domain::AppError::internal(format!(
                    "background job failed to join: {error}"
                ))),
            }
        },
        map,
    )
}
