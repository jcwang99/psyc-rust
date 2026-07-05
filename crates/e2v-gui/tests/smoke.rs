use e2v_gui::{Screen, boot};

#[test]
fn boot_starts_on_home_screen_without_a_selected_repository() {
    let (app, task) = boot();

    assert_eq!(app.screen, Screen::Home);
    assert!(app.selected_repository.is_none());
    assert_eq!(task.units(), 0);
}
