#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod convert;
mod error;
mod font;
mod geom;
mod package;
mod parse;
mod pathgeom;
mod pdfout;
mod scene;
mod gui;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--gui") {
        gui::run();
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "xps2pdf 把 XPS / OXPS 转成 PDF\n\
             \n\
             xps2pdf                 打开窗口\n\
             xps2pdf a.xps           在源文件旁边写出 a.pdf\n\
             xps2pdf a.xps -o b.pdf  写到指定 PDF\n\
             xps2pdf a.xps b.xps -o 目录"
        );
        return ExitCode::SUCCESS;
    }
    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn run_cli(args: &[String]) -> Result<(), error::Error> {
    let mut inputs = Vec::new();
    let mut out: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-o" || arg == "--output" {
            out = iter.next().cloned();
        } else if arg.starts_with('-') {
            return Err(error::Error::msg(format!("未知参数 {arg}")));
        } else {
            inputs.push(PathBuf::from(arg));
        }
    }
    let files = convert::collect_inputs(&inputs);
    if files.is_empty() {
        return Err(error::Error::msg("没有 XPS / OXPS 文件"));
    }
    let stop = std::sync::atomic::AtomicBool::new(false);
    if files.len() == 1 {
        let src = &files[0];
        let dst = if let Some(out) = &out {
            let p = PathBuf::from(out);
            if p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                p
            } else {
                convert::dest_pdf(&p, src, &mut Vec::new())
            }
        } else {
            src.with_extension("pdf")
        };
        let report = convert::convert_file(src, &dst, &stop, |page, total| {
            eprintln!("{}  {page}/{total}", src.display());
        })?;
        eprintln!("{} -> {} ({} 页)", src.display(), dst.display(), report.pages);
        for w in &report.warnings {
            eprintln!("  {w}");
        }
        return Ok(());
    }
    let dir = out.map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut used = Vec::new();
    for src in &files {
        let dst = convert::dest_pdf(&dir, src, &mut used);
        let report = convert::convert_file(src, &dst, &stop, |page, total| {
            eprintln!("{}  {page}/{total}", src.display());
        })?;
        eprintln!("{} -> {} ({} 页)", src.display(), dst.display(), report.pages);
        for w in &report.warnings {
            eprintln!("  {w}");
        }
    }
    Ok(())
}

pub fn ui_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "PingFang SC"
    } else if cfg!(windows) {
        "Microsoft YaHei UI"
    } else {
        "sans-serif"
    }
}
