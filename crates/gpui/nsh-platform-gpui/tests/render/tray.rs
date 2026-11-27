//! Opt-in test of the real macOS runner, on the process main thread.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use nsh_core::{
    desktop::TrayTitleRun,
    element::{
        desktop::{current_window, quit, set_tray, tray},
        run_on_main_thread, spawn, timer,
    },
    prelude::*,
};
use nsh_platform_gpui::{AppOptions, AppSource, Root};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSStatusBarButton, NSView};

static PASSED: AtomicBool = AtomicBool::new(false);
static CLICKED: AtomicBool = AtomicBool::new(false);
const TITLE: &str = "NSH GPUI Tray Test";

// AppKit has no public list of status items. Like the native backend's tray
// test, count visible status bar windows of this process instead.
fn status_windows() -> Vec<isize> {
    let mtm = MainThreadMarker::new().unwrap();
    NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .filter(|window| {
            window.isVisible()
                && window
                    .class()
                    .name()
                    .to_string_lossy()
                    .contains("StatusBar")
        })
        .map(|window| window.windowNumber())
        .collect()
}

async fn expect_status_windows(expected: usize) -> Vec<isize> {
    for _ in 0..100 {
        let windows = run_on_main_thread(status_windows).await.unwrap();
        if windows.len() == expected {
            return windows;
        }
        timer(Duration::from_millis(30)).await;
    }
    panic!("expected {expected} visible AppKit status bar windows");
}

fn activate_status_button(number: isize) {
    fn activate(view: &NSView) -> bool {
        if let Some(button) = view.downcast_ref::<NSStatusBarButton>() {
            let (Some(action), Some(target)) = (button.action(), button.target()) else {
                return false;
            };
            // SAFETY: send the installed action to its actual, retained target.
            return unsafe { button.sendAction_to(Some(action), Some(&target)) };
        }
        view.subviews().iter().any(|child| activate(&child))
    }

    let mtm = MainThreadMarker::new().unwrap();
    let window = NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .find(|window| window.windowNumber() == number)
        .expect("status bar window disappeared");
    assert!(activate(&window.contentView().unwrap()));
}

#[component]
fn TrayApp() -> impl IntoElement {
    let window = current_window();
    spawn(async move {
        expect_status_windows(0).await;
        window.hide();
        set_tray(tray().title([TrayTitleRun::new("First")]));
        let first = expect_status_windows(1).await;
        set_tray(
            tray()
                .title([TrayTitleRun::new("Updated")])
                .on_click(|event| {
                    assert!(event.bounds.is_some());
                    CLICKED.store(true, Ordering::SeqCst);
                }),
        );
        assert_eq!(expect_status_windows(1).await, first);
        run_on_main_thread(move || activate_status_button(first[0]))
            .await
            .unwrap();
        for _ in 0..100 {
            if CLICKED.load(Ordering::SeqCst) {
                break;
            }
            timer(Duration::from_millis(30)).await;
        }
        assert!(
            CLICKED.load(Ordering::SeqCst),
            "the native action was not returned to the app"
        );
        set_tray(None);
        expect_status_windows(0).await;
        PASSED.store(true, Ordering::SeqCst);
        // GPUI may terminate the process without returning from run_app.
        println!("tray: create, update, activate and clear while the runner window is hidden: ok");
        quit();
    });
    view().child(text(TITLE))
}

pub(super) fn run() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(15));
        eprintln!("tray: timed out before the runner exited");
        std::process::exit(1);
    });
    let code = nsh_platform_gpui::run_app(
        AppSource::Rust(Root::of::<TrayApp>()),
        AppOptions {
            title: TITLE.into(),
            background: true,
            ..super::render_options()
        },
    );
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(PASSED.load(Ordering::SeqCst));
}
