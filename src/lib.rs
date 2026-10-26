//! Crusteo, as a library: [`run`] opens the app. The `crusteo` binary is
//! that and nothing more; other binaries can run it with extensions of
//! their own.

pub mod app;
pub mod app_menu;
pub mod assets;
pub mod extensions;
pub mod fuel;
pub mod help;
pub mod layouts;
pub mod needs_you;
pub mod popout;
pub mod shell;
pub mod state_list;
pub mod theme;
pub mod toolbars;
pub mod ui;
pub mod widgets;
pub mod workspace_archive;

// GPUI rebuilds the element tree every frame, which is allocation-heavy.
// The default Windows heap is several times slower than mimalloc for that.
#[global_allocator]
#[cfg(target_os = "windows")]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `crusteo --version`. A Windows release build has no console of its own,
/// so it writes to the one it was started from.
fn print_version() {
    #[cfg(windows)]
    // Safe: no arguments to check; it fails harmlessly when there is no
    // parent console or the output is already redirected.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
    println!("crusteo {}", app_menu::version());
}

/// Runs the app until it quits.
pub fn run() {
    run_with(Vec::new());
}

/// [`run`], with `extensions` beside the [`extensions::builtin`] ones.
pub fn run_with(extensions: Vec<Box<dyn extensions::Extension>>) {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        print_version();
        return;
    }
    eprintln!("crusteo {} starting", app_menu::version());

    // CRUSTEO_PROFILE=<file> samples this, the UI thread, into folded stacks.
    sampler::from_env();

    // With no console to print to, a crash is written to crash.log in the
    // data folder instead, after the usual message.
    if cfg!(all(windows, not(debug_assertions))) {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let path = app::config::data_dir().join("crash.log");
            let report = format!(
                "{} crusteo {} panicked: {info}\n{}\n\n",
                app::store::iso_from_millis(app::store::now_millis()),
                env!("CARGO_PKG_VERSION"),
                std::backtrace::Backtrace::force_capture()
            );
            let _ = std::fs::create_dir_all(app::config::data_dir());
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, report.as_bytes()));
            default(info);
        }));
    }

    // GPUI picks Wayland whenever WAYLAND_DISPLAY is set. On a WSL desktop
    // that compositor does not draw a title bar, so the window has to paint
    // its own. An X11 window gets the desktop's chrome instead.
    if std::env::var_os("WSL_DISTRO_NAME").is_some() && std::env::var_os("DISPLAY").is_some() {
        // Safe: this runs before GPUI starts any threads.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    let mut all = extensions::builtin();
    all.extend(extensions);
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            extensions::set_pending(all, cx);
            // Windows shows system notifications only for an app with an
            // identity, registered before any window opens.
            cx.set_app_identity("dev.crusteo.Crusteo", "Crusteo");
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            theme::apply(cx);
            shell::bind_keys(cx);
            popout::bind_keys(cx);
            app_menu::install(cx);
            layouts::paseo::bind_keys(cx);
            layouts::paseo::bind_terminal_keys(cx);
            widgets::label_picker::bind_keys(cx);
            widgets::add_project::bind_keys(cx);
            widgets::workspace_form::bind_keys(cx);
            widgets::new_agent::bind_keys(cx);
            shell::open(cx);
        });
}
