pub mod app_menus;
pub mod appearance;
pub mod icons;
pub mod settings;
pub mod shell;
pub mod theme;
pub mod theme_library;
pub mod typography;

use std::path::PathBuf;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

#[derive(Debug, Clone)]
pub struct UiConfig {
    pub data_dir: PathBuf,
    pub api_base_url: String,
}

struct ReopenState(UiConfig);

impl gpui::Global for ReopenState {}

pub fn run_app(config: UiConfig) {
    let app = gpui_platform::application().with_assets(icons::Assets);
    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<ReopenState>()
        {
            open_main_window(reopen.0.clone(), cx);
        }
    });
    app.run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        let ui_settings = settings::UiSettings::load(&config.data_dir);
        settings::init(ui_settings.clone(), config.data_dir.clone(), cx);
        let font_availability = typography::register_fonts(cx);
        typography::init(
            ui_settings.ui_font_family.clone(),
            ui_settings.ui_font_size,
            font_availability,
            cx,
        );
        theme_library::init(config.data_dir.clone(), cx);
        appearance::init(
            ui_settings.appearance,
            ui_settings.theme_selection,
            ui_settings.accent,
            ui_settings.surface,
            cx,
        );
        app_menus::init(cx);
        app_menus::bind_keys(cx);
        cx.register_url_scheme("keiki").detach();
        cx.on_app_quit(|cx| {
            settings::flush(cx);
            async {}
        })
        .detach();
        cx.set_global(ReopenState(config.clone()));
        open_main_window(config.clone(), cx);
        cx.set_menus(app_menus::app_menus());
        cx.activate(true);
    });
}

fn open_main_window(config: UiConfig, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(600.))),
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(14.), px(14.))),
            }),
            app_owns_titlebar_drag: true,
            window_decorations: cfg!(target_os = "linux")
                .then_some(gpui::WindowDecorations::Client),
            window_background: theme::Theme::of(cx).window_background_appearance(),
            app_id: Some("keiki".into()),
            ..Default::default()
        },
        move |window, cx| {
            window.set_rem_size(px(typography::font_size(cx).pixels()));
            appearance::observe_window(window, cx).detach();
            cx.new(|cx| shell::Shell::new(config.api_base_url, window, cx))
        },
    )
    .expect("failed to open window");
    appearance::reapply_window_background(cx);
}
