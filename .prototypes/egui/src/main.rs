#![windows_subsystem = "windows"]

use eframe::egui;

#[derive(Default)]
struct Demo {
    progress: u8,
}

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(12.0);
            ui.heading("Codex i18n 管理器 · egui 原型");
            ui.add_space(12.0);
            ui.label("这是界面演示，不会修改 Codex 或创建文件。");
            ui.label("Store 版本：26.1103.4239.0（示例）");
            ui.label("修复版状态：尚未创建（示例）");
            ui.add_space(12.0);
            if ui.button("模拟创建修复版").clicked() {
                self.progress = (self.progress + 25).min(100);
            }
            if ui.button("重置演示").clicked() {
                self.progress = 0;
            }
            ui.add_space(12.0);
            ui.add(
                egui::ProgressBar::new(self.progress as f32 / 100.0)
                    .text(format!("模拟进度：{}%", self.progress)),
            );
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Codex i18n - egui 原型",
        options,
        Box::new(|_| Ok(Box::new(Demo::default()))),
    )
}
