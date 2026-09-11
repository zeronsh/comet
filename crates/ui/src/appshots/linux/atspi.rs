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
    pub snapshot: AccessibilitySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    object: ObjectRefOwned,
    pid: u32,
    title: String,
}

/// Bound the whole operation, including connection setup and every D-Bus
/// request. Dropping an expired enrichment must not hold the capture service.
async fn within_deadline<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    budget: Duration,
) -> anyhow::Result<T> {
    match futures::future::select(Box::pin(future), Box::pin(async_io::Timer::after(budget))).await
    {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => anyhow::bail!("Accessibility enrichment timed out"),
    }
}

pub(super) async fn capture_window(pid: u32, title: &str) -> anyhow::Result<SemanticCapture> {
    anyhow::ensure!(
        pid != 0 && !title.trim().is_empty(),
        "Missing native window identity"
    );
    within_deadline(
        async {
            let started = Instant::now();
            let connection = AccessibilityConnection::new().await?;
            let (identity, app_name) = select_window(&connection, pid, title).await?;
            let snapshot = traverse(&connection, identity.object.clone(), started).await;
            let (after, _) = select_window(&connection, pid, title).await?;
            anyhow::ensure!(
                identity == after,
                "Accessibility window changed during capture"
            );
            Ok(SemanticCapture { app_name, snapshot })
        },
        DEADLINE,
    )
    .await
}

/// PID comes from the accessibility bus, never from a display name. Require
/// exactly one matching top-level window, with an exact nonempty title, and
/// retain its bus-name/object-path identity across traversal. Duplicate titles
/// and missing metadata intentionally produce screenshot-only captures.
async fn select_window(
    connection: &AccessibilityConnection,
    pid: u32,
    title: &str,
) -> anyhow::Result<(WindowIdentity, String)> {
    let registry = connection.root_accessible_on_registry().await?;
    let bus = atspi::zbus::fdo::DBusProxy::new(connection.connection()).await?;
    let mut matches = Vec::new();
    for application_ref in registry.get_children().await? {
        let Some(name) = application_ref.name() else {
            continue;
        };
        let process = bus
            .get_connection_unix_process_id(name.clone().into())
            .await?;
        if process != pid {
            continue;
        }
        let application = connection.object_as_accessible(&application_ref).await?;
        let app_name = application.name().await?;
        for window_ref in application.get_children().await? {
            let window = connection.object_as_accessible(&window_ref).await?;
            if window.name().await? != title {
                continue;
            }
            let states = window.get_state().await?;
            matches.push((
                WindowIdentity {
                    object: window_ref,
                    pid,
                    title: title.to_owned(),
                },
                app_name.clone(),
                states.contains(State::Active) || states.contains(State::Focused),
            ));
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "Accessibility window identity is ambiguous"
    );
    let (identity, app, active) = matches.pop().unwrap();
    anyhow::ensure!(active, "Accessibility window is no longer active");
    Ok((identity, app))
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

    fn identity(bus: &'static str, path: &'static str, pid: u32, title: &str) -> WindowIdentity {
        WindowIdentity {
            object: ObjectRefOwned::from_static_str_unchecked(bus, path),
            pid,
            title: title.into(),
        }
    }

    #[test]
    fn window_identity_never_uses_fuzzy_titles_or_application_names() {
        let original = identity(":1.2", "/window/1", 10, "a-b");
        assert_ne!(original, identity(":1.3", "/window/1", 20, "a-b"));
        assert_ne!(original, identity(":1.2", "/window/2", 10, "a-b"));
        assert_ne!(original, identity(":1.2", "/window/1", 10, "ab"));
    }

    #[test]
    fn hung_enrichment_releases_the_capture_pipeline() {
        futures::executor::block_on(async {
            assert!(
                within_deadline(
                    futures::future::pending::<anyhow::Result<()>>(),
                    Duration::from_millis(10)
                )
                .await
                .is_err()
            );
            assert_eq!(
                within_deadline(async { Ok(42) }, Duration::from_millis(100))
                    .await
                    .unwrap(),
                42
            );
        });
    }
}
