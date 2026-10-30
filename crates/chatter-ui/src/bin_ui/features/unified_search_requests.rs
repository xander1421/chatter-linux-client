//! UI request ownership for unified search. No backend or platform effects.
//! Completions publish only while their query, palette lifetime and session
//! still own the row; selectable identities are committed with the paint.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    generation: u64,
    person_generation: u64,
    session: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PersonRow {
    pub id: String,
    pub handle: String,
    pub name: String,
    pub status: String,
    pub error: String,
}

#[derive(Default)]
pub(super) struct SearchRequests {
    generation: u64,
    person_generation: u64,
    session: u64,
    query: String,
    messages: Vec<String>,
    person: Option<PersonRow>,
}

impl SearchRequests {
    pub fn same_query(&self, query: &str) -> bool {
        self.query == query
    }

    /// Every edit (including clear), open and close replaces the request owner.
    pub fn invalidate(&mut self, session: u64, query: &str) -> Ticket {
        self.generation = self.generation.wrapping_add(1);
        self.person_generation = self.person_generation.wrapping_add(1);
        self.session = session;
        self.query = query.to_owned();
        self.messages.clear();
        self.person = None;
        self.ticket()
    }

    pub fn begin_person(&mut self, session: u64, query: &str) -> Ticket {
        if self.session != session || self.query != query {
            self.invalidate(session, query);
        }
        self.person_generation = self.person_generation.wrapping_add(1);
        self.person = None;
        self.ticket()
    }

    fn ticket(&self) -> Ticket {
        Ticket {
            generation: self.generation,
            person_generation: self.person_generation,
            session: self.session,
        }
    }

    fn current(&self, ticket: Ticket, session: u64, open: bool) -> bool {
        open && self.session == session
            && ticket.session == session
            && ticket.generation == self.generation
    }

    /// A debounce worker checks ownership before touching the local index.
    pub fn pending(&self, ticket: Ticket, session: u64) -> bool {
        self.current(ticket, session, true)
    }

    pub fn complete_messages(
        &mut self,
        ticket: Ticket,
        session: u64,
        open: bool,
        ids: Vec<String>,
        paint: impl FnOnce(),
    ) -> bool {
        if !self.current(ticket, session, open) {
            return false;
        }
        self.messages = ids;
        paint();
        true
    }

    pub fn complete_person(
        &mut self,
        ticket: Ticket,
        session: u64,
        open: bool,
        row: PersonRow,
        paint: impl FnOnce(&PersonRow),
    ) -> bool {
        if !self.current(ticket, session, open)
            || ticket.person_generation != self.person_generation
        {
            return false;
        }
        self.person = Some(row);
        paint(self.person.as_ref().unwrap());
        true
    }

    pub fn session_current(&self, session: u64, open: bool) -> bool {
        open && self.session == session
    }

    pub fn message_selectable(&self, id: &str, session: u64, open: bool) -> bool {
        self.session_current(session, open) && self.messages.iter().any(|row| row == id)
    }

    pub fn person_selectable(&self, id: &str, session: u64, open: bool) -> bool {
        open && self.session == session
            && self
                .person
                .as_ref()
                .is_some_and(|row| row.status == "found" && !row.id.is_empty() && row.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(id: &str, handle: &str) -> PersonRow {
        PersonRow {
            id: id.into(),
            handle: handle.into(),
            name: format!("Name {handle}"),
            status: "found".into(),
            error: String::new(),
        }
    }

    #[test]
    fn messages_b_then_a_only_b_paints_and_controls_spinner() {
        let mut state = SearchRequests::default();
        let a = state.invalidate(7, "A");
        let b = state.invalidate(7, "B");
        let mut painted = Vec::new();
        let mut searching = true;
        assert!(!state.complete_messages(a, 7, true, vec!["a".into()], || searching = false));
        assert!(searching, "late A must not clear B's spinner");
        assert!(state.complete_messages(b, 7, true, vec!["b".into()], || {
            painted.push("B");
            searching = false;
        }));
        assert!(!state.complete_messages(a, 7, true, vec!["a".into()], || painted.push("A")));
        assert_eq!(painted, ["B"]);
        assert!(!searching);
        assert!(state.message_selectable("b", 7, true));
        assert!(!state.message_selectable("a", 7, true));
    }

    #[test]
    fn clear_discards_pending_messages_and_disables_navigation() {
        let mut state = SearchRequests::default();
        let a = state.invalidate(7, "A");
        assert!(state.complete_messages(a, 7, true, vec!["a".into()], || {}));
        let pending = state.invalidate(7, "A again");
        assert!(
            !state.message_selectable("a", 7, true),
            "editing clears selectable results"
        );
        state.invalidate(7, "");
        assert!(
            !state.complete_messages(pending, 7, true, vec!["a".into()], || panic!(
                "cleared search painted"
            ))
        );
        assert!(!state.message_selectable("a", 7, true));
    }

    #[test]
    fn person_a_late_after_edit_b_cannot_paint_or_select_a() {
        let mut state = SearchRequests::default();
        state.invalidate(7, "@A");
        let a = state.begin_person(7, "@A");
        state.invalidate(7, "@B");
        assert!(
            !state.complete_person(a, 7, true, person("account-a", "A"), |_| panic!(
                "A painted under B"
            ))
        );
        assert!(state.person.is_none());
        assert!(!state.person_selectable("account-a", 7, true));
        let b = state.begin_person(7, "@B");
        assert!(
            state.complete_person(b, 7, true, person("account-b", "B"), |row| {
                assert_eq!(
                    (&*row.id, &*row.handle, &*row.status),
                    ("account-b", "B", "found")
                );
            })
        );
        assert!(state.person_selectable("account-b", 7, true));
        assert!(!state.person_selectable("account-a", 7, true));
        state.invalidate(7, "");
        assert!(!state.person_selectable("account-b", 7, true));
        assert!(
            !state.complete_person(b, 7, true, person("account-b", "B"), |_| panic!(
                "clear repainted person"
            ))
        );
    }

    #[test]
    fn open_close_reopen_and_session_change_discard_both_completions() {
        for lifecycle in ["close", "reopen", "session"] {
            let mut state = SearchRequests::default();
            let message = state.invalidate(7, "A");
            let person_ticket = state.begin_person(7, "A");
            let (session, open) = match lifecycle {
                "close" => {
                    state.invalidate(7, "");
                    (7, false)
                }
                "reopen" => {
                    state.invalidate(7, "");
                    state.invalidate(7, "A");
                    (7, true)
                }
                _ => (8, true),
            };
            assert!(
                !state.complete_messages(message, session, open, vec!["a".into()], || panic!(
                    "stale message"
                ))
            );
            assert!(!state.complete_person(
                person_ticket,
                session,
                open,
                person("a", "A"),
                |_| panic!("stale person")
            ));
            assert!(!state.message_selectable("a", session, open));
            assert!(!state.person_selectable("a", session, open));
        }
    }

    #[test]
    fn repeated_lookup_latest_wins_and_resolved_link_handle_is_atomic() {
        let mut state = SearchRequests::default();
        state.invalidate(7, "chatter://u/test");
        let old = state.begin_person(7, "chatter://u/test");
        let current = state.begin_person(7, "chatter://u/test");
        assert!(
            !state.complete_person(old, 7, true, person("old", "Old"), |_| panic!("old lookup"))
        );
        assert!(
            state.complete_person(current, 7, true, person("owner", "resolved.123"), |row| {
                assert_eq!((&*row.id, &*row.handle), ("owner", "resolved.123"));
            })
        );
        assert!(!state.person_selectable("owner", 8, true));
        assert!(!state.person_selectable("owner", 7, false));
    }
}
