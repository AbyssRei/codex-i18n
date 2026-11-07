#![windows_subsystem = "windows"]

#[cfg(not(windows))]
fn main() {
    eprintln!("此程序仅支持 Windows。")
}

#[cfg(windows)]
mod gui {
    use codex_i18n::{manager, platform};
    use gpui_kit::base::Disableable;
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::component::{ActiveTheme, Theme};
    use gpui_kit::prelude::FluentBuilder;
    use gpui_kit::*;
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    enum ResultMessage {
        Scan {
            package: Result<platform::Package, String>,
            current: Result<Option<manager::State>, String>,
            launch: bool,
        },
        Built(manager::State, bool),
        Launched,
        Recheck,
        RunningConflict(manager::State, bool),
        Shortcut(PathBuf),
    }
    enum Message {
        Progress(String, u8),
        Done(Result<ResultMessage, String>),
    }
    enum Job {
        Scan(bool),
        Build(bool),
        Launch(manager::State, bool, bool),
        Shortcut(Option<PathBuf>),
    }

    struct App {
        root: PathBuf,
        package: Option<platform::Package>,
        state: Option<manager::State>,
        package_error: Option<String>,
        state_error: Option<String>,
        status: String,
        progress: u8,
        busy: bool,
        building: bool,
        launch_after_build: bool,
        decision: bool,
        running_decision: Option<(manager::State, bool)>,
        exit_after_launch: bool,
        close_after_job: bool,
        cancel: Arc<AtomicBool>,
    }

    fn version(v: [u16; 4]) -> String {
        format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3])
    }

    impl App {
        fn request_close(&mut self) -> bool {
            if !self.busy {
                return true;
            }
            self.cancel.store(true, Ordering::Relaxed);
            self.close_after_job = true;
            self.status = "正在结束当前操作并清理暂存文件…".into();
            false
        }

        fn report_failure(&mut self, error: &str, launch_after_build: bool) {
            self.decision = launch_after_build && self.state.is_some();
            self.status = if self.decision {
                format!("操作未完成：{error}\n可以继续运行已有修复版。")
            } else {
                format!("操作未完成：{error}")
            };
        }

        fn new(cx: &mut Context<Self>, launch: bool, root: PathBuf) -> Self {
            let mut app = Self {
                root,
                package: None,
                state: None,
                package_error: None,
                state_error: None,
                status: "正在检查安装状态…".into(),
                progress: 0,
                busy: false,
                building: false,
                launch_after_build: false,
                decision: false,
                running_decision: None,
                exit_after_launch: launch,
                close_after_job: false,
                cancel: Arc::new(AtomicBool::new(false)),
            };
            app.start(Job::Scan(launch), cx);
            app
        }

        fn start(&mut self, job: Job, cx: &mut Context<Self>) {
            if self.busy {
                return;
            }
            self.busy = true;
            self.building = matches!(job, Job::Build(_));
            self.launch_after_build = matches!(job, Job::Build(true));
            self.decision = false;
            self.running_decision = None;
            self.cancel = Arc::new(AtomicBool::new(false));
            self.progress = 0;
            self.status = match job {
                Job::Scan(_) => "正在检查安装状态…",
                Job::Build(_) => "正在准备独立副本…",
                Job::Launch(_, _, _) => "正在验证并启动修复版…",
                Job::Shortcut(_) => "正在创建桌面快捷方式…",
            }
            .into();
            let root = self.root.clone();
            let cancel = self.cancel.clone();
            let (sender, receiver) = async_channel::bounded(32);
            let spawn = std::thread::Builder::new()
                .name("codex-i18n-worker".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || -> anyhow::Result<ResultMessage> {
                            Ok(match job {
                                Job::Scan(launch) => {
                                    let status = manager::status(&root);
                                    ResultMessage::Scan {
                                        package: status.package.map_err(|e| format!("{e:#}")),
                                        current: status.current.map_err(|e| format!("{e:#}")),
                                        launch,
                                    }
                                }
                                Job::Build(launch) => ResultMessage::Built(
                                    manager::build(&root, &cancel, |text, progress| {
                                        let _ = sender.try_send(Message::Progress(text, progress));
                                    })?,
                                    launch,
                                ),
                                Job::Launch(state, allow_stale, allow_running) => {
                                    match manager::launch(
                                        &root,
                                        &state,
                                        allow_stale,
                                        allow_running,
                                    )? {
                                        manager::LaunchOutcome::Started => ResultMessage::Launched,
                                        manager::LaunchOutcome::RunningConflict => {
                                            ResultMessage::RunningConflict(state, allow_stale)
                                        }
                                        manager::LaunchOutcome::Recheck => ResultMessage::Recheck,
                                    }
                                }
                                Job::Shortcut(icon) => ResultMessage::Shortcut(
                                    platform::create_shortcut(icon.as_deref())?,
                                ),
                            })
                        },
                    ));
                    let result = match result {
                        Ok(result) => result.map_err(|e| format!("{e:#}")),
                        Err(_) => Err("后台任务发生异常，操作已停止".into()),
                    };
                    let _ = sender.send_blocking(Message::Done(result));
                });
            if let Err(error) = spawn {
                self.busy = false;
                self.building = false;
                self.report_failure(
                    &format!("无法创建后台线程：{error}"),
                    self.launch_after_build,
                );
                self.launch_after_build = false;
                cx.notify();
                return;
            }
            cx.spawn(async move |this, cx| {
                while let Ok(message) = receiver.recv().await {
                    let done = matches!(message, Message::Done(_));
                    if this.update(cx, |view, cx| {
                        match message {
                            Message::Progress(text, value) => { view.status = text; view.progress = value; }
                            Message::Done(result) => {
                                let launch_after_build = view.launch_after_build;
                                view.busy = false; view.building = false; view.launch_after_build = false;
                                if view.close_after_job { cx.quit(); return; }
                                match result {
                                    Err(error) => { view.report_failure(&error, launch_after_build); }
                                    Ok(ResultMessage::Scan { package, current, launch }) => {
                                        (view.package, view.package_error) = match package { Ok(p) => (Some(p), None), Err(e) => (None, Some(e)) };
                                        (view.state, view.state_error) = match current { Ok(s) => (s, None), Err(e) => (None, Some(e)) };
                                        if let Some(error) = &view.state_error { view.status = format!("无法读取修复版：{error}"); }
                                        else if launch && view.state.is_some() {
                                            let same = manager::launch_decision(view.package.as_ref(), view.state.as_ref()) == manager::LaunchDecision::Current;
                                            if same { view.start(Job::Launch(view.state.clone().unwrap(), false, false), cx); }
                                            else { view.decision = true; view.status = if let Some(e) = &view.package_error { format!("无法检查 Store 版本：{e}\n可以继续运行现有修复版。") } else { "检测到 Store 版本与修复版不同。".into() }; }
                                        } else if launch { view.status = "尚未创建修复版，请先生成。".into(); }
                                        else if let Some(e) = &view.package_error { view.status = format!("无法检查 Store 版本：{e}"); }
                                        else { view.status = "选择操作。修复版与正式版可能共用用户数据，请避免同时运行。".into(); }
                                    }
                                    Ok(ResultMessage::Built(state, launch)) => {
                                        view.state = Some(state.clone()); view.progress = 100; view.status = "修复版已生成。可创建快捷方式或启动。".into();
                                        if launch { view.start(Job::Launch(state, false, false), cx); } else { view.start(Job::Scan(false), cx); }
                                    }
                                    Ok(ResultMessage::Launched) => { view.status = "修复版已启动。".into(); if view.exit_after_launch { cx.quit(); } }
                                    Ok(ResultMessage::Recheck) => { view.start(Job::Scan(true), cx); }
                                    Ok(ResultMessage::RunningConflict(state, allow_stale)) => { view.running_decision = Some((state, allow_stale)); view.status = "检测到 ChatGPT.exe 正在运行。若两个程序共用用户数据，可能发生冲突；请先退出已有程序，或自行选择仍然启动。".into(); }
                                    Ok(ResultMessage::Shortcut(path)) => { view.status = format!("快捷方式已创建：{}", path.display()); }
                                }
                            }
                        }
                        cx.notify();
                    }).is_err() || done { break; }
                }
            }).detach();
            cx.notify();
        }
    }
    impl Drop for App {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn install_close_handler(window: &Window, cx: &gpui_kit::App, entity: &Entity<App>) {
        let weak = entity.downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let mut allow = true;
            let _ = weak.update(cx, |view, cx| {
                allow = view.request_close();
                if !allow {
                    cx.notify();
                }
            });
            allow
        });
    }

    impl Render for App {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let installed = self
                .package
                .as_ref()
                .map(|p| version(p.version))
                .unwrap_or_else(|| "无法读取".into());
            let current = self
                .state
                .as_ref()
                .map(|s| version(s.version))
                .unwrap_or_else(|| "尚未创建".into());
            let busy = self.busy;
            let can_launch = self.state.is_some();
            let can_build = self.package.is_some();
            div()
                .flex()
                .flex_col()
                .size_full()
                .p_6()
                .gap_3()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(div().text_xl().child("Codex i18n 管理器"))
                .child(format!("Store 版本：{installed}    修复版：{current}"))
                .child(format!("保存位置：{}", self.root.display()))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            Button::new("build")
                                .primary()
                                .label("创建 / 更新修复版")
                                .disabled(busy || !can_build)
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.start(Job::Build(false), cx)),
                                ),
                        )
                        .child(
                            Button::new("shortcut")
                                .label("创建桌面快捷方式")
                                .disabled(busy)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    let icon = view
                                        .state
                                        .as_ref()
                                        .map(|s| {
                                            manager::app_path(&view.root, s).join("ChatGPT.exe")
                                        })
                                        .or_else(|| {
                                            view.package
                                                .as_ref()
                                                .map(|p| p.app_dir.join("ChatGPT.exe"))
                                        });
                                    view.start(Job::Shortcut(icon), cx);
                                })),
                        )
                        .child(
                            Button::new("launch")
                                .label("启动修复版")
                                .disabled(busy || !can_launch)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.exit_after_launch = false;
                                    view.start(Job::Scan(true), cx);
                                })),
                        ),
                )
                .when(self.decision, |this| {
                    this.child(
                        div()
                            .flex()
                            .gap_3()
                            .child(
                                Button::new("update-launch")
                                    .primary()
                                    .label("重新生成并启动")
                                    .disabled(!can_build)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.start(Job::Build(true), cx)
                                    })),
                            )
                            .child(
                                Button::new("continue")
                                    .label("继续运行现有修复版")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        if let Some(state) = view.state.clone() {
                                            view.start(Job::Launch(state, true, false), cx);
                                        }
                                    })),
                            )
                            .child(Button::new("dismiss").label("取消").on_click(cx.listener(
                                |view, _, _, cx| {
                                    view.decision = false;
                                    view.status = "已取消启动。".into();
                                    cx.notify();
                                },
                            ))),
                    )
                })
                .when(self.running_decision.is_some(), |this| {
                    this.child(
                        div()
                            .flex()
                            .gap_3()
                            .child(Button::new("force-launch").label("仍然启动").on_click(
                                cx.listener(|view, _, _, cx| {
                                    if let Some((state, allow_stale)) = view.running_decision.take()
                                    {
                                        view.start(Job::Launch(state, allow_stale, true), cx);
                                    }
                                }),
                            ))
                            .child(Button::new("cancel-running").label("取消").on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.running_decision = None;
                                    view.status = "已取消启动。".into();
                                    cx.notify();
                                }),
                            )),
                    )
                })
                .when(self.building, |this| {
                    this.child(
                        Button::new("cancel")
                            .label("取消生成")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.cancel.store(true, Ordering::Relaxed);
                                view.status = "正在取消，请稍候…".into();
                                cx.notify();
                            })),
                    )
                })
                .child(
                    div().w_full().h(px(10.)).bg(cx.theme().muted).child(
                        div()
                            .w(relative(self.progress as f32 / 100.))
                            .h_full()
                            .bg(cx.theme().primary),
                    ),
                )
                .child(self.status.clone())
                .child("提示：修补后的副本签名失效，且不具有 Store 包身份。")
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let root = manager::root()?;
        let launch = std::env::args_os().skip(1).any(|a| a == "--launch");
        application().with_assets(assets::Assets).run(move |cx| {
            init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(760.), px(460.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Codex i18n 管理器".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            open_window(options, cx, move |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                let entity = cx.new(|cx| {
                    cx.observe_window_appearance(window, |_, window, cx| {
                        Theme::sync_system_appearance(Some(window), cx)
                    })
                    .detach();
                    App::new(cx, launch, root)
                });
                install_close_handler(window, cx, &entity);
                entity
            })
            .expect("无法创建窗口");
        });
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{App, manager, platform};
        use std::path::PathBuf;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        fn fixture_state() -> manager::State {
            manager::State {
                schema: 1,
                source_full_name: "old-package".into(),
                version: [1, 2, 3, 4],
                directory: "version-test".into(),
                exe_sha256: "x".into(),
                asar_sha256: "y".into(),
            }
        }

        fn fixture_app() -> App {
            App {
                root: PathBuf::from("fixture"),
                package: Some(platform::Package {
                    full_name: "new-package".into(),
                    version: [1, 2, 3, 5],
                    app_dir: PathBuf::from("source"),
                }),
                state: Some(fixture_state()),
                package_error: None,
                state_error: None,
                status: "检测到 Store 版本与修复版不同。".into(),
                progress: 0,
                busy: false,
                building: false,
                launch_after_build: false,
                decision: true,
                running_decision: None,
                exit_after_launch: false,
                close_after_job: false,
                cancel: Arc::new(AtomicBool::new(false)),
            }
        }

        #[test]
        fn closing_a_busy_window_requests_cancellation() {
            let mut view = fixture_app();
            view.busy = true;
            view.building = true;
            assert!(!view.request_close());
            assert!(view.cancel.load(Ordering::Relaxed));
            assert!(view.close_after_job);
            view.busy = false;
            assert!(view.request_close());
        }

        #[test]
        fn failed_update_keeps_continue_choice_only_with_an_existing_copy() {
            let mut view = fixture_app();
            view.report_failure("写入失败", true);
            assert!(view.decision);
            assert!(view.status.contains("继续运行已有修复版"));
            view.state = None;
            view.report_failure("写入失败", true);
            assert!(!view.decision);
            assert!(!view.status.contains("继续运行已有修复版"));
            view.state = Some(fixture_state());
            view.report_failure("写入失败", false);
            assert!(!view.decision);
        }
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = gui::run() {
        use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        use windows::core::{PCWSTR, w};
        let message: Vec<u16> = format!("{error:#}").encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(message.as_ptr()),
                w!("Codex i18n 管理器"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}
