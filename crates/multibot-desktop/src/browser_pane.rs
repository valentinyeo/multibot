use crate::app::MultibotView;
use gpui_kit::{Context, Window};

#[cfg(windows)]
#[path = "browser_pane_windows.rs"]
pub(crate) mod native;

#[cfg(windows)]
mod windows {
    use super::{Context, MultibotView, Window, native};
    use crate::{
        computer_view::normalise_user_input,
        state::{ActiveModal, MainSurface},
        theme,
    };
    use gpui_kit::{
        AnyElement, AppContext, Entity, InteractiveElement, IntoElement, Keystroke, ParentElement,
        Styled, Subscription, Task,
        component::{
            Sizable,
            button::Button,
            h_flex,
            input::{Input, InputEvent, InputState},
            v_flex,
        },
        div, px, relative,
    };
    use multibot_core::browser_pane::{BrowserPaneRect, BrowserPaneState, DEFAULT_URL};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::sync::{mpsc, oneshot};

    struct AddressBar {
        key: String,
        input: Entity<InputState>,
        _submit: Subscription,
    }

    enum Tab {
        Starting(u64, native::Browser),
        Ready(u64, native::Browser),
        Stopping(u64),
        Failed(String),
    }

    impl Tab {
        fn generation(&self) -> Option<u64> {
            match self {
                Self::Starting(id, _) | Self::Ready(id, _) | Self::Stopping(id) => Some(*id),
                Self::Failed(_) => None,
            }
        }
    }

    #[derive(Default)]
    pub(crate) struct DesktopBrowserPane {
        model: BrowserPaneState,
        tabs: HashMap<String, Tab>,
        shortcuts: Option<native::Shortcuts>,
        clock: Option<Instant>,
        shown: Option<String>,
        focus_on_show: Option<String>,
        address: Option<AddressBar>,
        hibernation: Option<(u64, Task<()>)>,
        generation: u64,
        capture_id: Option<u64>,
        captures: Option<Arc<tempfile::TempDir>>,
    }

    impl DesktopBrowserPane {
        fn now_ms(&self) -> u64 {
            self.clock
                .map_or(0, |clock| clock.elapsed().as_millis() as u64)
        }

        fn place(&mut self, key: &str, rect: Option<BrowserPaneRect>) {
            if let Some(Tab::Ready(_, browser)) = self.tabs.get_mut(key) {
                browser.place(rect);
                if let Some(shortcuts) = &self.shortcuts {
                    shortcuts.set_visible(browser.visible_window());
                }
            }
        }

        fn visible_window(&self, key: &str) -> Option<usize> {
            if self.shown.as_deref() != Some(key) {
                return None;
            }
            match self.tabs.get(key) {
                Some(Tab::Ready(_, browser)) => browser.visible_window(),
                _ => None,
            }
        }

        fn stop_tab(&mut self, key: &str) {
            if let Some(tab) = self.tabs.get_mut(key) {
                if let Some(id) = tab.generation() {
                    *tab = Tab::Stopping(id);
                } else {
                    self.tabs.remove(key);
                }
            }
        }

        fn close_all(&mut self) {
            // Keep exit notifications for profiles whose processes are still shutting
            // down, so an immediate reopen cannot race Chromium's profile lock.
            self.tabs.retain(|_, tab| {
                let Some(id) = tab.generation() else {
                    return false;
                };
                *tab = Tab::Stopping(id);
                true
            });
            self.shortcuts = None;
            self.shown = None;
            self.address = None;
            self.focus_on_show = None;
            self.hibernation = None;
            self.model.release_all_tabs();
        }
    }

    fn parent_handle(window: &Window) -> Option<usize> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
            _ => None,
        }
    }

    impl MultibotView {
        fn browser_session(&self) -> Option<String> {
            (self.main_surface == MainSurface::Chat
                && self.active_modal == ActiveModal::None
                && !self.table_modal_state.is_open()
                && !self.computer.is_open())
            .then(|| self.selected_session.as_ref())
            .flatten()
            .filter(|session| session.agent != multibot_proto::AgentKind::Shell)
            .map(|session| self.session_key_for(session))
        }

        pub(crate) fn toggle_browser_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let Some(key) = self.browser_session() else {
                return;
            };
            self.browser.clock.get_or_insert_with(Instant::now);
            if !self.browser.model.toggle(&key) {
                self.browser.close_all();
                self.focus_browser_composer(window, cx);
            }
            cx.notify();
        }

        fn focus_browser_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.browser.focus_on_show = None;
            if let Some(parent) = parent_handle(window) {
                native::focus(parent);
            }
            self.prompt_input
                .update(cx, |input, cx| input.focus(window, cx));
        }

        pub(crate) fn focus_browser_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let Some(key) = self.browser_session() else {
                return;
            };
            if parent_handle(window).is_some_and(native::has_focus) {
                self.focus_browser_composer(window, cx);
            } else if let Some(hwnd) = self.browser.visible_window(&key) {
                native::focus(hwnd);
            } else if self.browser.model.is_open(&key)
                && !matches!(self.browser.tabs.get(&key), Some(Tab::Failed(_)))
            {
                self.browser.focus_on_show = Some(key);
                cx.notify();
            } else {
                self.focus_browser_composer(window, cx);
            }
        }

        pub(crate) fn capture_browser_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let Some(key) = self.browser_session() else {
                return;
            };
            let Some(hwnd) = self.browser.visible_window(&key) else {
                return;
            };
            if self.browser.capture_id.is_some() {
                return;
            }
            let directory = match &self.browser.captures {
                Some(existing) => existing.clone(),
                None => match native::capture_directory() {
                    Ok(directory) => {
                        let directory = Arc::new(directory);
                        self.browser.captures = Some(directory.clone());
                        directory
                    }
                    Err(err) => {
                        self.notice = Some(format!("Could not save browser screenshot: {err}"));
                        cx.notify();
                        return;
                    }
                },
            };
            self.browser.generation += 1;
            let capture_id = self.browser.generation;
            self.browser.capture_id = Some(capture_id);
            let path = directory
                .path()
                .join(format!("browser-screenshot-{capture_id}.png"));
            let capture_path = path.clone();
            let generation = self.browser.tabs.get(&key).and_then(Tab::generation);
            let task = cx.background_executor().spawn(async move {
                let _directory = directory;
                native::capture(hwnd, &capture_path)
            });
            let weak = cx.entity().downgrade();
            window
                .spawn(cx, async move |cx| {
                    let result = task.await;
                    let mut staged = false;
                    let _ = cx.update(|window, cx| {
                        let _ = weak.update(cx, |view, cx| {
                            view.browser.capture_id = None;
                            if view.browser_session().as_deref() != Some(key.as_str())
                                || view.browser.tabs.get(&key).and_then(Tab::generation)
                                    != generation
                            {
                                return;
                            }
                            match result {
                                Ok(()) => {
                                    staged = view.stage_browser_screenshot(&path, window, cx);
                                    if staged {
                                        view.focus_browser_composer(window, cx);
                                    }
                                }
                                Err(err) => {
                                    view.notice = Some(format!(
                                        "Could not capture browser screenshot: {err}"
                                    ));
                                    cx.notify();
                                }
                            }
                        });
                    });
                    if !staged {
                        let _ = std::fs::remove_file(path);
                    }
                })
                .detach();
        }

        fn submit_browser_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let Some(key) = self.browser_session() else {
                return;
            };
            let Some(address) = self
                .browser
                .address
                .as_ref()
                .filter(|address| address.key == key)
            else {
                return;
            };
            let value = address.input.read(cx).value();
            let url = if matches!(value.trim(), "" | DEFAULT_URL) {
                DEFAULT_URL.to_owned()
            } else {
                match normalise_user_input(value.as_ref()) {
                    Ok(url) => url.as_str().to_owned(),
                    Err(err) => {
                        self.notice = Some(err.message().to_owned());
                        cx.notify();
                        return;
                    }
                }
            };
            self.browser.model.set_url(&key, url);
            self.browser.place(&key, None);
            self.browser.stop_tab(&key);
            if !self.browser.tabs.contains_key(&key) {
                self.start_browser(key, window, cx);
            }
            cx.notify();
        }

        fn install_browser_shortcuts(
            &mut self,
            parent: usize,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> anyhow::Result<()> {
            if self.browser.shortcuts.is_some() {
                return Ok(());
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            self.browser.shortcuts = Some(native::Shortcuts::new(parent, tx)?);
            let weak = cx.entity().downgrade();
            window
                .spawn(cx, async move |cx| {
                    while let Some(chord) = rx.recv().await {
                        if cx
                            .update(|window, cx| {
                                let Some(entity) = weak.upgrade() else { return };
                                match chord {
                                    "alt-q" => entity.update(cx, |view, cx| {
                                        if view.browser_session().is_some() {
                                            view.focus_browser_composer(window, cx);
                                        }
                                    }),
                                    "ctrl-shift-s" => entity.update(cx, |view, cx| {
                                        view.capture_browser_pane(window, cx);
                                    }),
                                    _ => {
                                        native::focus(parent);
                                        entity.read(cx).root_focus_handle.clone().focus(window, cx);
                                        if let Ok(key) = Keystroke::parse(chord) {
                                            window.dispatch_keystroke(key, cx);
                                        }
                                    }
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            Ok(())
        }

        fn start_browser(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
            let result = (|| {
                let parent = parent_handle(window)
                    .ok_or_else(|| anyhow::anyhow!("No Windows window handle"))?;
                self.install_browser_shortcuts(parent, window, cx)?;
                let url = self
                    .browser
                    .model
                    .tab(&key)
                    .and_then(|tab| tab.url.as_deref())
                    .unwrap_or(DEFAULT_URL);
                native::Browser::launch(parent, &key, url)
            })();
            let (browser, process) = match result {
                Ok(result) => result,
                Err(err) => {
                    self.browser.tabs.insert(key, Tab::Failed(err.to_string()));
                    return;
                }
            };
            self.browser.generation += 1;
            let generation = self.browser.generation;
            self.browser
                .tabs
                .insert(key.clone(), Tab::Starting(generation, browser));
            let (ready_tx, ready_rx) = oneshot::channel();
            let (exit_tx, exit_rx) = oneshot::channel();
            let spawn = std::thread::Builder::new()
                .name("docked-browser".into())
                .spawn(move || {
                    let result = process.find_window();
                    if ready_tx.send(result).is_ok() {
                        process.wait();
                        let _ = exit_tx.send(());
                    }
                });
            if let Err(err) = spawn {
                self.browser.tabs.insert(key, Tab::Failed(err.to_string()));
                return;
            }
            cx.spawn(async move |this, cx| {
                if let Ok(result) = ready_rx.await {
                    let _ = this.update(cx, |view, cx| {
                        if !matches!(view.browser.tabs.get(&key), Some(Tab::Starting(id, _)) if *id == generation) {
                            return;
                        }
                        let Some(Tab::Starting(_, mut browser)) = view.browser.tabs.remove(&key) else { return };
                        let tab = match result.and_then(|hwnd| browser.attach(hwnd)) {
                            Ok(()) => Tab::Ready(generation, browser),
                            Err(err) => Tab::Failed(err.to_string()),
                        };
                        view.browser.tabs.insert(key.clone(), tab);
                        cx.notify();
                    });
                }
                if exit_rx.await.is_ok() {
                    let _ = this.update(cx, |view, cx| {
                        match view.browser.tabs.get(&key) {
                            Some(Tab::Stopping(id)) if *id == generation => {
                                view.browser.tabs.remove(&key);
                            }
                            Some(Tab::Ready(id, _)) if *id == generation => {
                                view.browser.place(&key, None);
                                view.browser.tabs.insert(key, Tab::Failed("Browser closed. Press Go to reopen.".into()));
                            }
                            _ => return,
                        }
                        cx.notify();
                    });
                }
            }).detach();
        }

        fn hibernate_browser_tabs(&mut self, cx: &mut Context<Self>) {
            let now = self.browser.now_ms();
            let expired = self
                .browser
                .model
                .hibernate_idle(now)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            for key in expired {
                self.browser.stop_tab(&key);
            }
            let next = self.browser.model.next_hibernation_ms();
            if self
                .browser
                .hibernation
                .as_ref()
                .map(|(deadline, _)| *deadline)
                == next
            {
                return;
            }
            self.browser.hibernation = next.map(|deadline| {
                let timer = cx
                    .background_executor()
                    .timer(Duration::from_millis(deadline.saturating_sub(now)));
                let task = cx.spawn(async move |this, cx| {
                    timer.await;
                    let _ = this.update(cx, |view, cx| {
                        view.browser.hibernation.take();
                        view.hibernate_browser_tabs(cx);
                        if view.browser.tabs.is_empty() {
                            view.browser.shortcuts = None;
                        }
                    });
                });
                (deadline, task)
            });
        }

        pub(crate) fn hide_browser_pane(&mut self) {
            self.browser.focus_on_show = None;
            if let Some(key) = self.browser.shown.as_ref().cloned() {
                self.browser.place(&key, None);
            }
        }

        pub(crate) fn sync_browser_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if self.browser.clock.is_none() {
                return;
            }
            let active = self
                .browser_session()
                .filter(|key| self.browser.model.is_open(key));
            if active != self.browser.shown {
                if self.browser.focus_on_show != active {
                    self.browser.focus_on_show = None;
                }
                if let Some(previous) = self.browser.shown.take() {
                    self.browser.place(&previous, None);
                }
                let now = self.browser.now_ms();
                self.browser.model.select_visible(active.as_deref(), now);
                self.browser.shown.clone_from(&active);
            }
            self.hibernate_browser_tabs(cx);
            let Some(key) = active else {
                if self.browser.tabs.is_empty() {
                    self.browser.shortcuts = None;
                }
                return;
            };
            if self
                .browser
                .address
                .as_ref()
                .is_none_or(|bar| bar.key != key)
            {
                let url = self
                    .browser
                    .model
                    .tab(&key)
                    .and_then(|tab| tab.url.as_deref())
                    .unwrap_or_default();
                let input = cx.new(|cx| {
                    let mut input = InputState::new(window, cx).placeholder("Enter a web address");
                    input.set_value(url, window, cx);
                    input
                });
                let submit = cx.subscribe_in(&input, window, |view, _, event, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.submit_browser_address(window, cx);
                    }
                });
                self.browser.address = Some(AddressBar {
                    key: key.clone(),
                    input,
                    _submit: submit,
                });
            }
            if !self.browser.tabs.contains_key(&key) {
                self.start_browser(key, window, cx);
            }
        }

        pub(crate) fn render_browser_pane(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
            let key = self.browser.shown.clone()?;
            let address = self.browser.address.as_ref()?;
            let status = match self.browser.tabs.get(&key) {
                Some(Tab::Ready(..)) => "",
                Some(Tab::Failed(error)) => error,
                _ => "Opening browser…",
            };
            let weak = cx.entity().downgrade();
            Some(
                v_flex()
                    .w(relative(BrowserPaneRect::split_width(
                        1.,
                        self.browser.model.split_ratio,
                    )))
                    .h_full()
                    .flex_shrink_0()
                    .min_w_0()
                    .bg(theme::bg_canvas())
                    .border_l_1()
                    .border_color(theme::border_subtle())
                    .child(
                        h_flex()
                            .w_full()
                            .h(px(44.))
                            .px_2()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&address.input).small()),
                            )
                            .child(Button::new("browser-go").label("Go").small().on_click(
                                cx.listener(|view, _, window, cx| {
                                    view.submit_browser_address(window, cx)
                                }),
                            ))
                            .child(
                                Button::new("browser-close")
                                    .label("Close")
                                    .small()
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.toggle_browser_pane(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .p_2()
                            .text_sm()
                            .text_color(theme::text_muted())
                            .child(status.to_owned()),
                    )
                    .on_children_prepainted(move |bounds, window, cx| {
                        let Some(body) = bounds.last() else { return };
                        let rect = BrowserPaneRect::from_bounds(
                            body.origin.x.as_f32(),
                            body.origin.y.as_f32(),
                            body.size.width.as_f32(),
                            body.size.height.as_f32(),
                            window.scale_factor(),
                        );
                        let _ = weak.update(cx, |view, _| {
                            if view.browser.shown.as_deref() == Some(key.as_str()) {
                                view.browser.place(&key, Some(rect));
                                if view.browser.focus_on_show.as_deref() == Some(key.as_str())
                                    && let Some(hwnd) = view.browser.visible_window(&key)
                                {
                                    view.browser.focus_on_show = None;
                                    native::focus(hwnd);
                                }
                            }
                        });
                    })
                    .id("docked-browser-pane")
                    .into_any_element(),
            )
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::DesktopBrowserPane;

#[cfg(not(windows))]
impl MultibotView {
    pub(crate) fn toggle_browser_pane(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.main_surface == crate::state::MainSurface::Chat
            && self.active_modal == crate::state::ActiveModal::None
            && !self.table_modal_state.is_open()
        {
            self.notice = Some("The docked Chrome and Edge pane requires Windows.".into());
            cx.notify();
        }
    }

    pub(crate) fn focus_browser_pane(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
    pub(crate) fn capture_browser_pane(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
}
