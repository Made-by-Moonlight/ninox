pub fn session_detail<'a>(
    app: &'a crate::app::App,
    session_id: &str,
) -> iced::Element<'a, crate::app::Message> {
    match app.sessions.get(session_id) {
        Some(s) => iced::widget::text(format!("Session: {} — terminal in M4", s.name)).into(),
        None => iced::widget::text("Session not found").into(),
    }
}
