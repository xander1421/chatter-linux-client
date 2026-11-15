//! Window-local history for desktop menu navigation. Store destinations only:
//! forms, confirmations and account recovery screens are never replayed.
use std::{cell::RefCell, rc::Rc};

use slint::{ComponentHandle, SharedString};

use crate::{MainWindow, SettingsPane, SidebarPane};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    tab: SharedString,
    screens: Vec<SharedString>,
    settings_section: SharedString,
}

impl Route {
    fn read(w: &MainWindow) -> Option<Self> {
        if w.get_current_view() != "main" || w.global::<crate::AppLockPane>().get_locked() {
            return None;
        }
        let screens = super::stack(w);
        if screens.iter().any(|screen| {
            !matches!(
                screen.as_str(),
                "settings" | "sessions" | "keyboard-shortcuts" | "server-records" | "activity"
            )
        }) {
            return None;
        }
        let settings_section = if screens.iter().any(|s| s == "settings") {
            w.global::<SettingsPane>().get_navigation_section()
        } else {
            SharedString::default()
        };
        Some(Self {
            tab: w.global::<SidebarPane>().get_active_tab(),
            screens,
            settings_section,
        })
    }

    fn restore(&self, w: &MainWindow) {
        w.global::<SidebarPane>().set_active_tab(self.tab.clone());
        if !self.settings_section.is_empty() {
            w.global::<SettingsPane>()
                .set_navigation_section(self.settings_section.clone());
        }
        super::set_stack(w, self.screens.clone());
    }
}

#[derive(Default)]
struct History {
    current: Option<Route>,
    past: Vec<Route>,
    future: Vec<Route>,
}

impl History {
    fn observe(&mut self, route: Option<Route>) {
        if route == self.current {
            return;
        }
        if route.is_none() {
            *self = Self::default();
            return;
        }
        if let Some(previous) = self.current.take() {
            self.past.push(previous);
            if self.past.len() > 64 {
                self.past.remove(0);
            }
        }
        self.future.clear();
        self.current = route;
    }

    fn travel(&mut self, forward: bool) -> Option<Route> {
        let current = self.current.clone()?;
        let (source, destination) = if forward {
            (&mut self.future, &mut self.past)
        } else {
            (&mut self.past, &mut self.future)
        };
        let target = source.pop()?;
        destination.push(current);
        self.current = Some(target.clone());
        Some(target)
    }
}

pub(super) fn wire(w: &MainWindow) {
    let history = Rc::new(RefCell::new(History::default()));
    history.borrow_mut().observe(Route::read(w));
    let weak = w.as_weak();
    let observed = history.clone();
    w.on_navigation_changed(move || {
        if let Some(w) = weak.upgrade() {
            observed.borrow_mut().observe(Route::read(&w));
        }
    });
    let weak = w.as_weak();
    w.on_mouse_navigation(move |forward| {
        let Some(w) = weak.upgrade() else { return };
        if w.get_is_mobile_platform() {
            return;
        }
        let before = Route::read(&w);
        history.borrow_mut().observe(before.clone());
        if before.is_none() {
            if !forward {
                super::handle_back(&w);
            }
            return;
        }
        if !forward {
            // Let Back dismiss transient UI first. A popup closing leaves
            // the route unchanged, and must not also navigate its owner.
            let consumed = super::handle_back(&w);
            if consumed && Route::read(&w) == before {
                return;
            }
        }
        let target = history.borrow_mut().travel(forward);
        if let Some(target) = target {
            target.restore(&w);
        } else if !forward && Route::read(&w) != before {
            // Back may leave an initial destination with no recorded parent.
            // Keep that departure available to Forward, never to Back again.
            let mut history = history.borrow_mut();
            history.current = Route::read(&w);
            if let Some(previous) = before {
                history.future.push(previous);
            }
        } else {
            history.borrow_mut().observe(Route::read(&w));
        }
    });
}
