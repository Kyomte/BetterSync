//! The tray icon and its menu (Windows and macOS).

use std::time::Duration;

use anyhow::Result;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::status::{Api, Status, TrayState, config_dir, open, start_daemon};
use crate::{autostart, icons};

/// How often the daemon is asked for its status. A local HTTP request
/// every few seconds costs next to nothing.
const POLL: Duration = Duration::from_secs(3);

enum UserEvent {
    Status(Option<Status>),
    Menu(MenuEvent),
}

struct Ui {
    tray: TrayIcon,
    summary: MenuItem,
    folders: Submenu,
    folder_items: Vec<(MenuId, String)>,
    pause: MenuItem,
    window: MenuItem,
    login: CheckMenuItem,
    quit: MenuItem,
    state: TrayState,
    last: Option<Status>,
}

fn icon(state: TrayState) -> Option<Icon> {
    Icon::from_rgba(icons::rgba(state), icons::SIZE, icons::SIZE).ok()
}

impl Ui {
    fn new() -> Result<Self> {
        let summary = MenuItem::new("Starting…", false, None);
        let folders = Submenu::new("Open folder", false);
        let pause = MenuItem::new("Pause syncing", false, None);
        let window = MenuItem::new("Status and devices…", true, None);
        let login = CheckMenuItem::new("Start at login", true, autostart::is_enabled(), None);
        let quit = MenuItem::new("Quit BetterSync", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &summary,
            &PredefinedMenuItem::separator(),
            &folders,
            &pause,
            &window,
            &PredefinedMenuItem::separator(),
            &login,
            &quit,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("BetterSync")
            .with_icon(icon(TrayState::Offline).ok_or_else(|| anyhow::anyhow!("bad icon"))?)
            .build()?;
        Ok(Self {
            tray,
            summary,
            folders,
            folder_items: Vec::new(),
            pause,
            window,
            login,
            quit,
            state: TrayState::Offline,
            last: None,
        })
    }

    fn update(&mut self, status: Option<Status>) {
        let state = status
            .as_ref()
            .map_or(TrayState::Offline, Status::tray_state);
        if state != self.state {
            let _ = self.tray.set_icon(icon(state));
            self.state = state;
        }
        let text = status
            .as_ref()
            .map_or_else(|| "BetterSync is not running".to_owned(), Status::summary);
        self.summary.set_text(&text);
        let _ = self.tray.set_tooltip(Some(format!("BetterSync: {text}")));
        self.pause.set_enabled(status.is_some());
        self.pause
            .set_text(if status.as_ref().is_some_and(Status::all_paused) {
                "Resume syncing"
            } else {
                "Pause syncing"
            });

        // Rebuild the folder list only when it changes.
        let wanted: Vec<(String, String)> = status
            .as_ref()
            .map(|s| {
                s.folders
                    .iter()
                    .map(|f| (f.id.clone(), f.path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let current: Vec<(String, String)> = self
            .last
            .as_ref()
            .map(|s| {
                s.folders
                    .iter()
                    .map(|f| (f.id.clone(), f.path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if wanted != current || (self.folder_items.is_empty() && !wanted.is_empty()) {
            while self.folders.remove_at(0).is_some() {}
            self.folder_items.clear();
            for (id, path) in &wanted {
                let item = MenuItem::new(id, true, None);
                let _ = self.folders.append(&item);
                self.folder_items.push((item.id().clone(), path.clone()));
            }
            self.folders.set_enabled(!wanted.is_empty());
        }
        self.last = status;
    }

    /// Returns false to quit.
    fn on_menu(&mut self, e: &MenuEvent) -> bool {
        let api = Api::locate(&config_dir());
        if e.id == self.quit.id() {
            return false;
        }
        if e.id == self.window.id() {
            match &api {
                Some(api) => {
                    let _ = open(&api.status_page_url());
                }
                None => {
                    let _ = start_daemon();
                }
            }
        } else if e.id == self.pause.id() {
            if let (Some(api), Some(last)) = (&api, &self.last) {
                let action = if last.all_paused() { "resume" } else { "pause" };
                for f in &last.folders {
                    let _ = api.post(&format!("/api/folders/{}/{action}", f.id));
                }
            }
        } else if e.id == self.login.id() {
            let on = self.login.is_checked();
            if autostart::set_enabled(on).is_err() {
                self.login.set_checked(!on);
            }
        } else if let Some((_, path)) = self.folder_items.iter().find(|(id, _)| *id == e.id) {
            let _ = open(path);
        }
        true
    }
}

pub fn run() -> Result<()> {
    if Api::locate(&config_dir())
        .and_then(|a| a.status().ok())
        .is_none()
    {
        // Not running yet (e.g. at login): start it.
        let _ = start_daemon();
    }
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        loop {
            let status = Api::locate(&config_dir()).and_then(|a| a.status().ok());
            if proxy.send_event(UserEvent::Status(status)).is_err() {
                return;
            }
            std::thread::sleep(POLL);
        }
    });

    let mut ui: Option<Ui> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS requires the tray to be created once the loop runs.
            Event::NewEvents(StartCause::Init) => match Ui::new() {
                Ok(u) => ui = Some(u),
                Err(e) => {
                    eprintln!("cannot create the tray icon: {e}");
                    *control_flow = ControlFlow::Exit;
                }
            },
            Event::UserEvent(UserEvent::Status(s)) => {
                if let Some(u) = ui.as_mut() {
                    u.update(s);
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if let Some(u) = ui.as_mut()
                    && !u.on_menu(&e)
                {
                    ui = None; // drops the tray icon
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}
