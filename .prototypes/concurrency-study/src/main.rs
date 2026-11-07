#![windows_subsystem = "windows"]

use gpui_kit::base::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const CHUNK: usize = 1024 * 1024;
const CHUNKS: usize = 128;

#[derive(Clone, Copy)]
enum Mode {
    Executor,
    Thread,
}
#[derive(Clone, Copy, PartialEq)]
enum Case {
    Normal,
    Cancel,
    Fail,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Executor => "GPUI 后台执行器",
            Mode::Thread => "专用标准库线程",
        }
    }
}

struct Metrics {
    millis: u128,
    attempted: u32,
    dropped: u32,
}
enum Message {
    Progress(u8),
    Finished(Result<Metrics, String>),
}

// Both modes call exactly this blocking worker with the same workload. tempfile()
// creates anonymous temporary files: Windows deletes them when handles close.
fn run_work(
    cancel: &AtomicBool,
    tx: &async_channel::Sender<Message>,
    case: Case,
) -> Result<Metrics, String> {
    let ioerr = |err: std::io::Error| err.to_string();
    let mut source = tempfile::tempfile().map_err(ioerr)?;
    let mut dest = tempfile::tempfile().map_err(ioerr)?;
    let block: Vec<u8> = (0..CHUNK)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    let mut source_hash = Sha256::new();
    for _ in 0..CHUNKS {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        source.write_all(&block).map_err(ioerr)?;
        source_hash.update(&block);
    }
    source.flush().map_err(ioerr)?;
    source.seek(SeekFrom::Start(0)).map_err(ioerr)?;
    let expected = source_hash.finalize();
    let started = Instant::now();
    let mut buffer = vec![0_u8; CHUNK];
    let mut dropped = 0;
    for index in 0..CHUNKS {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        if case == Case::Fail && index == 5 {
            return Err("注入的测试错误".into());
        }
        source.read_exact(&mut buffer).map_err(ioerr)?;
        dest.write_all(&buffer).map_err(ioerr)?;
        // Equal simulated slow-I/O delay in both modes, to expose UI heartbeat behavior.
        std::thread::sleep(Duration::from_millis(1));
        if tx
            .try_send(Message::Progress(((index + 1) * 90 / CHUNKS) as u8))
            .is_err()
        {
            dropped += 1;
        }
    }
    dest.flush().map_err(ioerr)?;
    dest.seek(SeekFrom::Start(0)).map_err(ioerr)?;
    let mut hash = Sha256::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let n = dest.read(&mut buffer).map_err(ioerr)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if hash.finalize() != expected {
        return Err("SHA-256 校验不一致".into());
    }
    Ok(Metrics {
        millis: started.elapsed().as_millis(),
        attempted: CHUNKS as u32,
        dropped,
    })
}

struct Demo {
    mode: Option<Mode>,
    status: String,
    progress: u8,
    received: u32,
    max_heartbeat: Duration,
    last_heartbeat: Instant,
    cancel: Arc<AtomicBool>,
    background: Option<Task<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    output: Option<PathBuf>,
    case: Case,
}

impl Demo {
    fn new(cx: &mut Context<Self>, auto: Option<(Mode, PathBuf, Case)>) -> Self {
        let mut demo = Self {
            mode: None,
            status: "选择一种方式开始。测试仅使用临时文件。".into(),
            progress: 0,
            received: 0,
            max_heartbeat: Duration::ZERO,
            last_heartbeat: Instant::now(),
            cancel: Arc::new(AtomicBool::new(false)),
            background: None,
            worker: None,
            output: auto.as_ref().map(|x| x.1.clone()),
            case: auto.as_ref().map(|x| x.2).unwrap_or(Case::Normal),
        };
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                if this
                    .update(cx, |view, _| {
                        let now = Instant::now();
                        if view.mode.is_some() {
                            view.max_heartbeat = view
                                .max_heartbeat
                                .max(now.duration_since(view.last_heartbeat));
                        }
                        view.last_heartbeat = now;
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        if let Some((mode, _, _)) = auto {
            demo.start(mode, cx);
        }
        demo
    }

    fn start(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode.is_some() {
            return;
        }
        self.background = None;
        self.worker = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.mode = Some(mode);
        self.status = format!("{}：正在生成 128 MiB 测试数据…", mode.name());
        self.progress = 0;
        self.received = 0;
        self.max_heartbeat = Duration::ZERO;
        self.last_heartbeat = Instant::now();
        let (tx, rx) = async_channel::bounded(8);
        let cancel = self.cancel.clone();
        let case = self.case;
        let job = move || {
            let result = run_work(&cancel, &tx, case);
            let _ = tx.send_blocking(Message::Finished(result));
        };
        match mode {
            Mode::Executor => {
                self.background = Some(cx.background_spawn(async move {
                    job();
                }))
            }
            Mode::Thread => match std::thread::Builder::new()
                .name("copy-study".into())
                .spawn(job)
            {
                Ok(handle) => self.worker = Some(handle),
                Err(error) => {
                    self.mode = None;
                    self.status = format!("创建线程失败：{error}");
                    cx.notify();
                    return;
                }
            },
        }
        cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                let done = matches!(message, Message::Finished(_));
                if this.update(cx, |view, cx| {
                    match message {
                        Message::Progress(p) => { view.progress = p; view.received += 1; view.status = format!("{}：复制中（校验随后进行）", mode.name()); if view.case == Case::Cancel { view.cancel.store(true, Ordering::Relaxed); } }
                        Message::Finished(result) => {
                            view.mode = None;
                            let report = match result {
                                Ok(m) => { view.progress = 100; format!("mode={}\nresult=ok\ncopy_verify_ms={}\nprogress_attempted={}\nprogress_received={}\nprogress_dropped={}\nmax_heartbeat_ms={}\n", mode.name(), m.millis, m.attempted, view.received, m.dropped, view.max_heartbeat.as_millis()) }
                                Err(error) => format!("mode={}\nresult=error\nerror={}\n", mode.name(), error),
                            };
                            view.status = report.clone();
                            if let Some(path) = &view.output { let _ = std::fs::write(path, report); cx.quit(); }
                        }
                    }
                    cx.notify();
                }).is_err() || done { break; }
            }
        }).detach();
        cx.notify();
    }
}
impl Drop for Demo {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.mode.is_some();
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_6()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_xl().child("Codex i18n · 并发方案实验"))
            .child("两种方案使用相同的 128 MiB 临时文件、复制和 SHA-256 校验。")
            .child("每 MiB 人为延迟 1 ms，方便观察界面响应；不访问 Codex 文件。")
            .child(
                Button::new("executor")
                    .primary()
                    .label("运行 GPUI 后台执行器")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.start(Mode::Executor, cx))),
            )
            .child(
                Button::new("thread")
                    .label("运行专用标准库线程")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.start(Mode::Thread, cx))),
            )
            .child(
                Button::new("cancel")
                    .label("取消当前测试")
                    .disabled(!busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cancel.store(true, Ordering::Relaxed);
                        this.status = "正在取消…".into();
                        cx.notify();
                    })),
            )
            .child(format!(
                "进度：{}%，已收到 {} 条进度消息",
                self.progress, self.received
            ))
            .child(
                div().w(px(300.0)).h(px(12.0)).bg(cx.theme().muted).child(
                    div()
                        .w(px(self.progress as f32 * 3.0))
                        .h_full()
                        .bg(cx.theme().primary),
                ),
            )
            .child(self.status.clone())
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let auto = if args.get(1).is_some_and(|x| x == "--bench") {
        let mode = match args.get(2).and_then(|s| s.to_str()) {
            Some("executor") => Mode::Executor,
            Some("thread") => Mode::Thread,
            _ => return,
        };
        let case = match args.get(4).and_then(|s| s.to_str()) {
            Some("cancel") => Case::Cancel,
            Some("fail") => Case::Fail,
            _ => Case::Normal,
        };
        Some((
            mode,
            PathBuf::from(args.get(3).expect("需要输出文件路径")),
            case,
        ))
    } else {
        None
    };
    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(680.0), px(490.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Codex i18n · 并发方案实验".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open_window(options, cx, move |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| {
                cx.observe_window_appearance(window, |_, window, cx| {
                    Theme::sync_system_appearance(Some(window), cx)
                })
                .detach();
                Demo::new(cx, auto)
            })
        })
        .expect("无法打开窗口");
    });
}
