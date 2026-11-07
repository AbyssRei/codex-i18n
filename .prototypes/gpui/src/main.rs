#![windows_subsystem = "windows"]

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::*;

#[derive(Default)]
struct Demo {
    progress: u8,
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_6()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_xl().child("Codex i18n 管理器 · GPUI Kit 原型"))
            .child("这是界面演示，不会修改 Codex 或创建文件。")
            .child("Store 版本：26.1103.4239.0（示例）")
            .child("修复版状态：尚未创建（示例）")
            .child(
                Button::new("create")
                    .primary()
                    .label("模拟创建修复版")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.progress = (this.progress + 25).min(100);
                        cx.notify();
                    })),
            )
            .child(Button::new("reset").label("重置演示").on_click(cx.listener(
                |this, _, _, cx| {
                    this.progress = 0;
                    cx.notify();
                },
            )))
            .child(format!("模拟进度：{}%", self.progress))
            .child(
                div().w(px(300.0)).h(px(12.0)).bg(cx.theme().muted).child(
                    div()
                        .w(px(self.progress as f32 * 3.0))
                        .h_full()
                        .bg(cx.theme().primary),
                ),
            )
    }
}

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(560.0), px(320.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Codex i18n - GPUI Kit 原型".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| {
                cx.observe_window_appearance(window, |_, window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                })
                .detach();
                Demo::default()
            })
        })
        .expect("无法打开窗口");
    });
}
