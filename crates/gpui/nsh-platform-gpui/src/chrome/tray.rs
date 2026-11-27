//! Native menu bar status item for the macOS GPUI runner. An embedded view
//! never creates one, even if it owns its window.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

use nsh_core::{
    EventKind, EventPayload, ImageSource, PayloadValue, ROOT_ID, ResizeMode,
    color::Rgba,
    desktop::{
        Key, MenuItem, MenuRole, Shortcut, Tray, TrayTitlePlacement, TrayTitleRun, menu_icon,
    },
    geometry::{Bounds, DevicePixels, Points, point, size},
    image::Want,
};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
    rc::Retained, runtime::NSObject, sel,
};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBitmapFormat, NSBitmapImageRep, NSCellImagePosition, NSColor,
    NSDeviceRGBColorSpace, NSEventMask, NSEventModifierFlags, NSEventType, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSMenu, NSMenuItem, NSScreen,
    NSStatusBar, NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSCopying, NSMutableAttributedString, NSRange, NSSize, NSString};

use crate::{
    image::{self, Load, MenuIcon},
    rt,
    surface::Surface,
};

const SIDE: Points = Points(18.0);

#[derive(Default)]
pub(super) struct RunnerTray(RefCell<Option<Item>>);

struct Item {
    status: Retained<NSStatusItem>,
    // AppKit does not retain the button target.
    _target: Retained<Target>,
    click: u32,
    menu_items: Arc<[MenuItem]>,
    menu: Option<Rc<NativeMenu>>,
    icon: Option<Option<ImageSource>>,
    icon_load: Option<Load>,
}

impl Drop for Item {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.status);
    }
}

impl RunnerTray {
    pub(super) fn clear(&self) {
        let item = self.0.borrow_mut().take();
        drop(item);
    }

    pub(super) fn set(&self, surface: &Rc<Surface>, tray: Option<&Tray>) {
        let Some(tray) = tray else {
            self.clear();
            return;
        };
        let mtm = MainThreadMarker::new().expect("the GPUI runner is on the main thread");
        let mut current = self.0.borrow_mut();
        let item = current.get_or_insert_with(|| {
            let status =
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            let target = Target::new(mtm, Action::Button(Rc::downgrade(surface)));
            if let Some(button) = status.button(mtm) {
                // SAFETY: the target answers trayClicked: and is retained by the item.
                unsafe {
                    button.setTarget(Some(&target));
                    button.setAction(Some(sel!(trayClicked:)));
                }
                button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
                button.setImage(app_icon(mtm).as_deref());
            }
            Item {
                status,
                _target: target,
                click: 0,
                menu_items: Arc::default(),
                menu: None,
                icon: None,
                icon_load: None,
            }
        });
        item.click = tray.click;
        item.menu_items = tray.menu.clone();
        let tooltip = if tray.tooltip.is_empty() {
            surface.chrome.app_name.borrow().clone()
        } else {
            tray.tooltip.clone()
        };
        if let Some(button) = item.status.button(mtm) {
            let tooltip = NSString::from_str(&tooltip);
            button.setToolTip(Some(&tooltip));
            button.setAccessibilityLabel(Some(&tooltip));
            set_title(&button, &tray.title, tray.title_placement);
            if item.icon.as_ref() != Some(&tray.icon) {
                item.icon_load = None;
                item.icon = Some(tray.icon.clone());
                item.icon_load = set_icon(surface, mtm, &button, tray.icon.as_ref());
            }
        }
        item.rebuild_menu(surface, mtm);
    }

    pub(super) fn relabel(&self, surface: &Rc<Surface>) {
        if let Some(item) = self.0.borrow_mut().as_mut() {
            let mtm = MainThreadMarker::new().expect("the GPUI runner is on the main thread");
            item.rebuild_menu(surface, mtm);
        }
    }
}

impl Item {
    fn rebuild_menu(&mut self, surface: &Rc<Surface>, mtm: MainThreadMarker) {
        let menu = (!self.menu_items.is_empty())
            .then(|| Rc::new(NativeMenu::new(surface, mtm, &self.menu_items)));
        self.status.setMenu(if self.click == 0 {
            menu.as_ref().map(|m| &*m.menu)
        } else {
            None
        });
        self.menu = menu;
    }
}

fn click(surface: &Rc<Surface>, mtm: MainThreadMarker, secondary: bool) {
    let Some(tray) = surface.chrome.runner_tray.get() else {
        return;
    };
    let Some((status, menu, id)) = tray
        .0
        .borrow()
        .as_ref()
        .map(|item| (item.status.clone(), item.menu.clone(), item.click))
    else {
        return;
    };
    if secondary {
        let Some(menu) = menu else { return };
        // Menu tracking can run other events. Keep the menu and its targets
        // alive, but don't hold a tray borrow across the tracking loop.
        status.setMenu(Some(&menu.menu));
        if let Some(button) = status.button(mtm) {
            // SAFETY: AppKit accepts a nil sender for a programmatic click.
            unsafe { button.performClick(None) };
        }
        let current = tray.0.borrow();
        if let Some(item) = current
            .as_ref()
            .filter(|item| std::ptr::eq(&*status, &*item.status))
        {
            status.setMenu(if item.click == 0 {
                item.menu.as_ref().map(|m| &*m.menu)
            } else {
                None
            });
        }
    } else if id != 0 {
        let mut payload = vec![("id", PayloadValue::Number(f64::from(id)))];
        if let Some(bounds) = bounds(&status, mtm) {
            payload.push(("bounds", PayloadValue::Bounds(bounds)));
        }
        surface.event(ROOT_ID, EventKind::Menu, EventPayload::Map(payload));
    }
}

fn bounds(status: &NSStatusItem, mtm: MainThreadMarker) -> Option<Bounds<Points>> {
    let frame = status.button(mtm)?.window()?.frame();
    let height = NSScreen::screens(mtm).firstObject()?.frame().size.height;
    Some(Bounds {
        origin: point(
            Points(frame.origin.x as f32),
            Points((height - frame.origin.y - frame.size.height) as f32),
        ),
        size: size(
            Points(frame.size.width as f32),
            Points(frame.size.height as f32),
        ),
    })
}

struct NativeMenu {
    menu: Retained<NSMenu>,
    token: Rc<()>,
    _loads: Vec<Load>,
}

impl NativeMenu {
    fn new(surface: &Rc<Surface>, mtm: MainThreadMarker, items: &[MenuItem]) -> Self {
        let token = Rc::new(());
        let mut entries = Vec::new();
        let mut loads = Vec::new();
        let target = Target::new(
            mtm,
            Action::Menu {
                surface: Rc::downgrade(surface),
                token: Rc::downgrade(&token),
                entries: RefCell::new(Vec::new()),
            },
        );
        let menu = build_menu(surface, &target, items, mtm, &mut entries, &mut loads);
        if let Action::Menu {
            entries: actions, ..
        } = target.ivars()
        {
            *actions.borrow_mut() = entries;
        }
        Self {
            menu,
            token,
            _loads: loads,
        }
    }
}

fn build_menu(
    surface: &Surface,
    target: &Target,
    items: &[MenuItem],
    mtm: MainThreadMarker,
    entries: &mut Vec<(u32, Option<MenuRole>)>,
    loads: &mut Vec<Load>,
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let mut separator = true;
    for item in items {
        let (label, enabled, icon, action) = match item {
            MenuItem::Separator => {
                if !separator {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                }
                separator = true;
                continue;
            }
            MenuItem::Action(action) => (
                super::menus::label(action, surface),
                action.enabled,
                action.icon.as_ref(),
                Some(action),
            ),
            MenuItem::Submenu {
                label,
                enabled,
                icon,
                ..
            } => (label.clone(), *enabled, icon.as_ref(), None),
        };
        separator = false;
        let (key, mask) = action
            .and_then(|a| a.shortcut.as_ref())
            .map(key_equivalent)
            .unwrap_or_else(|| (String::new(), NSEventModifierFlags::empty()));
        let handles_action = action.is_some_and(|a| a.id != 0 || a.role.is_some());
        // SAFETY: trayMenuSelected: receives the item and its target implements it.
        let native = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&label),
                handles_action.then_some(sel!(trayMenuSelected:)),
                &NSString::from_str(&key),
            )
        };
        if let Some(action) = action {
            if handles_action {
                native.setTag(entries.len() as isize);
                entries.push((action.id, action.role));
                // SAFETY: the target implements the selector. A tracked menu can
                // outlive its tray configuration, so each item retains its target.
                unsafe {
                    native.setRepresentedObject(Some(target));
                    native.setTarget(Some(target));
                }
            }
            if let Some(checked) = action.checked {
                native.setState(isize::from(checked));
            }
            native.setKeyEquivalentModifierMask(mask);
        }
        if let MenuItem::Submenu { items, .. } = item {
            native.setSubmenu(Some(&build_menu(
                surface, target, items, mtm, entries, loads,
            )));
        }
        native.setEnabled(enabled);
        if let Some(icon) = icon {
            let native = native.clone();
            let side = menu_icon::SIDE;
            loads.extend(image::load_menu_icon(
                surface,
                icon,
                icon_scale(mtm),
                move |result| {
                    if let Some(icon) = result.ok().and_then(|icon| native_image(&icon, side)) {
                        native.setImage(Some(&icon));
                    }
                },
            ));
        }
        menu.addItem(&native);
    }
    menu
}

enum Action {
    Button(Weak<Surface>),
    Menu {
        surface: Weak<Surface>,
        token: Weak<()>,
        entries: RefCell<Vec<(u32, Option<MenuRole>)>>,
    },
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Action]
    #[name = "NshGpuiTrayTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(trayClicked:))]
        fn clicked(&self, _sender: Option<&NSObject>) {
            let Action::Button(surface) = self.ivars() else { return };
            let mtm = self.mtm();
            let secondary = NSApplication::sharedApplication(mtm).currentEvent().is_some_and(|event| {
                matches!(event.r#type(), NSEventType::RightMouseDown | NSEventType::RightMouseUp)
                    || event.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if let Some(surface) = surface.upgrade() { click(&surface, mtm, secondary); }
        }

        #[unsafe(method(trayMenuSelected:))]
        fn selected(&self, sender: &NSMenuItem) {
            let Action::Menu { surface, token, entries } = self.ivars() else { return };
            let Some(&(id, role)) = entries.borrow().get(sender.tag() as usize) else { return };
            let surface = surface.clone();
            let token = token.clone();
            rt::on_app(move |_| {
                let (Some(surface), Some(token)) = (surface.upgrade(), token.upgrade()) else { return };
                let active = surface.chrome.runner_tray.get().is_some_and(|tray| {
                    tray.0.borrow().as_ref().and_then(|item| item.menu.as_ref())
                        .is_some_and(|menu| Rc::ptr_eq(&menu.token, &token))
                });
                if active { super::choose(&surface, id, role); }
            });
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker, action: Action) -> Retained<Self> {
        // SAFETY: NSObject's initializer on our allocated target.
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(action)), init] }
    }
}

fn icon_scale(mtm: MainThreadMarker) -> f32 {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| screen.backingScaleFactor() as f32)
        .fold(1.0, f32::max)
}

fn app_icon(mtm: MainThreadMarker) -> Option<Retained<NSImage>> {
    let icon = NSApplication::sharedApplication(mtm)
        .applicationIconImage()?
        .copy();
    icon.setSize(NSSize::new(f64::from(SIDE.0), f64::from(SIDE.0)));
    Some(icon)
}

fn set_icon(
    surface: &Surface,
    mtm: MainThreadMarker,
    button: &NSStatusBarButton,
    source: Option<&ImageSource>,
) -> Option<Load> {
    let Some(source) = source else {
        button.setImage(app_icon(mtm).as_deref());
        return None;
    };
    let uri = source.uri.clone();
    let button = button.retain();
    image::load_sized_icon(
        surface,
        source,
        Want {
            size: size(SIDE, SIDE),
            scale: icon_scale(mtm),
            mode: ResizeMode::Contain,
        },
        move |result| match result {
            Ok(icon) => {
                if let Some(icon) = native_image(&icon, SIDE) {
                    button.setImage(Some(&icon));
                }
            }
            Err(err) => log::warn!("tray icon {uri}: {err}"),
        },
    )
}

fn native_image(icon: &MenuIcon, side: Points) -> Option<Retained<NSImage>> {
    let (pixels, template) = icon.native_pixels()?;
    let (w, h) = icon.size();
    let row = isize::try_from(w).ok()?.checked_mul(4)?;
    let len = usize::try_from(row).ok()?.checked_mul(h as usize)?;
    if len == 0 || pixels.len() != len {
        return None;
    }
    // SAFETY: nil planes make AppKit allocate its own packed RGBA storage.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), w as isize, h as isize, 8, 4, true, false,
            NSDeviceRGBColorSpace, NSBitmapFormat::AlphaNonpremultiplied, row, 32,
        )?
    };
    let data = rep.bitmapData();
    if data.is_null() || rep.bytesPerRow() != row {
        return None;
    }
    // SAFETY: the rep owns exactly h rows of row bytes. Only we hold it here.
    let dest = unsafe { std::slice::from_raw_parts_mut(data, len) };
    for (&[b, g, r, a], rgba) in pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dest.as_chunks_mut::<4>().0)
    {
        *rgba = [r, g, b, a];
    }
    let fitted = menu_icon::fitted(size(DevicePixels(w as i32), DevicePixels(h as i32)), side);
    let fitted = NSSize::new(f64::from(fitted.width.0), f64::from(fitted.height.0));
    rep.setSize(fitted);
    let image = NSImage::initWithSize(NSImage::alloc(), fitted);
    image.addRepresentation(&rep);
    image.setTemplate(template);
    Some(image)
}

fn set_title(button: &NSStatusBarButton, title: &[TrayTitleRun], position: TrayTitlePlacement) {
    if title.is_empty() {
        button.setTitle(&NSString::new());
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        return;
    }
    let whole: String = title.iter().map(|run| run.text.as_str()).collect();
    let text = NSMutableAttributedString::from_nsstring(&NSString::from_str(&whole));
    let units = |s: &str| s.encode_utf16().count();
    // SAFETY: the font is the menu bar's own; the attribute's value is an
    // `NSFont`, the ranges are within the string.
    unsafe {
        text.addAttribute_value_range(
            NSFontAttributeName,
            &NSFont::menuBarFontOfSize(0.0),
            NSRange::new(0, units(&whole)),
        );
    }
    let mut at = 0;
    for run in title {
        let len = units(&run.text);
        // An attributed title draws in black where it names no color, not
        // in the menu bar's text color: `labelColor` is that color, resolved
        // for the bar's light or dark appearance as it draws.
        let color = run.color.map_or_else(NSColor::labelColor, |color| {
            let Rgba { r, g, b, a } = Rgba::from(color);
            NSColor::colorWithSRGBRed_green_blue_alpha(
                f64::from(r),
                f64::from(g),
                f64::from(b),
                f64::from(a),
            )
        });
        // SAFETY: the attribute's value is an `NSColor`, the range within the
        // string.
        unsafe {
            text.addAttribute_value_range(
                NSForegroundColorAttributeName,
                &color,
                NSRange::new(at, len),
            );
        }
        at += len;
    }
    button.setAttributedTitle(&text);
    // The image's side is the other one: `ImageRight` puts the title left.
    button.setImagePosition(match position {
        TrayTitlePlacement::Right => NSCellImagePosition::ImageLeft,
        TrayTitlePlacement::Left => NSCellImagePosition::ImageRight,
    });
}

fn key_equivalent(shortcut: &Shortcut) -> (String, NSEventModifierFlags) {
    let function = |code: u32| char::from_u32(code).map(String::from).unwrap_or_default();
    let key = match shortcut.key {
        Key::Char(c) => c.to_string(),
        Key::Enter => "\r".to_owned(),
        Key::Escape => "\u{1b}".to_owned(),
        Key::Tab => "\t".to_owned(),
        Key::Backspace => "\u{8}".to_owned(),
        Key::Space => " ".to_owned(),
        // AppKit's function key characters (NSUpArrowFunctionKey, ...).
        Key::Up => function(0xF700),
        Key::Down => function(0xF701),
        Key::Left => function(0xF702),
        Key::Right => function(0xF703),
        Key::F(n) => function(0xF704 + u32::from(n) - 1),
        Key::Insert => function(0xF727),
        Key::Delete => function(0xF728),
        Key::Home => function(0xF729),
        Key::End => function(0xF72B),
        Key::PageUp => function(0xF72C),
        Key::PageDown => function(0xF72D),
    };
    let mut mask = NSEventModifierFlags::empty();
    for (on, flag) in [
        (shortcut.meta, NSEventModifierFlags::Command),
        (shortcut.ctrl, NSEventModifierFlags::Control),
        (shortcut.alt, NSEventModifierFlags::Option),
        (shortcut.shift, NSEventModifierFlags::Shift),
    ] {
        if on {
            mask |= flag;
        }
    }
    (key, mask)
}
