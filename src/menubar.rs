#[cfg(target_os = "macos")]
mod mac {
    #![allow(unexpected_cfgs, unsafe_op_in_unsafe_fn)]

    use std::cell::RefCell;
    use std::rc::{Rc, Weak};

    use async_channel::Sender;
    use cocoa::{
        appkit::{
            NSApp, NSButton, NSEventMask, NSMenu, NSMenuItem, NSSquareStatusItemLength,
            NSStatusBar, NSStatusItem, NSView,
        },
        base::{NO, YES, id, nil},
        foundation::{NSPoint, NSRect, NSSize, NSString},
    };
    use objc::{
        class,
        declare::ClassDecl,
        msg_send,
        rc::StrongPtr,
        runtime::{Class, Object, Sel},
        sel, sel_impl,
    };

    use crate::service::{CommandEnvelope, ServiceCommand};

    thread_local! {
        static STATUS_ITEM: RefCell<Option<StatusItemHandle>> = const { RefCell::new(None) };
    }

    static mut VIEW_CLASS: *const Class = core::ptr::null();
    const STATE_IVAR: &str = "state";

    pub fn install(commands: Sender<CommandEnvelope>) {
        STATUS_ITEM.with(|slot| {
            if slot.borrow().is_none() {
                slot.borrow_mut()
                    .replace(unsafe { StatusItemHandle::new(commands) });
            }
        });
    }

    struct StatusItemHandle {
        _native_item: StrongPtr,
        _target: StrongPtr,
        _state: Rc<RefCell<StatusItemState>>,
    }

    struct StatusItemState {
        commands: Sender<CommandEnvelope>,
        status_item: id,
    }

    impl StatusItemHandle {
        unsafe fn new(commands: Sender<CommandEnvelope>) -> Self {
            ensure_view_class();

            let status_bar = NSStatusBar::systemStatusBar(nil);
            let native_item =
                StrongPtr::retain(status_bar.statusItemWithLength_(NSSquareStatusItemLength));
            let button = native_item.button();
            let img_size = NSSize::new(18.0, 18.0);
            let image: id = msg_send![class!(NSImage), alloc];
            let image: id = msg_send![image, initWithSize: img_size];
            let _: () = msg_send![image, lockFocus];
            let font: id = msg_send![class!(NSFont), systemFontOfSize: 12.0_f64];
            let attrs: id = msg_send![class!(NSMutableDictionary), dictionary];
            let _: () =
                msg_send![attrs, setObject: font forKey: NSString::alloc(nil).init_str("NSFont")];
            let alloc: id = msg_send![class!(NSAttributedString), alloc];
            let attr_str: id = msg_send![
                alloc,
                initWithString: NSString::alloc(nil).init_str("🪿")
                attributes: attrs
            ];
            let text_size: NSSize = msg_send![attr_str, size];
            let draw_point = NSPoint::new(
                (img_size.width - text_size.width) / 2.0,
                (img_size.height - text_size.height) / 2.0,
            );
            let _: () = msg_send![attr_str, drawAtPoint: draw_point];
            let _: () = msg_send![image, unlockFocus];
            let _: () = msg_send![image, setTemplate: YES];
            let _: () = msg_send![button, setImage: image];
            let _: () = msg_send![button, setToolTip: NSString::alloc(nil).init_str("fff-gpui")];

            let state = Rc::new(RefCell::new(StatusItemState {
                commands,
                status_item: *native_item,
            }));
            let target: id = msg_send![VIEW_CLASS, alloc];
            NSView::initWithFrame_(
                target,
                NSRect::new(NSPoint::new(0., 0.), button.frame().size),
            );
            (*target).set_ivar(
                STATE_IVAR,
                Weak::into_raw(Rc::downgrade(&state)) as *const core::ffi::c_void,
            );
            NSButton::setTarget_(button, target);
            button.setAction_(sel!(statusItemClicked:));
            let _: () = msg_send![
                button,
                sendActionOn: NSEventMask::NSLeftMouseUpMask | NSEventMask::NSRightMouseUpMask
            ];

            Self {
                _native_item: native_item,
                _target: StrongPtr::new(target),
                _state: state,
            }
        }
    }

    unsafe fn ensure_view_class() {
        if VIEW_CLASS.is_null() {
            let mut decl = ClassDecl::new("FFFStatusItemView", class!(NSView))
                .expect("failed to declare status item view class");
            decl.add_ivar::<*mut core::ffi::c_void>(STATE_IVAR);
            decl.add_method(sel!(dealloc), dealloc_view as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(statusItemClicked:),
                status_item_clicked as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(openMenuItemClicked:),
                open_menu_item_clicked as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(configMenuItemClicked:),
                config_menu_item_clicked as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(quitMenuItemClicked:),
                quit_menu_item_clicked as extern "C" fn(&Object, Sel, id),
            );
            VIEW_CLASS = decl.register();
        }
    }

    extern "C" fn status_item_clicked(this: &Object, _: Sel, _: id) {
        unsafe {
            if let Some(state) = get_state(this).upgrade() {
                let state_ref = state.borrow();
                let commands = state_ref.commands.clone();
                let event: id = msg_send![NSApp(), currentEvent];
                let is_right_click = if event == nil {
                    false
                } else {
                    let button_number: isize = msg_send![event, buttonNumber];
                    let event_type: usize = msg_send![event, type];
                    button_number != 0 || event_type == 4
                };

                if is_right_click {
                    let menu = build_menu(this);
                    let _: () = msg_send![state_ref.status_item, popUpStatusItemMenu: menu];
                } else {
                    let _ = commands.send_blocking((ServiceCommand::ToggleWindow, None));
                }
            }
        }
    }

    extern "C" fn open_menu_item_clicked(this: &Object, _: Sel, _: id) {
        unsafe {
            if let Some(state) = get_state(this).upgrade() {
                let commands = state.borrow().commands.clone();
                let _ = commands.send_blocking((ServiceCommand::ToggleWindow, None));
            }
        }
    }

    extern "C" fn config_menu_item_clicked(this: &Object, _: Sel, _: id) {
        unsafe {
            if let Some(state) = get_state(this).upgrade() {
                let commands = state.borrow().commands.clone();
                let _ = commands.send_blocking((ServiceCommand::OpenConfig, None));
            }
        }
    }

    extern "C" fn quit_menu_item_clicked(this: &Object, _: Sel, _: id) {
        unsafe {
            if let Some(state) = get_state(this).upgrade() {
                let commands = state.borrow().commands.clone();
                let _ = commands.send_blocking((ServiceCommand::Quit, None));
            }
        }
    }

    unsafe fn build_menu(this: &Object) -> id {
        let menu = NSMenu::new(nil);
        menu.setAutoenablesItems(NO);
        let open_title = NSString::alloc(nil).init_str("Open");
        let config_title = NSString::alloc(nil).init_str("Config");
        let quit_title = NSString::alloc(nil).init_str("Stop service");
        let empty = NSString::alloc(nil).init_str("");
        let open_item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
            open_title,
            sel!(openMenuItemClicked:),
            empty,
        );
        NSMenuItem::setTarget_(open_item, this as *const _ as id);
        let config_item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
            config_title,
            sel!(configMenuItemClicked:),
            empty,
        );
        NSMenuItem::setTarget_(config_item, this as *const _ as id);
        let quit_item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
            quit_title,
            sel!(quitMenuItemClicked:),
            empty,
        );
        NSMenuItem::setTarget_(quit_item, this as *const _ as id);

        menu.addItem_(open_item);
        menu.addItem_(config_item);
        menu.addItem_(quit_item);
        menu
    }

    extern "C" fn dealloc_view(this: &Object, _: Sel) {
        unsafe {
            drop_state(this);
            let _: () = msg_send![super(this, class!(NSView)), dealloc];
        }
    }

    unsafe fn get_state(object: &Object) -> Weak<RefCell<StatusItemState>> {
        let raw: *mut core::ffi::c_void = *object.get_ivar(STATE_IVAR);
        let weak1 = Weak::from_raw(raw as *mut RefCell<StatusItemState>);
        let weak2 = weak1.clone();
        let _ = Weak::into_raw(weak1);
        weak2
    }

    unsafe fn drop_state(object: &Object) {
        let raw: *const core::ffi::c_void = *object.get_ivar(STATE_IVAR);
        Weak::from_raw(raw as *const RefCell<StatusItemState>);
    }
}

#[cfg(target_os = "macos")]
pub fn stop_service() {
    use tracing::warn;

    let brew = BREW_PRIMARY;
    match std::process::Command::new(brew)
        .args(["services", "stop", "fff-gpui"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            warn!(?status, "brew services stop exited unsuccessfully");
        }
        Err(err) => {
            warn!(error = %err, "failed to run brew services stop");
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BREW_PRIMARY: &str = "/opt/homebrew/bin/brew";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const BREW_PRIMARY: &str = "/usr/local/bin/brew";

#[cfg(not(target_os = "macos"))]
pub fn stop_service() {}

#[cfg(target_os = "macos")]
pub use mac::install;

#[cfg(not(target_os = "macos"))]
pub fn install(_: async_channel::Sender<crate::service::CommandEnvelope>) {}
