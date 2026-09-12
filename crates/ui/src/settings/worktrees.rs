//! Settings → Worktrees: inspect and remove isolated session checkouts on
//! each registered device, and pick git worktrees or Rift as the backend.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};

use zeron_proto::{Chat, CheckoutIsolation, CheckoutIsolationStatus, Repo, RepoRef, Space};
use zeron_rpc::methods;

use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone)]
struct WorktreeRow {
    repo_path: String,
    repo_name: String,
    branch: String,
    path: String,
    isolation: CheckoutIsolation,
}

pub struct WorktreesPage {
    state: Entity<AppState>,
    worktrees: Loadable<Vec<WorktreeRow>>,
    isolation: Loadable<CheckoutIsolationStatus>,
    target_device: Option<String>,
    device_menu_open: bool,
    device_menu_pressed_open: bool,
    confirm_delete: Option<String>,
    busy: Option<String>,
    error: Option<String>,
    load_task: Option<Task<()>>,
    delete_task: Option<Task<()>>,
    isolation_task: Option<Task<()>>,
}

impl WorktreesPage {
    fn repos_from_spaces(spaces: &[Space], device_id: Option<&str>) -> Vec<Repo> {
        let mut seen = std::collections::HashSet::new();
        spaces
            .iter()
            .filter(|space| {
                space.git_detected
                    && device_id.is_some_and(|device_id| space.device_id == device_id)
                    && seen.insert(space.path.clone())
            })
            .map(|space| Repo {
                path: space.path.clone(),
                name: space.display_name().to_owned(),
                default_branch: None,
            })
            .collect()
    }

    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            worktrees: Loadable::Idle,
            isolation: Loadable::Idle,
            target_device: None,
            device_menu_open: false,
            device_menu_pressed_open: false,
            confirm_delete: None,
            busy: None,
            error: None,
            load_task: None,
            delete_task: None,
            isolation_task: None,
        };
        page.load(cx);
        page
    }

    fn with_target(target: Option<&str>, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        self.confirm_delete = None;
        self.error = None;
        self.worktrees = Loadable::Idle;
        self.isolation = Loadable::Idle;
        self.load(cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.worktrees = Loadable::Error("Engine not connected".into());
            cx.notify();
            return;
        };
        let target = self.target_device.clone();
        let space_repos = {
            let state = self.state.read(cx);
            let device_id = target.as_deref().or(state.local_device_id.as_deref());
            Self::repos_from_spaces(&state.spaces, device_id)
        };
        self.error = None;
        self.worktrees = Loadable::Loading;
        self.isolation = Loadable::Loading;
        self.load_isolation(cx);
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<Vec<WorktreeRow>, String> = async {
                let value = engine
                    .client()
                    .call(
                        methods::LIST_REPOS,
                        Self::with_target(target.as_deref(), serde_json::json!({})),
                    )
                    .await
                    .map_err(|err| err.to_string())?;
                let mut repos = serde_json::from_value::<Vec<Repo>>(value)
                    .map_err(|err| format!("Could not decode repositories: {err}"))?;
                let mut paths = repos
                    .iter()
                    .map(|repo| repo.path.clone())
                    .collect::<std::collections::HashSet<_>>();
                repos.extend(
                    space_repos
                        .into_iter()
                        .filter(|repo| paths.insert(repo.path.clone())),
                );
                let mut rows = Vec::new();
                for repo in repos {
                    let value = engine
                        .client()
                        .call(
                            methods::LIST_REFS,
                            Self::with_target(
                                target.as_deref(),
                                serde_json::json!({ "repoPath": repo.path }),
                            ),
                        )
                        .await
                        .map_err(|err| format!("{}: {err}", repo.name))?;
                    let refs = serde_json::from_value::<Vec<RepoRef>>(value)
                        .map_err(|err| format!("Could not decode refs for {}: {err}", repo.name))?;
                    rows.extend(refs.into_iter().filter_map(|git_ref| {
                        git_ref.worktree_path.map(|path| WorktreeRow {
                            repo_path: repo.path.clone(),
                            repo_name: repo.name.clone(),
                            branch: git_ref.name,
                            isolation: git_ref.isolation,
                            path,
                        })
                    }));
                }
                rows.sort_by(|a, b| {
                    a.repo_name
                        .cmp(&b.repo_name)
                        .then_with(|| a.branch.cmp(&b.branch))
                        .then_with(|| a.path.cmp(&b.path))
                });
                Ok(rows)
            }
            .await;
            this.update(cx, |page, cx| {
                page.worktrees = match result {
                    Ok(rows) => Loadable::Ready(rows),
                    Err(err) => Loadable::Error(err),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_isolation(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.isolation = Loadable::Error("Engine not connected".into());
            return;
        };
        let params = Self::with_target(self.target_device.as_deref(), serde_json::json!({}));
        self.isolation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_CHECKOUT_ISOLATION, params)
                .await;
            this.update(cx, |page, cx| {
                page.isolation = match result {
                    Ok(value) => match serde_json::from_value::<CheckoutIsolationStatus>(value) {
                        Ok(status) => Loadable::Ready(status),
                        Err(err) => Loadable::Error(format!("Could not decode isolation: {err}")),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_rift(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let isolation = if enabled {
            CheckoutIsolation::Rift
        } else {
            CheckoutIsolation::Git
        };
        let params = Self::with_target(
            self.target_device.as_deref(),
            serde_json::json!({ "isolation": isolation }),
        );
        self.error = None;
        self.isolation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_CHECKOUT_ISOLATION, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<CheckoutIsolationStatus>(value) {
                        Ok(status) => page.isolation = Loadable::Ready(status),
                        Err(err) => page.error = Some(format!("Could not decode isolation: {err}")),
                    },
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn delete(&mut self, row: WorktreeRow, cx: &mut Context<Self>) {
        if self.confirm_delete.as_deref() != Some(row.path.as_str()) {
            self.confirm_delete = Some(row.path);
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let target = self.target_device.clone();
        let path = row.path.clone();
        let params = Self::with_target(
            target.as_deref(),
            serde_json::json!({
                "repoPath": row.repo_path,
                "worktreePath": row.path,
            }),
        );
        self.busy = Some(path);
        self.error = None;
        self.delete_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::DELETE_WORKTREE, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match result {
                    Ok(_) => {
                        page.confirm_delete = None;
                        page.load(cx);
                    }
                    Err(err) => page.error = Some(format!("Delete failed: {err}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn effective_device_id(&self, cx: &Context<Self>) -> Option<String> {
        self.target_device
            .clone()
            .or_else(|| self.state.read(cx).local_device_id.clone())
    }

    fn chat_uses_worktree(chat: &Chat, path: &str) -> bool {
        let worktree = std::path::Path::new(path);
        chat.cwd
            .as_deref()
            .is_some_and(|cwd| std::path::Path::new(cwd).starts_with(worktree))
            || chat
                .harness_session_cwd
                .as_deref()
                .is_some_and(|cwd| std::path::Path::new(cwd).starts_with(worktree))
    }

    fn usage(
        &self,
        path: &str,
        cx: &Context<Self>,
    ) -> (usize, Option<chrono::DateTime<chrono::Utc>>) {
        let device_id = self.effective_device_id(cx);
        let state = self.state.read(cx);
        let mut count = 0usize;
        let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;
        for chat in &state.chats {
            if device_id.as_deref() != Some(chat.device_id.as_str())
                || !Self::chat_uses_worktree(chat, path)
            {
                continue;
            }
            count += 1;
            let used = chat.last_message_at.unwrap_or(chat.created_at);
            latest = Some(latest.map_or(used, |current| current.max(used)));
        }
        (count, latest)
    }

    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|device| Some(device.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|device| device.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|device| device.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let open = self.device_menu_open;
        let mut trigger = div()
            .id("worktrees-device-switcher")
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .bg(if open {
                crate::theme::ink(0.06)
            } else {
                gpui::transparent_black()
            })
            .when(!open, |el| {
                el.hover(|style| style.bg(crate::theme::ink(0.04)))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.device_menu_pressed_open = this.device_menu_open;
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                let pressed_open = std::mem::take(&mut this.device_menu_pressed_open);
                this.device_menu_open = !pressed_open && !this.device_menu_open;
                cx.notify();
            }))
            .child(
                icon(trigger_glyph)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(trigger_label),
            )
            .child(
                icon(icons::SORT_VERTICAL)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
            );

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, device)| {
                    let is_active = Some(device.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(device.id.as_str());
                    let glyph = platform_glyph(&device.platform);
                    let name: SharedString = device.name.clone().into();
                    let pick_id = device.id.clone();
                    popover::menu_row(theme, is_active, format!("worktrees-device-row-{ix}"))
                        .id(("worktrees-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_target_device((!is_local).then(|| pick_id.clone()), cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("worktrees-device-menu", menu, None));
        }
        trigger.into_any_element()
    }

    fn rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let Loadable::Ready(rows) = &self.worktrees else {
            return Vec::new();
        };
        let rows = rows.clone();
        let now = chrono::Utc::now();
        rows.into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let (session_count, latest) = self.usage(&row.path, cx);
                let confirmed = self.confirm_delete.as_deref() == Some(row.path.as_str());
                let busy = self.busy.as_deref() == Some(row.path.as_str());
                let mut meta = vec![
                    div()
                        .child(SharedString::from(row.repo_name.clone()))
                        .into_any_element(),
                    div()
                        .child(SharedString::from(match row.isolation {
                            CheckoutIsolation::Rift => "Rift",
                            CheckoutIsolation::Git => "Git worktree",
                        }))
                        .into_any_element(),
                    div()
                        .child(SharedString::from(row.path.clone()))
                        .into_any_element(),
                ];
                if let Some(latest) = latest {
                    meta.push(
                        div()
                            .child(SharedString::from(format!(
                                "last used {}",
                                crate::state::format_time_ago(latest, now)
                            )))
                            .into_any_element(),
                    );
                }
                let action_row = row.clone();
                let action = widgets::ghost_action(&theme)
                    .id(("worktree-delete", ix))
                    .when(!busy, |el| {
                        el.hover(|style| widgets::ghost_hover(&theme, style))
                    })
                    .when(confirmed, |el| el.text_color(theme.danger_muted))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.delete(action_row.clone(), cx);
                        }))
                    })
                    .child(
                        crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                            .size(px(14.0))
                            .text_color(if confirmed {
                                theme.danger_muted
                            } else {
                                theme.text_muted
                            }),
                    )
                    .child(SharedString::from(if busy {
                        "Deleting…"
                    } else if confirmed {
                        "Confirm delete"
                    } else {
                        "Delete"
                    }));

                widgets::card_row(&theme, ix == 0)
                    .child(widgets::row_tile(&theme, crate::icons::FOLDER_WITH_FILES))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, row.branch.clone()))
                            .child(widgets::meta_line(&theme, meta)),
                    )
                    .child(if session_count == 0 {
                        widgets::badge(&theme, "Unused").into_any_element()
                    } else {
                        widgets::badge(
                            &theme,
                            if session_count == 1 {
                                "1 session".to_string()
                            } else {
                                format!("{session_count} sessions")
                            },
                        )
                        .into_any_element()
                    })
                    .child(action)
                    .into_any_element()
            })
            .collect()
    }

    fn render_isolation_card(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (rift_on, rift_supported, isolation_error) = match &self.isolation {
            Loadable::Ready(status) => (
                status.isolation == CheckoutIsolation::Rift,
                status.rift_supported,
                None,
            ),
            Loadable::Error(message) => (false, false, Some(message.clone())),
            Loadable::Idle | Loadable::Loading => (false, false, None),
        };
        let interactive = isolation_error.is_none() && (rift_on || rift_supported);
        let description = if let Some(message) = isolation_error {
            message
        } else if rift_supported {
            "Use Rift instead of git worktrees for new isolated sessions. Existing checkouts stay as they are. Needs btrfs, a Linux filesystem with reflinks, or APFS. On btrfs, the first clone converts the project folder into a subvolume.".into()
        } else {
            "Install the rift CLI to enable this. Needs btrfs, a Linux filesystem with reflinks, or APFS. New isolated sessions keep using git worktrees until then.".into()
        };
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, crate::icons::FOLDER_WITH_FILES))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Use Rift"))
                            .child(widgets::meta_line(
                                theme,
                                vec![div().child(description).into_any_element()],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(theme, rift_on)
                            .id("worktrees-rift-toggle")
                            .when(interactive, |el| {
                                el.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_rift(!rift_on, cx);
                                    }))
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for WorktreesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let count = match &self.worktrees {
            Loadable::Ready(rows) => Some(rows.len()),
            _ => None,
        };
        let body: AnyElement = match &self.worktrees {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "worktrees-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => div()
                .child(widgets::error_strip(&theme, message.clone()))
                .child(
                    widgets::ghost_action(&theme)
                        .id("worktrees-retry")
                        .mt(px(8.0))
                        .hover(|style| widgets::ghost_hover(&theme, style))
                        .on_click(cx.listener(|page, _, _, cx| page.load(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(rows) if rows.is_empty() => widgets::section_card(&theme)
                .p(px(20.0))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("No isolated checkouts on this device."))
                .into_any_element(),
            Loadable::Ready(_) => widgets::section_card(&theme)
                .children(self.rows(cx))
                .into_any_element(),
        };
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());
        let switcher = self.render_device_switcher(&theme, cx);
        let isolation_card = self.render_isolation_card(&theme, cx);

        div()
            .id("worktrees-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(widgets::page_header(&theme, "Worktrees", count))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id("worktrees-refresh")
                                            .hover(|style| widgets::ghost_hover(&theme, style))
                                            .on_click(cx.listener(|page, _, _, cx| page.load(cx)))
                                            .child(
                                                crate::icons::icon(crate::icons::REFRESH)
                                                    .size(px(14.0)),
                                            )
                                            .child(SharedString::from("Refresh")),
                                    )
                                    .child(switcher),
                            ),
                    )
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Isolated session checkouts can accumulate as sessions create them. \
                             Git worktrees are the default. Rift is an optional copy-on-write clone \
                             for filesystems that can do it.",
                        )
                        .max_w(px(560.0))
                        .line_height(px(20.0)),
                    )
                    .child(isolation_card)
                    .children(error)
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn space(device_id: &str, path: &str, git_detected: bool) -> Space {
        Space {
            id: format!("{device_id}:{path}"),
            device_id: device_id.to_owned(),
            path: path.to_owned(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn worktree_discovery_uses_git_spaces_for_the_selected_device() {
        let spaces = vec![
            space("omicron", "/code/AbiNotes", true),
            space("omicron", "/code/not-a-repo", false),
            space("phone", "/code/mobile", true),
        ];

        let repos = WorktreesPage::repos_from_spaces(&spaces, Some("omicron"));

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].path, "/code/AbiNotes");
        assert_eq!(repos[0].name, "AbiNotes");
    }

    #[test]
    fn worktree_discovery_deduplicates_spaces_by_path() {
        let spaces = vec![
            space("omicron", "/code/AbiNotes", true),
            space("omicron", "/code/AbiNotes", true),
        ];

        let repos = WorktreesPage::repos_from_spaces(&spaces, Some("omicron"));

        assert_eq!(repos.len(), 1);
    }
}
