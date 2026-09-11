//! Render the compact panel at common scales and exercise its full-details link.

use std::{cell::RefCell, rc::Rc};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, Rgb8Pixel, VecModel};

struct TestPlatform(Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>);

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        self.0.borrow_mut().push(window.clone());
        Ok(window)
    }
}

fn draw(window: &MinimalSoftwareWindow, name: &str) {
    window.request_redraw();
    let mut rendered = false;
    window.draw_if_needed(|renderer| {
        let size = window.size();
        let mut buffer = vec![Rgb8Pixel::default(); (size.width * size.height) as usize];
        renderer.render(&mut buffer, size.width as usize);
        assert!(buffer.iter().any(|pixel| pixel.r != pixel.b));
        if let Some(dir) = std::env::var_os("ATOLL_RENDER_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut bytes = format!("P6\n{} {}\n255\n", size.width, size.height).into_bytes();
            bytes.extend(buffer.iter().flat_map(|pixel| [pixel.r, pixel.g, pixel.b]));
            std::fs::write(dir.join(format!("{name}.ppm")), bytes).unwrap();
        }
        rendered = true;
    });
    assert!(rendered);
}

fn click(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
fn waiting_preview_renders_and_opens_full_details_at_common_scales() {
    let windows = Rc::new(RefCell::new(Vec::new()));
    slint::platform::set_platform(Box::new(TestPlatform(windows.clone()))).unwrap();
    let panel = super::ui::FlyoutWindow::new().unwrap();
    panel.set_compact(true);
    panel.set_waiting_total(8);
    let expanded = Rc::new(std::cell::Cell::new(false));
    panel.on_expand({
        let expanded = expanded.clone();
        move || expanded.set(true)
    });
    panel.show().unwrap();
    let window = windows.borrow().last().unwrap().clone();
    for scale in [1.0, 1.5, 2.0] {
        window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        for count in [1, super::flyout::PEEK_LIMIT] {
            let rows = (0..count)
                .map(|n| super::ui::SessionRow {
                    id: format!("session-{n}").into(),
                    title: if n % 2 == 0 {
                        "atoll · Update background notifications".into()
                    } else {
                        "项目 · 等待确认终端命令".into()
                    },
                    detail: "Waiting for permission · shell".into(),
                    source: if n % 2 == 0 {
                        "codex".into()
                    } else {
                        "claude".into()
                    },
                    phase: "waitingForApproval".into(),
                    jumpable: true,
                })
                .collect::<Vec<_>>();
            panel.set_sessions(ModelRc::new(VecModel::from(rows)));
            let height = super::flyout::peek_height(count);
            panel
                .window()
                .set_size(slint::LogicalSize::new(super::FLYOUT_WIDTH, height));
            draw(&window, &format!("preview-{count}-{scale}"));
            assert!(
                window
                    .size()
                    .height
                    .abs_diff((height * scale).round() as u32)
                    <= 1
            );
            expanded.set(false);
            click(&window, 120.0, height - 24.0);
            assert!(
                expanded.get(),
                "full-details link must remain clickable at {scale}x with {count} rows"
            );
        }
    }
    panel.hide().unwrap();

    let settings = super::ui::SettingsWindow::new().unwrap();
    settings.set_claude_status("Not installed".into());
    settings.set_codex_status("8 of 8 hooks installed".into());
    settings.set_codex_installed(true);
    settings.set_taskbar_status("Sitting in the taskbar, above the notification area.".into());
    settings
        .set_message("Review the new Codex hooks with /hooks, then start a new session.".into());
    settings
        .window()
        .set_size(slint::LogicalSize::new(460.0, 380.0));
    settings.show().unwrap();
    let window = windows.borrow().last().unwrap().clone();
    draw(&window, "settings-setup");
    click(&window, 260.0, 23.0);
    draw(&window, "settings-general");
    settings.hide().unwrap();

    let card = super::ui::CardWindow::new().unwrap();
    card.set_card(3);
    card.set_card_source("codex".into());
    card.set_card_title("atoll · Codex".into());
    card.set_card_tool("Question".into());
    card.set_form_progress("1 / 3 · 实现范围".into());
    card.set_form_question(
        "这次优先实现哪些能力？选项说明和自由文本都应完整显示，较长内容可以滚动查看。".into(),
    );
    card.set_form_free_text(true);
    card.set_form_options(ModelRc::new(VecModel::from(vec![
        super::ui::FormOption {
            label: "Windows Terminal".into(),
            description: "返回对应窗口、隐藏标签页和原来的分屏。".into(),
            selected: true,
        },
        super::ui::FormOption {
            label: "Codex desktop".into(),
            description: "Open the exact local conversation in the desktop app.".into(),
            selected: false,
        },
    ])));
    card.set_form_text("暂时只支持 Codex\n保留当前终端的操作方式。".into());
    card.set_form_can_next(true);
    card.window().set_size(slint::LogicalSize::new(
        super::cardview::CARD_WIDTH,
        super::cardview::card_height(super::cardview::CardKind::Form),
    ));
    card.show().unwrap();
    let window = windows.borrow().last().unwrap().clone();
    for scale in [1.0, 1.5, 2.0] {
        window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        card.window().set_size(slint::LogicalSize::new(
            super::cardview::CARD_WIDTH,
            super::cardview::card_height(super::cardview::CardKind::Form),
        ));
        draw(&window, &format!("codex-question-{scale}"));
    }
    card.set_form_options(ModelRc::default());
    card.set_form_secret(true);
    card.set_form_text("hidden secret text".into());
    draw(&window, "codex-secret-question");
    card.hide().unwrap();
    codex_tui_editor_renders_and_applies_only_explicit_actions(&windows);
}

fn codex_tui_editor_renders_and_applies_only_explicit_actions(
    windows: &Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
) {
    use atoll_core::install::codex_tui as config;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# keep my config\n[features]\nhooks = true\n[tui]\ntheme = \"nord\"\n";
    std::fs::write(&path, original).unwrap();
    let editor = super::codex_tui::Editor::new(path.clone()).unwrap();
    editor.show().unwrap();
    // The displayed path is illustrative in the screenshots; all writes still
    // go to the explicit temporary path held by the controller.
    editor.window.set_config_path("~/.codex/config.toml".into());
    let window = windows.borrow().last().unwrap().clone();
    assert_eq!(editor.window.get_selected_count(), 3);
    assert!(!editor.window.get_dirty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    for scale in [1.0, 1.5, 2.0] {
        window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        for (width, height) in [(740.0, 720.0), (620.0, 620.0)] {
            editor
                .window
                .window()
                .set_size(slint::LogicalSize::new(width, height));
            draw(&window, &format!("codex-tui-default-{width}-{scale}"));
            if width == 740.0 && scale == 1.0 {
                window.dispatch_event(WindowEvent::WindowActiveChanged(true));
                window.dispatch_event(WindowEvent::KeyPressed {
                    text: slint::platform::Key::Tab.into(),
                });
                window.dispatch_event(WindowEvent::KeyReleased {
                    text: slint::platform::Key::Tab.into(),
                });
            }
            click(&window, 31.0, 307.0);
            assert_eq!(
                editor.window.get_selected_count(),
                2,
                "checkbox at {width}px / {scale}x"
            );
            assert!(editor.window.get_dirty());
            window.dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
            window.dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
            assert_eq!(
                editor.window.get_selected_count(),
                3,
                "checkbox retains keyboard focus after toggling"
            );
            assert!(!editor.window.get_dirty());
        }
    }
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
    editor
        .window
        .window()
        .set_size(slint::LogicalSize::new(740.0, 720.0));
    editor
        .window
        .invoke_toggle("model-with-reasoning".into(), false);
    assert!(editor.window.get_dirty());
    assert!(!editor.window.get_preview().contains("gpt-6-astra"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    editor.window.invoke_toggle("git-branch".into(), true);
    draw(&window, "codex-tui-draft");
    click(&window, 690.0, 600.0);
    assert!(
        !editor.window.get_error(),
        "{}",
        editor.window.get_message()
    );
    assert!(!editor.window.get_dirty());
    assert_eq!(
        config::read(&path).unwrap().visible_items(),
        vec!["current-dir", "thread-name", "git-branch"]
    );
    for component in config::COMPONENTS {
        editor.window.invoke_toggle(component.id.into(), false);
    }
    assert_eq!(editor.window.get_selected_count(), 0);
    draw(&window, "codex-tui-hidden");
    editor.window.invoke_apply();
    assert_eq!(config::read(&path).unwrap().items, Some(vec![]));
    click(&window, 100.0, 600.0);
    assert_eq!(config::read(&path).unwrap().items, None);
    assert!(!editor.window.get_custom());
    assert_eq!(editor.window.get_selected_count(), 3);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("theme = \"nord\"")
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("hooks = true")
    );
    for component in config::COMPONENTS {
        editor.window.invoke_toggle(component.id.into(), true);
    }
    draw(&window, "codex-tui-all");
    let external = "[tui]\nstatus_line = [\"future-component\", \"model\"]\n";
    std::fs::write(&path, external).unwrap();
    editor.window.invoke_apply();
    assert!(editor.window.get_error());
    assert!(editor.window.get_dirty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
    draw(&window, "codex-tui-conflict");
    editor.window.hide().unwrap();
    editor.show().unwrap();
    assert!(!editor.window.get_error());
    assert_eq!(editor.window.get_selected_count(), 2);
    assert!(editor.window.get_preview().contains("[future-component]"));
    editor.window.hide().unwrap();
}
