//! Native regression coverage: use hidden windows owned by the test process,
//! never Explorer or the user's running readout.

use super::*;

struct TestWindow(HWND);

impl TestWindow {
    fn new(style: WINDOW_STYLE) -> Self {
        Self(unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_WINDOWEDGE,
                windows::core::w!("STATIC"),
                windows::core::w!("Atoll readout test"),
                style,
                0,
                0,
                68,
                82,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        })
    }

    fn handle(&self) -> isize {
        self.0.0 as isize
    }

    fn assert_readout(&self, embedded: bool) {
        assert_readout(self.0, embedded);
    }
}

fn assert_readout(window: HWND, embedded: bool) {
    let style = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
    let extended = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } as u32;
    assert_eq!(
        style & WS_CAPTION.0,
        0,
        "caption styles returned: {style:#x}"
    );
    assert_eq!(style & (WS_THICKFRAME | WS_SYSMENU).0, 0);
    assert_eq!(style & WS_CHILD.0 != 0, embedded);
    assert_eq!(
        extended & (WS_EX_APPWINDOW | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE).0,
        0
    );
    assert_ne!(extended & WS_EX_TOOLWINDOW.0, 0);
    let mut client = RECT::default();
    unsafe { GetClientRect(window, &mut client) }.unwrap();
    let outer = rect_of(window).unwrap();
    assert_eq!(
        (client.right, client.bottom),
        (outer.right - outer.left, outer.bottom - outer.top)
    );
    assert!(!unsafe { IsWindowVisible(window) }.as_bool());
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

#[test]
fn embedded_readout_has_no_native_caption_even_after_backend_style_updates() {
    let host = TestWindow::new(WS_POPUP);
    // Winit implements a frameless top-level window in WM_NCCALCSIZE, while
    // retaining these caption bits for features such as snapping.
    let readout = TestWindow::new(WS_OVERLAPPEDWINDOW);
    assert!(embed_in(readout.handle(), host.handle()));
    readout.assert_readout(true);

    for height in [58, 82, 106, 58, 82] {
        unsafe {
            // Model winit's full style rewrite when its window flags change.
            SetWindowLongPtrW(readout.0, GWL_STYLE, WS_OVERLAPPEDWINDOW.0 as isize);
            SetWindowLongPtrW(
                readout.0,
                GWL_EXSTYLE,
                (WS_EX_APPWINDOW | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE).0 as isize,
            );
            SetWindowPos(
                readout.0,
                None,
                0,
                0,
                68,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .unwrap();
        }
        assert_eq!(parent_of(readout.handle()), Some(host.handle()));
        readout.assert_readout(true);
    }
}

#[test]
fn a_failed_embed_keeps_the_floating_readout_frameless_and_preserves_compositing() {
    let readout = TestWindow::new(WS_OVERLAPPEDWINDOW);
    assert!(!embed_in(readout.handle(), -1));
    readout.assert_readout(false);
    assert_eq!(parent_of(readout.handle()), None);
    let compositing = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE;
    unsafe {
        SetWindowLongPtrW(
            readout.0,
            GWL_EXSTYLE,
            (compositing | WS_EX_APPWINDOW).0 as isize,
        );
        SetWindowLongPtrW(readout.0, GWL_STYLE, WS_OVERLAPPEDWINDOW.0 as isize);
    }
    readout.assert_readout(false);
    assert_eq!(
        unsafe { GetWindowLongPtrW(readout.0, GWL_EXSTYLE) } as u32 & compositing.0,
        compositing.0
    );
}

#[test]
#[ignore = "exercises the real Windows Slint/FemtoVG backend; run separately on a desktop"]
fn native_slint_readout_stays_frameless_through_layout_and_visibility_changes() {
    use crate::app::taskbar::{Along, Chip, TaskbarView};
    use crate::app::ui::{FlyoutWindow, TaskbarBar};
    use atoll_core::protocol::HookSource;
    use atoll_core::state::AgentTasks;
    use slint::ComponentHandle;
    use slint::winit_030::{WinitWindowAccessor, winit};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Duration;
    use winit::platform::windows::EventLoopBuilderExtWindows;

    let mut event_loop =
        winit::event_loop::EventLoop::<slint::winit_030::SlintEvent>::with_user_event();
    event_loop.with_any_thread(true);
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .with_winit_event_loop_builder(event_loop)
        .with_winit_window_attributes_hook(|attrs| {
            attrs
                .with_position(winit::dpi::PhysicalPosition::new(-32000, -32000))
                .with_active(false)
        })
        .select()
        .unwrap();
    let host = TestWindow::new(WS_POPUP);
    let taskbar = Taskbar {
        handle: host.handle(),
        rect: Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 600,
        },
        notify: Rect {
            left: 0,
            top: 500,
            right: 100,
            bottom: 600,
        },
    };
    let ui = TaskbarBar::new().unwrap();
    let bar = TaskbarView::new(ui.clone_strong());
    bar.show();
    let flyout = FlyoutWindow::new().unwrap();
    let step = Rc::new(Cell::new(0_usize));
    let failure = Rc::new(RefCell::new(None));
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), {
        let bar = Rc::clone(&bar);
        let step = Rc::clone(&step);
        let failure = Rc::clone(&failure);
        let flyout = flyout.clone_strong();
        move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert!(
                    bar.attach(Some(taskbar)),
                    "attach at step {}, handle {:?}, native {:?}",
                    step.get(),
                    bar.window_handle(),
                    ui.window()
                        .with_winit_window(|window| format!("{:?}", window.id()))
                );
                assert_readout(hwnd(bar.window_handle().unwrap()), true);
                let n = step.get();
                ui.window()
                    .with_winit_window(|window| {
                        window.set_resizable(n.is_multiple_of(2));
                        window.set_window_level(if n.is_multiple_of(2) {
                            winit::window::WindowLevel::Normal
                        } else {
                            winit::window::WindowLevel::AlwaysOnTop
                        });
                    })
                    .unwrap();
                assert_readout(hwnd(bar.window_handle().unwrap()), true);
                let mut chips = vec![Chip {
                    agent: Some(HookSource::Codex),
                    value: "28%".into(),
                    tier: "warn",
                    tasks: AgentTasks {
                        running: n % 3,
                        done: n % 2,
                        pending: 0,
                    },
                }];
                // Activity can hide an agent and a later hook brings it back.
                if !n.is_multiple_of(3) {
                    chips.insert(
                        0,
                        Chip {
                            agent: Some(HookSource::Claude),
                            value: "23%".into(),
                            tier: "warn",
                            tasks: AgentTasks::default(),
                        },
                    );
                }
                bar.set_chips(
                    &chips,
                    if n.is_multiple_of(2) {
                        Along::Vertical
                    } else {
                        Along::Horizontal
                    },
                );
                if n.is_multiple_of(2) {
                    flyout.show().unwrap();
                } else {
                    flyout.hide().unwrap();
                }
                if n % 4 == 3 {
                    bar.hide();
                    bar.show();
                }
                step.set(n + 1);
            }));
            if let Err(error) = result {
                *failure.borrow_mut() = Some(error);
            }
            if failure.borrow().is_some() || step.get() == 24 {
                slint::quit_event_loop().unwrap();
            }
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    bar.hide();
    flyout.hide().unwrap();
    if let Some(error) = failure.borrow_mut().take() {
        std::panic::resume_unwind(error);
    }
    assert_eq!(step.get(), 24);
}
