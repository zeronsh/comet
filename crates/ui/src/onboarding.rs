//! First-run onboarding: a focused six-step journey. Durable choices continue
//! to live in their authoritative stores; this module only persists
//! navigation/lifecycle state in `UiSettings`.

use std::sync::Arc;

use gpui::{
    AnyElement, Context, Empty, Entity, FocusHandle, Image, ImageFormat, IntoElement, KeyDownEvent,
    Pixels, ScrollHandle, SharedString, Task, div, prelude::*, px,
};
use serde::{Deserialize, Serialize};
use zeron_engine::registry::{HarnessDescriptor, TitleSettings, descriptor_enabled};
use zeron_proto::{AgentAccountsSnapshot, HarnessId, Model, ReasoningLevel, SteeringMode};
use zeron_theme::{AccentPreset, AccentSelection, SurfacePreference, ThemeRegistry};

use crate::appearance::AppearanceMode;
use crate::icons::icon;
use crate::popover::{self, Loadable, Popup};
use crate::settings::composer::ComposerDefaults;
use crate::shell::Shell;
use crate::state::AppState;
use crate::theme::{Theme, ink};

pub const SCHEMA_VERSION: u16 = 1;
pub const STEP_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OnboardingDisposition {
    InProgress,
    Deferred,
    Skipped,
    #[default]
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OnboardingStep {
    #[default]
    Workspace,
    Appearance,
    Harnesses,
    Defaults,
    Titles,
    Project,
    /// Kept only so an in-progress v1 snapshot from the previous build still
    /// deserializes. `OnboardingUi::step` folds it into `Project`.
    FirstSession,
}

impl OnboardingStep {
    pub const ALL: [Self; STEP_COUNT] = [
        Self::Workspace,
        Self::Appearance,
        Self::Harnesses,
        Self::Defaults,
        Self::Titles,
        Self::Project,
    ];

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(STEP_COUNT - 1)
    }

    pub fn next(self) -> Self {
        Self::ALL
            .get(self.index() + 1)
            .copied()
            .unwrap_or(Self::Project)
    }

    pub fn previous(self) -> Self {
        self.index()
            .checked_sub(1)
            .and_then(|index| Self::ALL.get(index))
            .copied()
            .unwrap_or(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceMode {
    Local,
    Synced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OnboardingState {
    pub schema_version: u16,
    pub disposition: OnboardingDisposition,
    pub step: OnboardingStep,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_mode: Option<WorkspaceMode>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            disposition: OnboardingDisposition::Completed,
            step: OnboardingStep::Workspace,
            workspace_mode: None,
        }
    }
}

impl OnboardingState {
    pub fn fresh() -> Self {
        Self {
            disposition: OnboardingDisposition::InProgress,
            workspace_mode: Some(WorkspaceMode::Local),
            ..Self::default()
        }
    }

    pub fn is_active(&self) -> bool {
        self.disposition == OnboardingDisposition::InProgress
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingFixture {
    Welcome,
    WorkspaceSync,
    Appearance,
    Harnesses,
    HarnessError,
    Defaults,
    Titles,
    Project,
    Projectless,
    Narrow,
}

impl OnboardingFixture {
    pub fn from_route(route: Option<&str>) -> Option<Self> {
        match route {
            Some("onboarding") | Some("onboarding/welcome") => Some(Self::Welcome),
            Some("onboarding/workspace-sync") => Some(Self::WorkspaceSync),
            Some("onboarding/appearance") => Some(Self::Appearance),
            Some("onboarding/agents") | Some("onboarding/harnesses") => Some(Self::Harnesses),
            Some("onboarding/agents-error") | Some("onboarding/harness-error") => {
                Some(Self::HarnessError)
            }
            Some("onboarding/defaults") => Some(Self::Defaults),
            Some("onboarding/titles") => Some(Self::Titles),
            Some("onboarding/project") => Some(Self::Project),
            Some("onboarding/projectless") => Some(Self::Projectless),
            Some("onboarding/first-session") => Some(Self::Project),
            Some("onboarding/narrow") => Some(Self::Narrow),
            _ => None,
        }
    }

    fn step(self) -> OnboardingStep {
        match self {
            Self::Welcome | Self::WorkspaceSync => OnboardingStep::Workspace,
            Self::Appearance => OnboardingStep::Appearance,
            Self::Harnesses | Self::HarnessError => OnboardingStep::Harnesses,
            Self::Defaults | Self::Narrow => OnboardingStep::Defaults,
            Self::Titles => OnboardingStep::Titles,
            Self::Project | Self::Projectless => OnboardingStep::Project,
        }
    }
}

/// Session-scoped load and selection state. The persisted lifecycle snapshot is
/// mirrored into `UiSettings`; agent/model/title/project choices are written to
/// their normal stores by `Shell`.
pub struct OnboardingUi {
    pub state: OnboardingState,
    pub fixture: Option<OnboardingFixture>,
    pub harnesses: Loadable<Vec<HarnessDescriptor>>,
    pub models: Loadable<Vec<Model>>,
    pub accounts: Loadable<AgentAccountsSnapshot>,
    pub title_settings: Loadable<TitleSettings>,
    pub title_models: Loadable<Vec<Model>>,
    pub selected_harness: Option<HarnessId>,
    pub selected_model: Option<String>,
    pub selected_reasoning: Option<ReasoningLevel>,
    pub error: Option<SharedString>,
    pub close_confirm: bool,
    /// Control that owned focus before the close confirmation opened.
    pub close_return_focus: Option<usize>,
    /// The first mounted frame explicitly places keyboard focus in the journey.
    pub focus_initialized: bool,
    pub agent_settings_open: bool,
    pub focus: FocusHandle,
    pub controls: Vec<FocusHandle>,
    pub brand_mark: Arc<Image>,
    pub theme_menu: Popup<usize>,
    pub theme_scroll: ScrollHandle,
    pub harness_scroll: ScrollHandle,
    pub step_scroll: ScrollHandle,
    pub harness_task: Option<Task<()>>,
    pub model_task: Option<Task<()>>,
    pub accounts_task: Option<Task<()>>,
    pub title_task: Option<Task<()>>,
    pub title_model_task: Option<Task<()>>,
}

impl OnboardingUi {
    pub fn new(
        mut state: OnboardingState,
        defaults: ComposerDefaults,
        fixture: Option<OnboardingFixture>,
        cx: &mut Context<Shell>,
    ) -> Self {
        // The former sixth step only waited for a composer submit. Treat an
        // in-progress snapshot already parked there as completed instead of
        // sending an upgraded user back through the project choice.
        if state.is_active() && state.step == OnboardingStep::FirstSession {
            state.step = OnboardingStep::Project;
            state.disposition = OnboardingDisposition::Completed;
        }
        let controls = (0..32).map(|_| cx.focus_handle().tab_stop(true)).collect();
        let mut ui = Self {
            selected_harness: defaults.harness,
            selected_model: defaults
                .harness
                .and_then(|harness| defaults.model_for(harness))
                .map(|model| model.id.clone()),
            selected_reasoning: defaults.reasoning,
            state,
            fixture,
            harnesses: Loadable::Idle,
            models: Loadable::Idle,
            accounts: Loadable::Idle,
            title_settings: Loadable::Idle,
            title_models: Loadable::Idle,
            error: None,
            close_confirm: false,
            close_return_focus: None,
            focus_initialized: false,
            agent_settings_open: false,
            focus: cx.focus_handle(),
            controls,
            brand_mark: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../../../apps/landing/public/assets/zeron-app-icon.png").to_vec(),
            )),
            theme_menu: Popup::default(),
            theme_scroll: ScrollHandle::new(),
            harness_scroll: ScrollHandle::new(),
            step_scroll: ScrollHandle::new(),
            harness_task: None,
            model_task: None,
            accounts_task: None,
            title_task: None,
            title_model_task: None,
        };
        if let Some(fixture) = fixture {
            ui.apply_fixture(fixture);
        }
        ui
    }

    pub fn active(&self) -> bool {
        self.fixture.is_some() || (self.state.is_active() && !self.agent_settings_open)
    }

    pub fn step(&self) -> OnboardingStep {
        let step = self
            .fixture
            .map(OnboardingFixture::step)
            .unwrap_or(self.state.step);
        if step == OnboardingStep::FirstSession {
            OnboardingStep::Project
        } else {
            step
        }
    }

    pub fn control(&self, index: usize) -> &FocusHandle {
        &self.controls[index.min(self.controls.len() - 1)]
    }

    /// Keep a valid saved choice, otherwise choose the first usable agent in
    /// registry order. Usability deliberately does not depend on OAuth account
    /// discovery: API-key-backed CLIs are valid even without an account row.
    pub(crate) fn resolve_default_harness(&mut self) {
        let selected = self
            .harnesses
            .ready()
            .and_then(|rows| preferred_harness(rows, &self.accounts, self.selected_harness));
        if self.selected_harness != selected {
            self.selected_harness = selected;
            self.selected_model = None;
            self.selected_reasoning = None;
        }
    }

    /// Resolve the reasoning ladder exactly once for rendering and keyboard
    /// interaction. Automatic model selection falls back to the harness ladder.
    pub(crate) fn reasoning_levels(&self) -> Vec<ReasoningLevel> {
        reasoning_levels(
            &self.models,
            self.selected_model.as_deref(),
            &self.harnesses,
            self.selected_harness,
        )
    }

    fn apply_fixture(&mut self, fixture: OnboardingFixture) {
        self.state = OnboardingState::fresh();
        self.state.step = fixture.step();
        self.state.workspace_mode = Some(if fixture == OnboardingFixture::WorkspaceSync {
            WorkspaceMode::Synced
        } else {
            WorkspaceMode::Local
        });
        self.harnesses = if fixture == OnboardingFixture::HarnessError {
            Loadable::Error("Unable to inspect agents on this device.".into())
        } else {
            Loadable::Ready(fixture_harnesses())
        };
        self.models = Loadable::Ready(fixture_models());
        self.accounts = Loadable::Ready(AgentAccountsSnapshot {
            accounts: vec![zeron_proto::AgentAccount {
                id: "fixture-codex".into(),
                harness: HarnessId::Codex,
                email: Some("you@example.com".into()),
                plan_label: Some("Pro".into()),
                active: true,
                usage_windows: Vec::new(),
                display_name: None,
                organization: None,
                auth_kind: Some(zeron_proto::AgentAuthKind::Oauth),
                switchable: true,
                saved_at: None,
            }],
            warnings: Vec::new(),
        });
        self.title_settings = Loadable::Ready(TitleSettings::default());
        self.title_models = Loadable::Ready(fixture_models());
        self.selected_harness = Some(HarnessId::Codex);
        self.selected_model = Some("gpt-5.6-sol".into());
        self.selected_reasoning = Some(ReasoningLevel::High);
    }
}

fn fixture_harnesses() -> Vec<HarnessDescriptor> {
    let descriptor = |id, name: &str, installed, enabled| HarnessDescriptor {
        id,
        name: name.into(),
        supports_steering: true,
        steering_mode: SteeringMode::StepBoundary,
        reasoning_levels: vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ],
        installed,
        enabled: Some(enabled),
    };
    vec![
        descriptor(HarnessId::ClaudeCode, "Claude Code", true, true),
        descriptor(HarnessId::Codex, "Codex", true, true),
        descriptor(HarnessId::Cursor, "Cursor", true, false),
        descriptor(HarnessId::Opencode, "OpenCode", false, false),
        descriptor(HarnessId::Devin, "Devin", false, false),
        descriptor(HarnessId::Grok, "Grok", false, false),
        descriptor(HarnessId::Hermes, "Hermes", false, false),
        descriptor(HarnessId::Pi, "Pi", false, false),
    ]
}

fn fixture_models() -> Vec<Model> {
    vec![
        Model {
            id: "gpt-5.6-sol".into(),
            label: "GPT-5.6 Sol".into(),
            description: Some("Reliable agentic workhorse".into()),
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            options: Vec::new(),
        },
        Model {
            id: "gpt-6-astra".into(),
            label: "GPT-6 Astra".into(),
            description: Some("Most capable for demanding work".into()),
            reasoning_levels: vec![
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
            ],
            options: Vec::new(),
        },
    ]
}

pub fn harness_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Pi => "Pi",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Mock => "Mock",
    }
}

pub fn reasoning_name(reasoning: ReasoningLevel) -> &'static str {
    match reasoning {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-high",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

fn heading(theme: &Theme, id: &'static str, text: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Heading)
        .aria_level(1)
        .text_size(crate::typography::ui_rems(28.0))
        .line_height(px(32.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(text)
}

fn body(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt(px(10.0))
        .max_w(px(440.0))
        .text_size(crate::typography::ui_rems(14.0))
        .line_height(px(21.0))
        .text_color(theme.text_muted)
        .child(text.into())
}

fn choice_card(
    theme: &Theme,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    selected: bool,
    harness: Option<HarnessId>,
) -> gpui::Div {
    let label = label.into();
    div()
        .w_full()
        .min_h(px(76.0))
        .px(px(15.0))
        .py(px(13.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(if selected {
            theme.accent.opacity(0.65)
        } else {
            theme.border
        })
        .bg(theme.card_glass_bg())
        .flex()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.element_hover))
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .child(
            div()
                .mt(px(2.0))
                .size(px(18.0))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    theme.accent
                } else {
                    theme.border_strong
                })
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |dot| {
                    dot.child(div().size(px(8.0)).rounded_full().bg(theme.accent))
                }),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .text_size(crate::typography::ui_rems(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .when_some(harness, |row, harness| {
                            let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
                            row.child(
                                icon(icon_path)
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(theme.text)),
                            )
                        })
                        .child(label),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.text_muted)
                        .child(description.into()),
                ),
        )
}

fn action_button(theme: &Theme, label: &'static str) -> gpui::Div {
    div()
        .w_full()
        .min_h(px(44.0))
        .px(px(16.0))
        .rounded(px(10.0))
        .bg(theme.solid)
        .text_color(theme.on_solid)
        .text_size(crate::typography::ui_rems(14.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.opacity(0.9))
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .child(label)
}

fn quiet_button(theme: &Theme, label: &'static str) -> gpui::Div {
    div()
        .min_h(px(36.0))
        .px(px(10.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(crate::typography::ui_rems(12.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.element_hover).text_color(theme.text))
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .child(label)
}

pub(crate) fn activates(event: &KeyDownEvent) -> bool {
    matches!(event.keystroke.key.as_str(), "enter" | "space")
        && !event.keystroke.modifiers.modified()
}

pub(crate) fn close_dialog_tab_target(focused: Option<usize>, shift: bool) -> usize {
    if shift {
        if focused == Some(26) { 27 } else { 26 }
    } else if focused == Some(27) {
        26
    } else {
        27
    }
}

pub(crate) fn project_target_is_chosen(has_project: bool, no_project: bool) -> bool {
    has_project || no_project
}

fn toggled(value: bool) -> gpui::Toggled {
    if value {
        gpui::Toggled::True
    } else {
        gpui::Toggled::False
    }
}

fn faded_step_scroll(id: &'static str, handle: &ScrollHandle, content: AnyElement) -> AnyElement {
    crate::edge_fade::edge_faded(
        22.0,
        true,
        true,
        div()
            .id(id)
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(handle)
            .child(content),
    )
    .fade_overflow_y(handle)
    .into_any_element()
}

fn harness_status(
    descriptor: &HarnessDescriptor,
    accounts: &Loadable<AgentAccountsSnapshot>,
) -> (&'static str, bool) {
    if !descriptor.installed {
        return ("Not installed", false);
    }
    if !descriptor_enabled(descriptor) {
        return ("Installed but disabled", false);
    }
    if !matches!(
        descriptor.id,
        HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Cursor
    ) {
        return ("Ready", true);
    }
    match accounts {
        Loadable::Idle | Loadable::Loading => ("Checking sign-in…", true),
        Loadable::Error(_) => ("Sign-in status unavailable", true),
        Loadable::Ready(snapshot) => {
            if snapshot
                .warnings
                .iter()
                .any(|warning| warning.harness == descriptor.id)
            {
                ("Sign-in status unavailable", true)
            } else if snapshot
                .accounts
                .iter()
                .any(|account| account.harness == descriptor.id && account.active)
            {
                ("Ready", true)
            } else {
                ("Sign-in not detected", true)
            }
        }
    }
}

pub(crate) fn harness_is_usable(
    descriptor: &HarnessDescriptor,
    accounts: &Loadable<AgentAccountsSnapshot>,
) -> bool {
    harness_status(descriptor, accounts).1
}

fn preferred_harness(
    rows: &[HarnessDescriptor],
    accounts: &Loadable<AgentAccountsSnapshot>,
    selected: Option<HarnessId>,
) -> Option<HarnessId> {
    selected
        .filter(|selected| {
            rows.iter().any(|row| {
                row.id == *selected && row.id != HarnessId::Mock && harness_is_usable(row, accounts)
            })
        })
        .or_else(|| {
            rows.iter()
                .find(|row| row.id != HarnessId::Mock && harness_is_usable(row, accounts))
                .map(|row| row.id)
        })
}

fn reasoning_levels(
    models: &Loadable<Vec<Model>>,
    selected_model: Option<&str>,
    harnesses: &Loadable<Vec<HarnessDescriptor>>,
    selected_harness: Option<HarnessId>,
) -> Vec<ReasoningLevel> {
    models
        .ready()
        .and_then(|models| {
            models
                .iter()
                .find(|model| Some(model.id.as_str()) == selected_model)
                .map(|model| model.reasoning_levels.clone())
        })
        .or_else(|| {
            harnesses.ready().and_then(|rows| {
                rows.iter()
                    .find(|row| Some(row.id) == selected_harness)
                    .map(|row| row.reasoning_levels.clone())
            })
        })
        .unwrap_or_default()
}

pub(crate) fn harness_is_interactive(descriptor: &HarnessDescriptor) -> bool {
    descriptor.installed || descriptor_enabled(descriptor)
}

pub(crate) fn title_harness_is_available(descriptor: &HarnessDescriptor) -> bool {
    descriptor.installed
        && descriptor_enabled(descriptor)
        && zeron_harness::supports_titles(descriptor.id)
        && descriptor.id != HarnessId::Mock
}

fn render_workspace_step(ui: &OnboardingUi, theme: &Theme, cx: &mut Context<Shell>) -> AnyElement {
    let local = ui.state.workspace_mode == Some(WorkspaceMode::Local);
    let synced = ui.state.workspace_mode == Some(WorkspaceMode::Synced);
    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(theme, "onboarding-heading-workspace", "Workspace"))
        .child(body(theme, "Choose where Zeron keeps your sessions."))
        .child(
            div()
                .mt(px(28.0))
                .flex_1()
                .min_h_0()
                .child(faded_step_scroll(
                    "onboarding-workspace-scroll",
                    &ui.step_scroll,
                    div()
                        .id("onboarding-workspace-choices")
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .role(gpui::Role::RadioGroup)
                        .aria_label("Workspace mode")
                        .child(
                            choice_card(
                                theme,
                                "This device",
                                "Store sessions on this device.",
                                local,
                                None,
                            )
                            .id("onboarding-workspace-local")
                            .role(gpui::Role::RadioButton)
                            .aria_toggled(toggled(local))
                            .track_focus(ui.control(0))
                            .on_click(cx.listener(
                                |shell, _, _, cx| {
                                    shell.onboarding_pick_workspace(WorkspaceMode::Local, cx)
                                },
                            )),
                        )
                        .child(
                            choice_card(
                                theme,
                                "Sync devices",
                                "Sign in to use the same workspace on your devices.",
                                synced,
                                None,
                            )
                            .id("onboarding-workspace-sync")
                            .role(gpui::Role::RadioButton)
                            .aria_toggled(toggled(synced))
                            .track_focus(ui.control(1))
                            .on_click(cx.listener(
                                |shell, _, _, cx| {
                                    shell.onboarding_pick_workspace(WorkspaceMode::Synced, cx)
                                },
                            )),
                        )
                        .into_any_element(),
                )),
        )
        .child(
            action_button(theme, "Continue")
                .mt(px(28.0))
                .id("onboarding-continue-workspace")
                .role(gpui::Role::Button)
                .track_focus(ui.control(2))
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.onboarding_continue(cx);
                    shell.onboarding_focus_control(0, window, cx);
                })),
        )
        .into_any_element()
}

fn model_appearance(appearance: crate::theme::Appearance) -> zeron_theme::Appearance {
    if appearance.is_dark() {
        zeron_theme::Appearance::Dark
    } else {
        zeron_theme::Appearance::Light
    }
}

pub(crate) fn theme_variants(
    appearance: crate::theme::Appearance,
) -> Vec<zeron_theme::ThemeVariant> {
    ThemeRegistry::active()
        .variants_for(model_appearance(appearance))
        .cloned()
        .collect()
}

fn palette_preview(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .rounded(px(5.0))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .flex()
        .child(div().w_1_3().h_full().bg(theme.surface))
        .child(div().w_1_3().h_full().bg(theme.bg))
        .child(div().w_1_3().h_full().bg(theme.accent))
}

pub(crate) fn surface_label(surface: SurfacePreference) -> &'static str {
    match surface {
        SurfacePreference::ThemeDefault => "Theme default",
        SurfacePreference::Frosted => "Frosted glass",
        SurfacePreference::Opaque => "Solid",
    }
}

fn render_appearance_step(ui: &OnboardingUi, theme: &Theme, cx: &mut Context<Shell>) -> AnyElement {
    let current_mode = crate::appearance::mode(cx);
    let current_accent = crate::appearance::accent(cx);
    let current_surface = crate::appearance::surface(cx);
    let effective_appearance = theme.appearance;
    let current_theme_id = theme.variant_id.to_string();
    let theme_variants = theme_variants(effective_appearance);
    let selected_variant = theme_variants
        .iter()
        .find(|variant| variant.id == current_theme_id)
        .or_else(|| theme_variants.first())
        .expect("the built-in registry has both appearances");
    let selected_variant_id = selected_variant.id.clone();
    let selected_variant_name = selected_variant.name.clone();
    let selected_theme = Theme::for_selection(
        effective_appearance,
        &selected_variant_id,
        AccentSelection::ThemeDefault,
        theme.surface_preference,
    );
    let mode_choices = AppearanceMode::ALL
        .into_iter()
        .enumerate()
        .map(|(index, mode)| {
            let selected = mode == current_mode;
            chip(theme, mode.label().into(), selected, None)
                .id(("onboarding-appearance-mode", index))
                .min_h(px(44.0))
                .role(gpui::Role::RadioButton)
                .aria_toggled(toggled(selected))
                .track_focus(ui.control(index))
                .on_click(
                    cx.listener(move |shell, _, _, cx| shell.onboarding_pick_appearance(mode, cx)),
                )
        });
    let theme_trigger = div()
        .id("onboarding-theme-selector")
        .relative()
        .w_full()
        .h(px(42.0))
        .px(px(12.0))
        .rounded(px(9.0))
        .border_1()
        .border_color(if ui.theme_menu.is_open() {
            theme.border_strong
        } else {
            theme.border
        })
        .bg(theme.card_glass_bg())
        .flex()
        .items_center()
        .gap(px(9.0))
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(format!("Theme, selected {selected_variant_name}"))
        .aria_expanded(ui.theme_menu.is_open())
        .track_focus(ui.control(3))
        .hover(|style| style.bg(theme.element_hover))
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|shell, _, _, _| shell.onboarding_note_theme_trigger_press()),
        )
        .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_toggle_theme_menu(cx)))
        .child(palette_preview(&selected_theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(12.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(selected_variant_name)),
        )
        .child(
            icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(ui.theme_menu.get(), |trigger, _| {
            let theme_rows = theme_variants.iter().enumerate().map(|(index, variant)| {
                let id = variant.id.clone();
                let selected = id == current_theme_id;
                let sample = Theme::for_selection(
                    effective_appearance,
                    &id,
                    AccentSelection::ThemeDefault,
                    theme.surface_preference,
                );
                popover::menu_row_nav(
                    theme,
                    selected,
                    ui.theme_menu.as_open().copied() == Some(index),
                    format!("onboarding-theme-row-{index}"),
                )
                .id(("onboarding-theme-row", index))
                .on_click(cx.listener(move |shell, _, _, cx| {
                    shell.onboarding_pick_theme(effective_appearance, id.clone(), cx)
                }))
                .child(palette_preview(&sample))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(variant.name.clone())),
                )
                .child(div().w(px(18.0)).flex_none().when(selected, |slot| {
                    slot.child(
                        icon(crate::icons::CHECK)
                            .size(px(14.0))
                            .text_color(theme.accent),
                    )
                }))
            });
            let theme_menu = popover::popover_card(theme)
                .id("onboarding-theme-menu")
                .w(px(320.0))
                .max_h(px(320.0))
                .overflow_y_scroll()
                .track_scroll(&ui.theme_scroll)
                .on_mouse_down_out(
                    cx.listener(|shell, _, _, cx| shell.onboarding_close_theme_menu(cx)),
                )
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(
                    theme,
                    if effective_appearance.is_dark() {
                        "Dark themes"
                    } else {
                        "Light themes"
                    },
                ))
                .children(theme_rows)
                .into_any_element();
            trigger.child(popover::anchored_menu_below(
                "onboarding-theme-menu-layer",
                theme_menu,
                ui.theme_menu.closing_since(),
            ))
        });
    let accent_choices = AccentPreset::ALL
        .into_iter()
        .enumerate()
        .map(|(index, preset)| {
            let selection = AccentSelection::Preset(preset);
            let selected = current_accent == selection
                || (preset == AccentPreset::Zeron
                    && current_accent == AccentSelection::ThemeDefault);
            let swatch_theme = Theme::for_preferences(theme.appearance, preset.into());
            div()
                .id(("onboarding-accent", index))
                .size(px(44.0))
                .rounded_full()
                .border_2()
                .border_color(if selected {
                    theme.text
                } else {
                    theme.border_strong
                })
                .bg(swatch_theme.accent)
                .cursor_pointer()
                .role(gpui::Role::RadioButton)
                .aria_label(format!("{} accent", preset.label()))
                .aria_toggled(toggled(selected))
                .track_focus(ui.control(8 + index))
                .hover(|style| style.opacity(0.82))
                .focus_visible(|style| style.border_2().border_color(theme.text))
                .on_click(
                    cx.listener(move |shell, _, _, cx| shell.onboarding_pick_accent(selection, cx)),
                )
        });
    let surface_choices = SurfacePreference::ALL
        .into_iter()
        .enumerate()
        .map(|(index, surface)| {
            let selected = surface == current_surface;
            chip(theme, surface_label(surface).into(), selected, None)
                .id(("onboarding-surface", index))
                .role(gpui::Role::RadioButton)
                .aria_toggled(toggled(selected))
                .track_focus(ui.control(16 + index))
                .on_click(
                    cx.listener(move |shell, _, _, cx| shell.onboarding_pick_surface(surface, cx)),
                )
        });

    let controls = div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Appearance"),
                )
                .child(
                    div()
                        .id("onboarding-appearance-modes")
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .role(gpui::Role::RadioGroup)
                        .aria_label("Appearance")
                        .children(mode_choices),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Theme"),
                )
                .child(
                    div()
                        .id("onboarding-themes")
                        .role(gpui::Role::Group)
                        .aria_label("Theme")
                        .child(theme_trigger),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Accent color"),
                )
                .child(
                    div()
                        .id("onboarding-accents")
                        .flex()
                        .flex_wrap()
                        .gap(px(10.0))
                        .role(gpui::Role::RadioGroup)
                        .aria_label("Accent color")
                        .children(accent_choices),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Surface"),
                )
                .child(
                    div()
                        .id("onboarding-surfaces")
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .role(gpui::Role::RadioGroup)
                        .aria_label("Surface")
                        .children(surface_choices),
                ),
        )
        .into_any_element();

    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(
            theme,
            "onboarding-heading-appearance",
            "Appearance",
        ))
        .child(body(theme, "Choose a theme and accent."))
        .child(
            div()
                .mt(px(24.0))
                .flex_1()
                .min_h_0()
                .child(faded_step_scroll(
                    "onboarding-appearance-scroll",
                    &ui.step_scroll,
                    controls,
                )),
        )
        .child(
            action_button(theme, "Continue")
                .mt(px(24.0))
                .id("onboarding-continue-appearance")
                .role(gpui::Role::Button)
                .track_focus(ui.control(25))
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.onboarding_continue(cx);
                    shell.onboarding_focus_control(0, window, cx);
                })),
        )
        .into_any_element()
}

fn render_harness_step(
    ui: &OnboardingUi,
    theme: &Theme,
    state: &Entity<AppState>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let devices = {
        let state = state.read(cx);
        let effective = state.effective_device_id();
        let now = chrono::Utc::now();
        state
            .devices
            .iter()
            .take(5)
            .map(|device| {
                (
                    device.id.clone(),
                    device.name.clone(),
                    state.device_online(&device.id, now),
                    effective.as_deref() == Some(device.id.as_str()),
                )
            })
            .collect::<Vec<_>>()
    };
    let multiple_devices = devices.len() > 1;
    let device_switcher: AnyElement = if multiple_devices {
        div()
            .mt(px(18.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child("Set up agents on"),
            )
            .child(
                div()
                    .id("onboarding-device-choices")
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .role(gpui::Role::RadioGroup)
                    .aria_label("Device")
                    .children(devices.into_iter().enumerate().map(
                        |(index, (id, name, online, selected))| {
                            let label: SharedString = if online {
                                name.into()
                            } else {
                                format!("{name} · Offline").into()
                            };
                            chip(theme, label, selected, None)
                                .id(("onboarding-device", index))
                                .role(gpui::Role::RadioButton)
                                .aria_toggled(toggled(selected))
                                .track_focus(ui.control(20 + index))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.onboarding_pick_device(id.clone(), cx)
                                }))
                        },
                    )),
            )
            .into_any_element()
    } else {
        Empty.into_any_element()
    };
    let list: AnyElement = match &ui.harnesses {
        Loadable::Idle | Loadable::Loading => div()
            .id("onboarding-harness-loading")
            .role(gpui::Role::ProgressIndicator)
            .aria_label("Loading coding agents")
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children((0..4).map(|_| div().h(px(54.0)).rounded(px(10.0)).bg(ink(0.045))))
            .into_any_element(),
        Loadable::Error(message) => div()
            .id("onboarding-harness-load-error")
            .role(gpui::Role::Alert)
            .p(px(14.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.danger_muted.opacity(0.5))
            .bg(theme.danger.opacity(0.06))
            .text_size(crate::typography::ui_rems(12.5))
            .line_height(px(18.0))
            .text_color(theme.danger_muted)
            .child(message.clone())
            .child(
                quiet_button(theme, "Retry")
                    .id("onboarding-harness-retry")
                    .role(gpui::Role::Button)
                    .track_focus(ui.control(11))
                    .mt(px(8.0))
                    .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_load_harnesses(cx))),
            )
            .into_any_element(),
        Loadable::Ready(harnesses) => div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(
                harnesses
                    .iter()
                    .enumerate()
                    .filter(|(_, descriptor)| descriptor.id != HarnessId::Mock)
                    .map(|(index, descriptor)| {
                        let descriptor = descriptor.clone();
                        let installed = descriptor.installed;
                        let enabled = descriptor_enabled(&descriptor);
                        let (status, _) = harness_status(&descriptor, &ui.accounts);
                        let interactive = harness_is_interactive(&descriptor);
                        let focus_index = harnesses[..index]
                            .iter()
                            .filter(|row| row.id != HarnessId::Mock && harness_is_interactive(row))
                            .count();
                        let (icon_path, tint) = crate::pickers::harness_brand_icon(descriptor.id);
                        div()
                            .id(("onboarding-harness", index))
                            .min_h(px(54.0))
                            .px(px(12.0))
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(if enabled {
                                theme.accent.opacity(0.65)
                            } else {
                                theme.border
                            })
                            .bg(theme.card_glass_bg())
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .when(interactive, |row| {
                                let descriptor = descriptor.clone();
                                row.cursor_pointer()
                                    .hover(|s| s.bg(theme.element_hover))
                                    .role(gpui::Role::CheckBox)
                                    .aria_toggled(toggled(enabled))
                                    .track_focus(ui.control(focus_index))
                                    .on_click(cx.listener(move |shell, _, _, cx| {
                                        shell.onboarding_toggle_harness(descriptor.id, !enabled, cx)
                                    }))
                            })
                            .focus_visible(|style| style.border_2().border_color(theme.accent))
                            .when(!installed, |row| row.opacity(0.58))
                            .when(!interactive, |row| {
                                row.aria_description(format!(
                                    "{} is unavailable until its command-line tool is installed",
                                    descriptor.name
                                ))
                            })
                            .child(
                                div()
                                    .size(px(32.0))
                                    .rounded(px(9.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(icon_path)
                                            .size(px(15.0))
                                            .text_color(tint.unwrap_or(theme.text_muted)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(13.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(descriptor.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(theme.text_muted)
                                            .child(if interactive {
                                                SharedString::from(status)
                                            } else {
                                                format!(
                                                    "Install {}'s CLI, then retry",
                                                    descriptor.name
                                                )
                                                .into()
                                            }),
                                    ),
                            )
                            .when(interactive, |row| {
                                row.child(
                                    div()
                                        .w(px(34.0))
                                        .h(px(20.0))
                                        .rounded_full()
                                        .p(px(2.0))
                                        .bg(if enabled {
                                            theme.accent_strong
                                        } else {
                                            theme.border_strong
                                        })
                                        .flex()
                                        .justify_end()
                                        .when(!enabled, |toggle| toggle.justify_start())
                                        .child(
                                            div().size(px(16.0)).rounded_full().bg(theme.on_accent),
                                        ),
                                )
                            })
                    }),
            )
            .into_any_element(),
    };
    let ready = matches!(&ui.harnesses, Loadable::Ready(rows) if rows
        .iter()
        .any(|h| h.id != HarnessId::Mock && harness_is_usable(h, &ui.accounts)));
    let harness_list = div()
        .mt(px(if multiple_devices { 14.0 } else { 22.0 }))
        .flex_1()
        .min_h_0()
        .child(faded_step_scroll(
            "onboarding-harness-scroll",
            &ui.harness_scroll,
            list,
        ));
    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(
            theme,
            "onboarding-heading-harnesses",
            "Coding agents",
        ))
        .child(body(theme, "Turn on the agents you want to use."))
        .child(device_switcher)
        .child(harness_list)
        .when(matches!(ui.harnesses, Loadable::Ready(_)), |column| {
            column.child(
                quiet_button(theme, "Retry agent detection")
                    .id("onboarding-harness-retry-ready")
                    .mt(px(8.0))
                    .role(gpui::Role::Button)
                    .track_focus(ui.control(11))
                    .on_click(cx.listener(|shell, _, _, cx| {
                        shell.onboarding_load_harnesses(cx);
                        shell.onboarding_load_accounts(cx);
                    })),
            )
        })
        .child(
            quiet_button(theme, "Manage agent sign-ins")
                .id("onboarding-manage-agent-signins")
                .mt(px(8.0))
                .role(gpui::Role::Button)
                .track_focus(ui.control(12))
                .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_open_agent_settings(cx))),
        )
        .when_some(ui.error.clone(), |column, error| {
            column.child(
                div()
                    .id("onboarding-harness-error")
                    .mb(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .role(gpui::Role::Alert)
                    .child(error),
            )
        })
        .child(
            action_button(theme, "Continue")
                .mt(px(18.0))
                .id("onboarding-continue-harnesses")
                .role(gpui::Role::Button)
                .when(!ready, |button| {
                    button
                        .opacity(0.45)
                        .cursor_default()
                        .aria_description("Unavailable until at least one agent is ready")
                })
                .when(ready, |button| {
                    button.track_focus(ui.control(10)).on_click(cx.listener(
                        |shell, _, window, cx| {
                            shell.onboarding_continue(cx);
                            shell.onboarding_focus_control(0, window, cx);
                        },
                    ))
                }),
        )
        .into_any_element()
}

fn chip(
    theme: &Theme,
    label: SharedString,
    selected: bool,
    harness: Option<HarnessId>,
) -> gpui::Div {
    div()
        .min_h(px(38.0))
        .px(px(12.0))
        .rounded(px(9.0))
        .border_1()
        .border_color(if selected {
            theme.accent.opacity(0.65)
        } else {
            theme.border
        })
        .bg(theme.card_glass_bg())
        .text_size(crate::typography::ui_rems(12.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if selected {
            theme.text
        } else {
            theme.text_muted
        })
        .flex()
        .items_center()
        .justify_center()
        .gap(px(7.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.element_hover))
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .when_some(harness, |chip, harness| {
            let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
            chip.child(
                icon(icon_path)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(tint.unwrap_or(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })),
            )
        })
        .child(label)
}

fn render_defaults_step(ui: &OnboardingUi, theme: &Theme, cx: &mut Context<Shell>) -> AnyElement {
    let harnesses: Vec<HarnessDescriptor> = ui
        .harnesses
        .ready()
        .map(|rows| {
            rows.iter()
                .filter(|h| h.id != HarnessId::Mock && harness_is_usable(h, &ui.accounts))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let models = ui.models.ready().cloned().unwrap_or_default();
    let reasoning = ui.reasoning_levels();
    let field = |label: &'static str, content: AnyElement| {
        div()
            .flex()
            .flex_col()
            .gap(px(9.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(label),
            )
            .child(content)
    };
    let harness_chips = div()
        .id("onboarding-default-harnesses")
        .flex()
        .flex_wrap()
        .gap(px(8.0))
        .role(gpui::Role::RadioGroup)
        .aria_label("Default agent")
        .children(
            harnesses
                .into_iter()
                .enumerate()
                .map(|(index, descriptor)| {
                    let id = descriptor.id;
                    chip(
                        theme,
                        descriptor.name.into(),
                        ui.selected_harness == Some(id),
                        Some(id),
                    )
                    .id(("onboarding-default-harness", index))
                    .role(gpui::Role::RadioButton)
                    .aria_toggled(toggled(ui.selected_harness == Some(id)))
                    .track_focus(ui.control(index))
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        shell.onboarding_pick_default_harness(id, cx)
                    }))
                }),
        );
    let model_content: AnyElement =
        match &ui.models {
            Loadable::Idle | Loadable::Loading => div()
                .id("onboarding-model-loading")
                .role(gpui::Role::ProgressIndicator)
                .aria_label("Loading models")
                .h(px(38.0))
                .rounded(px(9.0))
                .bg(ink(0.045))
                .into_any_element(),
            Loadable::Error(error) => div()
                .id("onboarding-model-load-error")
                .role(gpui::Role::Alert)
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.danger_muted)
                .child(error.clone())
                .into_any_element(),
            Loadable::Ready(_) => div()
                .id("onboarding-default-models")
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .role(gpui::Role::RadioGroup)
                .aria_label("Default model")
                .child(
                    chip(theme, "Automatic".into(), ui.selected_model.is_none(), None)
                        .id("onboarding-default-model-auto")
                        .role(gpui::Role::RadioButton)
                        .aria_toggled(toggled(ui.selected_model.is_none()))
                        .track_focus(ui.control(8))
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.onboarding_pick_default_model(None, cx)
                        })),
                )
                .children(
                    models
                        .into_iter()
                        .take(4)
                        .enumerate()
                        .map(|(index, model)| {
                            let selected = ui.selected_model.as_deref() == Some(model.id.as_str());
                            let id = model.id;
                            chip(theme, model.label.into(), selected, ui.selected_harness)
                                .id(("onboarding-default-model", index))
                                .role(gpui::Role::RadioButton)
                                .aria_toggled(toggled(selected))
                                .track_focus(ui.control(9 + index))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.onboarding_pick_default_model(Some(id.clone()), cx)
                                }))
                        }),
                )
                .into_any_element(),
        };
    let reasoning_chips =
        div()
            .id("onboarding-default-reasoning-levels")
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .role(gpui::Role::RadioGroup)
            .aria_label("Default reasoning level")
            .child(
                chip(
                    theme,
                    "Automatic".into(),
                    ui.selected_reasoning.is_none(),
                    None,
                )
                .id("onboarding-default-reasoning-auto")
                .role(gpui::Role::RadioButton)
                .aria_toggled(toggled(ui.selected_reasoning.is_none()))
                .track_focus(ui.control(16))
                .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_pick_reasoning(None, cx))),
            )
            .children(reasoning.into_iter().enumerate().map(|(index, level)| {
                chip(
                    theme,
                    reasoning_name(level).into(),
                    ui.selected_reasoning == Some(level),
                    None,
                )
                .id(("onboarding-default-reasoning", index))
                .role(gpui::Role::RadioButton)
                .aria_toggled(toggled(ui.selected_reasoning == Some(level)))
                .track_focus(ui.control(17 + index))
                .on_click(cx.listener(move |shell, _, _, cx| {
                    shell.onboarding_pick_reasoning(Some(level), cx)
                }))
            }));
    let fields = div()
        .flex()
        .flex_col()
        .gap(px(22.0))
        .child(field("Agent", harness_chips.into_any_element()))
        .child(field("Model", model_content))
        .child(field("Reasoning", reasoning_chips.into_any_element()))
        .into_any_element();
    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(
            theme,
            "onboarding-heading-defaults",
            "Session defaults",
        ))
        .child(body(theme, "Choose defaults for new sessions."))
        .child(
            div()
                .mt(px(26.0))
                .flex_1()
                .min_h_0()
                .child(faded_step_scroll(
                    "onboarding-defaults-scroll",
                    &ui.step_scroll,
                    fields,
                )),
        )
        .child(
            action_button(theme, "Continue")
                .mt(px(28.0))
                .id("onboarding-continue-defaults")
                .role(gpui::Role::Button)
                // Keep the full nine-level reasoning ladder addressable; its
                // final option occupies control 25.
                .track_focus(ui.control(26))
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.onboarding_continue(cx);
                    shell.onboarding_focus_control(0, window, cx);
                })),
        )
        .into_any_element()
}

fn render_titles_step(ui: &OnboardingUi, theme: &Theme, cx: &mut Context<Shell>) -> AnyElement {
    let current = match &ui.title_settings {
        Loadable::Ready(current) => current.clone(),
        Loadable::Idle | Loadable::Loading => {
            return div()
                .size_full()
                .min_h_0()
                .flex()
                .flex_col()
                .child(heading(
                    theme,
                    "onboarding-heading-titles",
                    "Session titles",
                ))
                .child(body(theme, "Choose how new sessions are named."))
                .child(
                    div()
                        .id("onboarding-title-settings-loading")
                        .mt(px(26.0))
                        .h(px(44.0))
                        .rounded(px(10.0))
                        .bg(ink(0.045))
                        .role(gpui::Role::ProgressIndicator)
                        .aria_label("Loading title settings"),
                )
                .child(
                    action_button(theme, "Continue")
                        .mt(px(28.0))
                        .id("onboarding-continue-titles-loading")
                        .role(gpui::Role::Button)
                        .track_focus(ui.control(25))
                        .on_click(cx.listener(|shell, _, window, cx| {
                            shell.onboarding_continue(cx);
                            shell.onboarding_focus_control(0, window, cx);
                        })),
                )
                .into_any_element();
        }
        Loadable::Error(error) => {
            return div()
                .size_full()
                .min_h_0()
                .flex()
                .flex_col()
                .child(heading(
                    theme,
                    "onboarding-heading-titles",
                    "Session titles",
                ))
                .child(body(theme, "Choose how new sessions are named."))
                .child(
                    div()
                        .id("onboarding-title-settings-error")
                        .mt(px(26.0))
                        .p(px(14.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.danger_muted.opacity(0.5))
                        .bg(theme.danger.opacity(0.06))
                        .text_size(crate::typography::ui_rems(12.5))
                        .line_height(px(18.0))
                        .text_color(theme.danger_muted)
                        .role(gpui::Role::Alert)
                        .child(format!("Could not load title settings: {error}")),
                )
                .child(
                    quiet_button(theme, "Retry")
                        .id("onboarding-title-settings-retry")
                        .mt(px(10.0))
                        .role(gpui::Role::Button)
                        .track_focus(ui.control(24))
                        .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_load_titles(cx))),
                )
                .child(
                    action_button(theme, "Continue")
                        .mt(px(18.0))
                        .id("onboarding-continue-titles-error")
                        .role(gpui::Role::Button)
                        .track_focus(ui.control(25))
                        .on_click(cx.listener(|shell, _, window, cx| {
                            shell.onboarding_continue(cx);
                            shell.onboarding_focus_control(0, window, cx);
                        })),
                )
                .into_any_element();
        }
    };
    let available: Vec<HarnessId> = ui
        .harnesses
        .ready()
        .map(|rows| {
            rows.iter()
                .filter(|h| title_harness_is_available(h))
                .map(|h| h.id)
                .collect()
        })
        .unwrap_or_default();
    let mut choices = div()
        .id("onboarding-title-harnesses")
        .flex()
        .flex_col()
        .gap(px(10.0))
        .role(gpui::Role::RadioGroup)
        .aria_label("Title agent")
        .child(
            choice_card(
                theme,
                "Automatic",
                "Use an available agent, or the first seven words.",
                current.harness.is_none(),
                None,
            )
            .id("onboarding-title-auto")
            .role(gpui::Role::RadioButton)
            .aria_toggled(toggled(current.harness.is_none()))
            .track_focus(ui.control(0))
            .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_pick_title_harness(None, cx))),
        );
    for (index, harness) in available.into_iter().enumerate() {
        choices = choices.child(
            choice_card(
                theme,
                harness_name(harness),
                "Generate a title without project tools.",
                current.harness == Some(harness),
                Some(harness),
            )
            .id(("onboarding-title-harness", index))
            .role(gpui::Role::RadioButton)
            .aria_toggled(toggled(current.harness == Some(harness)))
            .track_focus(ui.control(1 + index))
            .on_click(cx.listener(move |shell, _, _, cx| {
                shell.onboarding_pick_title_harness(Some(harness), cx)
            })),
        );
    }
    let title_models: AnyElement = if current.harness.is_none() {
        Empty.into_any_element()
    } else {
        match &ui.title_models {
            Loadable::Idle | Loadable::Loading => div()
                .id("onboarding-title-model-loading")
                .mt(px(18.0))
                .h(px(38.0))
                .rounded(px(9.0))
                .bg(ink(0.045))
                .role(gpui::Role::ProgressIndicator)
                .aria_label("Loading title models")
                .into_any_element(),
            Loadable::Error(error) => div()
                .id("onboarding-title-model-error")
                .mt(px(18.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.danger_muted)
                .role(gpui::Role::Alert)
                .child(error.clone())
                .into_any_element(),
            Loadable::Ready(models) => div()
                .mt(px(18.0))
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Title model"),
                )
                .child(
                    div()
                        .id("onboarding-title-models")
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .role(gpui::Role::RadioGroup)
                        .aria_label("Title model")
                        .child(
                            chip(
                                theme,
                                "Automatic · cheapest suitable".into(),
                                current.model.is_none(),
                                None,
                            )
                            .id("onboarding-title-model-auto")
                            .role(gpui::Role::RadioButton)
                            .aria_toggled(toggled(current.model.is_none()))
                            .track_focus(ui.control(8))
                            .on_click(cx.listener(
                                |shell, _, _, cx| shell.onboarding_pick_title_model(None, cx),
                            )),
                        )
                        .children(models.iter().take(4).enumerate().map(|(index, model)| {
                            let selected = current.model.as_deref() == Some(model.id.as_str());
                            let id = model.id.clone();
                            chip(theme, model.label.clone().into(), selected, current.harness)
                                .id(("onboarding-title-model", index))
                                .role(gpui::Role::RadioButton)
                                .aria_toggled(toggled(selected))
                                .track_focus(ui.control(9 + index))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.onboarding_pick_title_model(Some(id.clone()), cx)
                                }))
                        })),
                )
                .into_any_element(),
        }
    };
    let title_options = div()
        .child(choices)
        .child(title_models)
        .when_some(ui.error.clone(), |column, error| {
            column.child(
                div()
                    .id("onboarding-title-save-error")
                    .mb(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .role(gpui::Role::Alert)
                    .child(error),
            )
        })
        .into_any_element();
    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(
            theme,
            "onboarding-heading-titles",
            "Session titles",
        ))
        .child(body(theme, "Choose how new sessions are named."))
        .child(
            div()
                .mt(px(25.0))
                .flex_1()
                .min_h_0()
                .child(faded_step_scroll(
                    "onboarding-titles-scroll",
                    &ui.step_scroll,
                    title_options,
                )),
        )
        .child(
            action_button(theme, "Continue")
                .mt(px(28.0))
                .id("onboarding-continue-titles")
                .role(gpui::Role::Button)
                .track_focus(ui.control(25))
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.onboarding_continue(cx);
                    shell.onboarding_focus_control(0, window, cx);
                })),
        )
        .into_any_element()
}

fn render_project_step(
    ui: &OnboardingUi,
    theme: &Theme,
    state: &Entity<AppState>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let (selected_name, selected, no_project): (SharedString, bool, bool) =
        if ui.fixture == Some(OnboardingFixture::Project) {
            ("Comet".into(), true, false)
        } else if ui.fixture == Some(OnboardingFixture::Projectless) {
            (
                "Choose a folder on any connected device".into(),
                false,
                true,
            )
        } else {
            let state = state.read(cx);
            let selected = state.selected_space_row();
            (
                selected
                    .map(|space| space.display_name().to_string().into())
                    .unwrap_or_else(|| "Choose a folder on any connected device".into()),
                selected.is_some(),
                state.no_project,
            )
        };
    let choices = div()
        .id("onboarding-project-choices")
        .flex()
        .flex_col()
        .gap(px(12.0))
        .role(gpui::Role::RadioGroup)
        .aria_label("Project target")
        .child(
            choice_card(
                theme,
                "Start in a project",
                selected_name,
                selected && !no_project,
                None,
            )
            .id("onboarding-project-choose")
            .role(gpui::Role::RadioButton)
            .aria_toggled(toggled(selected && !no_project))
            .track_focus(ui.control(0))
            .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_open_project(cx))),
        )
        .child(
            choice_card(
                theme,
                "Continue without a project",
                "Add a project when you need one.",
                no_project,
                None,
            )
            .id("onboarding-project-none")
            .role(gpui::Role::RadioButton)
            .aria_toggled(toggled(no_project))
            .track_focus(ui.control(1))
            .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_pick_no_project(cx))),
        )
        .when_some(ui.error.clone(), |column, error| {
            column.child(
                div()
                    .id("onboarding-project-error")
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .role(gpui::Role::Alert)
                    .child(error),
            )
        })
        .into_any_element();
    div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(heading(theme, "onboarding-heading-project", "Project"))
        .child(body(theme, "Choose a folder now, or add one later."))
        .child(
            div()
                .mt(px(27.0))
                .flex_1()
                .min_h_0()
                .child(faded_step_scroll(
                    "onboarding-project-scroll",
                    &ui.step_scroll,
                    choices,
                )),
        )
        .child(
            action_button(theme, "Finish setup")
                .mt(px(28.0))
                .id("onboarding-continue-project")
                .role(gpui::Role::Button)
                .track_focus(ui.control(2))
                .on_click(cx.listener(|shell, _, _, cx| shell.onboarding_continue(cx))),
        )
        .into_any_element()
}

fn render_progress(step: OnboardingStep, theme: &Theme) -> AnyElement {
    div()
        .id("onboarding-progress")
        .flex()
        .gap(px(6.0))
        .role(gpui::Role::ProgressIndicator)
        .aria_label(format!("Step {} of {STEP_COUNT}", step.index() + 1))
        .children(
            OnboardingStep::ALL
                .into_iter()
                .enumerate()
                .map(|(index, _)| {
                    div()
                        .h(px(3.0))
                        .w(px(34.0))
                        .rounded_full()
                        .bg(if index <= step.index() {
                            theme.accent
                        } else {
                            theme.border_strong
                        })
                }),
        )
        .into_any_element()
}

pub fn render(
    ui: &OnboardingUi,
    state: &Entity<AppState>,
    title_bar: AnyElement,
    viewport: gpui::Size<Pixels>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let theme = Theme::of(cx).clone();
    let step = ui.step();
    let viewport_width = f32::from(viewport.width);
    let viewport_height = f32::from(viewport.height);
    let constrained = viewport_width < 680.0 || viewport_height < 680.0;
    let outer_x = if constrained { 12.0 } else { 32.0 };
    let outer_y = if constrained { 12.0 } else { 24.0 };
    let inner_pad = if constrained { 20.0 } else { 32.0 };
    let card_max_height = if matches!(step, OnboardingStep::Workspace | OnboardingStep::Project) {
        480.0
    } else {
        720.0
    };
    let decision = match step {
        OnboardingStep::Workspace => render_workspace_step(ui, &theme, cx),
        OnboardingStep::Appearance => render_appearance_step(ui, &theme, cx),
        OnboardingStep::Harnesses => render_harness_step(ui, &theme, state, cx),
        OnboardingStep::Defaults => render_defaults_step(ui, &theme, cx),
        OnboardingStep::Titles => render_titles_step(ui, &theme, cx),
        OnboardingStep::Project => render_project_step(ui, &theme, state, cx),
        OnboardingStep::FirstSession => render_project_step(ui, &theme, state, cx),
    };
    let decision_content = div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .child(decision);
    let decision_content = crate::motion::fade_quick(
        SharedString::from(format!("onboarding-step-{}", step.index())),
        decision_content,
    );
    let journey = div()
        .w_full()
        .h_full()
        .max_w(px(560.0))
        .max_h(px(card_max_height))
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .p(px(inner_pad))
        .rounded(px(18.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.card_glass_bg())
        .shadow_sm()
        .flex()
        .flex_col()
        .child(div().flex_1().min_h_0().child(decision_content))
        .child(
            div()
                .h(px(40.0))
                .flex_none()
                .flex()
                .items_end()
                .justify_center()
                .child(render_progress(step, &theme)),
        );
    let panel = div()
        .absolute()
        .inset_0()
        .pt(px(Theme::TITLEBAR_HEIGHT + outer_y))
        .px(px(outer_x))
        .pb(px(outer_y))
        .min_w_0()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .child(journey);
    let close_confirm: AnyElement = if ui.close_confirm {
        div()
            .absolute()
            .inset_0()
            .bg(theme.scrim())
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(crate::frost::frosted(
                14.0,
                crate::frost::MENU_BLUR,
                div()
                    .id("onboarding-close-dialog")
                    .w(px(360.0))
                    .p(px(22.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.glass_overlay())
                    .shadow_lg()
                    .role(gpui::Role::Dialog)
                    .aria_label("Continue onboarding later")
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(16.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Continue setting up later?"),
                    )
                    .child(body(
                        &theme,
                        "Your choices are saved. Zeron will return to this step the next time you open the app.",
                    ))
                    .child(
                        div()
                            .mt(px(20.0))
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                quiet_button(&theme, "Keep setting up")
                                    .id("onboarding-close-cancel")
                                    .role(gpui::Role::Button)
                                    .track_focus(ui.control(26))
                                    .on_click(cx.listener(|shell, _, window, cx| {
                                        shell.onboarding_cancel_close(window, cx);
                                    })),
                            )
                            .child(
                                action_button(&theme, "Continue later")
                                    .id("onboarding-close-confirm")
                                    .role(gpui::Role::Button)
                                    .track_focus(ui.control(27))
                                    .w_auto()
                                    .on_click(cx.listener(|shell, _, _, cx| {
                                        shell.onboarding_defer(cx)
                                    })),
                            ),
                    ),
            ))
            .into_any_element()
    } else {
        Empty.into_any_element()
    };
    div()
        .id("onboarding-root")
        .role(gpui::Role::Main)
        .aria_label(format!(
            "Zeron setup, step {} of {STEP_COUNT}",
            step.index() + 1
        ))
        .size_full()
        .relative()
        .track_focus(&ui.focus)
        .text_color(theme.text)
        .font_family(theme.font_sans.clone())
        // Keep the journey and native drag region mounted behind the modal.
        // The full-window occluding scrim owns pointer input, while the shell's
        // key handler traps focus inside the dialog.
        .child(panel)
        .child(title_bar)
        .child(close_confirm)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_navigation_is_bounded() {
        assert_eq!(
            OnboardingStep::Workspace.previous(),
            OnboardingStep::Workspace
        );
        assert_eq!(OnboardingStep::Workspace.next(), OnboardingStep::Appearance);
        assert_eq!(OnboardingStep::Appearance.next(), OnboardingStep::Harnesses);
        assert_eq!(OnboardingStep::Project.next(), OnboardingStep::Project);
        assert_eq!(OnboardingStep::Project.index() + 1, STEP_COUNT);
    }

    #[test]
    fn established_profiles_default_to_completed() {
        assert_eq!(
            OnboardingState::default().disposition,
            OnboardingDisposition::Completed
        );
        assert!(OnboardingState::fresh().is_active());
        assert_eq!(
            OnboardingState::fresh().workspace_mode,
            Some(WorkspaceMode::Local)
        );
    }

    #[test]
    fn fixture_routes_are_stable() {
        assert_eq!(
            OnboardingFixture::from_route(Some("onboarding/agents")),
            Some(OnboardingFixture::Harnesses)
        );
        assert_eq!(
            OnboardingFixture::from_route(Some("onboarding/appearance"))
                .map(OnboardingFixture::step),
            Some(OnboardingStep::Appearance)
        );
        assert_eq!(
            OnboardingFixture::from_route(Some("onboarding/first-session"))
                .map(OnboardingFixture::step),
            Some(OnboardingStep::Project)
        );
        assert_eq!(
            OnboardingFixture::from_route(Some("onboarding/narrow")).map(OnboardingFixture::step),
            Some(OnboardingStep::Defaults)
        );
    }

    #[test]
    fn readiness_requires_installation_and_enablement_but_not_oauth_detection() {
        let rows = fixture_harnesses();
        let codex = rows.iter().find(|row| row.id == HarnessId::Codex).unwrap();
        let opencode = rows
            .iter()
            .find(|row| row.id == HarnessId::Opencode)
            .unwrap();
        assert!(harness_is_usable(codex, &Loadable::Loading));
        assert!(harness_is_usable(
            codex,
            &Loadable::Ready(AgentAccountsSnapshot {
                accounts: Vec::new(),
                warnings: Vec::new(),
            })
        ));
        assert!(!harness_is_usable(
            opencode,
            &Loadable::Ready(AgentAccountsSnapshot {
                accounts: Vec::new(),
                warnings: Vec::new(),
            })
        ));
        let mut enabled_opencode = opencode.clone();
        enabled_opencode.installed = true;
        enabled_opencode.enabled = Some(true);
        assert!(harness_is_usable(&enabled_opencode, &Loadable::Idle));

        let accounts = Loadable::Ready(AgentAccountsSnapshot {
            accounts: vec![zeron_proto::AgentAccount {
                id: "codex".into(),
                harness: HarnessId::Codex,
                email: None,
                plan_label: None,
                active: true,
                usage_windows: Vec::new(),
                display_name: None,
                organization: None,
                auth_kind: Some(zeron_proto::AgentAuthKind::Oauth),
                switchable: true,
                saved_at: None,
            }],
            warnings: Vec::new(),
        });
        assert!(harness_is_usable(codex, &accounts));
    }

    #[test]
    fn default_harness_is_stable_across_account_callback_order() {
        let rows = fixture_harnesses();
        let no_accounts = Loadable::Ready(AgentAccountsSnapshot {
            accounts: Vec::new(),
            warnings: Vec::new(),
        });

        assert_eq!(
            preferred_harness(&rows, &Loadable::Loading, None),
            Some(HarnessId::ClaudeCode)
        );
        assert_eq!(
            preferred_harness(&rows, &no_accounts, None),
            Some(HarnessId::ClaudeCode)
        );
        assert_eq!(
            preferred_harness(&rows, &no_accounts, Some(HarnessId::Codex)),
            Some(HarnessId::Codex)
        );
    }

    #[test]
    fn automatic_model_uses_the_selected_harness_reasoning_ladder() {
        let harnesses = Loadable::Ready(fixture_harnesses());
        let models = Loadable::Ready(fixture_models());
        let automatic = reasoning_levels(&models, None, &harnesses, Some(HarnessId::ClaudeCode));
        assert_eq!(
            automatic,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );

        let explicit = reasoning_levels(
            &models,
            Some("gpt-6-astra"),
            &harnesses,
            Some(HarnessId::ClaudeCode),
        );
        assert_eq!(
            explicit,
            vec![
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh
            ]
        );
    }

    #[test]
    fn title_harnesses_include_enabled_claude_code_and_codex() {
        let rows = fixture_harnesses();
        let claude = rows
            .iter()
            .find(|row| row.id == HarnessId::ClaudeCode)
            .unwrap();
        let codex = rows.iter().find(|row| row.id == HarnessId::Codex).unwrap();
        let cursor = rows.iter().find(|row| row.id == HarnessId::Cursor).unwrap();

        assert!(title_harness_is_available(claude));
        assert!(title_harness_is_available(codex));
        assert!(!title_harness_is_available(cursor));
    }

    #[test]
    fn close_dialog_tabs_never_escape_its_two_actions() {
        assert_eq!(close_dialog_tab_target(None, false), 27);
        assert_eq!(close_dialog_tab_target(Some(26), false), 27);
        assert_eq!(close_dialog_tab_target(Some(27), false), 26);
        assert_eq!(close_dialog_tab_target(None, true), 26);
        assert_eq!(close_dialog_tab_target(Some(26), true), 27);
        assert_eq!(close_dialog_tab_target(Some(27), true), 26);
    }

    #[test]
    fn project_continue_requires_an_explicit_target() {
        assert!(!project_target_is_chosen(false, false));
        assert!(project_target_is_chosen(true, false));
        assert!(project_target_is_chosen(false, true));
    }
}
