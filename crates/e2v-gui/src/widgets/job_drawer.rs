pub fn running_jobs(jobs: &[crate::domain::JobRecord]) -> usize {
    jobs.iter()
        .filter(|job| matches!(job.state, crate::domain::JobState::Running))
        .count()
}
