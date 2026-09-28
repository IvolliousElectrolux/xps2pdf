//! 打开或拖入 XPS / OXPS, 转到输出目录.

use std::path::PathBuf;

use gpui::actions;
use gpui::prelude::*;
use gpui::{
    div, px, rgb, size, App, Application, Bounds, Context, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Render, SharedString,
    Styled, Window, WindowBounds, WindowOptions,
};

use crate::config::{self, Config};
use crate::convert::{self, Event, Handle, Job};
use crate::ui_font;

actions!(xps2pdf, [OpenFile, Start, Stop, PickOutDir, ClearQueue]);

struct Item {
    id: u64,
    path: PathBuf,
    name: String,
    status: Status,
}

enum Status {
    Ready,
    Running(String),
    Done(String),
    Failed(String),
}

pub struct XpsApp {
    focus_handle: FocusHandle,
    items: Vec<Item>,
    next_id: u64,
    out_dir: PathBuf,
    status: SharedString,
    running: bool,
    engine: Option<Handle>,
    btn_press: Option<SharedString>,
}

impl XpsApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let cfg = config::load();
        let out_dir = if cfg.out_dir.is_empty() {
            PathBuf::from(Config::default().out_dir)
        } else {
            PathBuf::from(&cfg.out_dir)
        };
        Self {
            focus_handle: cx.focus_handle(),
            items: Vec::new(),
            next_id: 1,
            out_dir,
            status: "拖入或打开 XPS / OXPS / PDF. Windows 和 macOS 用同一套转换, 不依赖系统组件.".into(),
            running: false,
            engine: None,
            btn_press: None,
        }
    }

    fn persist(&self) {
        config::save(&Config { out_dir: self.out_dir.display().to_string() });
    }

    fn spawn_native_dialog<T, F, A>(cx: &mut Context<Self>, work: F, apply: A)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        A: FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    {
        let (tx, rx) = async_channel::bounded::<T>(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(work());
        });
        cx.spawn(async move |this, cx| {
            if let Ok(val) = rx.recv().await {
                this.update(cx, |view, cx| apply(view, val, cx)).ok();
            }
        })
        .detach();
    }

    fn btn(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        active: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bg = if active { rgb(0x2563eb) } else { rgb(0xe2e8f0) };
        let fg = if active { rgb(0xffffff) } else { rgb(0x0f172a) };
        let hover = if active { rgb(0x1d4ed8) } else { rgb(0xcbd5e1) };
        let id: SharedString = id.into();
        let id_down = id.clone();
        let id_up = id.clone();
        let id_out = id.clone();
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .bg(bg)
            .border_1()
            .border_color(rgb(0x94a3b8))
            .text_color(fg)
            .text_sm()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(label.into())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.btn_press = Some(id_down.clone());
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    let press = this.btn_press.take();
                    if press.as_ref() != Some(&id_up) {
                        return;
                    }
                    on_click(this, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    if this.btn_press.as_ref() == Some(&id_out) {
                        this.btn_press = None;
                    }
                }),
            )
    }

    fn open_files(&mut self, cx: &mut Context<Self>) {
        Self::spawn_native_dialog(
            cx,
            || {
                rfd::FileDialog::new()
                    .set_title("打开 XPS")
                    .add_filter("XPS", &["xps", "oxps", "pdf"])
                    .pick_files()
            },
            |this, files, cx| {
                if let Some(files) = files {
                    this.add_paths(files, cx);
                }
            },
        );
    }

    fn pick_out_dir(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let start = self.out_dir.clone();
        Self::spawn_native_dialog(
            cx,
            move || rfd::FileDialog::new().set_title("输出目录").set_directory(&start).pick_folder(),
            |this, dir, cx| {
                if let Some(p) = dir {
                    this.out_dir = p;
                    this.persist();
                    cx.notify();
                }
            },
        );
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let found = convert::collect_inputs(&paths);
        if found.is_empty() {
            self.status = "没有 XPS / OXPS / PDF.".into();
            cx.notify();
            return;
        }
        let mut added = 0u32;
        for path in found {
            if self.items.iter().any(|i| i.path == path) {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("xps").to_string();
            let id = self.next_id;
            self.next_id += 1;
            self.items.push(Item { id, path, name, status: Status::Ready });
            added += 1;
        }
        self.status = if added == 0 {
            "这些文件已经在队列里.".into()
        } else {
            format!("已加入 {added} 个文件.").into()
        };
        cx.notify();
    }

    fn can_start(&self) -> bool {
        !self.running && self.items.iter().any(|i| matches!(i.status, Status::Ready | Status::Failed(_)))
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        std::fs::create_dir_all(&self.out_dir).ok();
        let mut used = Vec::new();
        let jobs: Vec<Job> = self
            .items
            .iter()
            .filter(|i| matches!(i.status, Status::Ready | Status::Failed(_)))
            .map(|i| Job {
                id: i.id,
                dest: convert::dest_pdf(&self.out_dir, &i.path, &mut used),
                path: i.path.clone(),
            })
            .collect();
        if jobs.is_empty() {
            self.status = "没有待转换的文件.".into();
            cx.notify();
            return;
        }
        let ids: Vec<u64> = jobs.iter().map(|j| j.id).collect();
        for item in &mut self.items {
            if ids.contains(&item.id) {
                item.status = Status::Running(String::new());
            }
        }
        let (handle, rx) = convert::start(jobs);
        self.engine = Some(handle);
        self.running = true;
        self.status = "开始转换…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                let leave = this
                    .update(cx, |view, cx| {
                        view.on_event(ev, cx);
                        !view.running
                    })
                    .unwrap_or(true);
                if leave {
                    break;
                }
            }
        })
        .detach();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.stop();
            self.status = "正在停止…".into();
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.items.clear();
        self.status = "队列已清空.".into();
        cx.notify();
    }

    fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.items.retain(|i| i.id != id);
        cx.notify();
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::Progress { id, page, total } => {
                let label = self.items.iter().find(|i| i.id == id).map(|i| i.name.clone()).unwrap_or_default();
                if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
                    item.status = Status::Running(format!("第 {page}/{total} 页"));
                }
                self.status = format!("{label} · 第 {page}/{total} 页").into();
            }
            Event::Done { id, path, detail } => {
                if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
                    item.status = Status::Done(detail.clone());
                }
                self.status = format!("{detail} → {}", path.display()).into();
            }
            Event::Failed { id, err } => {
                if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
                    item.status = Status::Failed(err.clone());
                }
                self.status = err.into();
            }
            Event::Finished { stopped } => {
                self.running = false;
                self.engine = None;
                for item in &mut self.items {
                    if matches!(item.status, Status::Running(_)) {
                        item.status = Status::Ready;
                    }
                }
                let failed = self.items.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
                self.status = if stopped {
                    "已停止.".into()
                } else if failed > 0 {
                    format!("完成, {failed} 个失败.").into()
                } else {
                    "完成.".into()
                };
            }
        }
        cx.notify();
    }

    fn queue_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running;
        let mut list = div().id("queue").flex().flex_col().flex_1().min_h(px(0.)).gap_1().overflow_scroll();
        if self.items.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(rgb(0x64748b))
                    .child("还没有文件. 打开或拖入 .xps / .oxps / .pdf, 也可以拖入文件夹."),
            );
        }
        for item in &self.items {
            let id = item.id;
            let st = status_text(&item.status);
            let st_color = match &item.status {
                Status::Failed(_) => rgb(0xb91c1c),
                Status::Done(_) => rgb(0x047857),
                Status::Running(_) => rgb(0x1d4ed8),
                Status::Ready => rgb(0x64748b),
            };
            let mut row = div()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xe2e8f0))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w(px(0.)).overflow_hidden().text_xs().child(item.name.clone()))
                .child(div().text_xs().text_color(st_color).child(st));
            if !running {
                row = row.child(
                    div()
                        .id(SharedString::from(format!("rm-{id}")))
                        .px_1()
                        .text_xs()
                        .text_color(rgb(0x64748b))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(0xb91c1c)))
                        .child("×")
                        .on_mouse_up(MouseButton::Left, cx.listener(move |this, _, _, cx| this.remove(id, cx))),
                );
            }
            list = list.child(row);
        }
        list
    }
}

fn status_text(status: &Status) -> String {
    match status {
        Status::Ready => "就绪".into(),
        Status::Running(n) if n.is_empty() => "转换中".into(),
        Status::Running(n) => n.clone(),
        Status::Done(d) => d.clone(),
        Status::Failed(e) => {
            let t: String = e.chars().take(42).collect();
            if e.chars().count() > 42 { format!("失败 {t}…") } else { format!("失败 {t}") }
        }
    }
}

fn short_path(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    let count = s.chars().count();
    if count > 42 {
        let tail: String = s.chars().skip(count - 40).collect();
        format!("…{tail}")
    } else {
        s
    }
}

impl Focusable for XpsApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for XpsApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_start = self.can_start();
        div()
            .key_context("Xps")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf8fafc))
            .text_color(rgb(0x0f172a))
            .font_family(ui_font())
            .on_action(cx.listener(|this, _: &OpenFile, _, cx| this.open_files(cx)))
            .on_action(cx.listener(|this, _: &Start, _, cx| this.start(cx)))
            .on_action(cx.listener(|this, _: &Stop, _, cx| this.stop(cx)))
            .on_action(cx.listener(|this, _: &PickOutDir, _, cx| this.pick_out_dir(cx)))
            .on_action(cx.listener(|this, _: &ClearQueue, _, cx| this.clear(cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let list: Vec<PathBuf> = paths.paths().iter().cloned().collect();
                this.add_paths(list, cx);
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0xcbd5e1))
                    .bg(rgb(0xf1f5f9))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child("XPS 转 PDF"))
                    .child(self.btn("open", "打开", false, |this, _, cx| this.open_files(cx), cx))
                    .child(self.btn("outdir", "输出目录", false, |this, _, cx| this.pick_out_dir(cx), cx))
                    .child(div().flex_1())
                    .child(self.btn("start", "开始", can_start, |this, _, cx| this.start(cx), cx))
                    .child(self.btn("stop", "停止", self.running, |this, _, cx| this.stop(cx), cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .p_3()
                            .gap_2()
                            .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("队列 ({})", self.items.len())))
                            .child(self.queue_list(cx)),
                    )
                    .child(
                        div()
                            .id("side")
                            .w(px(280.))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_3()
                            .border_l_1()
                            .border_color(rgb(0xcbd5e1))
                            .bg(rgb(0xf8fafc))
                            .overflow_scroll()
                            .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child("说明"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child("纯 Rust 读取 XPS 压缩包, 把路径, 字形和图像写成 PDF. 不调用 Windows XPS 打印接口, 所以 macOS 上也能转."),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child("文字按嵌入字体的轮廓绘制, 并用 ActualText 保留可复制的原文. 混淆过的 .odttf 会先还原."),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child("暂不支持 VisualBrush 和 JPEG XR. 这两种会跳过并在结果里提示."),
                            )
                            .child(div().flex().flex_col().gap_1().child(div().text_xs().text_color(rgb(0x334155)).child("输出目录")).child(
                                div().text_xs().text_color(rgb(0x0f172a)).child(short_path(&self.out_dir)),
                            ))
                            .child(self.btn("clear", "清空队列", false, |this, _, cx| this.clear(cx), cx)),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(0xcbd5e1))
                    .text_xs()
                    .text_color(rgb(0x475569))
                    .child(self.status.clone()),
            )
    }
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("secondary-o", OpenFile, Some("Xps")),
            KeyBinding::new("ctrl-o", OpenFile, Some("Xps")),
            KeyBinding::new("secondary-enter", Start, Some("Xps")),
            KeyBinding::new("ctrl-enter", Start, Some("Xps")),
            KeyBinding::new("escape", Stop, Some("Xps")),
        ]);
        let bounds = Bounds::centered(None, size(px(880.), px(560.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("XPS 转 PDF".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let app = XpsApp::new(cx);
                    app.focus_handle.focus(window);
                    app
                })
            },
        )
        .unwrap();
    });
}
