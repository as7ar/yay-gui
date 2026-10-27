use adw::gio;
use adw::gtk::{
    glib, Align, Box as GBox, Button, ListBox, Orientation, PasswordEntry, ScrolledWindow,
    SearchEntry, SelectionMode, TextView, WrapMode,
};
use adw::prelude::*;
use libadwaita as adw;
use serde::Deserialize;
use std::cell::Cell;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::rc::Rc;

#[derive(Deserialize)]
struct RpcResponse {
    results: Vec<AurPackage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AurPackage {
    name: String,
    version: String,
    description: Option<String>,
    num_votes: Option<u32>,
}

enum Msg {
    Line(String),
    Done(bool),
}

struct Ctx {
    window: adw::ApplicationWindow,
    search_list: ListBox,
    installed_list: ListBox,
    entry: SearchEntry,
    toasts: adw::ToastOverlay,
    generation: Cell<u64>,
}

fn askpass_main() -> glib::ExitCode {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "관리자 비밀번호를 입력하세요".to_string());
    let app = adw::Application::builder()
        .application_id("dev.astar.aurgui.askpass")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        let entry = PasswordEntry::builder().show_peek_icon(true).build();
        let dialog = adw::MessageDialog::builder()
            .application(app)
            .heading("인증 필요")
            .body(prompt.as_str())
            .extra_child(&entry)
            .default_response("ok")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "취소"), ("ok", "확인")]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

        let d = dialog.clone();
        entry.connect_activate(move |_| d.response("ok"));

        let e = entry.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "ok" {
                std::process::exit(1);
            }
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{}", e.text());
            let _ = out.flush();
            std::process::exit(0);
        });
        dialog.present();
    });

    app.run_with_args::<&str>(&[])
}

fn run_streamed(program: &'static str, args: Vec<String>, tx: async_channel::Sender<Msg>) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let spawned = Command::new(program)
        .args(&args)
        .env("SUDO_ASKPASS", exe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = spawned else {
        let _ = tx.send_blocking(Msg::Done(false));
        return;
    };

    let streams: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    let handles: Vec<_> = streams
        .into_iter()
        .map(|stream| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for chunk in BufReader::new(stream).split(b'\n').flatten() {
                    let text = String::from_utf8_lossy(&chunk).to_string();
                    let last = text
                        .rsplit('\r')
                        .find(|p| !p.trim().is_empty())
                        .unwrap_or("")
                        .to_string();
                    if last.is_empty() {
                        continue;
                    }
                    let _ = tx.send_blocking(Msg::Line(last));
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }

    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    let _ = tx.send_blocking(Msg::Done(ok));
}

fn show_log(ctx: &Rc<Ctx>, title: &str, program: &'static str, args: Vec<String>) {
    let view = TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .build();
    let buffer = view.buffer();
    let mark = buffer.create_mark(None, &buffer.end_iter(), false);
    let scroll = ScrolledWindow::builder().vexpand(true).child(&view).build();

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));

    let window = adw::Window::builder()
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(720)
        .default_height(460)
        .title(title)
        .content(&toolbar)
        .deletable(false)
        .build();
    window.present();

    let (tx, rx) = async_channel::unbounded::<Msg>();
    std::thread::spawn(move || run_streamed(program, args, tx));

    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            match msg {
                Msg::Line(line) => {
                    buffer.insert(&mut buffer.end_iter(), &format!("{line}\n"));
                    buffer.move_mark(&mark, &buffer.end_iter());
                    view.scroll_mark_onscreen(&mark);
                }
                Msg::Done(ok) => {
                    header.set_show_end_title_buttons(true);
                    window.set_deletable(true);
                    ctx.toasts.add_toast(adw::Toast::new(if ok {
                        "완료"
                    } else {
                        "실패 - 로그를 확인하세요"
                    }));
                    refresh(&ctx);
                    return;
                }
            }
        }
    });
}

fn review_then_install(ctx: &Rc<Ctx>, name: &str) {
    let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
    let pkg = name.to_string();
    std::thread::spawn(move || {
        let result = reqwest::blocking::Client::new()
            .get("https://aur.archlinux.org/cgit/aur.git/plain/PKGBUILD")
            .query(&[("h", pkg.as_str())])
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .map_err(|e| e.to_string());
        let _ = tx.send_blocking(result);
    });

    let ctx = ctx.clone();
    let name = name.to_string();
    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else {
            return;
        };
        let Ok(text) = result else {
            ctx.toasts
                .add_toast(adw::Toast::new("PKGBUILD를 불러오지 못했습니다"));
            return;
        };

        let view = TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(WrapMode::WordChar)
            .build();
        view.buffer().set_text(&text);
        let scroll = ScrolledWindow::builder()
            .min_content_height(360)
            .min_content_width(640)
            .child(&view)
            .build();

        let dialog = adw::MessageDialog::builder()
            .transient_for(&ctx.window)
            .heading(format!("{name} PKGBUILD 검토"))
            .body("설치 시 아래 스크립트가 빌드 과정에서 실행됩니다. 내용을 확인한 뒤 진행하세요.")
            .extra_child(&scroll)
            .default_response("cancel")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "취소"), ("install", "설치")]);
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);

        let ctx2 = ctx.clone();
        let name2 = name.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "install" {
                return;
            }
            let args = vec![
                "-S".to_string(),
                "--noconfirm".to_string(),
                "--needed".to_string(),
                "--color".to_string(),
                "never".to_string(),
                "--sudoloop".to_string(),
                "--sudoflags=-A".to_string(),
                name2.clone(),
            ];
            show_log(&ctx2, &format!("{name2} 설치"), "yay", args);
        });
        dialog.present();
    });
}

fn build_row(
    name: &str,
    version: &str,
    desc: &str,
    installed: bool,
    ctx: &Rc<Ctx>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&format!("{name}  {version}")))
        .subtitle(glib::markup_escape_text(desc))
        .subtitle_lines(2)
        .build();

    let button = Button::builder()
        .label(if installed { "삭제" } else { "설치" })
        .valign(Align::Center)
        .build();
    button.add_css_class(if installed {
        "destructive-action"
    } else {
        "suggested-action"
    });
    row.add_suffix(&button);

    let name = name.to_string();
    let ctx = ctx.clone();
    button.connect_clicked(move |_| {
        if !installed {
            review_then_install(&ctx, &name);
            return;
        }

        let dialog = adw::MessageDialog::builder()
            .transient_for(&ctx.window)
            .heading(format!("{name} 삭제"))
            .body("더 이상 필요 없는 의존성도 함께 삭제됩니다 (-Rns).")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "취소"), ("remove", "삭제")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);

        let ctx2 = ctx.clone();
        let name2 = name.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "remove" {
                return;
            }
            let args = vec![
                "-A".to_string(),
                "pacman".to_string(),
                "-Rns".to_string(),
                "--noconfirm".to_string(),
                "--color".to_string(),
                "never".to_string(),
                name2.clone(),
            ];
            show_log(&ctx2, &format!("{name2} 삭제"), "sudo", args);
        });
        dialog.present();
    });

    row
}

fn refresh(ctx: &Rc<Ctx>) {
    let id = ctx.generation.get() + 1;
    ctx.generation.set(id);

    let query = ctx.entry.text().to_string();
    let (tx, rx) = async_channel::bounded::<(Vec<(String, String)>, Vec<AurPackage>)>(1);

    std::thread::spawn(move || {
        let installed: Vec<(String, String)> = match Command::new("pacman").arg("-Qm").output() {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        let mut found: Vec<AurPackage> = Vec::new();
        if query.trim().chars().count() >= 2 {
            let resp = reqwest::blocking::Client::new()
                .get("https://aur.archlinux.org/rpc/")
                .query(&[
                    ("v", "5"),
                    ("type", "search"),
                    ("by", "name-desc"),
                    ("arg", query.trim()),
                ])
                .send()
                .and_then(|r| r.json::<RpcResponse>());
            if let Ok(r) = resp {
                found = r.results;
            }
        }
        found.sort_by(|a, b| b.num_votes.cmp(&a.num_votes));
        found.truncate(50);

        let _ = tx.send_blocking((installed, found));
    });

    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let Ok((installed, found)) = rx.recv().await else {
            return;
        };
        if ctx.generation.get() != id {
            return;
        }

        while let Some(c) = ctx.search_list.first_child() {
            ctx.search_list.remove(&c);
        }
        while let Some(c) = ctx.installed_list.first_child() {
            ctx.installed_list.remove(&c);
        }

        let names: HashSet<&str> = installed.iter().map(|(n, _)| n.as_str()).collect();
        for p in &found {
            let row = build_row(
                &p.name,
                &p.version,
                p.description.as_deref().unwrap_or(""),
                names.contains(p.name.as_str()),
                &ctx,
            );
            ctx.search_list.append(&row);
        }
        for (n, v) in &installed {
            let row = build_row(n, v, "", true, &ctx);
            ctx.installed_list.append(&row);
        }
    });
}

fn main() -> glib::ExitCode {
    if std::env::args().nth(1).is_some() {
        return askpass_main();
    }

    let app = adw::Application::builder()
        .application_id("dev.astar.aurgui")
        .build();

    app.connect_activate(|app| {
        let make_list = || {
            let list = ListBox::builder()
                .selection_mode(SelectionMode::None)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            list.add_css_class("boxed-list");
            list
        };
        let wrap = |list: &ListBox| {
            let clamp = adw::Clamp::builder().maximum_size(800).child(list).build();
            ScrolledWindow::builder()
                .vexpand(true)
                .child(&clamp)
                .build()
        };

        let entry = SearchEntry::builder()
            .placeholder_text("AUR 패키지 검색")
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        entry.set_property("search-delay", 500u32);
        let search_list = make_list();
        let installed_list = make_list();

        let search_page = GBox::new(Orientation::Vertical, 0);
        search_page.append(&entry);
        search_page.append(&wrap(&search_list));

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &search_page,
            Some("search"),
            "검색",
            "system-search-symbolic",
        );
        stack.add_titled_with_icon(
            &wrap(&installed_list),
            Some("installed"),
            "설치됨",
            "view-list-symbolic",
        );

        let header = adw::HeaderBar::new();
        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        header.set_title_widget(Some(&switcher));

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&view));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("AUR Manager")
            .default_width(800)
            .default_height(700)
            .content(&toasts)
            .build();

        let ctx = Rc::new(Ctx {
            window: window.clone(),
            search_list,
            installed_list,
            entry: entry.clone(),
            toasts,
            generation: Cell::new(0),
        });

        let c = ctx.clone();
        entry.connect_search_changed(move |_| refresh(&c));

        refresh(&ctx);
        window.present();
    });

    app.run()
}
