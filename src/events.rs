pub(crate) fn events_allowed(
    method: &str,
    override_events: Option<&[String]>,
    default_events: Option<&[String]>,
) -> bool {
    match override_events.or(default_events) {
        Some(events) => events.iter().any(|value| value == "*" || value == method),
        None => true,
    }
}
