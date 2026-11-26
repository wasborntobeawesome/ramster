//! ramster-bar: a native macOS menu bar app (AppKit via objc2, no SwiftUI, no webview).
//!
//! Efficiency model:
//! - Idle: a coalesced 15 s timer reads system memory (one host_statistics64 call) to
//!   update the title. No process scanning.
//! - Menu open: one process scan (~20 ms). CPU is measured against the previous scan,
//!   so there is no sleep and no lag.
//! - Freeing runs on a background thread; the UI never blocks on it.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ramster-bar is macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    bar::run();
}

#[cfg(target_os = "macos")]
mod bar {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject, Sel};
    use objc2::{
        define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
        NSApplicationActivationPolicy, NSColor, NSFont, NSFontAttributeName,
        NSForegroundColorAttributeName, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
        NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSStatusBar, NSStatusItem,
        NSTextAlignment, NSTextTab, NSVariableStatusItemLength,
    };
    use objc2_foundation::{
        NSArray, NSAttributedString, NSAttributedStringKey, NSDictionary, NSObject,
        NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer,
    };
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use ramster::analyze::rules::{Finding, CONFIRM, INFO, SAFE};
    use ramster::analyze::{act, report};
    use ramster::config::Config;
    use ramster::util::{fmt_gb, fmt_mb, now_unix, run as run_cmd};
    use ramster::{daemon, history, mcp, probe};

    const TAB_STOP: f64 = 300.0;
    const TOP_APPS: usize = 8;

    #[derive(Default)]
    struct State {
        baseline: Option<(Vec<probe::Proc>, Instant)>,
        findings: Vec<Finding>,
    }

    pub struct Ivars {
        item: RefCell<Option<Retained<NSStatusItem>>>,
        state: RefCell<State>,
        busy: Arc<AtomicBool>,
        last_msg: Arc<Mutex<Option<String>>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "RamsterBar"]
        #[ivars = Ivars]
        pub struct Bar;

        impl Bar {
            #[unsafe(method(tick:))]
            fn tick(&self, _timer: &NSTimer) {
                self.refresh_title();
            }

            #[unsafe(method(freeSafe:))]
            fn free_safe(&self, _sender: &NSMenuItem) {
                self.spawn_free(vec!["safe".into()], "safe items".into());
            }

            #[unsafe(method(freeOne:))]
            fn free_one(&self, sender: &NSMenuItem) {
                let f = {
                    let st = self.ivars().state.borrow();
                    match st.findings.get(sender.tag() as usize) {
                        Some(f) => f.clone(),
                        None => return,
                    }
                };
                if f.safety == CONFIRM && !self.confirm(&f) {
                    return;
                }
                self.spawn_free(vec![f.id.clone()], f.title.clone());
            }

            #[unsafe(method(showReport:))]
            fn show_report(&self, _sender: &NSMenuItem) {
                self.report_alert();
            }

            #[unsafe(method(toggleSampler:))]
            fn toggle_sampler(&self, _sender: &NSMenuItem) {
                self.toggle_agent();
            }

            #[unsafe(method(toggleLoginItem:))]
            fn toggle_login_item(&self, _sender: &NSMenuItem) {
                self.toggle_login();
            }

            #[unsafe(method(openMenu:))]
            fn open_menu(&self, _timer: &NSTimer) {
                // Self-test (RAMSTER_BAR_SELFTEST=n): open the menu, close it 3 s
                // later from a common-modes timer, repeat n times.
                let close = unsafe {
                    NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(3.0, self.target(), sel!(closeMenu:), None, false)
                };
                unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&close, NSRunLoopCommonModes) };
                if let Some(item) = self.ivars().item.borrow().as_ref() {
                    if let Some(button) = item.button(self.mtm()) {
                        unsafe { button.performClick(None) };
                    }
                }
            }

            #[unsafe(method(closeMenu:))]
            fn close_menu(&self, _timer: &NSTimer) {
                if let Some(item) = self.ivars().item.borrow().as_ref() {
                    if let Some(menu) = item.menu(self.mtm()) {
                        menu.cancelTracking();
                    }
                }
                let left: u32 = std::env::var("RAMSTER_BAR_SELFTEST").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                if left > 1 {
                    std::env::set_var("RAMSTER_BAR_SELFTEST", (left - 1).to_string());
                    unsafe {
                        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0, self.target(), sel!(openMenu:), None, false)
                    };
                } else {
                    std::env::remove_var("RAMSTER_BAR_SELFTEST");
                }
            }

            #[unsafe(method(quit:))]
            fn quit(&self, _sender: &NSMenuItem) {
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
        }

        unsafe impl NSObjectProtocol for Bar {}

        unsafe impl NSMenuDelegate for Bar {
            #[unsafe(method(menuNeedsUpdate:))]
            fn menu_needs_update(&self, menu: &NSMenu) {
                self.rebuild(menu);
            }
        }
    );

    // ---- helpers ------------------------------------------------------------

    fn ns(s: &str) -> Retained<NSString> {
        NSString::from_str(s)
    }

    fn item(
        mtm: MainThreadMarker,
        title: &str,
        action: Option<Sel>,
        key: &str,
    ) -> Retained<NSMenuItem> {
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &ns(title),
                action,
                &ns(key),
            )
        }
    }

    /// "left<TAB>right" with a right-aligned tab stop so sizes line up.
    fn styled(
        text: &str,
        color: Option<&NSColor>,
        bold: bool,
        size: f64,
    ) -> Retained<NSAttributedString> {
        let style = NSMutableParagraphStyle::new();
        let tab = unsafe {
            NSTextTab::initWithTextAlignment_location_options(
                NSTextTab::alloc(),
                NSTextAlignment::Right,
                TAB_STOP,
                &NSDictionary::new(),
            )
        };
        style.setTabStops(Some(&NSArray::from_retained_slice(&[tab])));
        let font = if bold {
            NSFont::boldSystemFontOfSize(size)
        } else {
            NSFont::menuFontOfSize(size)
        };
        let mut keys: Vec<&NSAttributedStringKey> =
            unsafe { vec![NSParagraphStyleAttributeName, NSFontAttributeName] };
        let mut vals: Vec<&AnyObject> = vec![&style, &font];
        if let Some(c) = color {
            keys.push(unsafe { NSForegroundColorAttributeName });
            vals.push(c);
        }
        let attrs = NSDictionary::from_slices(&keys, &vals);
        unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &ns(text),
                Some(&attrs),
            )
        }
    }

    fn row(
        mtm: MainThreadMarker,
        left: &str,
        right: &str,
        color: Option<&NSColor>,
    ) -> Retained<NSMenuItem> {
        let it = item(mtm, left, None, "");
        it.setAttributedTitle(Some(&styled(
            &format!("{left}\t{right}"),
            color,
            false,
            0.0,
        )));
        it
    }

    /// "Gradle daemon 9.8.2 · up 35m" so identical titles are distinguishable.
    fn with_age(f: &Finding) -> String {
        match f.detail.split(", ").find_map(|p| p.strip_prefix("up ")) {
            Some(age) => format!("{} · up {age}", f.title),
            None => f.title.clone(),
        }
    }

    /// Short imperative label for a confirm item.
    fn verb(f: &Finding) -> String {
        match f.rule.as_str() {
            "docker" => "Stop Docker Desktop".into(),
            "ios-simulator" => "Shut down iOS Simulators".into(),
            "idle-app" => format!("Quit {}", f.title.trim_end_matches(" is idle")),
            _ => format!("Stop {}", with_age(f)),
        }
    }

    fn pressure_color(level: i32) -> Option<Retained<NSColor>> {
        match level {
            l if l >= 4 => Some(NSColor::systemRedColor()),
            l if l >= 2 => Some(NSColor::systemOrangeColor()),
            _ => None,
        }
    }

    fn notify(title: &str, body: &str) {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let script = format!("display notification {} with title {}", q(body), q(title));
        let _ = run_cmd(Duration::from_secs(10), "osascript", &["-e", &script]);
    }

    /// The CLI binary that should run as the launchd daemon: our sibling in the
    /// app bundle / build dir, else whatever `ramster` is on PATH.
    fn cli_path() -> Option<PathBuf> {
        let sibling = std::env::current_exe().ok()?.with_file_name("ramster");
        if sibling.is_file() {
            return Some(sibling);
        }
        ramster::util::which("ramster", &[]).map(PathBuf::from)
    }

    /// SMAppService.mainAppService only works from an app bundle
    /// (`ramster.app/Contents/MacOS/ramster-bar`), not from target/release.
    fn in_app_bundle() -> bool {
        let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
        else {
            return false;
        };
        let contents = exe.parent().and_then(|m| {
            (m.file_name()? == "MacOS")
                .then_some(m.parent()?)
                .filter(|c| c.file_name().is_some_and(|n| n == "Contents"))
        });
        contents.is_some_and(|c| {
            c.join("Info.plist").is_file()
                && c.parent()
                    .and_then(|a| a.extension())
                    .is_some_and(|x| x == "app")
        })
    }

    /// Launch-at-login status, or None when not running from an app bundle.
    fn login_status() -> Option<SMAppServiceStatus> {
        in_app_bundle().then(|| unsafe { SMAppService::mainAppService().status() })
    }

    impl Bar {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars {
                item: RefCell::new(None),
                state: RefCell::new(State::default()),
                busy: Arc::new(AtomicBool::new(false)),
                last_msg: Arc::new(Mutex::new(None)),
            });
            unsafe { msg_send![super(this), init] }
        }

        fn target(&self) -> &AnyObject {
            self.as_ref()
        }

        fn action_item(&self, title: &str, action: Sel, key: &str) -> Retained<NSMenuItem> {
            let it = item(self.mtm(), title, Some(action), key);
            unsafe { it.setTarget(Some(self.target())) };
            it
        }

        /// Cheap: one host_statistics64 call.
        fn refresh_title(&self) {
            let Ok(m) = probe::memory() else { return };
            let guard = self.ivars().item.borrow();
            let Some(status) = guard.as_ref() else { return };
            let Some(button) = status.button(self.mtm()) else {
                return;
            };
            let busy = self.ivars().busy.load(Ordering::Relaxed);
            let text = format!(
                " {:.0}G{}",
                m.used as f64 / (1u64 << 30) as f64,
                if busy { "…" } else { "" }
            );
            let color = pressure_color(m.pressure);
            let font = NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.0);
            let mut keys: Vec<&NSAttributedStringKey> = unsafe { vec![NSFontAttributeName] };
            let mut vals: Vec<&AnyObject> = vec![&font];
            if let Some(c) = &color {
                keys.push(unsafe { NSForegroundColorAttributeName });
                vals.push(c);
            }
            let attrs = NSDictionary::from_slices(&keys, &vals);
            let title = unsafe {
                NSAttributedString::initWithString_attributes(
                    NSAttributedString::alloc(),
                    &ns(&text),
                    Some(&attrs),
                )
            };
            button.setAttributedTitle(&title);
            button.setToolTip(Some(&ns(&format!(
                "ramster · {} of {} used · {} compressed · pressure {}",
                fmt_gb(m.used),
                fmt_gb(m.total),
                fmt_gb(m.compressed),
                probe::pressure_name(m.pressure)
            ))));
        }

        fn rebuild(&self, menu: &NSMenu) {
            let mtm = self.mtm();
            menu.removeAllItems();
            let started = Instant::now();
            let cfg = Config::load();
            let mut st = self.ivars().state.borrow_mut();
            let result = {
                let base = st.baseline.as_ref().map(|(p, t)| (p.as_slice(), *t));
                mcp::findings_since(&cfg, base)
            };
            let (snap, fs) = match result {
                Ok(v) => v,
                Err(e) => {
                    menu.addItem(&item(mtm, &format!("ramster: {e}"), None, ""));
                    return;
                }
            };
            st.baseline = Some((daemon::slim(&snap.procs), Instant::now()));
            let m = &snap.mem;
            let pcolor = pressure_color(m.pressure);

            // Header
            let head = item(mtm, "", None, "");
            head.setAttributedTitle(Some(&styled(
                &format!(
                    "Memory {} / {}\t{}",
                    fmt_gb(m.used),
                    fmt_gb(m.total),
                    probe::pressure_name(m.pressure)
                ),
                pcolor.as_deref(),
                true,
                0.0,
            )));
            menu.addItem(&head);
            let sub = NSColor::secondaryLabelColor();
            let detail = item(mtm, "", None, "");
            detail.setAttributedTitle(Some(&styled(
                &format!(
                    "Compressed {} · swap {} · free {}",
                    fmt_gb(m.compressed),
                    fmt_gb(m.swap_used),
                    fmt_mb(mcp::headroom_mb(&snap).max(0) as u64)
                ),
                Some(&sub),
                false,
                12.0,
            )));
            menu.addItem(&detail);
            if self.ivars().busy.load(Ordering::Relaxed) {
                menu.addItem(&row(mtm, "Freeing memory…", "", Some(&sub)));
            } else if let Some(msg) = self.ivars().last_msg.lock().ok().and_then(|g| g.clone()) {
                menu.addItem(&row(
                    mtm,
                    &format!("✓ {msg}"),
                    "",
                    Some(&NSColor::systemGreenColor()),
                ));
            }

            // Top apps
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
                &ns("Top memory users"),
                mtm,
            ));
            for g in snap.groups.iter().take(TOP_APPS) {
                menu.addItem(&row(mtm, &g.name, &fmt_mb(g.mb), None));
            }

            // Actions
            let safe: Vec<(usize, &Finding)> = fs
                .iter()
                .enumerate()
                .filter(|(_, f)| f.safety == SAFE && f.actionable())
                .collect();
            let confirm: Vec<(usize, &Finding)> = fs
                .iter()
                .enumerate()
                .filter(|(_, f)| f.safety == CONFIRM && f.actionable())
                .collect();
            let info: Vec<&Finding> = fs
                .iter()
                .filter(|f| f.safety == INFO || !f.actionable())
                .collect();
            let busy = self.ivars().busy.load(Ordering::Relaxed);

            if !safe.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                let total: u64 = safe.iter().map(|(_, f)| f.frees_mb).sum();
                let it = self.action_item("", sel!(freeSafe:), "f");
                it.setAttributedTitle(Some(&styled(
                    &format!("Free {} safely\t{} items", fmt_mb(total), safe.len()),
                    Some(&NSColor::systemGreenColor()),
                    true,
                    0.0,
                )));
                it.setEnabled(!busy);
                it.setToolTip(Some(&ns("Idle build daemons, orphaned MCP/LSP servers and the like. They restart on demand.")));
                menu.addItem(&it);
                for (i, f) in &safe {
                    let r = self.action_item("", sel!(freeOne:), "");
                    r.setAttributedTitle(Some(&styled(
                        &format!("{}\t{}", with_age(f), fmt_mb(f.frees_mb)),
                        Some(&sub),
                        false,
                        0.0,
                    )));
                    r.setIndentationLevel(1);
                    r.setTag(*i as isize);
                    r.setEnabled(!busy);
                    r.setToolTip(Some(&ns(&format!("{}\n{}", f.detail, f.action))));
                    menu.addItem(&r);
                }
            }
            if !confirm.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
                    &ns("Needs your OK"),
                    mtm,
                ));
                for (i, f) in &confirm {
                    let r = self.action_item("", sel!(freeOne:), "");
                    r.setAttributedTitle(Some(&styled(
                        &format!("{}…\t{}", verb(f), fmt_mb(f.frees_mb)),
                        None,
                        false,
                        0.0,
                    )));
                    r.setTag(*i as isize);
                    r.setEnabled(!busy);
                    let mut tip = format!("{}\nAction: {}", f.detail, f.action);
                    if !f.tip.is_empty() {
                        tip += &format!("\n\nTip: {}", f.tip);
                    }
                    r.setToolTip(Some(&ns(&tip)));
                    menu.addItem(&r);
                }
            }
            if !info.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                let adv = item(mtm, &format!("Advice ({})", info.len()), None, "");
                let submenu = NSMenu::new(mtm);
                submenu.setAutoenablesItems(false);
                for f in &info {
                    let r = item(mtm, &f.title, None, "");
                    r.setEnabled(true);
                    if !f.tip.is_empty() {
                        r.setToolTip(Some(&ns(&f.tip)));
                        let tip = item(mtm, &format!("→ {}", f.tip), None, "");
                        tip.setIndentationLevel(1);
                        tip.setEnabled(false);
                        submenu.addItem(&r);
                        submenu.addItem(&tip);
                    } else {
                        submenu.addItem(&r);
                    }
                }
                adv.setSubmenu(Some(&submenu));
                menu.addItem(&adv);
            }
            if fs.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                menu.addItem(&row(mtm, "Nothing obvious to free", "", Some(&sub)));
            }

            // Footer
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&self.action_item("Usage report…", sel!(showReport:), "r"));
            let sampler = self.action_item(
                "Learn my habits (background sampler)",
                sel!(toggleSampler:),
                "",
            );
            sampler.setState(if daemon::agent_installed() { 1 } else { 0 });
            sampler.setToolTip(Some(&ns("Samples memory once a minute (~5 MB RAM) so ramster can spot what you leave open and idle.")));
            menu.addItem(&sampler);
            let login = self.action_item("Launch at login", sel!(toggleLoginItem:), "");
            let status = login_status();
            match status {
                None => {
                    login.setEnabled(false);
                    login.setToolTip(Some(&ns("Only available when ramster runs from ramster.app (make app), not from a bare binary.")));
                }
                Some(s) if s == SMAppServiceStatus::Enabled => login.setState(1),
                Some(s) if s == SMAppServiceStatus::RequiresApproval => {
                    login.setState(-1); // mixed
                    login.setToolTip(Some(&ns(
                        "Waiting for your approval in System Settings › General › Login Items.",
                    )));
                }
                Some(_) => login.setState(0),
            }
            menu.addItem(&login);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&self.action_item("Quit ramster", sel!(quit:), "q"));

            if std::env::var_os("RAMSTER_BAR_DEBUG").is_some() {
                eprintln!(
                    "menu rebuilt in {} ms: {} groups, {} findings, launch at login: {}",
                    started.elapsed().as_millis(),
                    snap.groups.len(),
                    fs.len(),
                    status.map_or("n/a (not bundled)".into(), |s| format!("{}", s.0))
                );
            }
            st.findings = fs;
            drop(st);
            self.refresh_title();
        }

        fn alert(&self, title: &str, text: &str, buttons: &[&str], critical: bool) -> bool {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            let a = NSAlert::new(mtm);
            a.setMessageText(&ns(title));
            a.setInformativeText(&ns(text));
            a.setAlertStyle(if critical {
                NSAlertStyle::Critical
            } else {
                NSAlertStyle::Informational
            });
            for b in buttons {
                a.addButtonWithTitle(&ns(b));
            }
            a.runModal() == NSAlertFirstButtonReturn
        }

        fn confirm(&self, f: &Finding) -> bool {
            let mut text = format!(
                "{}\n\nThis will {}.\nFrees about {}.",
                f.detail,
                f.action,
                fmt_mb(f.frees_mb)
            );
            if !f.tip.is_empty() {
                text += &format!("\n\nTip: {}", f.tip);
            }
            self.alert(
                &format!("{}?", f.title),
                &text,
                &["Stop it", "Cancel"],
                false,
            )
        }

        /// Re-derive fresh findings off the main thread (CPU-verified) and execute.
        fn spawn_free(&self, ids: Vec<String>, label: String) {
            if self.ivars().busy.swap(true, Ordering::SeqCst) {
                return;
            }
            let busy = self.ivars().busy.clone();
            let last = self.ivars().last_msg.clone();
            self.refresh_title();
            std::thread::spawn(move || {
                let cfg = Config::load();
                let msg = match mcp::findings(&cfg) {
                    Err(e) => format!("could not free {label}: {e}"),
                    Ok((snap, fs)) => {
                        let before = snap.mem.used;
                        let chosen = mcp::select(&fs, &ids);
                        let n = chosen.len();
                        let mut est = 0;
                        let mut ok = 0;
                        for f in chosen {
                            let r = act::execute(f, false);
                            if r.ok {
                                ok += 1;
                                est += r.estimated_freed_mb;
                            }
                        }
                        std::thread::sleep(Duration::from_secs(2));
                        let after = probe::memory().map(|m| m.used).unwrap_or(before);
                        let dropped = before.saturating_sub(after) >> 20;
                        if n == 0 {
                            format!("{label}: nothing to free anymore")
                        } else {
                            format!("Freed ~{} ({ok}/{n} items)", fmt_mb(est.max(dropped)))
                        }
                    }
                };
                notify("ramster", &msg);
                if let Ok(mut g) = last.lock() {
                    *g = Some(msg);
                }
                busy.store(false, Ordering::SeqCst);
            });
        }

        fn report_alert(&self) {
            let samples = history::load(now_unix() - 24 * 3600);
            if samples.is_empty() {
                let enable = self.alert(
                    "No usage history yet",
                    "Turn on \"Learn my habits\" so ramster samples memory once a minute (~5 MB RAM). After a few hours the report shows what you leave open and idle.",
                    &["Turn it on", "Not now"],
                    false,
                );
                if enable && !daemon::agent_installed() {
                    self.toggle_agent();
                }
                return;
            }
            let r = report::build(&samples);
            let mut t = format!(
                "Last {:.1} h · pressure elevated {:.0}% of the time · avg compressed {:.1} GB · peak swap {:.1} GB\n",
                r.covered_hours,
                r.pressure_warning_percent + r.pressure_critical_percent,
                r.avg_compressed_gb,
                r.peak_swap_gb
            );
            if !r.forgotten.is_empty() {
                t += "\nForgotten (memory held while idle):\n";
                for u in r.forgotten.iter().take(5) {
                    t += &format!(
                        "  • {}: {} avg, idle {:.0}% ({:.1} GB·h)\n",
                        u.name,
                        fmt_mb(u.avg_mb),
                        u.idle_percent,
                        u.idle_gb_hours
                    );
                }
            }
            t += "\nHeaviest:\n";
            for u in r.apps.iter().take(6) {
                t += &format!("  • {}: {} avg\n", u.name, fmt_mb(u.avg_mb));
            }
            if !r.advice.is_empty() {
                t += "\nWhat to change:\n";
                for (i, a) in r.advice.iter().enumerate() {
                    t += &format!("{}. {a}\n", i + 1);
                }
            }
            self.alert("Your memory habits", &t, &["OK"], false);
        }

        fn toggle_agent(&self) {
            let installed = daemon::agent_installed();
            let Some(cli) = cli_path() else {
                self.alert(
                    "ramster CLI not found",
                    "Install it with `make install` so the background sampler can run.",
                    &["OK"],
                    true,
                );
                return;
            };
            let sub = if installed { "uninstall" } else { "install" };
            let (ok, out) = run_cmd(
                Duration::from_secs(20),
                &cli.to_string_lossy(),
                &["agent", sub],
            );
            if !ok {
                self.alert(
                    "Could not change the background sampler",
                    &out,
                    &["OK"],
                    true,
                );
            }
        }

        fn toggle_login(&self) {
            if !in_app_bundle() {
                self.alert(
                    "Launch at login needs the app",
                    "Build it with `make app` and run ramster.app; a bare binary can't be a login item.",
                    &["OK"],
                    true,
                );
                return;
            }
            let svc = unsafe { SMAppService::mainAppService() };
            let status = unsafe { svc.status() };
            if status == SMAppServiceStatus::RequiresApproval {
                if self.alert(
                    "Approve ramster in System Settings",
                    "macOS needs your OK in System Settings › General › Login Items before ramster can start at login.",
                    &["Open System Settings", "Later"],
                    false,
                ) {
                    unsafe { SMAppService::openSystemSettingsLoginItems() };
                }
                return;
            }
            let res = if status == SMAppServiceStatus::Enabled {
                unsafe { svc.unregisterAndReturnError() }
            } else {
                unsafe { svc.registerAndReturnError() }
            };
            if let Err(e) = res {
                self.alert(
                    "Could not change launch at login",
                    &e.localizedDescription().to_string(),
                    &["OK"],
                    true,
                );
            }
        }
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory); // no Dock icon

        let bar = Bar::new(mtm);
        let status =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = status.button(mtm) {
            if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &ns("memorychip"),
                Some(&ns("ramster")),
            ) {
                img.setTemplate(true);
                button.setImage(Some(&img));
            }
        }
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*bar)));
        status.setMenu(Some(&menu));
        *bar.ivars().item.borrow_mut() = Some(status);
        bar.refresh_title();

        // Warm the launchctl/lsof/docker facts so the first menu open has them.
        ramster::analyze::facts::prefetch(ramster::analyze::facts::Need {
            procs: true,
            docker: true,
        });

        // Baseline for CPU measurement on first menu open.
        if let Ok(procs) = probe::procs() {
            bar.ivars().state.borrow_mut().baseline = Some((daemon::slim(&procs), Instant::now()));
        }

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                15.0,
                bar.target(),
                sel!(tick:),
                None,
                true,
            )
        };
        timer.setTolerance(5.0); // let macOS coalesce wakeups
        if std::env::var_os("RAMSTER_BAR_SELFTEST").is_some() {
            // Used for screenshots and memory tests.
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    1.5,
                    bar.target(),
                    sel!(openMenu:),
                    None,
                    false,
                )
            };
        }
        app.run();
    }
}
