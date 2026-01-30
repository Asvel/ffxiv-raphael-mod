use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use raphael_solver::SolverException;
use raphael_translations::{t, t_format};

use egui::{Align, CursorIcon, Id, Layout, TextStyle};
use raphael_data::{Locale, action_name, get_job_name};

use raphael_sim::{Action, ActionImpl, HeartAndSoul, Manipulation, QuickInnovation};

use crate::config::{QualitySource, QualityTarget};
use crate::context::AppContext;
use crate::{thread_pool, widgets::*};

enum SolverEvent {
    NodesVisited(usize),
    Actions(Vec<Action>),
    LoadedFromHistory(),
    Finished(Option<SolverException>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MinimumStats {
    pub craftsmanship: Option<u16>,
    pub control: Option<u16>,
    pub cp: Option<u16>,
}

#[cfg(any(debug_assertions, feature = "dev-panel"))]
#[derive(Debug, Default)]
struct DevPanelState {
    show_dev_panel: bool,
    render_info_state: RenderInfoState,
}

pub struct MacroSolverApp {
    app_context: AppContext,
    saved_rotations_sync_requests: VecDeque<Option<Rotation>>,

    #[cfg(any(debug_assertions, feature = "dev-panel"))]
    dev_panel_state: DevPanelState,

    main_window_focused_at: Option<std::time::Instant>,
    stats_edit_window_open: bool,
    saved_rotations_window_open: bool,
    missing_stats_error_window_open: bool,

    actions: Vec<Action>,
    solver_pending: bool,
    solver_progress: usize,
    start_time: web_time::Instant,
    duration: web_time::Duration,
    solver_error: Option<SolverException>,

    solver_events: Arc<Mutex<VecDeque<SolverEvent>>>,
    solver_interrupt: raphael_solver::AtomicFlag,

    minimum_stats: MinimumStats,
    minimum_stats_params_hash: u64,
}

impl MacroSolverApp {
    /// Called once before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let app_context = AppContext::new(cc);
        cc.egui_ctx
            .set_zoom_factor(f32::from(app_context.app_config.zoom_percentage) * 0.01);

        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals.interact_cursor = Some(CursorIcon::PointingHand);
            style.url_in_tooltip = true;
            style.always_scroll_the_only_direction = false;
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        });
        // Force scroll area state to be effectively temporary
        cc.egui_ctx
            .data_mut(egui::util::IdTypeMap::remove_by_type::<egui::scroll_area::State>);

        set_fonts(&cc.egui_ctx, app_context.locale);

        #[cfg(not(target_arch = "wasm32"))]
        crate::update::check_for_update();

        Self {
            app_context,
            saved_rotations_sync_requests: VecDeque::new(),

            #[cfg(any(debug_assertions, feature = "dev-panel"))]
            dev_panel_state: DevPanelState::default(),

            main_window_focused_at: None,
            stats_edit_window_open: false,
            saved_rotations_window_open: false,
            missing_stats_error_window_open: false,

            actions: Vec::new(),
            solver_pending: false,
            solver_progress: 0,
            start_time: web_time::Instant::now(),
            duration: web_time::Duration::ZERO,
            solver_error: None,

            solver_events: Arc::new(Mutex::new(VecDeque::new())),
            solver_interrupt: raphael_solver::AtomicFlag::new(),

            minimum_stats: MinimumStats::default(),
            minimum_stats_params_hash: 0,
        }
    }
}

impl eframe::App for MacroSolverApp {
    /// Called each time the UI needs repainting, which may be many times per second.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let locale = self.app_context.locale;

        set_fonts(ctx, locale);

        self.set_window_title(ctx);

        self.process_solver_events();

        self.process_storage_syncing(ctx, _frame);

        #[cfg(not(target_arch = "wasm32"))]
        crate::update::show_dialogues(ctx, locale);

        if self.missing_stats_error_window_open {
            egui::Modal::new(egui::Id::new("min_stats_warning")).show(ctx, |ui| {
                let req_cms = self.app_context.recipe_config.recipe.req_craftsmanship;
                let req_ctrl = self.app_context.recipe_config.recipe.req_control;
                ui.style_mut().spacing.item_spacing = egui::vec2(3.0, 3.0);
                ui.label(egui::RichText::new("Error").strong());
                ui.separator();
                ui.label(t!(
                    locale,
                    "Your stats are below the minimum requirement for this recipe."
                ));
                ui.label(t_format!(
                    locale,
                    "Requirement: {req_cms} Craftsmanship, {req_ctrl} Control."
                ));
                ui.separator();
                ui.vertical_centered_justified(|ui| {
                    if ui.button(t!(locale, "Close")).clicked() {
                        self.missing_stats_error_window_open = false;
                    }
                });
            });
        }

        if let Some(error) = self.solver_error.clone() {
            egui::Modal::new(egui::Id::new("solver_error")).show(ctx, |ui| {
                ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
                ui.set_width(480.0f32.min(ctx.content_rect().width() - 32.0));
                match error {
                    SolverException::NoSolution => {
                        ui.label(egui::RichText::new(t!(locale, "No solution")).strong());
                        ui.separator();
                        ui.label(t!(locale, "Cannot complete synthesis."));
                        if self.app_context.solver_config.must_reach_target_quality
                            && self.app_context.game_settings().max_quality != 0
                        {
                            ui.label(t!(locale, "Try lowering target quality."));
                        }
                    }
                    SolverException::Interrupted => {
                        self.solver_error = None;
                    }
                    SolverException::InternalError(message) => {
                        ui.label(egui::RichText::new(t!(locale, "Internal Solver Error")).strong());
                        ui.separator();
                        ui.add(
                            MultilineMonospace::new(message)
                                .max_height(320.0)
                                .scrollable(true),
                        );
                    }
                }
                ui.separator();
                ui.vertical_centered_justified(|ui| {
                    if ui.button(t!(locale, "Close")).clicked() {
                        self.solver_error = None;
                    }
                });
            });
        }

        if self.solver_pending {
            #[cfg(target_arch = "wasm32")]
            if crate::OOM_PANIC_OCCURED.load(std::sync::atomic::Ordering::Relaxed) {
                eframe::wasm_bindgen::throw_val("OOM panic".into());
            }
            let interrupt_pending = self.solver_interrupt.is_set();
            egui::Modal::new(egui::Id::new("solver_busy")).show(ctx, |ui| {
                ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
                ui.set_width(180.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(if interrupt_pending {
                                    t!(locale, "Cancelling ...")
                                } else {
                                    t!(locale, "Solving ...")
                                })
                                .strong(),
                            );
                            ui.label(format!("({:.2}s)", self.start_time.elapsed().as_secs_f32()));
                        });
                        if self.solver_progress == 0 {
                            ui.label(t!(locale, "Computing ..."));
                        } else {
                            // format with thousands separator
                            let num = self
                                .solver_progress
                                .to_string()
                                .as_bytes()
                                .rchunks(3)
                                .rev()
                                .map(std::str::from_utf8)
                                .collect::<Result<Vec<&str>, _>>()
                                .unwrap()
                                .join(",");
                            ui.label(t_format!(locale, "{num} nodes visited"));
                        }
                    });
                });

                ui.vertical_centered_justified(|ui| {
                    ui.separator();
                    let response =
                        ui.add_enabled(!interrupt_pending, egui::Button::new(t!(locale, "Cancel")));
                    if response.clicked() {
                        self.solver_interrupt.set();
                    }
                });
            });
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    egui::containers::menu::MenuBar::new().ui(ui, |ui| {
                        ui.label(egui::RichText::new("Raphael  |  FFXIV Crafting Solver").strong());
                        ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                        self.draw_app_config_menu_button(ui, ctx);

                        let selectable_locales = [
                            Locale::EN,
                            Locale::DE,
                            Locale::FR,
                            Locale::JP,
                            Locale::CN,
                            Locale::KR,
                            Locale::TW,
                        ];
                        ui.add(DropDown::new(
                            "LOCALE",
                            &mut self.app_context.locale,
                            selectable_locales,
                            Locale::short_code,
                        ));

                        ui.add(
                            egui::Hyperlink::from_label_and_url(
                                t!(locale, "View source on GitHub"),
                                "https://github.com/Asvel/ffxiv-raphael-mod",
                            )
                            .open_in_new_tab(true),
                        );
                        ui.label("/");
                        ui.add(
                            egui::Hyperlink::from_label_and_url(
                                t!(locale, "Support mainline author on Ko-fi"),
                                "https://ko-fi.com/konaeakira",
                            )
                            .open_in_new_tab(true),
                        );
                        #[cfg(debug_assertions)]
                        ui.allocate_space(egui::vec2(145.0, 0.0));
                        #[cfg(all(not(debug_assertions), feature = "dev-panel"))]
                        ui.allocate_space(egui::vec2(68.0, 0.0));
                        #[cfg(any(debug_assertions, feature = "dev-panel"))]
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .selectable_label(self.dev_panel_state.show_dev_panel, "Dev Panel")
                                .clicked()
                            {
                                self.dev_panel_state.show_dev_panel =
                                    !self.dev_panel_state.show_dev_panel;
                            }
                            egui::warn_if_debug_build(ui);
                            ui.separator();
                        });
                    });
                });
        });

        #[cfg(any(debug_assertions, feature = "dev-panel"))]
        if self.dev_panel_state.show_dev_panel {
            egui::SidePanel::right("dev_panel")
                .resizable(true)
                .show(ctx, |ui| {
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
                    RenderInfo::new(&mut self.dev_panel_state.render_info_state).ui(ui, _frame);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                self.draw_simulator_widget(ui);
                ui.with_layout(
                    Layout::left_to_right(Align::TOP).with_main_wrap(true),
                    |ui| {
                        let select_min_width: f32 = 612.0;
                        let config_min_width: f32 = 300.0;
                        let macro_min_width: f32 = 290.0;

                        let select_width;
                        let config_width;
                        let macro_width;

                        let row_width = ui.available_width();
                        if row_width >= select_min_width + config_min_width + macro_min_width {
                            select_width = row_width
                                - config_min_width
                                - macro_min_width
                                - 2.0 * ui.spacing().item_spacing.x;
                            config_width = config_min_width;
                            macro_width = macro_min_width;
                        } else if row_width >= select_min_width + config_min_width {
                            select_width =
                                row_width - config_min_width - ui.spacing().item_spacing.x;
                            config_width = config_min_width;
                            macro_width = row_width;
                        } else if row_width >= config_min_width + macro_min_width {
                            select_width = row_width;
                            config_width = config_min_width;
                            macro_width =
                                row_width - config_min_width - ui.spacing().item_spacing.x;
                        } else {
                            select_width = row_width;
                            config_width = row_width;
                            macro_width = row_width;
                        }

                        let response = ui
                            .allocate_ui(egui::vec2(select_width, 0.0), |ui| {
                                self.draw_list_select_widgets(ui);
                            })
                            .response;

                        let config_min_height = match ui.available_size_before_wrap().x {
                            x if x < config_width => 0.0,
                            _ => response.rect.height(),
                        };
                        let response = ui
                            .allocate_ui(egui::vec2(config_width, config_min_height), |ui| {
                                self.draw_config_and_results_widget(ui);
                            })
                            .response;

                        let macro_min_height = match ui.available_size_before_wrap().x {
                            x if x < macro_width => 0.0,
                            _ => response.rect.height(),
                        };
                        ui.allocate_ui(egui::vec2(macro_width, macro_min_height), |ui| {
                            self.draw_macro_output_widget(ui);
                        });
                    },
                );
            });
        });

        let maximum_visible_window_size =
            (ctx.content_rect().size() - egui::Vec2::new(14.0, 45.0)).max(egui::Vec2::ZERO);
        let stats_edit_window_size = maximum_visible_window_size.min(egui::Vec2::new(412.0, 650.0));
        egui::Window::new(
            egui::RichText::new(t!(locale, "Edit crafter stats"))
                .strong()
                .text_style(TextStyle::Body),
        )
        .id(egui::Id::new("STATS_EDIT"))
        .open(&mut self.stats_edit_window_open)
        .collapsible(false)
        .resizable(false)
        .min_size(stats_edit_window_size)
        .max_size(stats_edit_window_size)
        .show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
            ui.add(StatsEdit::new(&mut self.app_context));
        });

        egui::Window::new(
            egui::RichText::new(t!(locale, "Saved macros & solve history"))
                .strong()
                .text_style(TextStyle::Body),
        )
        .id(egui::Id::new("SAVED_ROTATIONS"))
        .open(&mut self.saved_rotations_window_open)
        .collapsible(false)
        .default_size((400.0, 600.0))
        .show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
            ui.add(SavedRotationsWidget::new(
                &mut self.app_context,
                &mut self.actions,
            ));
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.app_context.save(storage);
    }

    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }

    fn persist_egui_memory(&self) -> bool {
        // pause egui states saving to keep storage unchanged, to skip storage file rewriting,
        // to avoid most unwanted SAVED_ROTATIONS overwriting under multiple app instances
        match self.main_window_focused_at {
            Some(time) => time.elapsed().as_secs() >= 3,
            None => false,
        }
    }
}

impl MacroSolverApp {
    fn process_solver_events(&mut self) {
        let mut submit_new_rotation = false;
        let mut solver_events = self.solver_events.lock().unwrap();
        while let Some(event) = solver_events.pop_front() {
            match event {
                SolverEvent::NodesVisited(count) => self.solver_progress = count,
                SolverEvent::Actions(actions) => self.actions = actions,
                SolverEvent::LoadedFromHistory() => self.solver_progress = usize::MAX,
                SolverEvent::Finished(exception) => {
                    self.duration = self.start_time.elapsed();
                    self.solver_pending = false;
                    self.solver_interrupt.clear();
                    if exception.is_none() {
                        submit_new_rotation = true;
                    } else {
                        self.solver_error = exception;
                    }
                }
            }
        }
        drop(solver_events);

        if submit_new_rotation {
            self.find_minimum_stats();
            let new_rotation = Rotation::new(
                &self.app_context,
                self.actions.clone(),
                self.minimum_stats,
            );
            self.saved_rotations_sync_requests.push_back(Some(new_rotation));
        }
    }

    fn draw_app_config_menu_button(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let locale = self.app_context.locale;
        ui.add_enabled_ui(true, |ui| {
            ui.reset_style();
            egui::containers::menu::MenuButton::new(t!(locale, "⚙ Settings"))
                .config(
                    egui::containers::menu::MenuConfig::default()
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                )
                .ui(ui, |ui| {
                    ui.reset_style();
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
                    ui.horizontal(|ui| {
                        ui.label(t!(locale, "Zoom"));

                        let mut zoom_percentage = (ctx.zoom_factor() * 100.0).round() as u16;
                        ui.horizontal(|ui| {
                            ui.style_mut().spacing.item_spacing.x = 4.0;
                            ui.add_enabled_ui(zoom_percentage > 50, |ui| {
                                if ui.button(egui::RichText::new("-").monospace()).clicked() {
                                    zoom_percentage -= 10;
                                }
                            });
                            ui.add_enabled_ui(zoom_percentage != 100, |ui| {
                                if ui.button(t!(locale, "Reset")).clicked() {
                                    zoom_percentage = 100;
                                }
                            });
                            ui.add_enabled_ui(zoom_percentage < 500, |ui| {
                                if ui.button(egui::RichText::new("+").monospace()).clicked() {
                                    zoom_percentage += 10;
                                }
                            });
                        });

                        ui.add(
                            egui::DragValue::new(&mut zoom_percentage)
                                .range(50..=500)
                                .suffix("%")
                                // dragging would cause the UI scale to jump arround erratically
                                .speed(0.0)
                                .update_while_editing(false),
                        );

                        self.app_context.app_config.zoom_percentage = zoom_percentage;
                        ctx.set_zoom_factor(f32::from(zoom_percentage) * 0.01);
                    });

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(t!(locale, "Theme"));
                        egui::global_theme_preference_buttons(ui);
                    });
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label(t!(locale, "Max solver threads"));
                        ui.add_enabled_ui(!thread_pool::initialization_attempted(), |ui| {
                            let mut auto_thread_count =
                                self.app_context.app_config.num_threads.is_none();
                            if ui
                                .checkbox(&mut auto_thread_count, t!(locale, "Auto"))
                                .changed()
                            {
                                if auto_thread_count {
                                    self.app_context.app_config.num_threads = None;
                                } else {
                                    self.app_context.app_config.num_threads =
                                        Some(thread_pool::default_thread_count());
                                }
                            }
                            if thread_pool::is_initialized() {
                                ui.add_enabled(
                                    false,
                                    egui::DragValue::new(&mut rayon::current_num_threads()),
                                );
                            } else if let Some(num_threads) =
                                self.app_context.app_config.num_threads.as_mut()
                            {
                                ui.add(egui::DragValue::new(num_threads));
                            } else {
                                ui.add_enabled(
                                    false,
                                    egui::DragValue::new(&mut thread_pool::default_thread_count()),
                                );
                            }
                        });
                    });
                    if thread_pool::initialization_attempted() {
                        #[cfg(target_arch = "wasm32")]
                        let app_restart_text =
                            t!(locale, "Reload the page to change max solver threads.");
                        #[cfg(not(target_arch = "wasm32"))]
                        let app_restart_text =
                            t!(locale, "Restart the app to change max solver threads.");
                        ui.label(
                            egui::RichText::new(t!(
                                locale,
                                "⚠ Unavailable after the solver was started."
                            ))
                            .small()
                            .color(ui.visuals().warn_fg_color),
                        );
                        ui.label(
                            egui::RichText::new(app_restart_text)
                                .small()
                                .color(ui.visuals().warn_fg_color),
                        );
                    }
                });
        });
    }

    fn draw_simulator_widget(&mut self, ui: &mut egui::Ui) {
        self.find_minimum_stats();
        ui.add(Simulator::new(
            &mut self.app_context,
            ui.ctx(),
            &self.actions,
            &self.minimum_stats,
        ));
    }

    fn draw_list_select_widgets(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.add(RecipeSelect::new(&mut self.app_context));
            ui.add(FoodSelect::new(&mut self.app_context));
            ui.add(PotionSelect::new(&mut self.app_context));
        });
    }

    fn draw_config_and_results_widget(&mut self, ui: &mut egui::Ui) {
        let locale = self.app_context.locale;
        ui.group(|ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
            ui.vertical(|ui| {
                self.draw_configuration_widget(ui);
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if ui.button("📑").clicked() {
                        self.saved_rotations_window_open = true;
                    }
                    ui.add_space(-5.0);
                    ui.vertical_centered_justified(|ui| {
                        let text_color = ui.ctx().style().visuals.selection.stroke.color;
                        let text = egui::RichText::new(t!(locale, "Solve")).color(text_color);
                        let fill_color = ui.ctx().style().visuals.selection.bg_fill;
                        let id = egui::Id::new("SOLVE_INITIATED");
                        let mut solve_initiated = ui
                            .ctx()
                            .data(|data| data.get_temp::<bool>(id).unwrap_or_default());
                        let button = ui.add_enabled(
                            !solve_initiated,
                            egui::Button::new(text).fill(fill_color),
                        );
                        if button.clicked() {
                            ui.ctx().data_mut(|data| {
                                data.insert_temp(id, true);
                            });
                            solve_initiated = true;
                        }
                        if solve_initiated {
                            self.on_solve_initiated(ui.ctx());
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    if self.solver_progress == usize::MAX {
                        ui.label(t!(locale, "Loaded from saved rotations"));
                    } else if !self.duration.is_zero() {
                        ui.label(t_format!(
                            locale,
                            "Elapsed time: {dur:.2}s",
                            dur = self.duration.as_secs_f32()
                        ));
                    }
                });
                // fill the remaining space
                ui.with_layout(Layout::bottom_up(Align::LEFT), |_| {});
            });
        });
    }

    fn draw_configuration_widget(&mut self, ui: &mut egui::Ui) {
        let AppContext {
            locale,
            selected_food,
            selected_potion,
            ..
        } = self.app_context;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!(locale, "Configuration")).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.style_mut().spacing.item_spacing = [4.0, 4.0].into();
                if ui.button("✏").clicked() {
                    self.stats_edit_window_open = true;
                }
                ui.add(DropDown::new(
                    "SELECTED_JOB",
                    self.app_context.selected_job_mut(),
                    [0, 1, 2, 3, 4, 5, 6, 7],
                    |job_id: u8| get_job_name(job_id, locale),
                ));
                if ui.add_enabled(
                    self.app_context.crafter_config.is_detached(),
                    egui::Button::new(egui::RichText::from("↙").size(14.0))
                ).clicked() {
                    self.app_context.crafter_config.reset_to_job();
                }
            });
        });
        ui.separator();

        const BUFFED_STAT_BG_COLOR: egui::Color32 =
            egui::Color32::from_rgba_unmultiplied_const(144, 238, 144, 128);
        let consumables = &[selected_food, selected_potion];
        ui.label(egui::RichText::new(t!(locale, "Crafter stats")).strong());
        ui.horizontal(|ui| {
            ui.label(t!(locale, "Craftsmanship"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.style_mut().spacing.item_spacing.x = 5.0;

                let cms_base = &mut self.app_context.active_stats_mut().craftsmanship;
                ui.scope(|ui| {
                    let cms_bonus = raphael_data::craftsmanship_bonus(*cms_base, consumables);
                    if cms_bonus != 0 {
                        ui.visuals_mut().widgets.inactive.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.hovered.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.active.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                    }

                    let buffed = *cms_base + cms_bonus;
                    let mut final_value = buffed;
                    ui.add(
                        egui::DragValue::new(&mut final_value)
                            .range(0..=9999)
                            .update_while_editing(false),
                    );
                    if final_value != buffed
                        && let Some(unbuffed) =
                            raphael_data::craftsmanship_unbuffed(final_value, consumables)
                    {
                        *cms_base = unbuffed;
                    }
                });
                ui.label("➡");
                if ui.add(egui::DragValue::new(cms_base).range(0..=9000)).changed() {
                    self.app_context.crafter_config.detach_from_job();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(t!(locale, "Control"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.style_mut().spacing.item_spacing.x = 5.0;

                let control_base = &mut self.app_context.active_stats_mut().control;
                ui.scope(|ui| {
                    let control_bonus = raphael_data::control_bonus(*control_base, consumables);
                    if control_bonus != 0 {
                        ui.visuals_mut().widgets.inactive.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.hovered.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.active.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                    }

                    let buffed = *control_base + control_bonus;
                    let mut final_value = buffed;
                    ui.add(
                        egui::DragValue::new(&mut final_value)
                            .range(0..=9999)
                            .update_while_editing(false),
                    );
                    if final_value != buffed
                        && let Some(unbuffed) =
                            raphael_data::control_unbuffed(final_value, consumables)
                    {
                        *control_base = unbuffed;
                    }
                });
                ui.label("➡");
                if ui.add(egui::DragValue::new(control_base).range(0..=9000)).changed() {
                    self.app_context.crafter_config.detach_from_job();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(t!(locale, "CP"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.style_mut().spacing.item_spacing.x = 5.0;

                let cp_base = &mut self.app_context.active_stats_mut().cp;
                ui.scope(|ui| {
                    let cp_bonus = raphael_data::cp_bonus(*cp_base, consumables);
                    if cp_bonus != 0 {
                        ui.visuals_mut().widgets.inactive.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.hovered.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                        ui.visuals_mut().widgets.active.weak_bg_fill = BUFFED_STAT_BG_COLOR;
                    }

                    let buffed = *cp_base + cp_bonus;
                    let mut final_value = buffed;
                    ui.add(
                        egui::DragValue::new(&mut final_value)
                            .range(0..=9999)
                            .update_while_editing(false),
                    );
                    if final_value != buffed
                        && let Some(unbuffed) = raphael_data::cp_unbuffed(final_value, consumables)
                    {
                        *cp_base = unbuffed;
                    }
                });
                ui.label("➡");
                if ui.add(egui::DragValue::new(cp_base).range(0..=9000)).changed() {
                    self.app_context.crafter_config.detach_from_job();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(t!(locale, "Job level"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(
                    egui::DragValue::new(&mut self.app_context.active_stats_mut().level)
                        .range(1..=100),
                ).changed() {
                    self.app_context.crafter_config.detach_from_job();
                }
            });
        });
        ui.separator();

        ui.label(egui::RichText::new(t!(locale, "HQ materials")).strong());
        let mut has_hq_ingredient = false;
        let recipe_ingredients = self.app_context.recipe_config.recipe.ingredients;
        if let QualitySource::HqMaterialList(provided_ingredients) =
            &mut self.app_context.recipe_config.quality_source
        {
            for (index, ingredient) in recipe_ingredients.into_iter().enumerate() {
                if ingredient.item_id == 0 {
                    continue;
                }
                has_hq_ingredient = true;
                ui.horizontal(|ui| {
                    ui.add(GameDataNameLabel::new(&ingredient, locale));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui: &mut egui::Ui| {
                        let mut max_placeholder = ingredient.amount;
                        ui.add_enabled(false, egui::DragValue::new(&mut max_placeholder));
                        ui.monospace("/");
                        ui.add(
                            egui::DragValue::new(&mut provided_ingredients[index])
                                .range(0..=ingredient.amount),
                        );
                    });
                });
            }
        }
        if !has_hq_ingredient {
            ui.label(t!(locale, "None"));
        }
        ui.separator();

        ui.label(egui::RichText::new(t!(locale, "Actions")).strong());
        if self.app_context.active_stats().level >= Manipulation::LEVEL_REQUIREMENT {
            if ui.add(egui::Checkbox::new(
                &mut self.app_context.active_stats_mut().manipulation,
                action_name(Action::Manipulation, locale),
            )).changed() {
                self.app_context.crafter_config.detach_from_job();
            }
        } else {
            ui.add_enabled(
                false,
                egui::Checkbox::new(&mut false, action_name(Action::Manipulation, locale)),
            );
        }
        if self.app_context.active_stats().level >= HeartAndSoul::LEVEL_REQUIREMENT {
            if ui.add(egui::Checkbox::new(
                &mut self.app_context.active_stats_mut().heart_and_soul,
                action_name(Action::HeartAndSoul, locale),
            )).changed() {
                self.app_context.crafter_config.detach_from_job();
            }
        } else {
            ui.add_enabled(
                false,
                egui::Checkbox::new(&mut false, action_name(Action::HeartAndSoul, locale)),
            );
        }
        if self.app_context.active_stats().level >= QuickInnovation::LEVEL_REQUIREMENT {
            if ui.add(egui::Checkbox::new(
                &mut self.app_context.active_stats_mut().quick_innovation,
                action_name(Action::QuickInnovation, locale),
            )).changed() {
                self.app_context.crafter_config.detach_from_job();
            }
        } else {
            ui.add_enabled(
                false,
                egui::Checkbox::new(&mut false, action_name(Action::QuickInnovation, locale)),
            );
        }
        let heart_and_soul_enabled = self.app_context.active_stats().level
            >= HeartAndSoul::LEVEL_REQUIREMENT
            && self.app_context.active_stats().heart_and_soul;
        let quick_innovation_enabled = self.app_context.active_stats().level
            >= QuickInnovation::LEVEL_REQUIREMENT
            && self.app_context.active_stats().quick_innovation;
        if heart_and_soul_enabled || quick_innovation_enabled {
            #[cfg(not(target_arch = "wasm32"))]
            ui.label(
                egui::RichText::new(t!(
                    locale,
                    "⚠ Specialist actions substantially increase solve time and memory usage."
                ))
                .small()
                .color(ui.visuals().warn_fg_color),
            );
            #[cfg(target_arch = "wasm32")]
            {
                ui.label(
                    egui::RichText::new(
                        t!(locale, "⚠ Specialist actions substantially increase solve time and memory usage. It is recommended that you download and use the native version if you want to enable specialist actions."),
                    )
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
                ui.add(egui::Hyperlink::from_label_and_url(
                    egui::RichText::new(t!(locale, "Download latest release from GitHub")).small(),
                    "https://github.com/Asvel/ffxiv-raphael-mod/releases/latest",
                ));
            }
        }
        ui.separator();

        ui.label(egui::RichText::new(t!(locale, "Solver settings")).strong());
        ui.horizontal(|ui| {
            ui.label(t!(locale, "Target quality"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.style_mut().spacing.item_spacing = [4.0, 4.0].into();
                let game_settings = self.app_context.game_settings();
                let mut current_value = self
                    .app_context
                    .solver_config
                    .quality_target
                    .get_target(game_settings.max_quality);
                match &mut self.app_context.solver_config.quality_target {
                    QualityTarget::Custom(value) => {
                        ui.add(egui::DragValue::new(value));
                    }
                    _ => {
                        ui.add_enabled(false, egui::DragValue::new(&mut current_value));
                    }
                }
                let selectable_targets = [
                    QualityTarget::Zero,
                    QualityTarget::Half,
                    QualityTarget::CollectableT1,
                    QualityTarget::CollectableT2,
                    QualityTarget::CollectableT3,
                    QualityTarget::Full,
                    QualityTarget::Custom(current_value),
                ];
                ui.add(DropDown::new(
                    "TARGET_QUALITY",
                    &mut self.app_context.solver_config.quality_target,
                    selectable_targets,
                    |value: QualityTarget| value.display(locale),
                ));
            });
        });

        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self
                    .app_context
                    .solver_config
                    .must_reach_target_quality,
                t!(locale, "Solution must reach target quality"),
            );
            ui.add(HelpText::new(t!(locale, "Reduce memory usage by skipping candidate solutions that cannot reach the target quality. Basically, you either get a solution that reaches the target quality or you get no solution at all. If you want to know how close you are to reaching the target quality, keep this option turned off.")));
        });

        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.app_context.solver_config.backload_progress,
                t!(locale, "Backload progress"),
            );
            ui.add(HelpText::new(t!(locale, "Find a rotation that only uses Progress-increasing actions at the end of the rotation.\n  - May decrease achievable Quality.\n  - May increase macro duration.")));
        });

        if self.app_context.recipe_config.recipe.is_expert {
            self.app_context.solver_config.adversarial = false;
        }
        ui.horizontal(|ui| {
            ui.add_enabled(
                !self.app_context.recipe_config.recipe.is_expert,
                egui::Checkbox::new(
                    &mut self.app_context.solver_config.adversarial,
                    t!(locale, "Ensure 100% reliability"),
                ),
            );
            ui.add(HelpText::new(t!(locale, "Find a rotation that can reach the target quality no matter how unlucky the random conditions are.\n  - May decrease achievable Quality.\n  - May increase macro duration.\n  - Much longer solve time.\nThe solver never tries to use Tricks of the Trade to \"eat\" Excellent quality procs, so in some cases this option does not produce the optimal macro.")));
        });
        if self.app_context.solver_config.adversarial {
            ui.label(
                egui::RichText::new(Self::experimental_warning_text(locale))
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
    }

    fn on_solve_initiated(&mut self, ctx: &egui::Context) {
        if thread_pool::is_initialized() {
            ctx.data_mut(|data| {
                data.insert_temp(Id::new("SOLVE_INITIATED"), false);
            });

            let craftsmanship_req = self.app_context.recipe_config.recipe.req_craftsmanship;
            let control_req = self.app_context.recipe_config.recipe.req_control;
            let active_stats = self.app_context.active_stats();
            let craftsmanship_bonus = raphael_data::craftsmanship_bonus(
                active_stats.craftsmanship,
                &[
                    self.app_context.selected_food,
                    self.app_context.selected_potion,
                ],
            );
            let control_bonus = raphael_data::control_bonus(
                active_stats.control,
                &[
                    self.app_context.selected_food,
                    self.app_context.selected_potion,
                ],
            );
            if active_stats.craftsmanship + craftsmanship_bonus >= craftsmanship_req
                && active_stats.control + control_bonus >= control_req
            {
                self.solve(ctx);
            } else {
                self.missing_stats_error_window_open = true;
            }
        } else {
            thread_pool::attempt_initialization(self.app_context.app_config.num_threads);
            ctx.request_repaint();
        }
    }

    fn solve(&mut self, ctx: &egui::Context) {
        self.solver_pending = true;
        self.solver_interrupt.clear();

        let mut game_settings = self.app_context.game_settings();
        let initial_quality = self.app_context.initial_quality();
        ctx.data_mut(|data| {
            data.insert_temp(
                Id::new("LAST_SOLVE_PARAMS"),
                (
                    game_settings,
                    initial_quality,
                    self.app_context.solver_config,
                ),
            );
        });

        if self
            .app_context
            .saved_rotations_config
            .load_from_saved_rotations
            && let Some(actions) = self.app_context.saved_rotations_data.find_solved_rotation(
                &game_settings,
                initial_quality,
                &self.app_context.solver_config,
            )
        {
            let mut solver_events = self.solver_events.lock().unwrap();
            solver_events.push_back(SolverEvent::Actions(actions));
            solver_events.push_back(SolverEvent::LoadedFromHistory());
            solver_events.push_back(SolverEvent::Finished(None));
        } else {
            let target_quality = self
                .app_context
                .solver_config
                .quality_target
                .get_target(game_settings.max_quality);
            game_settings.max_quality = target_quality.saturating_sub(initial_quality);
            self.actions = Vec::new();
            self.solver_progress = 0;
            self.start_time = web_time::Instant::now();
            let solver_settings = raphael_solver::SolverSettings {
                simulator_settings: game_settings,
                allow_non_max_quality_solutions: !self
                    .app_context
                    .solver_config
                    .must_reach_target_quality,
            };
            spawn_solver(
                solver_settings,
                self.solver_events.clone(),
                self.solver_interrupt.clone(),
            );
        }
    }

    fn find_minimum_stats(&mut self) {
        if self.solver_pending
            || self.app_context.custom_recipe_overrides_config.use_base_increase_overrides {
            if self.minimum_stats_params_hash != 0 {
                self.minimum_stats_params_hash = 0;
                self.minimum_stats = MinimumStats::default();
            }
            return;
        }

        let mut game_settings = self.app_context.game_settings();
        let initial_quality = self.app_context.initial_quality();

        let target_progress = game_settings.max_progress as u32;
        let target_quality = self
            .app_context
            .solver_config
            .quality_target
            .get_target(game_settings.max_quality)
            .saturating_sub(initial_quality) as u32;

        let params_hash = egui::Id::new((&game_settings, target_quality, &self.actions)).value();
        if self.minimum_stats_params_hash == params_hash {
            return;
        }
        self.minimum_stats_params_hash = params_hash;

        let initial_state = raphael_sim::SimulationState::new(&game_settings);

        let mut actual_result = initial_state;
        for action in &self.actions {
            actual_result = actual_result
                .use_action(*action, raphael_sim::Condition::Normal, &game_settings)
                .unwrap_or(actual_result);
        }
        if actual_result.progress < target_progress {
            self.minimum_stats = MinimumStats::default();
            return;
        }
        self.minimum_stats.cp = Some(game_settings.max_cp - actual_result.cp);

        let (mut min_progress, mut max_progress) = (1u16, game_settings.base_progress);
        let (mut min_quality, mut max_quality, mut can_target_quality) =
            if actual_result.quality >= target_quality {
                (1u16, game_settings.base_quality, true)
            } else {
                (game_settings.base_quality, game_settings.base_quality * 3 / 2, false)
            };
        if target_quality == 0 || self.actions[0] == Action::TrainedEye {
            max_quality = 1;
        }
        while min_progress + 1 < max_progress || min_quality + 1 < max_quality {
            let mut state = initial_state;
            game_settings.base_progress = (min_progress + max_progress) / 2;
            game_settings.base_quality = (min_quality + max_quality) / 2;
            for action in &self.actions {
                state = state
                    .use_action(*action, raphael_sim::Condition::Normal, &game_settings)
                    .unwrap_or(state);
            }
            if state.progress < target_progress {
                min_progress = game_settings.base_progress;
            } else {
                max_progress = game_settings.base_progress;
            }
            if state.quality < target_quality {
                min_quality = game_settings.base_quality;
            } else {
                max_quality = game_settings.base_quality;
                can_target_quality = true;
            }
        }

        let max_level_scaling = self.app_context.recipe_config.recipe.max_level_scaling;
        let rlvl = if max_level_scaling != 0 {
            let job_level = std::cmp::min(max_level_scaling, game_settings.job_level);
            raphael_data::LEVEL_ADJUST_TABLE[job_level as usize] as usize
        } else {
            self.app_context.recipe_config.recipe.recipe_level as usize
        };
        let rlvl_record = raphael_data::RLVLS[rlvl];
        let mut craftsmanship = max_progress as f32;
        let mut control = max_quality as f32;
        if game_settings.job_level <= rlvl_record.job_level {
            craftsmanship = craftsmanship * 100.0 / rlvl_record.progress_mod as f32;
            control = control * 100.0 / rlvl_record.quality_mod as f32;
        }
        craftsmanship = (craftsmanship - 2.0) * rlvl_record.progress_div as f32 / 10.0;
        control = (control - 35.0) * rlvl_record.quality_div as f32 / 10.0;
        self.minimum_stats.craftsmanship = Some(craftsmanship.ceil() as u16);
        self.minimum_stats.control = if can_target_quality {
            Some(control.ceil() as u16)
        } else {
            None
        };
    }

    fn draw_macro_output_widget(&mut self, ui: &mut egui::Ui) {
        ui.add(MacroView::new(&mut self.app_context, &mut self.actions));
    }

    fn experimental_warning_text(locale: Locale) -> &'static str {
        #[cfg(not(target_arch = "wasm32"))]
        return t!(
            locale,
            "⚠ EXPERIMENTAL FEATURE\nThis option may use a lot of memory (sometimes well above 4GB) which may cause your system to run out of memory."
        );
        #[cfg(target_arch = "wasm32")]
        return t!(
            locale,
            "⚠ EXPERIMENTAL FEATURE\nMay crash the solver due to reaching the 4GB memory limit of 32-bit web assembly, causing the UI to get stuck in the \"solving\" state indefinitely."
        );
    }

    fn set_window_title(&self, ctx: &egui::Context) {
        let egui_id_current = egui::Id::new("title_item");
        let current_item_id = self.app_context.recipe_config.recipe.item_id;
        if ctx.data(|data| data.get_temp(egui_id_current)) != Some(current_item_id) {
            ctx.data_mut(|data| data.insert_temp(egui_id_current, current_item_id));
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                match raphael_data::get_raw_item_name(current_item_id, self.app_context.locale) {
                    Some(item_name) => format!("{item_name} - Raphael XIV"),
                    None => "Raphael XIV".to_owned(),
                }
            ));
        }
    }

    fn process_storage_syncing(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.input(|input| {
            for event in input.raw.events.iter().rev() {
                if let egui::Event::WindowFocused(focused) = event {
                    if *focused {
                        self.saved_rotations_sync_requests.push_back(None);
                        self.main_window_focused_at = Some(std::time::Instant::now());
                    } else {
                        self.main_window_focused_at = None;
                    }
                    break;
                }
            }
        });

        #[cfg(not(target_arch = "wasm32"))]
        // native storage caches values forever, we can only read it from file manually
        let mut sync_saved_rotations = || {
            match eframe::storage_dir("Raphael XIV Mod") {
                Some(path) => {
                    let uri = format!("file://{}", path.join("app.ron").to_str().unwrap());
                    match ctx.try_load_bytes(&uri) {
                        Ok(egui::load::BytesPoll::Ready { bytes, .. }) => {
                            type KV = std::collections::HashMap<String, String>;
                            if let Ok(kv) = ron::de::from_bytes::<KV>(&bytes) {
                                if let Some(value) = kv.get("SAVED_ROTATIONS") {
                                    if let Ok(value) = ron::de::from_str(value) {
                                        self.app_context.saved_rotations_data = value;
                                    }
                                }
                            }
                            for loader in ctx.loaders().bytes.lock().iter() {
                                loader.forget(&uri);
                            }
                            true
                        },
                        _ => false,  // waiting file IO
                    }
                },
                None => true,  // no storage, continue subsequent operations as success
            }
        };
        #[cfg(target_arch = "wasm32")]
        // web storage retrieves value from browser directly
        let mut sync_saved_rotations = || {
            if let Some(storage) = _frame.storage() {
                if let Some(value) = eframe::get_value(storage, "SAVED_ROTATIONS") {
                    self.saved_rotations_data = value;
                }
            }
            true
        };

        if self.saved_rotations_sync_requests.len() > 0 && sync_saved_rotations() {
            while let Some(request) = self.saved_rotations_sync_requests.pop_front() {
                if let Some(rotation) = request {
                    self.app_context.saved_rotations_data.add_solved_rotation(
                        rotation,
                        &self.app_context.saved_rotations_config,
                    );
                }
            }
        }
    }
}

fn set_fonts(ctx: &egui::Context, locale: Locale) {
    let egui_id_current = egui::Id::new("font_locale");
    if ctx.data(|data| data.get_temp(egui_id_current)) == Some(locale) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(egui_id_current, locale));

    let mut fonts = egui::FontDefinitions::default();
    let mut add_font = |font_name: &'static str, font_data: egui::FontData| {
        fonts.font_data.insert(font_name.to_owned(), Arc::new(font_data));
        for family in fonts.families.values_mut() {
            family.insert(family.len() - 2, font_name.to_owned());
        }
    };
    add_font(
        "XIV_Icon_Recreations",
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/XIV_Icon_Recreations/XIV_Icon_Recreations.ttf"
        ))
    );
    match locale {
        Locale::JP => add_font(
            "NotoSansJP",
            egui::FontData::from_static(include_bytes!(
                "../assets/fonts/Noto_Sans_JP/subset.ttf"
            ))
            .tweak(egui::FontTweak {
                y_offset_factor: -0.05,
                ..Default::default()
            })
        ),
        Locale::CN => add_font(
            "NotoSansSC",
            egui::FontData::from_static(include_bytes!(
                "../assets/fonts/Noto_Sans_SC/subset.ttf"
            ))
            .tweak(egui::FontTweak {
                y_offset_factor: -0.05,
                ..Default::default()
            })
        ),
        Locale::KR => add_font(
            "NotoSansKR",
            egui::FontData::from_static(include_bytes!(
                "../assets/fonts/Noto_Sans_KR/subset.ttf"
            ))
            .tweak(egui::FontTweak {
                y_offset_factor: -0.05,
                ..Default::default()
            })
        ),
        Locale::TW => add_font(
            "NotoSansTC",
            egui::FontData::from_static(include_bytes!(
                "../assets/fonts/Noto_Sans_TC/subset.ttf"
            ))
            .tweak(egui::FontTweak {
                y_offset_factor: -0.05,
                ..Default::default()
            })
        ),
        _ => (),
    }
    ctx.set_fonts(fonts);
}

fn spawn_solver(
    solver_settings: raphael_solver::SolverSettings,
    solver_events: Arc<Mutex<VecDeque<SolverEvent>>>,
    solver_interrupt: raphael_solver::AtomicFlag,
) {
    let events = solver_events.clone();
    let solution_callback = move |actions: &[raphael_sim::Action]| {
        let event = SolverEvent::Actions(actions.to_vec());
        events.lock().unwrap().push_back(event);
    };
    let events = solver_events.clone();
    let progress_callback = move |progress: usize| {
        let event = SolverEvent::NodesVisited(progress);
        events.lock().unwrap().push_back(event);
    };
    rayon::spawn(move || {
        log::debug!("Spawning solver: {solver_settings:?}");
        let mut macro_solver = raphael_solver::MacroSolver::new(
            solver_settings,
            Box::new(solution_callback),
            Box::new(progress_callback),
            solver_interrupt,
        );
        match macro_solver.solve() {
            Ok(actions) => {
                let mut solver_events = solver_events.lock().unwrap();
                solver_events.push_back(SolverEvent::Actions(actions));
                solver_events.push_back(SolverEvent::Finished(None));
            }
            Err(exception) => solver_events
                .lock()
                .unwrap()
                .push_back(SolverEvent::Finished(Some(exception))),
        }
    });
}
