use std::collections::VecDeque;
use std::time::{Duration, Instant};

use atspi::connection::P2P;
use atspi::proxy::text::TextProxy;
use atspi::{AccessibilityConnection, Interface, ObjectRefOwned, Role, State};

use crate::appshots::AccessibilitySnapshot;

const MAX_DEPTH: usize = 24;
const MAX_NODES: usize = 1_500;
const MAX_BYTES: usize = 96 * 1024;
const MAX_TEXT_CHARS: i32 = 4_096;
const DEADLINE: Duration = Duration::from_millis(900);

pub(super) struct SemanticCapture {
    pub app_name: String,
    pub window_title: Option<String>,
    pub snapshot: AccessibilitySnapshot,
}

impl SemanticCapture {
    pub(super) fn same_window(&self, other: &Self) -> bool {
        normalized(&self.app_name) == normalized(&other.app_name)
            && match (&self.window_title, &other.window_title) {
                (Some(left), Some(right)) => normalized(left) == normalized(right),
                (None, None) => true,
                _ => false,
            }
    }

    pub(super) fn matches_x11(&self, wm_class: Option<&str>, title: Option<&str>) -> bool {
        if let (Some(expected), Some(actual)) = (title, self.window_title.as_deref()) {
            return normalized(expected) == normalized(actual);
        }
        wm_class.is_some_and(|class| {
            let class = normalized(class);
            let app = normalized(&self.app_name);
            !class.is_empty() && (class.contains(&app) || app.contains(&class))
        })
    }
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) async fn capture_focused() -> anyhow::Result<SemanticCapture> {
    let started = Instant::now();
    let connection = AccessibilityConnection::new().await?;
    let registry = connection.root_accessible_on_registry().await?;
    let applications = registry.get_children().await?;

    let mut selected = None;
    for application_ref in applications {
        if started.elapsed() >= DEADLINE {
            break;
        }
        let application = match connection.object_as_accessible(&application_ref).await {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };
        let app_name = application
            .name()
            .await
            .unwrap_or_else(|_| "Linux application".into());
        let app_active = application
            .get_state()
            .await
            .is_ok_and(|state| state.contains(State::Active) || state.contains(State::Focused));
        let windows = application.get_children().await.unwrap_or_default();
        for window_ref in &windows {
            let window = match connection.object_as_accessible(window_ref).await {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            let active = window
                .get_state()
                .await
                .is_ok_and(|state| state.contains(State::Active) || state.contains(State::Focused));
            if active {
                let title = window
                    .name()
                    .await
                    .ok()
                    .filter(|value| !value.trim().is_empty());
                selected = Some((app_name.clone(), title, window_ref.clone()));
                break;
            }
        }
        if selected.is_none() && app_active {
            let target = windows.first().cloned().unwrap_or(application_ref);
            selected = Some((app_name, None, target));
        }
        if selected.is_some() {
            break;
        }
    }

    let (app_name, window_title, root) = selected
        .ok_or_else(|| anyhow::anyhow!("AT-SPI did not expose an active application window"))?;
    let snapshot = traverse(&connection, root, started).await;
    Ok(SemanticCapture {
        app_name,
        window_title,
        snapshot,
    })
}

async fn traverse(
    connection: &AccessibilityConnection,
    root: ObjectRefOwned,
    started: Instant,
) -> AccessibilitySnapshot {
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut output = String::new();
    let mut nodes = 0_usize;
    let mut truncated = false;

    while let Some((object_ref, depth)) = queue.pop_front() {
        if depth > MAX_DEPTH
            || nodes >= MAX_NODES
            || output.len() >= MAX_BYTES
            || started.elapsed() >= DEADLINE
        {
            truncated = true;
            break;
        }
        nodes += 1;
        let accessible = match connection.object_as_accessible(&object_ref).await {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };
        let role = accessible.get_role().await.unwrap_or(Role::Invalid);
        let name = accessible.name().await.unwrap_or_default();
        let description = accessible.description().await.unwrap_or_default();
        let interfaces = accessible.get_interfaces().await.ok();
        let text = if role != Role::PasswordText
            && interfaces.is_some_and(|set| set.contains(Interface::Text))
        {
            read_text(&accessible).await.unwrap_or_default()
        } else {
            String::new()
        };
        let mut fields = Vec::new();
        if !name.trim().is_empty() {
            fields.push(clean(&name));
        }
        if !description.trim().is_empty() && description.trim() != name.trim() {
            fields.push(clean(&description));
        }
        if !text.trim().is_empty() && text.trim() != name.trim() {
            fields.push(clean(&text));
        }
        if !fields.is_empty() {
            let line = format!(
                "{}{}: {}\n",
                "  ".repeat(depth),
                role.name(),
                fields.join(" | ")
            );
            if output.len() + line.len() > MAX_BYTES {
                truncated = true;
                break;
            }
            output.push_str(&line);
        }
        if let Ok(children) = accessible.get_children().await {
            queue.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }

    AccessibilitySnapshot {
        format_version: 1,
        content: output,
        truncated,
    }
}

async fn read_text(
    accessible: &atspi::proxy::accessible::AccessibleProxy<'_>,
) -> anyhow::Result<String> {
    let text = TextProxy::builder(accessible.inner().connection())
        .destination(accessible.inner().destination().clone())?
        .path(accessible.inner().path().clone())?
        .build()
        .await?;
    let count = text.character_count().await?.clamp(0, MAX_TEXT_CHARS);
    Ok(text.get_text(0, count).await?)
}

fn clean(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TEXT_CHARS as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic(app: &str, title: Option<&str>) -> SemanticCapture {
        SemanticCapture {
            app_name: app.into(),
            window_title: title.map(str::to_string),
            snapshot: AccessibilitySnapshot::unavailable(),
        }
    }

    #[test]
    fn identity_requires_the_same_application_and_window() {
        assert!(
            semantic("Firefox", Some("Issue 216"))
                .same_window(&semantic("firefox", Some("Issue 216")))
        );
        assert!(
            !semantic("Firefox", Some("Issue 216"))
                .same_window(&semantic("Terminal", Some("Issue 216")))
        );
        assert!(
            !semantic("Firefox", Some("Issue 216"))
                .same_window(&semantic("Firefox", Some("Passwords")))
        );
    }
}
