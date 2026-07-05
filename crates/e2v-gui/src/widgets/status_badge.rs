pub fn view_status_badge<Message: 'static>(
    label: impl Into<String>,
) -> iced::Element<'static, Message> {
    use iced::widget::{container, text};

    container(text(label.into()).size(14))
        .padding([4, 8])
        .into()
}
